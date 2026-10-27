//! Optional moderator reason DM before kick or ban.

use serenity::model::id::UserId;
use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;

use super::CmdEnv;
use crate::types::Behavior;

pub(crate) const MOD_REASON_DM_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModReasonDmOutcome {
    Sent,
    NotSent,
    Uncertain,
    Cancelled,
}

pub(crate) fn take_prefix_dm_flag(parts: &mut Vec<String>) -> bool {
    let mut dm = false;
    parts.retain(|part| {
        if part == "--dm" {
            dm = true;
            false
        } else {
            true
        }
    });
    dm
}

pub(crate) fn slash_dm_flag(options: &[(String, String)]) -> bool {
    options
        .iter()
        .any(|(name, value)| name == "dm" && value == "true")
}

pub(crate) fn slash_option_excluded_from_args(name: &str) -> bool {
    name == "dm"
}

pub(crate) fn command_supports_reason_dm(behavior: &Behavior) -> bool {
    matches!(behavior, Behavior::Kick { .. } | Behavior::Ban { .. })
}

pub(crate) fn classify_mod_reason_dm_delivery_error(error: &serenity::Error) -> ModReasonDmOutcome {
    match crate::cases::classify_discord(Some(error)) {
        crate::db::guild_data::CaseOutcome::Failed => ModReasonDmOutcome::NotSent,
        _ => ModReasonDmOutcome::Uncertain,
    }
}

pub(crate) async fn bounded_mod_reason_dm_future<F>(future: F) -> ModReasonDmOutcome
where
    F: Future<Output = ModReasonDmOutcome>,
{
    match tokio::time::timeout(MOD_REASON_DM_TIMEOUT, future).await {
        Ok(outcome) => outcome,
        Err(_) => ModReasonDmOutcome::Uncertain,
    }
}

pub(crate) fn mod_reason_dm_vars(
    guild: &str,
    action: &str,
    reason: &str,
    actor: &str,
) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    vars.insert("guild".into(), guild.into());
    vars.insert("action".into(), action.into());
    vars.insert("reason".into(), reason.into());
    vars.insert("actor".into(), actor.into());
    vars
}

pub(crate) fn render_mod_reason_dm_body(vars: &BTreeMap<String, String>) -> String {
    jmodguard::templates::render_bounded(jmodguard::defaults::MOD_REASON_DM_TEMPLATE, vars, 2000)
}

/// Production DM text preparation: strips ban attempt tags before render.
pub(crate) fn render_mod_reason_dm_body_for_reason(
    guild: &str,
    action: &str,
    reason: &str,
    actor: &str,
) -> String {
    let clean = jmodguard::ban_reason::without_tag(reason);
    let vars = mod_reason_dm_vars(guild, action, clean, actor);
    render_mod_reason_dm_body(&vars)
}

pub(crate) fn render_mod_reason_dm_clause(outcome: ModReasonDmOutcome) -> String {
    let status = match outcome {
        ModReasonDmOutcome::Sent => jmodguard::defaults::MOD_REASON_DM_STATUS_SENT,
        ModReasonDmOutcome::NotSent => jmodguard::defaults::MOD_REASON_DM_STATUS_NOT_SENT,
        ModReasonDmOutcome::Uncertain => jmodguard::defaults::MOD_REASON_DM_STATUS_UNCERTAIN,
        ModReasonDmOutcome::Cancelled => return String::new(),
    };
    let mut vars = BTreeMap::new();
    vars.insert("dm_status".into(), status.into());
    jmodguard::templates::render(jmodguard::defaults::MOD_REASON_DM_CLAUSE_TEMPLATE, &vars)
}

pub(crate) async fn send_mod_reason_dm(
    env: &CmdEnv<'_>,
    target: UserId,
    action: &str,
    reason: &str,
) -> ModReasonDmOutcome {
    bounded_mod_reason_dm_future(send_mod_reason_dm_inner(env, target, action, reason)).await
}

async fn send_mod_reason_dm_inner(
    env: &CmdEnv<'_>,
    target: UserId,
    action: &str,
    reason: &str,
) -> ModReasonDmOutcome {
    let guild = env
        .ctx
        .cache
        .guild(env.guild_id)
        .map(|g| g.name.clone())
        .unwrap_or_else(|| env.guild_id.to_string());
    let actor = format!("{} (<@{}>)", env.actor_member.display_name(), env.actor);
    let text = render_mod_reason_dm_body_for_reason(&guild, action, reason, &actor);

    let channel = match crate::discord::race_discord_call(
        &env.app.activation,
        env.ctx
            .http
            .create_private_channel(&serde_json::json!({"recipient_id": target.to_string()})),
    )
    .await
    {
        crate::discord::DiscordCall::Cancelled => return ModReasonDmOutcome::Cancelled,
        crate::discord::DiscordCall::Finished(Err(error)) => {
            return classify_mod_reason_dm_delivery_error(&error);
        }
        crate::discord::DiscordCall::Finished(Ok(channel)) => channel,
    };

    match crate::discord::race_discord_call(&env.app.activation, channel.say(&env.ctx.http, &text))
        .await
    {
        crate::discord::DiscordCall::Cancelled => ModReasonDmOutcome::Cancelled,
        crate::discord::DiscordCall::Finished(Ok(_)) => ModReasonDmOutcome::Sent,
        crate::discord::DiscordCall::Finished(Err(error)) => {
            classify_mod_reason_dm_delivery_error(&error)
        }
    }
}

#[cfg(test)]
async fn fixture_context_for_dm_order_test(
    http: std::sync::Arc<serenity::http::Http>,
    cache: std::sync::Arc<serenity::cache::Cache>,
) -> serenity::client::Context {
    use std::sync::Arc;
    use serenity::client::Context;
    use serenity::gateway::{
        Shard, ShardManager, ShardManagerOptions, ShardMessenger, ShardRunner, ShardRunnerOptions,
    };
    use serenity::model::gateway::{GatewayIntents, ShardInfo};
    use serenity::model::id::ShardId;
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let url = Arc::new(tokio::sync::Mutex::new(format!(
        "ws://{}",
        listener.local_addr().unwrap()
    )));
    let accept = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        tokio_tungstenite::accept_async(stream).await.unwrap()
    });
    let intents = GatewayIntents::empty();
    let shard = Shard::new(
        Arc::clone(&url),
        "fixture",
        ShardInfo {
            id: ShardId(0),
            total: 1,
        },
        intents,
        None,
    )
    .await
    .unwrap();
    let socket = accept.await.unwrap();
    let data = Arc::default();
    let (manager, _) = ShardManager::new(ShardManagerOptions {
        data: Arc::clone(&data),
        event_handlers: vec![],
        raw_event_handlers: vec![],
        shard_index: 0,
        shard_init: 0,
        shard_total: 1,
        ws_url: url,
        cache: Arc::clone(&cache),
        http: Arc::clone(&http),
        intents,
        presence: None,
    });
    let runner = ShardRunner::new(ShardRunnerOptions {
        data: Arc::clone(&data),
        event_handlers: vec![],
        raw_event_handlers: vec![],
        manager: Arc::clone(&manager),
        shard,
        cache: Arc::clone(&cache),
        http: Arc::clone(&http),
    });
    let ctx = Context {
        data,
        shard: ShardMessenger::new(&runner),
        shard_id: ShardId(0),
        http,
        cache,
    };
    manager.shutdown_all().await;
    drop(runner);
    drop(socket);
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::Args;
    use std::sync::Arc;

    #[test]
    fn prefix_dm_flag_strips_whole_argument_only() {
        let mut parts = vec!["123".into(), "--dm".into(), "because".into()];
        assert!(take_prefix_dm_flag(&mut parts));
        assert_eq!(parts, vec!["123", "because"]);

        let mut reason_dm = vec!["123".into(), "dm".into()];
        assert!(!take_prefix_dm_flag(&mut reason_dm));
        assert_eq!(reason_dm, vec!["123", "dm"]);

        let mut reason_true = vec!["123".into(), "true".into()];
        assert!(!take_prefix_dm_flag(&mut reason_true));

        let mut embedded = vec!["123".into(), "foo--dm".into(), "bar".into()];
        assert!(!take_prefix_dm_flag(&mut embedded));
        assert_eq!(embedded, vec!["123", "foo--dm", "bar"]);
    }

    #[test]
    fn args_strip_dm_flag_leaves_reason_words() {
        let mut args = Args::new("123 dm");
        assert!(!args.strip_dm_flag());
        assert_eq!(args.next_arg().as_deref(), Some("123"));
        assert_eq!(args.rest(), "dm");

        let mut flagged = Args::new("123 --dm spam");
        assert!(flagged.strip_dm_flag());
        assert_eq!(flagged.next_arg().as_deref(), Some("123"));
        assert_eq!(flagged.rest(), "spam");
    }

    #[test]
    fn slash_dm_flag_is_true_only_for_named_true() {
        assert!(slash_dm_flag(&[("dm".into(), "true".into())]));
        assert!(!slash_dm_flag(&[("dm".into(), "false".into())]));
        assert!(!slash_dm_flag(&[]));
        assert!(!slash_dm_flag(&[("reason".into(), "true".into())]));
    }

    #[test]
    fn production_dm_render_strips_ban_attempt_tag() {
        let tag = jmodguard::ban_reason::BanAttemptTag::new();
        let reason = jmodguard::ban_reason::discord_reason("secret", Some(&tag));
        let body = render_mod_reason_dm_body_for_reason("Guild", "ban", &reason, "Mod");
        assert!(!body.contains("[jmg:"));
        assert!(body.contains("secret"));
    }

    #[test]
    fn dm_clause_renders_sent_not_sent_and_uncertain() {
        assert!(
            render_mod_reason_dm_clause(ModReasonDmOutcome::Sent)
                .contains(jmodguard::defaults::MOD_REASON_DM_STATUS_SENT,)
        );
        assert!(
            render_mod_reason_dm_clause(ModReasonDmOutcome::NotSent)
                .contains(jmodguard::defaults::MOD_REASON_DM_STATUS_NOT_SENT,)
        );
        assert!(
            render_mod_reason_dm_clause(ModReasonDmOutcome::Uncertain)
                .contains(jmodguard::defaults::MOD_REASON_DM_STATUS_UNCERTAIN,)
        );
        assert!(render_mod_reason_dm_clause(ModReasonDmOutcome::Cancelled).is_empty());
    }

    #[test]
    fn reason_dm_flag_applies_only_to_kick_and_ban() {
        let kick = crate::defaults::default_commands()
            .into_iter()
            .find(|cmd| cmd.name == "kick")
            .unwrap();
        let warn = crate::defaults::default_commands()
            .into_iter()
            .find(|cmd| cmd.name == "warn")
            .unwrap();
        assert!(command_supports_reason_dm(&kick.behavior));
        assert!(!command_supports_reason_dm(&warn.behavior));

        let mut warn_args = Args::new("42 --dm rule broken");
        let dm = if command_supports_reason_dm(&warn.behavior) {
            warn_args.strip_dm_flag()
        } else {
            false
        };
        assert!(!dm);
        assert_eq!(warn_args.next_arg().as_deref(), Some("42"));
        assert_eq!(warn_args.rest(), "--dm rule broken");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn slow_dm_future_times_out_as_uncertain() {
        let outcome = bounded_mod_reason_dm_future(async {
            tokio::time::sleep(MOD_REASON_DM_TIMEOUT + Duration::from_millis(250)).await;
            ModReasonDmOutcome::Sent
        })
        .await;
        assert_eq!(outcome, ModReasonDmOutcome::Uncertain);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn fast_dm_future_keeps_outcome() {
        let outcome = bounded_mod_reason_dm_future(async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            ModReasonDmOutcome::Sent
        })
        .await;
        assert_eq!(outcome, ModReasonDmOutcome::Sent);
        tokio::time::advance(MOD_REASON_DM_TIMEOUT).await;
    }

    async fn fixture_discord_http_error(status: u16, code: isize) -> serenity::Error {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let server = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut request = Vec::new();
                loop {
                    let mut buf = [0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                    if request.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                counter.fetch_add(1, Ordering::SeqCst);
                let body = format!(r#"{{"code":{code},"message":"fixture"}}"#);
                let response = format!(
                    "HTTP/1.1 {status} Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        let http = serenity::http::HttpBuilder::new("fixture")
            .proxy(format!("http://{address}"))
            .application_id(serenity::model::id::ApplicationId::new(1))
            .ratelimiter_disabled(true)
            .build();
        let err = serenity::model::id::ChannelId::new(1)
            .say(&http, "x")
            .await
            .unwrap_err();
        server.abort();
        err
    }

    #[test]
    fn dm_delivery_transport_error_is_uncertain() {
        let cutoff = crate::discord::cutoff_discord_error();
        assert_eq!(
            classify_mod_reason_dm_delivery_error(&cutoff),
            ModReasonDmOutcome::Uncertain
        );
        assert_eq!(
            crate::cases::classify_discord(Some(&cutoff)),
            crate::db::guild_data::CaseOutcome::Uncertain
        );
    }

    #[tokio::test]
    async fn dm_delivery_http_4xx_is_not_sent() {
        let forbidden = fixture_discord_http_error(403, 50007).await;
        assert_eq!(
            classify_mod_reason_dm_delivery_error(&forbidden),
            ModReasonDmOutcome::NotSent
        );
    }

    #[tokio::test]
    async fn kick_reason_dm_opens_channel_before_member_removal() {
        use serenity::model::event::GuildCreateEvent;
        use serenity::model::guild::{Guild, Member};
        use serenity::model::id::{ChannelId, GuildId, UserId};
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        const GUILD: u64 = 10;
        const TARGET: u64 = 91;
        const MOD: u64 = 7;

        let sequence = Arc::new(Mutex::new(Vec::new()));
        let seq = Arc::clone(&sequence);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut request = Vec::new();
                loop {
                    let mut buf = [0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                    if request.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let request_line = request.split(|&b| b == b'\n').next().unwrap_or(&[]);
                let line = String::from_utf8_lossy(request_line);
                let label = if line.contains("users/@me/channels") {
                    "dm_open"
                } else if line.contains("/messages") {
                    "dm_say"
                } else if line.contains("/members/") {
                    "kick"
                } else {
                    "other"
                };
                seq.lock().unwrap().push(label);
                let (status, body) = if label == "dm_open" {
                    (200, r#"{"id":"555"}"#)
                } else if label == "dm_say" {
                    (200, r#"{"id":"1"}"#)
                } else if label == "kick" {
                    (204, "")
                } else {
                    (404, r#"{"code":0,"message":"fixture"}"#)
                };
                let response = if body.is_empty() {
                    format!(
                        "HTTP/1.1 {status} No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                } else {
                    format!(
                        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                };
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });

        let http = Arc::new(
            serenity::http::HttpBuilder::new("fixture")
                .proxy(format!("http://{address}"))
                .application_id(serenity::model::id::ApplicationId::new(1))
                .ratelimiter_disabled(true)
                .build(),
        );
        let cache = Arc::new(serenity::cache::Cache::new());
        let mut guild = Guild::default();
        guild.id = GuildId::new(GUILD);
        guild.name = "Test Guild".into();
        for (user_id, name) in [(MOD, "mod"), (TARGET, "target")] {
            let mut user = serenity::model::user::User::default();
            user.id = UserId::new(user_id);
            user.name = name.into();
            let mut member = Member::default();
            member.user = user;
            member.guild_id = guild.id;
            guild.members.insert(member.user.id, member);
        }
        let mut event: GuildCreateEvent =
            serde_json::from_value(serde_json::to_value(guild).unwrap()).unwrap();
        cache.update(&mut event);

        let path =
            std::env::temp_dir().join(format!("kick_dm_order_{:032x}.db", rand::random::<u128>()));
        let app = Arc::new(
            crate::state::BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap())
                .unwrap(),
        );
        app.activation.mark_lock_held(true);
        let enrollment = crate::db::enroll_guild(&app.db, GUILD as i64, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();

        let ctx = fixture_context_for_dm_order_test(Arc::clone(&http), Arc::clone(&cache)).await;
        let kick = crate::defaults::default_commands()
            .into_iter()
            .find(|c| c.name == "kick")
            .unwrap();
        let actor = cache
            .guild(GuildId::new(GUILD))
            .and_then(|g| g.members.get(&UserId::new(MOD)).cloned())
            .expect("mod member");
        let env = crate::commands::CmdEnv::new(
            &app,
            &ctx,
            &kick,
            GuildId::new(GUILD),
            ChannelId::new(1),
            UserId::new(MOD),
            actor,
            crate::commands::Args::new(&TARGET.to_string()),
            true,
            None,
        );
        crate::commands::behaviors::run(&env)
            .await
            .expect("kick should succeed against fixture proxy");
        server.abort();

        let order = sequence.lock().unwrap().clone();
        let dm_open = order.iter().position(|step| *step == "dm_open");
        let kick_step = order.iter().position(|step| *step == "kick");
        assert!(
            dm_open.is_some() && kick_step.is_some() && dm_open < kick_step,
            "reason DM must open before kick removal, got {order:?}"
        );

        drop(app);
        crate::tenant::remove_sidecars(&path);
        let _ = std::fs::remove_file(path);
    }
}
