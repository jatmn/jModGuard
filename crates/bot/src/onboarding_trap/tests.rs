use super::*;
use crate::db::guild_data::{CaseAction, CaseOutcome};
use crate::handlers::Handler;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Response, Server};
use serenity::all::{ChannelId, EventHandler, Guild, HttpBuilder, MessageId, Permissions, Role};
use std::convert::Infallible;
use std::future::poll_fn;
use std::pin::{Pin, pin};
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;

const GUILD: u64 = 10;
const TRAP_ROLE: u64 = 20;
const NEW_TRAP_ROLE: u64 = 23;
const MEMBER: u64 = 30;
const REASON: &str = "[Automod] Kicked for suspected bot (onboarding)";

fn member(id: u64, roles: &[u64], joined_at: Timestamp) -> Member {
    let mut member = Member::default();
    member.guild_id = GuildId::new(GUILD);
    member.user.id = UserId::new(id);
    member.user.name = "member".into();
    member.roles = roles.iter().copied().map(RoleId::new).collect();
    member.joined_at = Some(joined_at);
    member
}

fn update(member: &Member) -> GuildMemberUpdateEvent {
    serde_json::from_value(serde_json::json!({
        "guild_id": member.guild_id,
        "user": member.user,
        "roles": member.roles,
        "joined_at": member.joined_at,
    }))
    .unwrap()
}

fn guild(everyone: Permissions) -> Guild {
    let mut guild = Guild::default();
    guild.id = GuildId::new(GUILD);
    guild.owner_id = UserId::new(34);
    for (id, permissions) in [
        (GUILD, everyone),
        (TRAP_ROLE, Permissions::empty()),
        (21, Permissions::MANAGE_MESSAGES),
        (22, Permissions::ADMINISTRATOR),
        (NEW_TRAP_ROLE, Permissions::empty()),
    ] {
        let mut role = Role::default();
        role.id = RoleId::new(id);
        role.guild_id = guild.id;
        role.permissions = permissions;
        guild.roles.insert(role.id, role);
    }
    guild
}

#[test]
fn join_window_includes_its_boundary_and_zero_checks_older_members() {
    let now = Timestamp::from_millis(Timestamp::now().timestamp_millis()).unwrap();
    let joined_at = Timestamp::from_millis(now.timestamp_millis() - 30 * 60 * 1000).unwrap();
    let member = member(MEMBER, &[TRAP_ROLE], joined_at);
    let event = RoleEvent {
        guild: member.guild_id,
        user: &member.user,
        display_name: member.display_name(),
        roles: &member.roles,
        old_roles: Some(&[]),
        joined_at,
    };
    let role = RoleId::new(TRAP_ROLE);
    let later = Timestamp::from_millis(now.timestamp_millis() + 1).unwrap();
    assert!(matches_role(&event, role, 30, now));
    assert!(!matches_role(&event, role, 30, later));
    assert!(matches_role(&event, role, 0, later));
    assert!(!matches_role(
        &event,
        role,
        0,
        Timestamp::from_millis(1).unwrap()
    ));
}

#[test]
fn saturated_replay_history_keeps_new_live_stays_eligible() {
    let recent = RecentMatches::default();
    let guild = GuildId::new(GUILD);
    let role = RoleId::new(TRAP_ROLE);
    let joined_at = Timestamp::now();
    let pending = (guild, UserId::new(MEMBER));
    let _in_flight = recent
        .claim(pending, joined_at, role, false, (0, 0))
        .unwrap();
    for dry in [true, false] {
        for index in 0..=MATCH_LIMIT {
            let subject = (guild, UserId::new(100 + index as u64));
            let claim = recent.claim(subject, joined_at, role, dry, (0, 0)).unwrap();
            assert!(claim.complete(dry, |_| true));
        }
    }
    assert!(
        recent
            .claim(pending, joined_at, role, false, (0, 0))
            .is_none()
    );
    let fresh = (GuildId::new(GUILD + 1), UserId::new(MEMBER));
    let claim = recent.claim(fresh, joined_at, role, false, (0, 0)).unwrap();
    assert!(claim.complete(false, |_| true));
    assert!(!claim.complete(false, |_| true));
}

#[test]
fn invalidated_claim_cannot_complete_or_release_its_successor() {
    let recent = RecentMatches::default();
    let subject = (GuildId::new(GUILD), UserId::new(MEMBER));
    let role = RoleId::new(TRAP_ROLE);
    let joined_at = Timestamp::now();
    let first = recent
        .claim(subject, joined_at, role, false, (0, 0))
        .unwrap();
    assert!(
        recent
            .claim(subject, joined_at, role, false, (0, 0))
            .is_none()
    );
    recent.observe(subject.0, subject.1, Some((joined_at, &[])));
    let successor = recent
        .claim(subject, joined_at, role, false, (0, 0))
        .unwrap();

    assert!(!first.complete(false, |_| true));
    drop(first);
    assert!(
        recent
            .claim(subject, joined_at, role, false, (0, 0))
            .is_none()
    );
    assert!(successor.complete(false, |_| true));
    drop(successor);
    assert!(
        recent
            .claim(subject, joined_at, role, false, (0, 0))
            .is_none()
    );
}

async fn poll_once<F: Future>(mut future: Pin<&mut F>) -> Poll<F::Output> {
    poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await
}

#[tokio::test]
async fn repeated_replacements_cancel_stalled_preflights_and_only_latest_dispatches() {
    use std::cell::Cell;

    // Use one of the per-mode permits so each handoff hits the production cap.
    let recent = &RecentMatches {
        claims: Mutex::default(),
        preflights: std::array::from_fn(|_| Semaphore::new(1)),
    };
    let ready = &Semaphore::new(0);
    let lookup_count = &Cell::new(0);
    let subject = (GuildId::new(GUILD), UserId::new(MEMBER));
    let joined_at = Timestamp::now();
    let role = RoleId::new(TRAP_ROLE);
    let grant = |revision| async move {
        let claim = recent
            .claim(subject, joined_at, role, false, (0, revision))
            .unwrap();
        claim
            .preflight(async {
                lookup_count.set(lookup_count.get() + 1);
                let _response = ready.acquire().await.ok()?;
                claim.complete(false, |_| true).then_some(revision)
            })
            .await
    };
    let mut pending = Box::pin(grant(1));
    assert!(poll_once(pending.as_mut()).await.is_pending());
    assert_eq!(lookup_count.get(), 1);
    for revision in 2..=8 {
        let mut successor = Box::pin(grant(revision));
        assert!(poll_once(successor.as_mut()).await.is_pending());
        assert_eq!(
            lookup_count.get(),
            revision - 1,
            "obsolete lookup still owns the permit"
        );
        assert!(
            recent
                .claim(subject, joined_at, role, false, (0, revision - 1))
                .is_none()
        );
        assert_eq!(poll_once(pending.as_mut()).await, Poll::Ready(None));
        assert!(poll_once(successor.as_mut()).await.is_pending());
        assert_eq!(
            lookup_count.get(),
            revision,
            "latest lookup used the released permit"
        );
        pending = successor;
    }
    ready.add_permits(1);
    assert_eq!(pending.await, Some(8));
    assert!(
        recent
            .claim(subject, joined_at, role, false, (0, 9))
            .is_none()
    );
}

#[test]
fn committed_kick_keeps_same_stay_reserved_even_if_replay_history_is_evicted() {
    let recent = RecentMatches::default();
    let subject = (GuildId::new(GUILD), UserId::new(MEMBER));
    let joined_at = Timestamp::now();
    let claim = recent
        .claim(subject, joined_at, RoleId::new(TRAP_ROLE), false, (0, 1))
        .unwrap();
    assert!(claim.complete(false, |_| true));
    recent.claims.lock().history[0].evict_oldest();
    recent.observe(subject.0, subject.1, Some((joined_at, &[])));
    assert!(
        recent
            .claim(
                subject,
                joined_at,
                RoleId::new(NEW_TRAP_ROLE),
                false,
                (0, 2)
            )
            .is_none()
    );
}

struct Call {
    method: Method,
    path: String,
    body: String,
    audit_reason: String,
}

async fn gateway_case(
    member: Member,
    discord_member: Member,
    guild: Guild,
    event_kind: &str,
    minutes: u32,
    dry: bool,
    expected_kick: bool,
) {
    let path = std::env::temp_dir().join(format!(
        "onboarding_trap_{:032x}.db",
        rand::random::<u128>()
    ));
    let ungrouped = event_kind == "cleanup ungrouped";
    let rejections = event_kind == "cleanup rejected destination";
    let mut config = jmodguard::config::BotProcessConfig::default();
    if ungrouped {
        config.log.group_automod_cleanup_deletes = false;
    }
    let app = BotState::new(config, crate::db::Db::open(&path, 2).unwrap()).unwrap();
    assert!(
        crate::db::claim_active_bot(
            &app.db,
            app.activation.instance(),
            jmodguard::control::now_ms(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await
        .unwrap()
    );
    app.activation.mark_lock_held(true);
    app.activation.note_discord_connected();
    let enrollment = crate::db::enroll_guild(&app.db, GUILD as i64, 100)
        .await
        .unwrap()
        .unwrap();
    app.tenant.activate(enrollment.clone()).await.unwrap();
    let (store, scope) = app.tenant.live(enrollment).await.unwrap();
    let mut rule = crate::types::RULE_IDS
        .into_iter()
        .zip(crate::defaults::default_rules())
        .find(|(id, _)| *id == RULE_ID)
        .unwrap()
        .1;
    rule.enabled = event_kind != "disabled";
    rule.dry_run = dry;
    rule.params =
        serde_json::json!({ "role_id": TRAP_ROLE.to_string(), "join_window_minutes": minutes });
    if event_kind.starts_with("cleanup") {
        if event_kind != "cleanup off" {
            rule.params["cleanup_window_minutes"] = serde_json::json!(5);
        }
        let now_ms = Timestamp::now().timestamp_millis();
        let joined_at_ms = member.joined_at.unwrap().timestamp_millis();
        for (guild, author, id, at_ms) in [
            (GUILD, MEMBER, 101, now_ms - 1_000),
            (GUILD, MEMBER, 102, joined_at_ms - 1),
            (GUILD, MEMBER, 103, now_ms - 301_000),
            (GUILD, MEMBER, 104, now_ms + 60_000),
            (GUILD, MEMBER + 1, 105, now_ms - 1_000),
            (GUILD + 1, MEMBER, 106, now_ms - 1_000),
        ] {
            app.recent_messages.remember(guild, author, 77, id, at_ms);
        }
        if rejections {
            for (channel, id) in [(78, 109), (78, 108), (77, 107)] {
                app.recent_messages
                    .remember(GUILD, MEMBER, channel, id, now_ms - 1_000);
            }
        }
    }
    crate::db::upsert_rule(&app.db, GUILD as i64, RULE_ID, &rule)
        .await
        .unwrap();
    for (event, channel) in [("member_kick", 70), ("automod", 71)] {
        crate::db::upsert_log_channel(&app.db, GUILD as i64, event, channel)
            .await
            .unwrap();
    }
    if ungrouped {
        crate::db::upsert_log_channel(&app.db, GUILD as i64, "automod_delete", 72)
            .await
            .unwrap();
    }
    app.invalidate_configs();

    let fenced = event_kind == "retry after fence";
    if fenced {
        use crate::db::SqlExecutor as _;
        let destination = scope.placement().store_id().as_str().to_owned();
        app.db
            .with_conn(move |conn| {
                conn.execute_bounded(
                    "INSERT INTO guild_write_fences(
                    guild_id,generation,phase,destination_store_id,destination_revision,created_at
                 ) VALUES(?1,?2,'freeze',?3,?2,unixepoch())",
                    crate::db::params![GUILD as i64, format!("{:032x}", 9_u128), destination],
                    64 * 1024,
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }
    let staff_veto = Arc::new(AtomicBool::new(event_kind == "retry after staff veto"));
    let server_staff_veto = staff_veto.clone();
    let mut staff_member = discord_member.clone();
    staff_member.roles.push(RoleId::new(21));
    let staff_json = serde_json::to_string(&staff_member).unwrap();

    let calls = Arc::new(Mutex::new(Vec::<Call>::new()));
    let captured = Arc::clone(&calls);
    let server = Server::bind(&([127, 0, 0, 1], 0).into());
    let http = Arc::new(
        HttpBuilder::new("fixture")
            .proxy(format!("http://{}", server.local_addr()))
            .ratelimiter_disabled(true)
            .build(),
    );
    let ctx = crate::commands::mod_reason_dm::fixture_context_for_dm_order_test(
        http,
        Arc::new(Cache::new()),
    )
    .await;
    let guild_json = serde_json::to_string(&guild).unwrap();
    let member_json = serde_json::to_string(&discord_member).unwrap();
    let mut member_with_new_role = discord_member.clone();
    member_with_new_role.roles.push(RoleId::new(NEW_TRAP_ROLE));
    let member_with_new_role_json = serde_json::to_string(&member_with_new_role).unwrap();
    let new_role_granted = Arc::new(AtomicBool::new(false));
    let server_new_role_granted = Arc::clone(&new_role_granted);
    let change_cleanup_on_kick = matches!(
        event_kind,
        "cleanup disabled during kick" | "cleanup user exemption" | "cleanup role exemption"
    );
    let pause = (event_kind.starts_with("during dispatch") || change_cleanup_on_kick).then(|| {
        Arc::new((
            tokio::sync::Notify::new(),
            tokio::sync::Notify::new(),
            AtomicBool::new(true),
        ))
    });
    let server_pause = pause.clone();
    let server = tokio::spawn(server.serve(make_service_fn(move |_| {
        let captured = Arc::clone(&captured);
        let (guild_json, member_json) = (guild_json.clone(), member_json.clone());
        let staff_json = staff_json.clone();
        let staff_veto = server_staff_veto.clone();
        let new_role_granted = Arc::clone(&server_new_role_granted);
        let member_with_new_role_json = member_with_new_role_json.clone();
        let pause = server_pause.clone();
        async move {
            Ok::<_, Infallible>(service_fn(move |request| {
                let captured = Arc::clone(&captured);
                let (guild_json, member_json) = (guild_json.clone(), member_json.clone());
                let staff_json = staff_json.clone();
                let staff_veto = staff_veto.clone();
                let new_role_granted = Arc::clone(&new_role_granted);
                let member_with_new_role_json = member_with_new_role_json.clone();
                let pause = pause.clone();
                async move {
                    let (parts, body) = request.into_parts();
                    let body = hyper::body::to_bytes(body).await.unwrap();
                    let call = Call {
                        method: parts.method,
                        path: parts.uri.path().to_owned(),
                        body: String::from_utf8(body.to_vec()).unwrap(),
                        audit_reason: parts.headers.get("x-audit-log-reason")
                            .map(|value| value.to_str().unwrap().to_owned()).unwrap_or_default(),
                    };
                    let paused_method = if change_cleanup_on_kick { Method::DELETE } else { Method::GET };
                    if let Some(pause) = pause
                        && call.method == paused_method && call.path.contains("/members/")
                        && pause.2.swap(false, Ordering::Relaxed)
                    {
                        pause.0.notify_one();
                        pause.1.notified().await;
                    }
                    let (status, body) = match call.method {
                        Method::GET if call.path.contains("/members/") => {
                            let body = if staff_veto.load(Ordering::Relaxed) {
                                staff_json
                            } else if new_role_granted.load(Ordering::Relaxed) {
                                member_with_new_role_json
                            } else {
                                member_json
                            };
                            (200, body)
                        }
                        Method::GET => (200, guild_json),
                        Method::DELETE if rejections && call.path.contains("/channels/78/messages/") => (
                            403, r#"{"code":50013,"message":"Missing Permissions"}"#.into(),
                        ),
                        Method::DELETE if rejections && call.path.ends_with("/channels/77/messages/107") => (
                            404, r#"{"code":10008,"message":"Unknown Message"}"#.into(),
                        ),
                        Method::DELETE => (204, String::new()),
                        Method::POST => (200, serde_json::json!({
                            "id": "91", "channel_id": "70",
                            "author": {"id": "1", "username": "fixture", "discriminator": "0", "avatar": null},
                            "content": "", "timestamp": "2026-01-01T00:00:00Z", "tts": false,
                            "mention_everyone": false, "mentions": [], "mention_roles": [],
                            "attachments": [], "embeds": [], "pinned": false, "type": 0
                        }).to_string()),
                        _ => (500, String::new()),
                    };
                    captured.lock().push(call);
                    Ok::<_, Infallible>(Response::builder()
                        .status(status).header("content-type", "application/json")
                        .body(Body::from(body)).unwrap())
                }
            }))
        }
    })));

    let handler = Handler {
        app: Arc::clone(&app),
    };
    let mut old = member.clone();
    if event_kind != "unchanged" {
        old.roles.clear();
    }
    if event_kind.starts_with("retry after ") {
        assert!(calls.lock().iter().all(|call| call.method == Method::GET));
        assert!(
            store
                .case_ids(scope.clone(), None, 10, crate::tenant::case_limits())
                .await
                .unwrap()
                .is_empty()
        );
        staff_veto.store(false, Ordering::Relaxed);
        if fenced {
            use crate::db::SqlExecutor as _;
            app.db
                .with_conn(|conn| {
                    conn.execute_bounded(
                        "DELETE FROM guild_write_fences WHERE guild_id=?1",
                        crate::db::params![GUILD as i64],
                        64 * 1024,
                    )?;
                    Ok(())
                })
                .await
                .unwrap();
        }
        handler
            .guild_member_update(ctx.clone(), None, None, update(&member))
            .await;
    }
    let mut changed_rule = rule.clone();
    if let Some(pause) = pause {
        let removed_before_claim =
            event_kind == "during dispatch: new trap role removed before claim";
        let trigger_finished = Notify::new();
        let trigger = async {
            handler
                .guild_member_update(ctx.clone(), None, None, update(&member))
                .await;
            trigger_finished.notify_one();
        };
        let contradict = async {
            tokio::time::timeout(Duration::from_secs(5), pause.0.notified())
                .await
                .unwrap();
            let mut latest = member.clone();
            match event_kind {
                "during dispatch: role removed" | "during dispatch: regrant" => {
                    latest.roles.clear()
                }
                "during dispatch: rejoined" => {
                    latest.joined_at = Some(Timestamp::now());
                    latest.roles.clear();
                }
                "during dispatch: promoted" => latest.roles.push(RoleId::new(21)),
                "during dispatch: left" => {
                    handler
                        .guild_member_removal(
                            ctx.clone(),
                            member.guild_id,
                            member.user.clone(),
                            Some(member.clone()),
                        )
                        .await;
                }
                "during dispatch: disabled"
                | "during dispatch: dry run"
                | "during dispatch: new trap role"
                | "during dispatch: new trap role grant"
                | "during dispatch: new trap role removed before claim" => {
                    match event_kind {
                        "during dispatch: disabled" => changed_rule.enabled = false,
                        "during dispatch: dry run" => changed_rule.dry_run = true,
                        _ => {
                            changed_rule.params["role_id"] =
                                serde_json::json!(NEW_TRAP_ROLE.to_string());
                        }
                    }
                    changed_rule = db::compare_and_set_onboarding_trap(
                        &app.db,
                        GUILD as i64,
                        None,
                        &rule,
                        &changed_rule,
                    )
                    .await
                    .unwrap();
                    app.invalidate_configs();
                    if event_kind == "during dispatch: new trap role grant" || removed_before_claim
                    {
                        new_role_granted.store(true, Ordering::Relaxed);
                        latest.roles.push(RoleId::new(NEW_TRAP_ROLE));
                    }
                }
                "cleanup disabled during kick"
                | "cleanup user exemption"
                | "cleanup role exemption" => {
                    match event_kind {
                        "cleanup disabled during kick" => {
                            changed_rule.params["cleanup_window_minutes"] = serde_json::json!(0);
                        }
                        "cleanup user exemption" => changed_rule.whitelist_users.push(MEMBER),
                        _ => changed_rule.whitelist_roles.push(TRAP_ROLE),
                    }
                    db::compare_and_set_onboarding_trap(
                        &app.db,
                        GUILD as i64,
                        None,
                        &rule,
                        &changed_rule,
                    )
                    .await
                    .unwrap();
                }
                _ => unreachable!(),
            }
            if removed_before_claim {
                // Occupy both core connections so the grant and removal record
                // Gateway roles before either can enter trap matching.
                let mut holders = Vec::new();
                for _ in 0..2 {
                    let db = app.db.clone();
                    let (held, acquired) = tokio::sync::oneshot::channel();
                    let (release, resume) = tokio::sync::oneshot::channel::<()>();
                    let holder = tokio::spawn(async move {
                        db.with_conn(move |_| {
                            held.send(()).unwrap();
                            let _ = resume.blocking_recv();
                            Ok(())
                        })
                        .await
                        .unwrap();
                    });
                    acquired.await.unwrap();
                    holders.push((release, holder));
                }
                assert!(ctx.cache.guild(member.guild_id).is_none());
                let mut grant = pin!(handler.guild_member_update(
                    ctx.clone(),
                    Some(member.clone()),
                    None,
                    update(&latest)
                ));
                assert!(poll_once(grant.as_mut()).await.is_pending());
                let mut removal = pin!(handler.guild_member_update(
                    ctx.clone(),
                    Some(latest.clone()),
                    None,
                    update(&member)
                ));
                assert!(poll_once(removal.as_mut()).await.is_pending());
                for (release, holder) in holders {
                    release.send(()).unwrap();
                    holder.await.unwrap();
                }
                tokio::join!(grant, removal);
                assert!(
                    calls
                        .lock()
                        .iter()
                        .any(|call| call.method == Method::GET && call.path.contains("/members/")),
                    "the delayed grant must reach the stale REST lookup"
                );
            } else if latest.roles != member.roles {
                handler
                    .guild_member_update(ctx.clone(), Some(member.clone()), None, update(&latest))
                    .await;
            }
            if event_kind == "during dispatch: regrant" {
                handler
                    .guild_member_update(ctx.clone(), Some(latest), None, update(&member))
                    .await;
            }
            if matches!(
                event_kind,
                "during dispatch: regrant" | "during dispatch: new trap role grant"
            ) {
                assert_eq!(
                    calls
                        .lock()
                        .iter()
                        .filter(|call| call.method == Method::DELETE)
                        .count(),
                    1,
                    "the qualifying grant must dispatch while the original lookup is still parked"
                );
                tokio::time::timeout(Duration::from_secs(5), trigger_finished.notified())
                    .await
                    .expect("the superseded handler must finish before the lookup is released");
            }
            pause.1.notify_one();
        };
        tokio::join!(trigger, contradict);
    } else {
        match event_kind {
            "join" => {
                handler
                    .guild_member_addition(ctx.clone(), member.clone())
                    .await
            }
            "uncached" => {
                handler
                    .guild_member_update(ctx.clone(), None, None, update(&member))
                    .await
            }
            _ => {
                handler
                    .guild_member_update(
                        ctx.clone(),
                        Some(old),
                        Some(member.clone()),
                        update(&member),
                    )
                    .await
            }
        }
    }
    let downgraded = event_kind == "during dispatch: dry run";
    if downgraded {
        // The downgraded match already logged in the dry-run mode.
        handler
            .guild_member_update(ctx.clone(), None, None, update(&member))
            .await;
        {
            let calls = calls.lock();
            let effects: Vec<_> = calls
                .iter()
                .filter(|call| call.method != Method::GET)
                .collect();
            assert_eq!(effects.len(), 1);
            assert_eq!(effects[0].method, Method::POST);
            assert!(effects[0].path.ends_with("/channels/71/messages"));
            assert!(effects[0].body.contains("[DRY RUN]"));
        }
        assert!(
            store
                .case_ids(scope.clone(), None, 10, crate::tenant::case_limits())
                .await
                .unwrap()
                .is_empty()
        );
        db::compare_and_set_onboarding_trap(&app.db, GUILD as i64, None, &changed_rule, &rule)
            .await
            .unwrap();
        app.invalidate_configs();
    }
    if ungrouped {
        handler
            .message_delete(
                ctx.clone(),
                ChannelId::new(77),
                MessageId::new(101),
                Some(GuildId::new(GUILD)),
            )
            .await;
    }
    if expected_kick || dry {
        // Both events can replay an uncached old state. The server deliberately
        // still reports the same member so we exercise the process replay guard.
        let replay = if new_role_granted.load(Ordering::Relaxed) {
            &member_with_new_role
        } else {
            &member
        };
        tokio::join!(
            handler.guild_member_update(ctx.clone(), None, None, update(replay)),
            handler.guild_member_update(ctx, None, None, update(replay)),
        );
    }
    server.abort();
    let _ = server.await;

    let cases = store
        .case_ids(scope.clone(), None, 10, crate::tenant::case_limits())
        .await
        .unwrap();
    let mut effects = calls
        .lock()
        .iter()
        .filter(|call| call.method != Method::GET)
        .map(|call| {
            (
                call.method.clone(),
                call.path.clone(),
                call.body.clone(),
                call.audit_reason.clone(),
            )
        })
        .collect::<Vec<_>>();
    if ungrouped {
        let reports = effects
            .iter()
            .filter(|effect| {
                effect.0 == Method::POST && effect.1.ends_with("/channels/72/messages")
            })
            .collect::<Vec<_>>();
        assert_eq!(reports.len(), 1, "REST success and its echo: {effects:?}");
        assert!(reports[0].2.contains(&rule.name));
        assert!(reports[0].2.contains("/channels/10/77/101"));
        effects.retain(|effect| !effect.1.ends_with("/channels/72/messages"));
    }
    let effects = if downgraded {
        &effects[1..]
    } else {
        &effects[..]
    };
    if expected_kick {
        let deletes_recent = event_kind == "cleanup" || ungrouped;
        let delete_count = if rejections {
            3
        } else {
            usize::from(deletes_recent)
        };
        assert_eq!(effects.len(), 3 + delete_count, "{event_kind}: {effects:?}");
        assert_eq!(effects[0].0, Method::DELETE);
        assert!(
            effects[0]
                .1
                .ends_with(&format!("/members/{}", member.user.id))
        );
        assert_eq!(
            effects[0].3,
            "%5BAutomod%5D%20Kicked%20for%20suspected%20bot%20%28onboarding%29"
        );
        assert_eq!(cases.len(), 1);
        let (indexed, _) = store
            .member_subject_case_entries(scope.clone(), member.user.id.get() as i64, 8)
            .await
            .unwrap();
        assert!(
            indexed.is_empty(),
            "the live member index must omit the private case"
        );
        store
            .initialize_member_directory(scope.clone(), 42, 1_000)
            .await
            .unwrap();
        assert!(
            store
                .backfill_member_case_index(scope.clone(), 42, 100)
                .await
                .unwrap()
        );
        let (listed, _) = store
            .member_case_entries(
                scope.clone(),
                member.user.id.get() as i64,
                8,
                crate::tenant::case_limits(),
            )
            .await
            .unwrap();
        assert!(
            listed.is_empty(),
            "the backfill must keep the private trap case out of member case summaries"
        );
        let case = store
            .case(scope, cases[0].clone(), crate::tenant::case_limits())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(case.action, CaseAction::Kick);
        assert_eq!(case.outcome, CaseOutcome::Succeeded);
        assert_eq!(case.reason.as_deref(), Some(REASON));
        assert!(effects[1].1.ends_with("/channels/70/messages"));
        assert!(effects[1].2.contains(cases[0].as_str()));
        if deletes_recent {
            assert_eq!(effects[2].0, Method::DELETE);
            assert!(effects[2].1.ends_with("/channels/77/messages/101"));
            assert!(effects[3].2.contains("tracked messages deleted (1)"));
        }
        if rejections {
            let destinations = effects[2..5]
                .iter()
                .map(|effect| {
                    assert_eq!(effect.0, Method::DELETE);
                    effect.1.split("/channels/").last().unwrap()
                })
                .collect::<Vec<_>>();
            assert_eq!(
                destinations,
                ["78/messages/109", "77/messages/107", "77/messages/101"]
            );
            assert!(effects[5].2.contains("tracked messages deleted (1)"));
        }
        let automod_log = effects.last().unwrap();
        assert!(automod_log.1.ends_with("/channels/71/messages"));
        assert!(automod_log.2.contains(REASON));
    } else {
        assert!(cases.is_empty(), "{event_kind}: {cases:?}");
        if dry {
            assert_eq!(effects.len(), 1, "{effects:?}");
            assert!(effects[0].1.ends_with("/channels/71/messages"));
            assert!(effects[0].2.contains("[DRY RUN]"));
            if event_kind == "cleanup" {
                assert!(effects[0].2.contains("would clean tracked messages"));
            }
        } else {
            assert!(effects.is_empty(), "{event_kind}: {effects:?}");
        }
    }
    drop(handler);
    drop(store);
    drop(app);
    crate::tenant::remove_sidecars(&path);
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn trap_cleans_only_tracked_messages_in_the_stay_after_a_confirmed_kick() {
    for (event, dry, joined_minutes_ago) in [
        ("cleanup", false, 1),
        ("cleanup", false, 10),
        ("cleanup", true, 1),
        ("cleanup off", false, 1),
        ("cleanup disabled during kick", false, 1),
        ("cleanup user exemption", false, 1),
        ("cleanup role exemption", false, 1),
    ] {
        let joined_at = Timestamp::from_millis(
            Timestamp::now().timestamp_millis() - joined_minutes_ago * 60_000,
        )
        .unwrap();
        let member = member(MEMBER, &[TRAP_ROLE], joined_at);
        gateway_case(
            member.clone(),
            member,
            guild(Permissions::empty()),
            event,
            30,
            dry,
            !dry,
        )
        .await;
    }
}

#[tokio::test]
async fn trap_cleanup_attributes_the_delete_once_without_grouping() {
    let member = member(
        MEMBER,
        &[TRAP_ROLE],
        Timestamp::from_millis(Timestamp::now().timestamp_millis() - 60_000).unwrap(),
    );
    gateway_case(
        member.clone(),
        member,
        guild(Permissions::empty()),
        "cleanup ungrouped",
        30,
        false,
        true,
    )
    .await;
}

#[tokio::test]
async fn trap_cleanup_skips_denied_destinations_but_not_unknown_messages() {
    let member = member(
        MEMBER,
        &[TRAP_ROLE],
        Timestamp::from_millis(Timestamp::now().timestamp_millis() - 60_000).unwrap(),
    );
    gateway_case(
        member.clone(),
        member,
        guild(Permissions::empty()),
        "cleanup rejected destination",
        30,
        false,
        true,
    )
    .await;
}

#[tokio::test]
async fn newer_gateway_membership_vetoes_a_stale_dispatch_response() {
    for event in [
        "during dispatch: role removed",
        "during dispatch: rejoined",
        "during dispatch: promoted",
        "during dispatch: left",
    ] {
        let joined_at =
            Timestamp::from_millis(Timestamp::now().timestamp_millis() - 60_000).unwrap();
        let member = member(MEMBER, &[TRAP_ROLE], joined_at);
        gateway_case(
            member.clone(),
            member,
            guild(Permissions::empty()),
            event,
            30,
            false,
            false,
        )
        .await;
    }
}

#[tokio::test]
async fn a_disable_or_role_change_saved_during_member_lookup_prevents_the_kick() {
    for event in [
        "during dispatch: disabled",
        "during dispatch: new trap role",
    ] {
        let member = member(MEMBER, &[TRAP_ROLE], Timestamp::now());
        gateway_case(
            member.clone(),
            member,
            guild(Permissions::empty()),
            event,
            30,
            false,
            false,
        )
        .await;
    }
}

#[tokio::test]
async fn a_grant_for_the_new_trap_role_supersedes_the_parked_old_role_match() {
    let member = member(MEMBER, &[TRAP_ROLE], Timestamp::now());
    gateway_case(
        member.clone(),
        member,
        guild(Permissions::empty()),
        "during dispatch: new trap role grant",
        30,
        false,
        true,
    )
    .await;
}

#[tokio::test]
async fn gateway_role_removal_while_the_replacement_waits_for_the_directory_vetoes_the_kick() {
    let joined_at = Timestamp::from_millis(1).unwrap();
    let member = member(MEMBER, &[TRAP_ROLE], joined_at);
    gateway_case(
        member.clone(),
        member,
        guild(Permissions::empty()),
        "during dispatch: new trap role removed before claim",
        0,
        false,
        false,
    )
    .await;
}

#[tokio::test]
async fn a_downgrade_during_member_lookup_logs_dry_and_preserves_the_live_claim() {
    let member = member(MEMBER, &[TRAP_ROLE], Timestamp::now());
    gateway_case(
        member.clone(),
        member,
        guild(Permissions::empty()),
        "during dispatch: dry run",
        30,
        false,
        true,
    )
    .await;
}

#[tokio::test]
async fn a_regrant_during_member_lookup_can_dispatch_once_for_the_same_stay() {
    let member = member(MEMBER, &[TRAP_ROLE], Timestamp::now());
    gateway_case(
        member.clone(),
        member,
        guild(Permissions::empty()),
        "during dispatch: regrant",
        30,
        false,
        true,
    )
    .await;
}

#[tokio::test]
async fn onboarding_trap_retries_the_same_stay_after_fence_or_staff_veto() {
    for event in ["retry after fence", "retry after staff veto"] {
        let member = member(MEMBER, &[TRAP_ROLE], Timestamp::now());
        gateway_case(
            member.clone(),
            member,
            guild(Permissions::empty()),
            event,
            30,
            false,
            true,
        )
        .await;
    }
}

#[tokio::test]
async fn join_role_update_and_uncached_update_each_kick_once() {
    for event in ["join", "update", "uncached"] {
        let member = member(MEMBER, &[TRAP_ROLE], Timestamp::now());
        gateway_case(
            member.clone(),
            member,
            guild(Permissions::empty()),
            event,
            30,
            false,
            true,
        )
        .await;
    }
}

#[tokio::test]
async fn dry_run_only_logs_and_ignores_replays() {
    let member = member(MEMBER, &[TRAP_ROLE], Timestamp::now());
    gateway_case(
        member.clone(),
        member,
        guild(Permissions::empty()),
        "join",
        30,
        true,
        false,
    )
    .await;
}

#[tokio::test]
async fn fresh_permissions_exempt_moderators_admins_and_the_guild_owner() {
    for (id, added, everyone) in [
        (31, &[TRAP_ROLE, 21][..], Permissions::empty()),
        (32, &[TRAP_ROLE, 22][..], Permissions::empty()),
        (33, &[TRAP_ROLE][..], Permissions::KICK_MEMBERS),
        (34, &[TRAP_ROLE][..], Permissions::empty()),
        (35, &[TRAP_ROLE, 99][..], Permissions::empty()),
    ] {
        let joined_at = Timestamp::now();
        gateway_case(
            member(id, &[TRAP_ROLE], joined_at),
            member(id, added, joined_at),
            guild(everyone),
            "update",
            30,
            false,
            false,
        )
        .await;
    }
}

#[tokio::test]
async fn disabled_unchanged_outside_window_and_stale_stay_do_not_act() {
    let joined_at = Timestamp::now();
    let recent = member(MEMBER, &[TRAP_ROLE], joined_at);
    let older = member(
        MEMBER,
        &[TRAP_ROLE],
        Timestamp::from_millis(joined_at.timestamp_millis() - 31 * 60 * 1000).unwrap(),
    );
    for (state, event_member, fresh_member) in [
        ("disabled", &recent, &recent),
        ("unchanged", &recent, &recent),
        ("expired", &older, &older),
        ("stale stay", &recent, &older),
    ] {
        gateway_case(
            event_member.clone(),
            fresh_member.clone(),
            guild(Permissions::empty()),
            state,
            30,
            false,
            false,
        )
        .await;
    }
}
