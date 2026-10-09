//! Private writer listener. Started only after this process holds the lease.

use crate::state::BotState;
use jmodguard::control::{self, NonceWindow};
use jmodguard::db::{self, ControlReply};
use std::io::Write;
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const QUEUE: usize = 8;

struct Job {
    op: db::ControlOp,
    actor_user_id: i64,
    expires_ms: i64,
    reply: std::sync::mpsc::Sender<ControlReply>,
}

pub fn spawn(app: Arc<BotState>) -> anyhow::Result<Option<std::net::SocketAddr>> {
    let key = match control::load_writer_key(app.cfg.bot.control_secret.as_str()) {
        Ok(key) => key,
        Err(error) => {
            tracing::error!("{error}");
            return Ok(None);
        }
    };
    let bind = control::parse_private_bind(&app.cfg.bot.control_bind)?;
    let listener =
        TcpListener::bind(bind).map_err(|_| anyhow::anyhow!("control bind is not private"))?;
    let bound = listener
        .local_addr()
        .map_err(|_| anyhow::anyhow!("control bind is not private"))?;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Job>(QUEUE);
    let worker = Arc::clone(&app);
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            while worker.activation.moderation_depth() > 0 && worker.activation.may_record() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let reply = if !worker.activation.may_record() {
                ControlReply::Waiting
            } else if control::now_ms() > job.expires_ms {
                ControlReply::Failed {
                    message: "request expired".into(),
                }
            } else {
                let decision = worker
                    .authorize_control_actor(&job.op, job.actor_user_id)
                    .await;
                if !worker.activation.may_record() {
                    ControlReply::Waiting
                } else if control::now_ms() > job.expires_ms {
                    ControlReply::Failed {
                        message: "request expired".into(),
                    }
                } else if let Some(rejected) = reject_unauthorized_control(decision) {
                    rejected
                } else {
                    match db::apply_control(
                        &worker.db,
                        Some(&worker.tenant),
                        job.op,
                        Some(worker.activation.instance()),
                    )
                    .await
                    {
                        Ok(reply) => reply,
                        Err(error) => ControlReply::Failed {
                            message: control::public_error(&error),
                        },
                    }
                }
            };
            let _ = job.reply.send(reply);
        }
    });
    let nonces = Arc::new(Mutex::new(NonceWindow::new()));
    let inflight = Arc::new(AtomicUsize::new(0));
    let activation = Arc::clone(&app.activation);
    std::thread::Builder::new()
        .name("writer-link".into())
        .spawn(move || accept_loop(listener, key, tx, nonces, inflight, activation))
        .map_err(|_| anyhow::anyhow!("control bind is not private"))?;
    Ok(Some(bound))
}

/// `Some` means the actor check failed and apply must not run.
fn reject_unauthorized_control(decision: Result<(), &'static str>) -> Option<ControlReply> {
    match decision {
        Err(message) => Some(ControlReply::Failed {
            message: message.to_owned(),
        }),
        Ok(()) => None,
    }
}

fn accept_loop(
    listener: TcpListener,
    key: Vec<u8>,
    tx: tokio::sync::mpsc::Sender<Job>,
    nonces: Arc<Mutex<NonceWindow>>,
    inflight: Arc<AtomicUsize>,
    activation: Arc<crate::activation::Activation>,
) {
    loop {
        let Ok((stream, _)) = listener.accept() else {
            break;
        };
        if inflight.load(Ordering::Relaxed) >= 32 {
            let _ = reply(stream, &ControlReply::Busy);
            continue;
        }
        inflight.fetch_add(1, Ordering::Relaxed);
        let tx = tx.clone();
        let key = key.clone();
        let nonces = Arc::clone(&nonces);
        let inflight_conn = Arc::clone(&inflight);
        let activation = Arc::clone(&activation);
        if std::thread::Builder::new()
            .name("writer-link-conn".into())
            .spawn(move || {
                handle(stream, &key, &tx, &nonces, &activation);
                inflight_conn.fetch_sub(1, Ordering::Relaxed);
            })
            .is_err()
        {
            inflight.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

fn handle(
    mut stream: std::net::TcpStream,
    key: &[u8],
    tx: &tokio::sync::mpsc::Sender<Job>,
    nonces: &Mutex<NonceWindow>,
    activation: &Arc<crate::activation::Activation>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let body = match control::read_frame(&mut stream) {
        Ok(body) => body,
        Err(_) => return,
    };
    let (op, actor_user_id, expires_ms) = match control::open_frame(key, &body, nonces) {
        Ok(opened) => opened,
        Err(error) => {
            let _ = reply(
                stream,
                &ControlReply::Failed {
                    message: control::public_error(&error),
                },
            );
            return;
        }
    };
    let Some(_hold) = activation.hold_control() else {
        let _ = reply(stream, &ControlReply::Waiting);
        return;
    };
    let (send, recv) = std::sync::mpsc::channel();
    if tx
        .try_send(Job {
            op,
            actor_user_id,
            expires_ms,
            reply: send,
        })
        .is_err()
    {
        let _ = reply(stream, &ControlReply::Busy);
        return;
    }
    let done = match recv.recv_timeout(Duration::from_secs(30)) {
        Ok(done) => done,
        Err(_) => ControlReply::StillWaiting,
    };
    let _ = reply(stream, &done);
}

fn reply(mut stream: std::net::TcpStream, done: &ControlReply) -> anyhow::Result<()> {
    control::write_frame(&mut stream, done)?;
    stream.flush().ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejected_actor_is_not_applied() {
        let mut applied = false;
        let reply = match reject_unauthorized_control(Err("actor is not allowed")) {
            Some(reply) => reply,
            None => {
                applied = true;
                ControlReply::Saved {
                    body: serde_json::Value::Null,
                }
            }
        };
        assert!(!applied);
        assert!(
            matches!(reply, ControlReply::Failed { message } if message == "actor is not allowed")
        );
        let mut applied = false;
        let reply = match reject_unauthorized_control(Ok(())) {
            Some(reply) => reply,
            None => {
                applied = true;
                ControlReply::Saved {
                    body: serde_json::Value::Null,
                }
            }
        };
        assert!(applied);
        assert!(matches!(reply, ControlReply::Saved { .. }));
    }

    use crate::config::BotProcessConfig;
    use crate::state::{BotState, ControlMemberFetch};
    use serenity::all::{Cache, Guild, GuildCreateEvent, Member, Permissions, Role};
    use serenity::model::id::{GuildId, RoleId, UserId};
    use std::io::Write;
    use std::net::TcpStream;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn scope(enrollment: &db::GuildEnrollment) -> db::ScopeWire {
        db::ScopeWire {
            global: false,
            guild_id: enrollment.guild_id,
            enrollment_id: enrollment.id.clone(),
            joined_at_ns: enrollment.joined_at_ns,
        }
    }

    fn patch(enrollment: &db::GuildEnrollment, prefix: &str) -> db::ControlOp {
        db::ControlOp::PatchSettings {
            scope: scope(enrollment),
            prefix: db::FieldPatch::Set(prefix.into()),
            bot_name: db::FieldPatch::Absent,
            reconcile: false,
        }
    }

    async fn writer_bot() -> (Arc<BotState>, std::path::PathBuf) {
        let path =
            std::env::temp_dir().join(format!("control_actor_{:032x}.db", rand::random::<u128>()));
        let database = db::Db::open(&path, 2).unwrap();
        let mut cfg = BotProcessConfig::default();
        cfg.bot.control_bind = "127.0.0.1:0".into();
        cfg.bot.control_secret = jmodguard::config::ControlSecret::new("x".repeat(32));
        let bot = BotState::new(cfg, database).unwrap();
        bot.activation.mark_lock_held(true);
        assert!(
            db::claim_active_bot(
                &bot.db,
                bot.activation.instance(),
                control::now_ms(),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap()
        );
        (bot, path)
    }

    fn cleanup(path: &std::path::Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        crate::tenant::remove_sidecars(path);
    }

    fn submit(addr: std::net::SocketAddr, frame: Vec<u8>) -> ControlReply {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(8)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(8)))
            .unwrap();
        stream.write_all(&frame).unwrap();
        let body = control::read_frame(&mut stream).unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn key() -> Vec<u8> {
        control::load_writer_key(&"x".repeat(32)).unwrap()
    }

    fn seal(op: &db::ControlOp, actor: i64) -> Vec<u8> {
        control::seal_with_expiry(&key(), op, actor, control::now_ms().saturating_add(60_000))
            .unwrap()
    }

    fn failure_message(reply: &ControlReply) -> &str {
        match reply {
            ControlReply::Failed { message } => message,
            other => panic!("expected a failure reply, got {other:?}"),
        }
    }

    fn remember_guild(cache: &Cache, guild_id: u64, member: Option<Member>) {
        let mut guild = Guild::default();
        guild.id = GuildId::new(guild_id);
        guild.owner_id = UserId::new(1);
        let mut role = Role::default();
        role.id = RoleId::new(20);
        role.guild_id = guild.id;
        role.permissions = Permissions::MANAGE_GUILD;
        guild.roles.insert(role.id, role);
        if let Some(member) = member {
            guild.members.insert(member.user.id, member);
        }
        let mut event: GuildCreateEvent =
            serde_json::from_value(serde_json::to_value(guild).unwrap()).unwrap();
        cache.update(&mut event);
    }

    fn actor_member(guild_id: u64, allowed: bool) -> Member {
        let mut member = Member::default();
        member.user.id = UserId::new(7);
        member.guild_id = GuildId::new(guild_id);
        if allowed {
            member.roles = vec![RoleId::new(20)];
        }
        member
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cached_admin_is_allowed_without_a_member_lookup() {
        let (bot, path) = writer_bot().await;
        let enrollment = db::enroll_guild(&bot.db, 10, 100).await.unwrap().unwrap();
        let cache = Arc::new(Cache::new());
        remember_guild(&cache, 10, Some(actor_member(10, true)));
        bot.install_discord_cache(cache);
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        bot.install_member_lookup(move |_, _| {
            seen.fetch_add(1, Ordering::SeqCst);
            async { ControlMemberFetch::Unavailable }
        });
        let bound = spawn(Arc::clone(&bot)).unwrap().unwrap();
        let reply = submit(bound, seal(&patch(&enrollment, "!saved"), 7));
        assert!(matches!(reply, ControlReply::Saved { .. }), "{reply:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            db::get_settings(&bot.db, 10).await.unwrap().prefix,
            "!saved"
        );
        drop(bot);
        cleanup(&path);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn uncached_admin_found_by_lookup_is_allowed() {
        let (bot, path) = writer_bot().await;
        let enrollment = db::enroll_guild(&bot.db, 10, 100).await.unwrap().unwrap();
        let cache = Arc::new(Cache::new());
        remember_guild(&cache, 10, None);
        bot.install_discord_cache(cache);
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        bot.install_member_lookup(move |guild, user| {
            seen.fetch_add(1, Ordering::SeqCst);
            async move {
                assert_eq!((guild, user), (10, 7));
                ControlMemberFetch::Found(Box::new(actor_member(guild, true)))
            }
        });
        let bound = spawn(Arc::clone(&bot)).unwrap().unwrap();
        let reply = submit(bound, seal(&patch(&enrollment, "!saved"), 7));
        assert!(matches!(reply, ControlReply::Saved { .. }), "{reply:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            db::get_settings(&bot.db, 10).await.unwrap().prefix,
            "!saved"
        );
        drop(bot);
        cleanup(&path);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn revoked_uncached_actor_is_denied_and_not_applied() {
        let (bot, path) = writer_bot().await;
        let enrollment = db::enroll_guild(&bot.db, 10, 100).await.unwrap().unwrap();
        let cache = Arc::new(Cache::new());
        remember_guild(&cache, 10, None);
        bot.install_discord_cache(cache);
        bot.install_member_lookup(|guild, _| async move {
            ControlMemberFetch::Found(Box::new(actor_member(guild, false)))
        });
        let bound = spawn(Arc::clone(&bot)).unwrap().unwrap();
        let reply = submit(bound, seal(&patch(&enrollment, "!revoked"), 7));
        assert_eq!(failure_message(&reply), "actor is not allowed");
        assert_ne!(
            db::get_settings(&bot.db, 10).await.unwrap().prefix,
            "!revoked"
        );
        drop(bot);
        cleanup(&path);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn member_lookup_timeout_is_not_applied() {
        let (bot, path) = writer_bot().await;
        let enrollment = db::enroll_guild(&bot.db, 10, 100).await.unwrap().unwrap();
        let cache = Arc::new(Cache::new());
        remember_guild(&cache, 10, None);
        bot.install_discord_cache(cache);
        bot.install_member_lookup(|_, _| async {
            tokio::time::sleep(Duration::from_secs(3)).await;
            ControlMemberFetch::Unavailable
        });
        let bound = spawn(Arc::clone(&bot)).unwrap().unwrap();
        let reply = submit(bound, seal(&patch(&enrollment, "!revoked"), 7));
        assert_eq!(
            failure_message(&reply),
            "actor permission could not be confirmed"
        );
        assert_ne!(
            db::get_settings(&bot.db, 10).await.unwrap().prefix,
            "!revoked"
        );
        drop(bot);
        cleanup(&path);
    }

    #[tokio::test]
    async fn copy_does_not_apply_when_the_source_permission_is_revoked() {
        let (bot, path) = writer_bot().await;
        let source = db::enroll_guild(&bot.db, 10, 100).await.unwrap().unwrap();
        let destination = db::enroll_guild(&bot.db, 11, 100).await.unwrap().unwrap();
        db::patch_settings(&bot.db, 10, Some(Some("!from".into())), None, None)
            .await
            .unwrap();
        db::patch_settings(&bot.db, 11, Some(Some("!keep".into())), None, None)
            .await
            .unwrap();
        let cache = Arc::new(Cache::new());
        remember_guild(&cache, 10, None);
        remember_guild(&cache, 11, None);
        bot.install_discord_cache(cache);
        let source_allowed = Arc::new(AtomicBool::new(true));
        let allowed = Arc::clone(&source_allowed);
        bot.install_member_lookup(move |guild, _| {
            let permitted = guild != 10 || allowed.load(Ordering::SeqCst);
            async move { ControlMemberFetch::Found(Box::new(actor_member(guild, permitted))) }
        });
        let bound = spawn(Arc::clone(&bot)).unwrap().unwrap();
        let hold = bot.activation.hold_rule();
        let op = db::ControlOp::CopyGuildConfig {
            source: scope(&source),
            destination: scope(&destination),
            sections: vec!["settings".into()],
        };
        let frame = seal(&op, 7);
        let pending = tokio::task::spawn_blocking(move || submit(bound, frame));
        tokio::time::sleep(Duration::from_millis(80)).await;
        source_allowed.store(false, Ordering::SeqCst);
        drop(hold);
        let reply = pending.await.unwrap();
        assert_eq!(failure_message(&reply), "actor is not allowed");
        assert_eq!(db::get_settings(&bot.db, 11).await.unwrap().prefix, "!keep");
        drop(bot);
        cleanup(&path);
    }

    #[tokio::test]
    async fn rule_upsert_does_not_apply_when_the_request_guild_permission_is_revoked() {
        let path =
            std::env::temp_dir().join(format!("control_actor_{:032x}.db", rand::random::<u128>()));
        let database = db::Db::open(&path, 2).unwrap();
        let mut cfg = BotProcessConfig::default();
        cfg.bot.control_bind = "127.0.0.1:0".into();
        cfg.bot.control_secret = jmodguard::config::ControlSecret::new("x".repeat(32));
        cfg.bot.owner_id = 7;
        let bot = BotState::new(cfg, database).unwrap();
        bot.activation.mark_lock_held(true);
        assert!(
            db::claim_active_bot(
                &bot.db,
                bot.activation.instance(),
                control::now_ms(),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap()
        );
        let enrollment = db::enroll_guild(&bot.db, 10, 100).await.unwrap().unwrap();
        let cache = Arc::new(Cache::new());
        remember_guild(&cache, 10, None);
        bot.install_discord_cache(cache);
        let source_allowed = Arc::new(AtomicBool::new(true));
        let allowed = Arc::clone(&source_allowed);
        bot.install_member_lookup(move |guild, _| {
            let permitted = guild != 10 || allowed.load(Ordering::SeqCst);
            async move { ControlMemberFetch::Found(Box::new(actor_member(guild, permitted))) }
        });
        let bound = spawn(Arc::clone(&bot)).unwrap().unwrap();
        let hold = bot.activation.hold_rule();
        let op = db::ControlOp::UpsertRule {
            request: scope(&enrollment),
            target: db::ScopeWire {
                global: true,
                guild_id: 0,
                enrollment_id: String::new(),
                joined_at_ns: 0,
            },
            id: "spam".into(),
            rule: crate::types::RuleDef {
                name: "spam".into(),
                description: String::new(),
                enabled: true,
                dry_run: false,
                params: serde_json::Value::Null,
                actions: Vec::new(),
                whitelist_roles: Vec::new(),
                whitelist_channels: Vec::new(),
                whitelist_users: Vec::new(),
            },
        };
        let frame = seal(&op, 7);
        let pending = tokio::task::spawn_blocking(move || submit(bound, frame));
        tokio::time::sleep(Duration::from_millis(80)).await;
        source_allowed.store(false, Ordering::SeqCst);
        drop(hold);
        let reply = pending.await.unwrap();
        assert_eq!(failure_message(&reply), "actor is not allowed");
        assert!(db::get_rule(&bot.db, 0, "spam").await.unwrap().is_none());
        drop(bot);
        cleanup(&path);
    }

    #[tokio::test]
    async fn queued_control_expires_before_apply() {
        let (bot, path) = writer_bot().await;
        let enrollment = db::enroll_guild(&bot.db, 10, 100).await.unwrap().unwrap();
        bot.install_control_actor_lookup(|actor, _| {
            (actor == 7).then_some(db::CachedGuildActor {
                guild_owner_id: 0,
                permissions: 1 << 5,
            })
        });
        let bound = spawn(Arc::clone(&bot)).unwrap().unwrap();
        let hold = bot.activation.hold_rule();
        let frame = control::seal_with_expiry(
            &key(),
            &patch(&enrollment, "!late"),
            7,
            control::now_ms().saturating_add(250),
        )
        .unwrap();
        let pending = tokio::task::spawn_blocking(move || submit(bound, frame));
        tokio::time::sleep(Duration::from_millis(500)).await;
        drop(hold);
        let reply = pending.await.unwrap();
        assert_eq!(failure_message(&reply), "request expired");
        assert_ne!(db::get_settings(&bot.db, 10).await.unwrap().prefix, "!late");
        drop(bot);
        cleanup(&path);
    }
}
