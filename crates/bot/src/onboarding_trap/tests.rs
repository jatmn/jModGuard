use super::*;
use crate::db::guild_data::{CaseAction, CaseOutcome};
use crate::handlers::Handler;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Response, Server};
use serenity::all::{EventHandler, Guild, HttpBuilder, Permissions, Role};
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};

const GUILD: u64 = 10;
const TRAP_ROLE: u64 = 20;
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
    let _in_flight = recent.claim(pending, joined_at, role, false).unwrap();
    for dry in [true, false] {
        for index in 0..=MATCH_LIMIT {
            let subject = (guild, UserId::new(100 + index as u64));
            let _claim = recent.claim(subject, joined_at, role, dry).unwrap();
            assert!(recent.complete_if_current(subject, joined_at, dry, |_| true));
        }
    }
    assert!(recent.claim(pending, joined_at, role, false).is_none());
    let fresh = (GuildId::new(GUILD + 1), UserId::new(MEMBER));
    let _claim = recent.claim(fresh, joined_at, role, false).unwrap();
    assert!(recent.complete_if_current(fresh, joined_at, false, |_| true));
    assert!(!recent.complete_if_current(fresh, joined_at, false, |_| true));
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
    let app = BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap();
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
    crate::db::upsert_rule(&app.db, GUILD as i64, RULE_ID, &rule)
        .await
        .unwrap();
    for (event, channel) in [("member_kick", 70), ("automod", 71)] {
        crate::db::upsert_log_channel(&app.db, GUILD as i64, event, channel)
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
    let pause = event_kind
        .starts_with("during dispatch")
        .then(|| Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new())));
    let server_pause = pause.clone();
    let server = tokio::spawn(server.serve(make_service_fn(move |_| {
        let captured = Arc::clone(&captured);
        let (guild_json, member_json) = (guild_json.clone(), member_json.clone());
        let staff_json = staff_json.clone();
        let staff_veto = server_staff_veto.clone();
        let pause = server_pause.clone();
        async move {
            Ok::<_, Infallible>(service_fn(move |request| {
                let captured = Arc::clone(&captured);
                let (guild_json, member_json) = (guild_json.clone(), member_json.clone());
                let staff_json = staff_json.clone();
                let staff_veto = staff_veto.clone();
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
                    if let Some(pause) = pause
                        && call.method == Method::GET && call.path.contains("/members/")
                    {
                        pause.0.notify_one();
                        pause.1.notified().await;
                    }
                    let (status, body) = match call.method {
                        Method::GET if call.path.contains("/members/") => {
                            let body = if staff_veto.load(Ordering::Relaxed) { staff_json } else { member_json };
                            (200, body)
                        }
                        Method::GET => (200, guild_json),
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
    if let Some(pause) = pause {
        let trigger = handler.guild_member_update(ctx.clone(), None, None, update(&member));
        let contradict = async {
            tokio::time::timeout(Duration::from_secs(5), pause.0.notified())
                .await
                .unwrap();
            let mut latest = member.clone();
            match event_kind {
                "during dispatch: role removed" => latest.roles.clear(),
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
                _ => unreachable!(),
            }
            if event_kind != "during dispatch: left" {
                handler
                    .guild_member_update(ctx.clone(), Some(member.clone()), None, update(&latest))
                    .await;
            }
            // The REST response is intentionally stale. The Gateway answer wins.
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
    if expected_kick || dry {
        // Both events can replay an uncached old state. The server deliberately
        // still reports the same member so we exercise the process replay guard.
        tokio::join!(
            handler.guild_member_update(ctx.clone(), None, None, update(&member)),
            handler.guild_member_update(ctx, None, None, update(&member)),
        );
    }
    server.abort();
    let _ = server.await;

    let cases = store
        .case_ids(scope.clone(), None, 10, crate::tenant::case_limits())
        .await
        .unwrap();
    let effects = calls
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
    if expected_kick {
        assert_eq!(effects.len(), 3, "{event_kind}: {effects:?}");
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
        assert!(effects[2].1.ends_with("/channels/71/messages"));
        assert!(effects[2].2.contains(REASON));
    } else {
        assert!(cases.is_empty(), "{event_kind}: {cases:?}");
        if dry {
            assert_eq!(effects.len(), 1, "{effects:?}");
            assert!(effects[0].1.ends_with("/channels/71/messages"));
            assert!(effects[0].2.contains("[DRY RUN]"));
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
