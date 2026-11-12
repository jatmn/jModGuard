use super::*;
use crate::commands::Args;
use crate::db::guild_data::{
    CaseAction, CaseData, CaseEvidence, CaseId, CaseLimits, CaseOutcome, CaseSource,
    MemberHistoryEntry, MemberHistoryPage, MemberObservation,
};
use crate::state::BotState;
use serenity::all::{Cache, Guild, GuildCreateEvent, Member, Role, RoleId, User, UserId};
use serenity::model::id::{ChannelId, GuildId};
use std::sync::Arc;

fn remember_guild(
    cache: &Cache,
    guild_id: u64,
    member: Option<Member>,
    voice: Option<serenity::model::voice::VoiceState>,
) {
    let mut guild = Guild::default();
    guild.id = GuildId::new(guild_id);
    guild.owner_id = UserId::new(99);
    let mut everyone = Role::default();
    everyone.id = RoleId::new(guild_id);
    everyone.guild_id = guild.id;
    everyone.permissions = serenity::model::permissions::Permissions::VIEW_CHANNEL;
    guild.roles.insert(everyone.id, everyone);
    let mut role = Role::default();
    role.id = RoleId::new(50);
    role.guild_id = guild.id;
    role.name = "Mod".into();
    guild.roles.insert(role.id, role);
    if let Some(member) = member {
        guild.members.insert(member.user.id, member);
    }
    if let Some(voice) = voice {
        let channel_id = voice.channel_id;
        guild.voice_states.insert(voice.user_id, voice);
        if let Some(channel_id) = channel_id {
            let mut channel = serenity::model::channel::GuildChannel::default();
            channel.id = channel_id;
            channel.guild_id = guild.id;
            channel.name = "Lobby".into();
            guild.channels.insert(channel_id, channel);
        }
    }
    let mut event: GuildCreateEvent =
        serde_json::from_value(serde_json::to_value(guild).unwrap()).unwrap();
    cache.update(&mut event);
}

fn target_member(guild_id: u64, user_id: u64, nick: Option<&str>) -> Member {
    let mut user = User::default();
    user.id = UserId::new(user_id);
    user.name = "targetuser".into();
    user.global_name = Some("Target Display".into());
    let mut member = Member::default();
    member.user = user;
    member.guild_id = GuildId::new(guild_id);
    member.nick = nick.map(str::to_string);
    member.roles = vec![RoleId::new(50)];
    member.joined_at = Some(serenity::model::prelude::Timestamp::now());
    member
}

#[test]
fn cached_member_renders_profile_warns_and_cases() {
    let cache = Cache::new();
    let member = target_member(10, 91, Some("Nick"));
    remember_guild(
        &cache,
        10,
        Some(member),
        Some(
            serde_json::from_value(serde_json::json!({
                "channel_id": "700",
                "user_id": "91",
                "session_id": "sess",
                "deaf": false,
                "mute": false,
                "self_deaf": false,
                "self_mute": false,
                "self_video": false,
                "suppress": false
            }))
            .unwrap(),
        ),
    );
    let snapshot = capture_member_snapshot(
        GuildId::new(10),
        UserId::new(91),
        UserId::new(8),
        &[],
        &cache,
    );
    assert!(snapshot.member_cached);
    assert_eq!(snapshot.nick.as_deref(), Some("Nick"));
    assert_eq!(snapshot.roles, vec!["Mod".to_string()]);
    assert!(matches!(
        snapshot.voice,
        VoiceSnapshot::Channel { id: 700, .. }
    ));

    let lines = vec![
        format_case_line("ban", Some("succeeded"), Some(1_700_000_000_000), None),
        format_case_line("warn", Some("succeeded"), Some(1_600_000_000_000), None),
    ];
    let body = render_userinfo_message(
        jmodguard::defaults::USER_INFO_TEMPLATE,
        &snapshot,
        &WarnsField::Count(2),
        lines,
    );
    assert!(body.contains("`91`"));
    assert!(body.contains("targetuser"));
    assert!(body.contains("Nick"));
    assert!(body.contains("Lobby"));
    assert!(body.contains("**Warnings:** 2"));
    assert!(body.contains("`ban`"));
    assert!(body.contains("`warn`"));
}

#[test]
fn case_reason_strips_ban_tag() {
    let tag = jmodguard::ban_reason::BanAttemptTag::new();
    let reason = jmodguard::ban_reason::discord_reason("spam links", Some(&tag));
    let line = format_case_line(
        "ban",
        Some("succeeded"),
        Some(1_700_000_000_000),
        Some(&reason),
    );
    assert!(line.contains("spam links"));
    assert!(!line.contains("jmg:"));
}

#[test]
fn profile_and_case_text_is_sanitized_like_rejoin_reports() {
    let snapshot = MemberSnapshot {
        user_id: 91,
        username: Some("ada <@123>".into()),
        global_name: Some("Ada @everyone".into()),
        nick: Some("nick\n<@123> @everyone".into()),
        joined_at: None,
        roles: vec!["crew <@&55> @here".into()],
        roles_cached: true,
        voice: VoiceSnapshot::Channel {
            name: "room <@8> @here".into(),
            id: 7,
        },
        member_cached: true,
        guild_cached: true,
    };
    let line = format_case_line(
        "ban",
        Some("succeeded"),
        Some(1_700_000_000_000),
        Some("line1\n- Case #99: <@123> @everyone `break`"),
    );
    let body = render_userinfo_message(
        jmodguard::defaults::USER_INFO_TEMPLATE,
        &snapshot,
        &WarnsField::Count(0),
        vec![line],
    );
    let nick = body
        .lines()
        .find(|line| line.contains("**Nick:**"))
        .expect("nick line");
    assert!(nick.contains("\\<\\@123\\>"), "{nick}");
    assert!(nick.contains("\\@everyone"), "{nick}");
    assert!(
        !contains_unescaped(nick, "<@123>"),
        "nickname must not keep a live mention: {nick}"
    );
    assert!(!contains_unescaped(nick, "@everyone"), "{nick}");
    assert!(
        nick.contains("nick \\<\\@123\\>"),
        "a newline in the nickname must collapse onto the nick line: {nick}"
    );
    assert!(body.contains("\\@everyone"));
    assert!(body.contains("crew \\<\\@&55\\> \\@here"), "{body}");
    assert!(body.contains("room \\<\\@8\\> \\@here"), "{body}");
    assert!(body.contains("Case \\#99"), "{body}");
    assert!(body.contains("\\`break\\`"), "{body}");
    assert!(!contains_unescaped(&body, "<@123>"), "{body}");
    assert!(!contains_unescaped(&body, "@everyone"), "{body}");
    assert!(!contains_unescaped(&body, "@here"), "{body}");
    let vars = userinfo_template_vars(
        &snapshot,
        &WarnsField::Count(0),
        &CasesField::None,
        &snapshot.roles,
        &[],
        "",
    );
    assert_eq!(vars["username"], "ada \\<\\@123\\>");
    assert_eq!(vars["global_name"], "Ada \\@everyone");
    assert_eq!(vars["nick"], "nick \\<\\@123\\> \\@everyone");
    assert_eq!(vars["display"], vars["nick"]);
    assert_eq!(vars["roles"], "crew \\<\\@&55\\> \\@here");
}

fn contains_unescaped(text: &str, needle: &str) -> bool {
    text.match_indices(needle)
        .any(|(index, _)| !text[..index].ends_with('\\'))
}

#[tokio::test]
async fn userinfo_prefix_and_slash_replies_do_not_activate_mentions() {
    const USER_MENTION: &str = "<@123456789012345678>";
    const ROLE_MENTION: &str = "<@&50>";
    let snapshot = MemberSnapshot {
        user_id: 91,
        username: Some("ada".into()),
        global_name: Some("Ada".into()),
        nick: Some(format!("{USER_MENTION} @everyone")),
        joined_at: None,
        roles: vec![format!("{ROLE_MENTION} @here")],
        roles_cached: true,
        voice: VoiceSnapshot::Channel {
            name: format!("room {USER_MENTION} @here"),
            id: 7,
        },
        member_cached: true,
        guild_cached: true,
    };
    let line = format_case_line(
        "ban",
        Some("succeeded"),
        Some(1_700_000_000_000),
        Some(&format!("{USER_MENTION} {ROLE_MENTION} @everyone @here")),
    );
    let body = render_userinfo_message(
        jmodguard::defaults::USER_INFO_TEMPLATE,
        &snapshot,
        &WarnsField::Count(1),
        vec![line],
    );
    let path = std::env::temp_dir().join(format!(
        "userinfo_mentions_{:032x}.db",
        rand::random::<u128>()
    ));
    let app = Arc::new(
        BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap(),
    );
    app.activation.mark_lock_held(true);
    let (http, captured, server) = capturing_proxy().await;
    let http = Arc::new(http);
    let cache = Cache::new();
    let mut userinfo = jmodguard::defaults::default_commands()
        .into_iter()
        .find(|cmd| matches!(cmd.behavior, jmodguard::types::Behavior::UserInfo))
        .unwrap();
    userinfo.log_event.clear();
    let outcome = super::super::BehaviorOutcome {
        response: Some(body),
        vars: std::collections::BTreeMap::new(),
        emitted_log_events: Vec::new(),
        suppress_response: false,
        embed: None,
    };
    let slash = super::super::SlashCtx {
        id: serenity::model::id::InteractionId::new(1),
        token: "token".into(),
        channel_id: ChannelId::new(2),
        guild_id: GuildId::new(10),
        actor: UserId::new(7),
        member: None,
        name: "userinfo".into(),
        options: Vec::new(),
    };
    super::super::deliver_slash_success(
        &app.activation,
        http.as_ref(),
        &slash,
        super::super::SlashResponseState::DeferredPublic,
        &userinfo,
        &outcome,
    )
    .await;
    super::super::deliver_slash_success(
        &app.activation,
        http.as_ref(),
        &slash,
        super::super::SlashResponseState::Initial,
        &userinfo,
        &outcome,
    )
    .await;
    super::super::send_outcome(
        &app,
        http.as_ref(),
        &cache,
        &userinfo,
        GuildId::new(10),
        ChannelId::new(2),
        UserId::new(7),
        outcome,
    )
    .await;
    let mut say = jmodguard::defaults::default_commands()
        .into_iter()
        .find(|cmd| matches!(cmd.behavior, jmodguard::types::Behavior::Say))
        .unwrap();
    say.response.clear();
    say.log_event.clear();
    super::super::send_outcome(
        &app,
        http.as_ref(),
        &cache,
        &say,
        GuildId::new(10),
        ChannelId::new(2),
        UserId::new(7),
        super::super::BehaviorOutcome {
            response: Some(format!("{USER_MENTION} @everyone")),
            vars: std::collections::BTreeMap::new(),
            emitted_log_events: Vec::new(),
            suppress_response: false,
            embed: None,
        },
    )
    .await;
    let requests = captured.lock().unwrap().clone();
    assert_eq!(
        requests.len(),
        4,
        "deferred slash, initial slash, prefix, and say: {requests:?}"
    );
    for request in &requests[..3] {
        let payload = message_fields(request);
        assert_eq!(
            payload["allowed_mentions"]["parse"],
            serde_json::json!([]),
            "{payload}"
        );
        assert_eq!(payload["allowed_mentions"]["replied_user"], false);
        let content = payload["content"].as_str().unwrap();
        for raw in [USER_MENTION, ROLE_MENTION, "@everyone", "@here"] {
            assert!(
                !contains_unescaped(content, raw),
                "{raw} remained mentionable in {content}"
            );
        }
    }
    let say_payload = message_fields(&requests[3]);
    assert!(
        say_payload.get("allowed_mentions").is_none(),
        "say must keep Discord's normal mention parsing: {say_payload}"
    );
    let say_content = say_payload["content"].as_str().unwrap();
    assert!(
        contains_unescaped(say_content, USER_MENTION),
        "{say_content}"
    );
    assert!(
        contains_unescaped(say_content, "@everyone"),
        "{say_content}"
    );
    server.abort();
    drop(app);
    crate::tenant::remove_sidecars(&path);
    let _ = std::fs::remove_file(path);
}

fn request_json(raw: &str) -> serde_json::Value {
    let (_, body) = raw
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("request had no body: {raw}"));
    serde_json::from_str(body).unwrap_or_else(|error| panic!("{error} in {body}"))
}

/// Channel posts and deferred edits put the message at the top level. The
/// initial interaction callback wraps that same object in `data`.
fn message_fields(raw: &str) -> serde_json::Value {
    let payload = request_json(raw);
    payload
        .get("data")
        .filter(|data| data.is_object())
        .cloned()
        .unwrap_or(payload)
}

async fn capturing_proxy() -> (
    serenity::http::Http,
    Arc<std::sync::Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    capturing_proxy_for_commands(false).await
}

async fn capturing_proxy_for_commands(
    success: bool,
) -> (
    serenity::http::Http,
    Arc<std::sync::Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    capturing_proxy_with(success, false).await
}

/// `reject_embeds` answers any request carrying an embed with Discord's
/// Missing Permissions (403/50013), as a channel without Embed Links does.
async fn capturing_proxy_with(
    success: bool,
    reject_embeds: bool,
) -> (
    serenity::http::Http,
    Arc<std::sync::Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let slot = Arc::clone(&captured);
    let server = tokio::spawn(async move {
        let mut banned = false;
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut request = Vec::new();
            let mut header_end = None;
            loop {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
                if header_end.is_none() {
                    header_end = request
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .map(|index| index + 4);
                }
                if let Some(end) = header_end {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if request.len() >= end + length {
                        break;
                    }
                }
            }
            let request = String::from_utf8_lossy(&request).into_owned();
            let route = request.lines().next().unwrap_or_default();
            let (status, body) = if !success {
                ("403 Forbidden", "{}".to_string())
            } else if reject_embeds && request.contains("\"embeds\":[{") {
                (
                    "403 Forbidden",
                    r#"{"code":50013,"message":"Missing Permissions"}"#.into(),
                )
            } else if route.starts_with("GET ") && route.contains("/bans/") && banned {
                ("200 OK", serde_json::json!({"user": target_member(10, 91, None).user, "reason": "test reason"}).to_string())
            } else if route.starts_with("GET ") && route.contains("/bans/") {
                (
                    "404 Not Found",
                    r#"{"code":10026,"message":"Unknown Ban"}"#.into(),
                )
            } else if route.starts_with("PATCH ") && route.contains("/members/") {
                (
                    "200 OK",
                    serde_json::to_string(&target_member(10, 91, None)).unwrap(),
                )
            } else if route.starts_with("PATCH ") && route.contains("/channels/") {
                let mut channel = serenity::model::channel::GuildChannel::default();
                channel.id = ChannelId::new(1);
                channel.guild_id = GuildId::new(10);
                ("200 OK", serde_json::to_string(&channel).unwrap())
            } else if route.starts_with("POST ") && route.contains("/messages") {
                (
                    "200 OK",
                    serde_json::to_string(&serenity::model::channel::Message::default()).unwrap(),
                )
            } else if route.starts_with("GET ") && route.contains("/messages") {
                ("200 OK", "[]".into())
            } else {
                ("204 No Content", String::new())
            };
            if route.starts_with("PUT ") && route.contains("/bans/") {
                banned = true;
            }
            if route.starts_with("DELETE ") && route.contains("/bans/") {
                banned = false;
            }
            slot.lock().unwrap().push(request);
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
    (http, captured, server)
}

#[test]
fn history_unavailable_does_not_say_none() {
    let snapshot = MemberSnapshot {
        user_id: 91,
        username: Some("u".into()),
        global_name: None,
        nick: None,
        joined_at: None,
        roles: vec![],
        roles_cached: true,
        voice: VoiceSnapshot::NotInChannel,
        member_cached: true,
        guild_cached: true,
    };
    let body = render_userinfo_with_cases(
        jmodguard::defaults::USER_INFO_TEMPLATE,
        &snapshot,
        &WarnsField::Count(0),
        CasesField::Unavailable,
        vec![],
    );
    assert!(body.contains(CASES_UNAVAILABLE));
    assert!(!body.to_lowercase().contains("none"));
}

fn stay_row(at: i64, stay_id: &str) -> MemberHistoryEntry {
    MemberHistoryEntry {
        kind: "join".into(),
        occurred_at_ms: Some(at),
        observed_at_ms: None,
        stay_id: Some(stay_id.into()),
        observation_id: None,
        case_id: None,
        joined_at_ms: Some(at),
        departed_at_ms: None,
        moderator_id: None,
        reason: None,
        outcome: None,
        source_kind: None,
    }
}

fn case_row(at: i64, case_id: &str) -> MemberHistoryEntry {
    MemberHistoryEntry {
        kind: "ban".into(),
        occurred_at_ms: Some(at),
        observed_at_ms: None,
        stay_id: None,
        observation_id: None,
        case_id: Some(case_id.into()),
        joined_at_ms: None,
        departed_at_ms: None,
        moderator_id: None,
        reason: None,
        outcome: Some("succeeded".into()),
        source_kind: None,
    }
}

fn absorb_case_page(
    cases: &mut Vec<MemberHistoryEntry>,
    page: &MemberHistoryPage,
    limit: usize,
) -> bool {
    for entry in &page.entries {
        if entry.case_id.is_none() {
            continue;
        }
        if cases.len() < limit {
            cases.push(entry.clone());
        } else {
            return true;
        }
    }
    false
}

fn collect_case_entries_from_pages(pages: &[MemberHistoryPage]) -> Vec<MemberHistoryEntry> {
    let limit = CASE_LINE_LIMIT as usize;
    let mut entries = Vec::new();
    for page in pages {
        if absorb_case_page(&mut entries, page, limit) {
            break;
        }
    }
    entries
}

#[tokio::test]
async fn load_case_lines_from_store_past_stay_rows_on_first_page() {
    let path = std::env::temp_dir().join(format!(
        "userinfo_store_cases_{:032x}.db",
        rand::random::<u128>()
    ));
    let app = Arc::new(
        BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap(),
    );
    let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
        .await
        .unwrap()
        .unwrap();
    app.tenant.activate(enrollment.clone()).await.unwrap();
    let (store, scope) = app.tenant.live(enrollment.clone()).await.unwrap();
    ready_userinfo_case_index(&app, &enrollment).await;
    crate::cases::record(
        &app,
        Some(enrollment.clone()),
        CaseAction::Ban,
        CaseOutcome::Succeeded,
        91,
        Some(7),
        Some("indexed ban".into()),
        CaseEvidence::Unavailable,
        crate::cases::application_source(),
    )
    .await;
    for round in 0..8_i64 {
        store
            .observe_member(
                scope.clone(),
                MemberObservation {
                    user_id: 91,
                    username: Some(Some("targetuser".into())),
                    global_name: None,
                    nickname: None,
                    joined_at_ms: Some(500 + round),
                    observed_at_ms: 10_000 + round,
                    is_join: true,
                },
            )
            .await
            .unwrap();
        store
            .depart_member(scope.clone(), 91, 20_000 + round)
            .await
            .unwrap();
    }
    let cases = load_case_lines_from_store(&store, scope, UserId::new(91), None).await;
    let lines = match cases {
        CasesField::Lines(lines) => lines,
        other => panic!("expected case lines, got {other:?}"),
    };
    assert!(lines.iter().any(|line| line.contains("`ban`")));
    drop(app);
    crate::tenant::remove_sidecars(&path);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn load_case_lines_from_store_rejects_default_budget_oversize_payload() {
    let path = std::env::temp_dir().join(format!(
        "userinfo_store_budget_{:032x}.db",
        rand::random::<u128>()
    ));
    let app = Arc::new(
        BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap(),
    );
    let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
        .await
        .unwrap()
        .unwrap();
    app.tenant.activate(enrollment.clone()).await.unwrap();
    let (store, scope) = app.tenant.live(enrollment.clone()).await.unwrap();
    ready_userinfo_case_index(&app, &enrollment).await;
    store
        .backfill_member_case_index(scope.clone(), 100, 100)
        .await
        .unwrap();
    let write_limits = CaseLimits {
        max_record_bytes: 1_048_576,
        ..CaseLimits::default()
    };
    store
        .record_case(
            scope.clone(),
            CaseId::new(),
            CaseData {
                ban_attempt_tag: None,
                source: CaseSource::Application {
                    operation_id: CaseId::new(),
                },
                action: CaseAction::Ban,
                outcome: CaseOutcome::Succeeded,
                subject_id: 91,
                actor_id: Some(7),
                occurred_at_ms: 3_000,
                reason: Some("z".repeat(262_144)),
                evidence: CaseEvidence::Unavailable,
            },
            write_limits,
        )
        .await
        .unwrap();
    let cases = load_case_lines_from_store(&store, scope, UserId::new(91), None).await;
    assert!(
        matches!(cases, CasesField::Unavailable),
        "oversize indexed case must not surface through userinfo: {cases:?}"
    );
    drop(app);
    crate::tenant::remove_sidecars(&path);
    let _ = std::fs::remove_file(path);
}

#[test]
fn paginate_case_lines_past_stay_rows_on_first_page() {
    let page1 = MemberHistoryPage {
        entries: vec![
            stay_row(9_000, "stay-a"),
            stay_row(8_000, "stay-b"),
            stay_row(7_000, "stay-c"),
            stay_row(6_000, "stay-d"),
            stay_row(5_000, "stay-e"),
            stay_row(4_000, "stay-f"),
            stay_row(3_000, "stay-g"),
            case_row(2_000, "case-one"),
        ],
        next_cursor: Some("2000:ban:case-one".into()),
    };
    let page2 = MemberHistoryPage {
        entries: vec![case_row(1_000, "case-two")],
        next_cursor: None,
    };
    let entries = collect_case_entries_from_pages(&[page1, page2]);
    assert_eq!(entries.len(), 2);
    let lines = case_lines_from_entries(&entries, None);
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("`ban`"));
    let snapshot = MemberSnapshot {
        user_id: 91,
        username: Some("u".into()),
        global_name: None,
        nick: None,
        joined_at: None,
        roles: vec![],
        roles_cached: true,
        voice: VoiceSnapshot::NotInChannel,
        member_cached: true,
        guild_cached: true,
    };
    let body = render_userinfo_message(
        jmodguard::defaults::USER_INFO_TEMPLATE,
        &snapshot,
        &WarnsField::Count(0),
        lines,
    );
    assert!(!body.contains(CASES_NONE));
    assert!(body.contains("`ban`"));
}

#[tokio::test]
async fn rest_lookup_failure_still_renders_warns_and_cases() {
    let path = std::env::temp_dir().join(format!(
        "userinfo_rest_fail_{:032x}.db",
        rand::random::<u128>()
    ));
    let app = Arc::new(
        BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap(),
    );
    let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
        .await
        .unwrap()
        .unwrap();
    app.tenant.activate(enrollment.clone()).await.unwrap();
    ready_userinfo_case_index(&app, &enrollment).await;
    crate::db::add_warn(&app.db, 10, 91, 7, "warned")
        .await
        .unwrap();
    crate::cases::record(
        &app,
        Some(enrollment.clone()),
        CaseAction::Warning,
        CaseOutcome::Succeeded,
        91,
        Some(7),
        Some("logged".into()),
        CaseEvidence::Unavailable,
        crate::cases::application_source(),
    )
    .await;
    let cache = Arc::new(Cache::new());
    remember_guild(&cache, 10, None, None);
    let (http, requested, server) = rejecting_user_proxy().await;
    let ctx = test_context(cache, Arc::new(http)).await;
    let cmd = jmodguard::defaults::default_commands()
        .into_iter()
        .find(|c| matches!(c.behavior, jmodguard::types::Behavior::UserInfo))
        .unwrap();
    let actor = target_member(10, 7, None);
    let env = crate::commands::CmdEnv {
        app: &app,
        ctx: &ctx,
        cmd: &cmd,
        guild_id: GuildId::new(10),
        channel_id: ChannelId::new(1),
        actor: UserId::new(7),
        actor_member: actor,
        args: Args::new("91"),
        dm: false,
        source_message: None,
    };
    let mut vars = crate::commands::behaviors::behaviors_base_vars(&env);
    let outcome = super::run(&env, Some(enrollment), &mut vars).await.unwrap();
    let body = outcome.response.unwrap();
    assert!(body.contains(MISSING));
    assert!(body.contains("**Warnings:** 1"));
    assert!(body.contains("`warning`"));
    assert!(!body.contains("Discord API error"));
    let requested = requested.lock().unwrap().clone();
    assert!(
        requested.contains("/users/91"),
        "user lookup must hit the local proxy, got {requested:?}"
    );
    server.abort();
    drop(app);
    crate::tenant::remove_sidecars(&path);
    let _ = std::fs::remove_file(path);
}

#[test]
fn empty_history_says_none() {
    let snapshot = MemberSnapshot {
        user_id: 91,
        username: Some("u".into()),
        global_name: None,
        nick: None,
        joined_at: None,
        roles: vec![],
        roles_cached: true,
        voice: VoiceSnapshot::NotInChannel,
        member_cached: true,
        guild_cached: true,
    };
    let body = render_userinfo_with_cases(
        jmodguard::defaults::USER_INFO_TEMPLATE,
        &snapshot,
        &WarnsField::Count(0),
        CasesField::None,
        vec![],
    );
    assert!(body.contains(CASES_NONE));
}

#[test]
fn empty_guild_cache_marks_voice_not_cached() {
    let cache = Cache::new();
    let snapshot = capture_member_snapshot(
        GuildId::new(10),
        UserId::new(91),
        UserId::new(8),
        &[],
        &cache,
    );
    assert!(!snapshot.guild_cached);
    assert_eq!(format_voice(&snapshot.voice), VOICE_NOT_CACHED);
}

#[test]
fn cached_member_does_not_need_rest_lookup() {
    let cache = Cache::new();
    remember_guild(&cache, 10, Some(target_member(10, 91, None)), None);
    let snapshot = capture_member_snapshot(
        GuildId::new(10),
        UserId::new(91),
        UserId::new(8),
        &[],
        &cache,
    );
    assert!(snapshot_has_cached_user(&snapshot));
}

#[test]
fn uncached_user_requires_rest_lookup() {
    let cache = Cache::new();
    let snapshot = capture_member_snapshot(
        GuildId::new(10),
        UserId::new(91),
        UserId::new(8),
        &[],
        &cache,
    );
    assert!(!snapshot_has_cached_user(&snapshot));
}

#[test]
fn clip_drops_roles_but_keeps_user_id() {
    let roles: Vec<String> = (0..80)
        .map(|i| format!("role-{i}-with-long-name"))
        .collect();
    let snapshot = MemberSnapshot {
        user_id: 42,
        username: Some("u".into()),
        global_name: None,
        nick: None,
        joined_at: Some("2020-01-01T00:00:00Z".into()),
        roles,
        roles_cached: true,
        voice: VoiceSnapshot::NotInChannel,
        member_cached: true,
        guild_cached: true,
    };
    let case_lines = (0..8)
        .map(|i| format_case_line("ban", Some("succeeded"), Some(1_700_000_000_000 + i), None))
        .collect();
    let body = render_userinfo_message(
        jmodguard::defaults::USER_INFO_TEMPLATE,
        &snapshot,
        &WarnsField::Count(0),
        case_lines,
    );
    assert!(body.contains("`42`"));
    assert!(body.contains("clipped"));
}

#[tokio::test]
async fn omitted_user_argument_defaults_to_actor() {
    let path = std::env::temp_dir().join(format!(
        "userinfo_actor_default_{:032x}.db",
        rand::random::<u128>()
    ));
    let app = Arc::new(
        BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap(),
    );
    let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
        .await
        .unwrap()
        .unwrap();
    app.tenant.activate(enrollment.clone()).await.unwrap();
    ready_userinfo_case_index(&app, &enrollment).await;
    let cache = Arc::new(Cache::new());
    remember_guild(&cache, 10, Some(target_member(10, 7, None)), None);
    let (http, requested, server) = rejecting_user_proxy().await;
    let ctx = test_context(cache, Arc::new(http)).await;
    let cmd = jmodguard::defaults::default_commands()
        .into_iter()
        .find(|c| matches!(c.behavior, jmodguard::types::Behavior::UserInfo))
        .unwrap();
    let actor = target_member(10, 7, Some("Mod"));
    let env = crate::commands::CmdEnv {
        app: &app,
        ctx: &ctx,
        cmd: &cmd,
        guild_id: GuildId::new(10),
        channel_id: ChannelId::new(1),
        actor: UserId::new(7),
        actor_member: actor,
        args: Args::new(""),
        dm: false,
        source_message: None,
    };
    let target = super::resolve_userinfo_target(&env, &mut env.args.clone())
        .await
        .unwrap();
    assert_eq!(target, UserId::new(7));
    let mut args_other = Args::new("91");
    let other = super::resolve_userinfo_target(&env, &mut args_other)
        .await
        .unwrap();
    assert_eq!(other, UserId::new(91));
    assert!(
        requested.lock().unwrap().is_empty(),
        "resolving a cached or numeric user must not call Discord"
    );
    server.abort();
    drop(app);
    crate::tenant::remove_sidecars(&path);
    let _ = std::fs::remove_file(path);
}

#[test]
fn default_template_renders_without_embedded_paths() {
    let snapshot = MemberSnapshot {
        user_id: 5,
        username: Some("name".into()),
        global_name: None,
        nick: None,
        joined_at: None,
        roles: vec![],
        roles_cached: true,
        voice: VoiceSnapshot::NotInChannel,
        member_cached: true,
        guild_cached: true,
    };
    let template = jmodguard::defaults::USER_INFO_TEMPLATE;
    assert!(!template.contains("/"));
    let body = render_userinfo_with_cases(
        template,
        &snapshot,
        &WarnsField::Count(1),
        CasesField::None,
        vec![],
    );
    assert!(body.contains("**ID:** `5`"));
}

async fn ready_userinfo_case_index(app: &BotState, enrollment: &crate::db::GuildEnrollment) {
    let (store, scope) = app.tenant.live(enrollment.clone()).await.unwrap();
    store
        .initialize_member_directory(scope.clone(), 100, 1_000)
        .await
        .unwrap();
    while !store
        .backfill_member_case_index(scope.clone(), 100, 100)
        .await
        .unwrap()
    {}
}

#[tokio::test]
async fn integration_warn_and_case_rows() {
    let path = std::env::temp_dir().join(format!("userinfo_db_{:032x}.db", rand::random::<u128>()));
    let app = Arc::new(
        BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap(),
    );
    let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
        .await
        .unwrap()
        .unwrap();
    app.tenant.activate(enrollment.clone()).await.unwrap();
    ready_userinfo_case_index(&app, &enrollment).await;
    crate::db::add_warn(&app.db, 10, 91, 7, "first")
        .await
        .unwrap();
    crate::cases::record(
        &app,
        Some(enrollment.clone()),
        CaseAction::Ban,
        CaseOutcome::Succeeded,
        91,
        Some(7),
        Some("visible".into()),
        CaseEvidence::Unavailable,
        crate::cases::application_source(),
    )
    .await;
    let cache = Arc::new(Cache::new());
    remember_guild(&cache, 10, Some(target_member(10, 91, Some("Nick"))), None);
    let (http, requested, server) = rejecting_user_proxy().await;
    let ctx = test_context(cache, Arc::new(http)).await;
    let cmd = jmodguard::defaults::default_commands()
        .into_iter()
        .find(|c| matches!(c.behavior, jmodguard::types::Behavior::UserInfo))
        .unwrap();
    let actor = target_member(10, 91, Some("Nick"));
    let env = crate::commands::CmdEnv {
        app: &app,
        ctx: &ctx,
        cmd: &cmd,
        guild_id: GuildId::new(10),
        channel_id: ChannelId::new(1),
        actor: UserId::new(91),
        actor_member: actor,
        args: Args::new(""),
        dm: false,
        source_message: None,
    };
    let mut vars = crate::commands::behaviors::behaviors_base_vars(&env);
    let outcome = super::run(&env, Some(enrollment), &mut vars).await.unwrap();
    let body = outcome.response.unwrap();
    assert!(body.contains("**Warnings:** 1"));
    assert!(body.contains("`ban`"));
    assert!(body.contains("`91`"));
    assert!(
        requested.lock().unwrap().is_empty(),
        "a cached member must not call Discord"
    );
    server.abort();
    drop(app);
    crate::tenant::remove_sidecars(&path);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn retained_case_visible_after_join_depart_noise() {
    let path = std::env::temp_dir().join(format!(
        "userinfo_cases_db_{:032x}.db",
        rand::random::<u128>()
    ));
    let app = Arc::new(
        BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap(),
    );
    let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
        .await
        .unwrap()
        .unwrap();
    app.tenant.activate(enrollment.clone()).await.unwrap();
    let (store, scope) = app.tenant.live(enrollment.clone()).await.unwrap();
    store
        .initialize_member_directory(scope.clone(), 100, 1_000)
        .await
        .unwrap();
    while !store
        .backfill_member_case_index(scope.clone(), 100, 100)
        .await
        .unwrap()
    {}
    crate::cases::record(
        &app,
        Some(enrollment.clone()),
        CaseAction::Ban,
        CaseOutcome::Succeeded,
        91,
        Some(7),
        Some("still visible".into()),
        CaseEvidence::Unavailable,
        crate::cases::application_source(),
    )
    .await;
    for round in 0..12_i64 {
        store
            .observe_member(
                scope.clone(),
                MemberObservation {
                    user_id: 91,
                    username: Some(Some("targetuser".into())),
                    global_name: None,
                    nickname: None,
                    joined_at_ms: Some(500 + round),
                    observed_at_ms: 10_000 + round,
                    is_join: true,
                },
            )
            .await
            .unwrap();
        store
            .depart_member(scope.clone(), 91, 20_000 + round)
            .await
            .unwrap();
    }
    let cases = load_case_lines_from_store(&store, scope, UserId::new(91), None).await;
    let lines = match cases {
        CasesField::Lines(lines) => lines,
        other => panic!("expected case lines, got {other:?}"),
    };
    assert!(
        lines.iter().any(|line| line.contains("`ban`")),
        "ban case must appear despite join/depart history: {lines:?}"
    );
    drop(app);
    crate::tenant::remove_sidecars(&path);
    let _ = std::fs::remove_file(path);
}

#[test]
fn guild_create_voice_without_guild_id_is_not_rejected() {
    let cache = Cache::new();
    remember_guild(
        &cache,
        10,
        Some(target_member(10, 91, None)),
        Some(
            serde_json::from_value(serde_json::json!({
                "channel_id": "700",
                "user_id": "91",
                "session_id": "sess",
                "deaf": false,
                "mute": false,
                "self_deaf": false,
                "self_mute": false,
                "self_video": false,
                "suppress": false
            }))
            .unwrap(),
        ),
    );
    let snapshot = capture_member_snapshot(
        GuildId::new(10),
        UserId::new(91),
        UserId::new(8),
        &[],
        &cache,
    );
    assert!(matches!(
        snapshot.voice,
        VoiceSnapshot::Channel { id: 700, .. }
    ));
}

async fn rejecting_user_proxy() -> (
    serenity::http::Http,
    Arc<std::sync::Mutex<String>>,
    tokio::task::JoinHandle<()>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let captured = Arc::new(std::sync::Mutex::new(String::new()));
    let slot = Arc::clone(&captured);
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
            *slot.lock().unwrap() = String::from_utf8_lossy(&request).into_owned();
            let body = r#"{"code":10013,"message":"fixture"}"#;
            let response = format!(
                "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
    (http, captured, server)
}

async fn test_context(
    cache: Arc<Cache>,
    http: Arc<serenity::http::Http>,
) -> serenity::client::Context {
    use serenity::client::Context;
    use serenity::gateway::{
        Shard, ShardManager, ShardManagerOptions, ShardMessenger, ShardRunner, ShardRunnerOptions,
    };
    use serenity::model::gateway::{GatewayIntents, ShardInfo};
    use serenity::model::id::ShardId;
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    // Test-only loopback connection with a synthetic token, never a production gateway.
    let url = Arc::new(tokio::sync::Mutex::new(format!(
        "ws://{}", // nosemgrep: javascript.lang.security.detect-insecure-websocket
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

#[tokio::test]
async fn userinfo_prefix_and_slash_hide_voice_the_caller_cannot_view() {
    use crate::commands::voice_lookup::{VoiceLookupSnapshot, snapshot_cached_voice};
    use crate::state::CachedGuildConfig;
    use serenity::model::channel::{
        ChannelType, GuildChannel, PermissionOverwrite, PermissionOverwriteType,
    };
    use serenity::model::permissions::Permissions;
    const GUILD: u64 = 10;
    const ACTOR: u64 = 7;
    const TARGET: u64 = 91;
    const ROLE: u64 = 50;
    const VOICE: u64 = 700;
    const VISIBLE_CASE_CHANNEL: u64 = 801;
    const HIDDEN_CASE_CHANNEL: u64 = 802;

    let path = std::env::temp_dir().join(format!(
        "userinfo_voice_view_{:032x}.db",
        rand::random::<u128>()
    ));
    let app = Arc::new(
        BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap(),
    );
    app.activation.mark_lock_held(true);
    let enrollment = crate::db::enroll_guild(&app.db, GUILD as i64, 100)
        .await
        .unwrap()
        .unwrap();
    app.tenant.activate(enrollment.clone()).await.unwrap();
    ready_userinfo_case_index(&app, &enrollment).await;
    crate::cases::record(
        &app,
        Some(enrollment),
        CaseAction::Warning,
        CaseOutcome::Succeeded,
        TARGET,
        Some(ACTOR),
        Some(format!(
            "see <#{VISIBLE_CASE_CHANNEL}> and <#{HIDDEN_CASE_CHANNEL}>"
        )),
        CaseEvidence::Unavailable,
        crate::cases::application_source(),
    )
    .await;
    let cache = Arc::new(Cache::new());
    let (http, captured, server) = capturing_proxy().await;
    let http = Arc::new(http);
    let ctx = test_context(Arc::clone(&cache), Arc::clone(&http)).await;
    let commands = jmodguard::defaults::default_commands()
        .into_iter()
        .map(|mut cmd| {
            if matches!(cmd.behavior, jmodguard::types::Behavior::UserInfo) {
                cmd.log_event.clear();
            }
            (cmd.name.clone(), cmd)
        })
        .collect();
    let cfg = Arc::new(CachedGuildConfig {
        version: 1,
        durable_version: 1,
        loaded_at: std::time::Instant::now(),
        settings: crate::types::GuildSettings {
            prefix: "!".into(),
            ..Default::default()
        },
        rules: Default::default(),
        commands,
        templates: Default::default(),
        log_channels: Default::default(),
        known_bad_hashes: Default::default(),
        known_spam: Default::default(),
        log_filters: Default::default(),
        regexes: Vec::new(),
        custom_patterns: Default::default(),
    });

    let install = |name: Option<&str>, hidden: bool| {
        let mut guild = Guild::default();
        guild.id = GuildId::new(GUILD);
        guild.owner_id = UserId::new(99);
        guild.name = "fixture".into();
        let mut everyone = Role::default();
        everyone.id = RoleId::new(GUILD);
        everyone.guild_id = guild.id;
        everyone.permissions = Permissions::empty();
        guild.roles.insert(everyone.id, everyone);
        let mut mod_role = Role::default();
        mod_role.id = RoleId::new(ROLE);
        mod_role.guild_id = guild.id;
        mod_role.name = "Mod".into();
        mod_role.permissions = Permissions::MANAGE_MESSAGES | Permissions::VIEW_CHANNEL;
        guild.roles.insert(mod_role.id, mod_role);
        let mut notes = GuildChannel::default();
        notes.id = ChannelId::new(VISIBLE_CASE_CHANNEL);
        notes.guild_id = guild.id;
        notes.name = "notes".into();
        guild.channels.insert(notes.id, notes);
        if let Some(name) = name {
            let mut voice = GuildChannel::default();
            voice.id = ChannelId::new(VOICE);
            voice.guild_id = guild.id;
            voice.kind = ChannelType::Voice;
            voice.name = name.into();
            if hidden {
                voice.permission_overwrites = vec![PermissionOverwrite {
                    kind: PermissionOverwriteType::Member(UserId::new(ACTOR)),
                    allow: Permissions::empty(),
                    deny: Permissions::VIEW_CHANNEL,
                }];
            }
            guild.channels.insert(voice.id, voice);
        }
        guild.members.insert(
            UserId::new(TARGET),
            target_member(GUILD, TARGET, Some("Nick")),
        );
        let mut caller = target_member(GUILD, ACTOR, None);
        caller.user.name = "mod".into();
        caller.roles = vec![RoleId::new(ROLE)];
        guild.members.insert(caller.user.id, caller);
        guild.voice_states.insert(
            UserId::new(TARGET),
            serde_json::from_value(serde_json::json!({
                "channel_id": VOICE.to_string(),
                "user_id": TARGET.to_string(),
                "guild_id": GUILD.to_string(),
                "session_id": "sess",
                "deaf": false,
                "mute": false,
                "self_deaf": false,
                "self_mute": false,
                "self_video": false,
                "suppress": false
            }))
            .unwrap(),
        );
        let mut event: GuildCreateEvent =
            serde_json::from_value(serde_json::to_value(guild).unwrap()).unwrap();
        cache.update(&mut event);
    };

    let mut author = User::default();
    author.id = UserId::new(ACTOR);
    author.name = "mod".into();
    let partial: serenity::model::guild::PartialMember =
        serde_json::from_value(serde_json::json!({
            "deaf": false,
            "joined_at": null,
            "mute": false,
            "nick": null,
            "roles": [ROLE.to_string()],
            "pending": false,
            "premium_since": null,
            "guild_id": GUILD.to_string(),
            "user": null,
            "permissions": null,
            "unusual_dm_activity_until": null,
            "avatar": null,
            "banner": null,
            "avatar_decoration_data": null,
        }))
        .unwrap();
    let mut message = serenity::model::channel::Message::default();
    message.id = serenity::model::id::MessageId::new(9);
    message.content = format!("!userinfo {TARGET}");
    message.guild_id = Some(GuildId::new(GUILD));
    message.channel_id = ChannelId::new(2);
    message.author = author.clone();
    message.member = Some(Box::new(partial));
    let mut slash_member = Member::default();
    slash_member.user = author.clone();
    slash_member.guild_id = GuildId::new(GUILD);
    slash_member.roles = vec![RoleId::new(ROLE)];
    let interaction: serenity::model::application::Interaction =
        serde_json::from_value(serde_json::json!({
            "id": "1",
            "application_id": "1",
            "type": 2,
            "token": "token",
            "version": 1,
            "guild_id": GUILD.to_string(),
            "channel_id": "2",
            "locale": "en-US",
            "app_permissions": "0",
            "entitlements": [],
            "authorizing_integration_owners": {},
            "attachment_size_limit": 10000000,
            "user": author,
            "member": slash_member,
            "data": {
                "id": "3",
                "name": "userinfo",
                "type": 1,
                "options": [{"name": "user", "type": 6, "value": TARGET.to_string()}]
            }
        }))
        .unwrap();

    let report_from = |label: &str| {
        let requests = captured.lock().unwrap().clone();
        captured.lock().unwrap().clear();
        let report = requests
            .iter()
            .find(|raw| raw.contains("**Voice:**"))
            .unwrap_or_else(|| panic!("{label} produced no userinfo report: {requests:?}"));
        assert!(
            report.contains("**User info**"),
            "{label} did not pass command authorization: {report}"
        );
        assert!(
            report.contains("\\#801") || report.contains("#801"),
            "{label} dropped a visible case channel: {report}"
        );
        assert!(
            !report.contains("802"),
            "{label} leaked an uncached case channel: {report}"
        );
        report.to_string()
    };

    install(Some("Lobby"), false);
    crate::commands::handle_prefix(
        &app,
        http.as_ref(),
        &cache,
        Arc::clone(&cfg),
        &message,
        Some(&ctx),
    )
    .await
    .unwrap();
    let prefix = report_from("visible prefix");
    crate::commands::handle_interaction(&app, &ctx, interaction.clone()).await;
    let slash = report_from("visible slash");
    for report in [&prefix, &slash] {
        let voice = voice_line(report);
        assert!(voice.contains("Lobby"), "{voice}");
        assert!(voice.contains("`700`") || voice.contains("700"), "{voice}");
    }

    install(Some("Secret staff room"), true);
    {
        let guild = cache.guild(GuildId::new(GUILD)).unwrap();
        assert_eq!(
            crate::commands::permission_level(0, &guild.roles, ACTOR, [ROLE]),
            crate::commands::PermLevel::Mod
        );
        assert!(!crate::commands::member_can_view_channel(
            &guild,
            UserId::new(ACTOR),
            &[RoleId::new(ROLE)],
            ChannelId::new(VOICE),
        ));
    }
    assert_eq!(
        snapshot_cached_voice(
            &cache,
            GuildId::new(GUILD),
            UserId::new(ACTOR),
            &[RoleId::new(ROLE)],
            UserId::new(TARGET),
        ),
        VoiceLookupSnapshot::ChannelUnavailable
    );
    crate::commands::handle_prefix(
        &app,
        http.as_ref(),
        &cache,
        Arc::clone(&cfg),
        &message,
        Some(&ctx),
    )
    .await
    .unwrap();
    let prefix = report_from("hidden prefix");
    crate::commands::handle_interaction(&app, &ctx, interaction.clone()).await;
    let slash = report_from("hidden slash");
    for report in [&prefix, &slash] {
        let voice = voice_line(report);
        assert!(voice.contains(VOICE_UNAVAILABLE), "{voice}");
        assert!(!voice.contains("Secret"), "{voice}");
        assert!(!voice.contains("700"), "{voice}");
        assert!(!report.contains("Secret staff room"), "{report}");
    }

    install(None, false);
    crate::commands::handle_prefix(&app, http.as_ref(), &cache, cfg, &message, Some(&ctx))
        .await
        .unwrap();
    let prefix = report_from("uncached prefix");
    crate::commands::handle_interaction(&app, &ctx, interaction).await;
    let slash = report_from("uncached slash");
    for report in [&prefix, &slash] {
        let voice = voice_line(report);
        assert!(voice.contains(VOICE_UNAVAILABLE), "{voice}");
        assert!(!voice.contains("700"), "{voice}");
        assert!(!report.contains("Ghost"), "{report}");
        assert!(!report.contains("700"), "{report}");
    }

    server.abort();
    drop(app);
    crate::tenant::remove_sidecars(&path);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn userinfo_prefix_and_slash_reject_invalid_mention_ids() {
    use crate::state::CachedGuildConfig;
    use serenity::model::channel::GuildChannel;
    use serenity::model::permissions::Permissions;
    const GUILD: u64 = 10;
    const ACTOR: u64 = 7;
    const TARGET: u64 = 91;
    const ROLE: u64 = 50;
    const VISIBLE: u64 = 700;
    const OVERFLOW: &str = "18446744073709551616";
    let reason = format!("<#0> <@0> <@&0> <#{OVERFLOW}> <@{OVERFLOW}> <@&{OVERFLOW}> <#{VISIBLE}>");

    let path = std::env::temp_dir().join(format!(
        "userinfo_bad_ids_{:032x}.db",
        rand::random::<u128>()
    ));
    let app = Arc::new(
        BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap(),
    );
    app.activation.mark_lock_held(true);
    let enrollment = crate::db::enroll_guild(&app.db, GUILD as i64, 100)
        .await
        .unwrap()
        .unwrap();
    app.tenant.activate(enrollment.clone()).await.unwrap();
    ready_userinfo_case_index(&app, &enrollment).await;
    crate::cases::record(
        &app,
        Some(enrollment),
        CaseAction::Warning,
        CaseOutcome::Succeeded,
        TARGET,
        Some(ACTOR),
        Some(reason),
        CaseEvidence::Unavailable,
        crate::cases::application_source(),
    )
    .await;
    let cache = Arc::new(Cache::new());
    let mut guild = Guild::default();
    guild.id = GuildId::new(GUILD);
    guild.owner_id = UserId::new(99);
    guild.name = "fixture".into();
    let mut everyone = Role::default();
    everyone.id = RoleId::new(GUILD);
    everyone.guild_id = guild.id;
    everyone.permissions = Permissions::VIEW_CHANNEL;
    guild.roles.insert(everyone.id, everyone);
    let mut mod_role = Role::default();
    mod_role.id = RoleId::new(ROLE);
    mod_role.guild_id = guild.id;
    mod_role.name = "Mod".into();
    mod_role.permissions = Permissions::MANAGE_MESSAGES;
    guild.roles.insert(mod_role.id, mod_role);
    let mut visible = GuildChannel::default();
    visible.id = ChannelId::new(VISIBLE);
    visible.guild_id = guild.id;
    visible.name = "notes".into();
    guild.channels.insert(visible.id, visible);
    guild.members.insert(
        UserId::new(TARGET),
        target_member(GUILD, TARGET, Some("Nick")),
    );
    let mut event: GuildCreateEvent =
        serde_json::from_value(serde_json::to_value(guild).unwrap()).unwrap();
    cache.update(&mut event);
    let (http, captured, server) = capturing_proxy().await;
    let http = Arc::new(http);
    let ctx = test_context(Arc::clone(&cache), Arc::clone(&http)).await;
    for token in ["0", "<@0>", OVERFLOW, &format!("<@{OVERFLOW}>")] {
        assert_eq!(
            crate::commands::resolve_user_name(&ctx, GuildId::new(GUILD), token),
            crate::commands::UserNameMatch::NotFound,
            "{token}"
        );
    }
    let commands = jmodguard::defaults::default_commands()
        .into_iter()
        .map(|mut cmd| {
            if matches!(cmd.behavior, jmodguard::types::Behavior::UserInfo) {
                cmd.log_event.clear();
            }
            (cmd.name.clone(), cmd)
        })
        .collect();
    let cfg = Arc::new(CachedGuildConfig {
        version: 1,
        durable_version: 1,
        loaded_at: std::time::Instant::now(),
        settings: crate::types::GuildSettings {
            prefix: "!".into(),
            ..Default::default()
        },
        rules: Default::default(),
        commands,
        templates: Default::default(),
        log_channels: Default::default(),
        known_bad_hashes: Default::default(),
        known_spam: Default::default(),
        log_filters: Default::default(),
        regexes: Vec::new(),
        custom_patterns: Default::default(),
    });
    let mut author = User::default();
    author.id = UserId::new(ACTOR);
    author.name = "mod".into();
    let partial: serenity::model::guild::PartialMember =
        serde_json::from_value(serde_json::json!({
            "deaf": false,
            "joined_at": null,
            "mute": false,
            "nick": null,
            "roles": [ROLE.to_string()],
            "pending": false,
            "premium_since": null,
            "guild_id": GUILD.to_string(),
            "user": null,
            "permissions": null,
            "unusual_dm_activity_until": null,
            "avatar": null,
            "banner": null,
            "avatar_decoration_data": null,
        }))
        .unwrap();
    let mut message = serenity::model::channel::Message::default();
    message.id = serenity::model::id::MessageId::new(9);
    message.content = format!("!userinfo {TARGET}");
    message.guild_id = Some(GuildId::new(GUILD));
    message.channel_id = ChannelId::new(2);
    message.author = author.clone();
    message.member = Some(Box::new(partial));
    let mut slash_member = Member::default();
    slash_member.user = author.clone();
    slash_member.guild_id = GuildId::new(GUILD);
    slash_member.roles = vec![RoleId::new(ROLE)];
    let interaction: serenity::model::application::Interaction =
        serde_json::from_value(serde_json::json!({
            "id": "1",
            "application_id": "1",
            "type": 2,
            "token": "token",
            "version": 1,
            "guild_id": GUILD.to_string(),
            "channel_id": "2",
            "locale": "en-US",
            "app_permissions": "0",
            "entitlements": [],
            "authorizing_integration_owners": {},
            "attachment_size_limit": 10000000,
            "user": author,
            "member": slash_member,
            "data": {
                "id": "3",
                "name": "userinfo",
                "type": 1,
                "options": [{"name": "user", "type": 6, "value": TARGET.to_string()}]
            }
        }))
        .unwrap();

    crate::commands::handle_prefix(
        &app,
        http.as_ref(),
        &cache,
        Arc::clone(&cfg),
        &message,
        Some(&ctx),
    )
    .await
    .unwrap();
    let prefix = report_content("prefix", &captured);
    crate::commands::handle_interaction(&app, &ctx, interaction).await;
    let slash = report_content("slash", &captured);
    for content in [prefix, slash] {
        let cases = content
            .lines()
            .find(|line| line.contains('`') && line.contains("unavailable"))
            .unwrap_or_else(|| panic!("no case line in {content}"));
        assert!(cases.matches("unavailable").count() >= 6, "{cases}");
        assert!(cases.contains("700"), "{cases}");
        assert!(!cases.contains("#0"), "{cases}");
        assert!(!cases.contains("<@0"), "{cases}");
        assert!(!cases.contains("<@&0"), "{cases}");
        assert!(!cases.contains(OVERFLOW), "{cases}");
        assert!(content.contains("**User info**"), "{content}");
    }

    server.abort();
    drop(app);
    crate::tenant::remove_sidecars(&path);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn userinfo_prefix_and_slash_hide_private_threads_without_access() {
    use crate::state::CachedGuildConfig;
    use serenity::model::channel::{
        ChannelType, GuildChannel, PermissionOverwrite, PermissionOverwriteType,
    };
    use serenity::model::permissions::Permissions;
    const GUILD: u64 = 10;
    const ACTOR: u64 = 7;
    const TARGET: u64 = 91;
    const ROLE: u64 = 50;
    const VOICE: u64 = 700;
    const PARENT: u64 = 800;
    const PRIVATE: u64 = 802;
    const PUBLIC_ARCHIVED: u64 = 803;
    const PRIVATE_ARCHIVED: u64 = 804;
    let reason = format!("see <#{PARENT}> <#{PRIVATE}> <#{PUBLIC_ARCHIVED}> <#{PRIVATE_ARCHIVED}>");

    let path = std::env::temp_dir().join(format!(
        "userinfo_private_thread_{:032x}.db",
        rand::random::<u128>()
    ));
    let app = Arc::new(
        BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap(),
    );
    app.activation.mark_lock_held(true);
    let enrollment = crate::db::enroll_guild(&app.db, GUILD as i64, 100)
        .await
        .unwrap()
        .unwrap();
    app.tenant.activate(enrollment.clone()).await.unwrap();
    ready_userinfo_case_index(&app, &enrollment).await;
    crate::cases::record(
        &app,
        Some(enrollment),
        CaseAction::Warning,
        CaseOutcome::Succeeded,
        TARGET,
        Some(ACTOR),
        Some(reason),
        CaseEvidence::Unavailable,
        crate::cases::application_source(),
    )
    .await;
    let cache = Arc::new(Cache::new());
    let (http, captured, server) = capturing_proxy().await;
    let http = Arc::new(http);
    let ctx = test_context(Arc::clone(&cache), Arc::clone(&http)).await;
    let commands = jmodguard::defaults::default_commands()
        .into_iter()
        .map(|mut cmd| {
            if matches!(cmd.behavior, jmodguard::types::Behavior::UserInfo) {
                cmd.log_event.clear();
            }
            (cmd.name.clone(), cmd)
        })
        .collect();
    let cfg = Arc::new(CachedGuildConfig {
        version: 1,
        durable_version: 1,
        loaded_at: std::time::Instant::now(),
        settings: crate::types::GuildSettings {
            prefix: "!".into(),
            ..Default::default()
        },
        rules: Default::default(),
        commands,
        templates: Default::default(),
        log_channels: Default::default(),
        known_bad_hashes: Default::default(),
        known_spam: Default::default(),
        log_filters: Default::default(),
        regexes: Vec::new(),
        custom_patterns: Default::default(),
    });

    let thread = |id: u64, kind: ChannelType, name: &str, archived: bool| {
        let mut channel = GuildChannel::default();
        channel.id = ChannelId::new(id);
        channel.guild_id = GuildId::new(GUILD);
        channel.parent_id = Some(ChannelId::new(PARENT));
        channel.kind = kind;
        channel.name = name.into();
        channel.thread_metadata = Some(
            serde_json::from_value(serde_json::json!({
                "archived": archived,
                "auto_archive_duration": 60,
                "locked": false
            }))
            .unwrap(),
        );
        channel.member = Some(
            serde_json::from_value(serde_json::json!({
                "join_timestamp": "2024-01-01T00:00:00.000000+00:00",
                "flags": 0
            }))
            .unwrap(),
        );
        channel
    };
    let install = |manage_threads: bool, deny_manage_threads: bool| {
        let mut guild = Guild::default();
        guild.id = GuildId::new(GUILD);
        guild.owner_id = UserId::new(99);
        guild.name = "fixture".into();
        let mut everyone = Role::default();
        everyone.id = RoleId::new(GUILD);
        everyone.guild_id = guild.id;
        everyone.permissions = Permissions::empty();
        guild.roles.insert(everyone.id, everyone);
        let mut mod_role = Role::default();
        mod_role.id = RoleId::new(ROLE);
        mod_role.guild_id = guild.id;
        mod_role.name = "Mod".into();
        mod_role.permissions = Permissions::MANAGE_MESSAGES | Permissions::VIEW_CHANNEL;
        if manage_threads {
            mod_role.permissions |= Permissions::MANAGE_THREADS;
        }
        guild.roles.insert(mod_role.id, mod_role);
        let mut parent = GuildChannel::default();
        parent.id = ChannelId::new(PARENT);
        parent.guild_id = guild.id;
        parent.kind = ChannelType::Text;
        parent.name = "notes".into();
        if deny_manage_threads {
            parent.permission_overwrites = vec![PermissionOverwrite {
                kind: PermissionOverwriteType::Role(RoleId::new(ROLE)),
                allow: Permissions::empty(),
                deny: Permissions::MANAGE_THREADS,
            }];
        }
        guild.channels.insert(parent.id, parent);
        let mut voice = GuildChannel::default();
        voice.id = ChannelId::new(VOICE);
        voice.guild_id = guild.id;
        voice.kind = ChannelType::Voice;
        voice.name = "Lobby".into();
        guild.channels.insert(voice.id, voice);
        guild.threads.push(thread(
            PRIVATE,
            ChannelType::PrivateThread,
            "secret-thread",
            false,
        ));
        guild.threads.push(thread(
            PUBLIC_ARCHIVED,
            ChannelType::PublicThread,
            "open-archive",
            true,
        ));
        guild.threads.push(thread(
            PRIVATE_ARCHIVED,
            ChannelType::PrivateThread,
            "old-secret",
            true,
        ));
        guild.members.insert(
            UserId::new(TARGET),
            target_member(GUILD, TARGET, Some("Nick")),
        );
        let mut caller = target_member(GUILD, ACTOR, None);
        caller.user.name = "mod".into();
        caller.roles = vec![RoleId::new(ROLE)];
        guild.members.insert(caller.user.id, caller);
        guild.voice_states.insert(
            UserId::new(TARGET),
            serde_json::from_value(serde_json::json!({
                "channel_id": VOICE.to_string(),
                "user_id": TARGET.to_string(),
                "guild_id": GUILD.to_string(),
                "session_id": "sess",
                "deaf": false,
                "mute": false,
                "self_deaf": false,
                "self_mute": false,
                "self_video": false,
                "suppress": false
            }))
            .unwrap(),
        );
        let mut event: GuildCreateEvent =
            serde_json::from_value(serde_json::to_value(guild).unwrap()).unwrap();
        cache.update(&mut event);
    };

    let mut author = User::default();
    author.id = UserId::new(ACTOR);
    author.name = "mod".into();
    let partial: serenity::model::guild::PartialMember =
        serde_json::from_value(serde_json::json!({
            "deaf": false,
            "joined_at": null,
            "mute": false,
            "nick": null,
            "roles": [ROLE.to_string()],
            "pending": false,
            "premium_since": null,
            "guild_id": GUILD.to_string(),
            "user": null,
            "permissions": null,
            "unusual_dm_activity_until": null,
            "avatar": null,
            "banner": null,
            "avatar_decoration_data": null,
        }))
        .unwrap();
    let mut message = serenity::model::channel::Message::default();
    message.id = serenity::model::id::MessageId::new(9);
    message.content = format!("!userinfo {TARGET}");
    message.guild_id = Some(GuildId::new(GUILD));
    message.channel_id = ChannelId::new(2);
    message.author = author.clone();
    message.member = Some(Box::new(partial));
    let mut slash_member = Member::default();
    slash_member.user = author.clone();
    slash_member.guild_id = GuildId::new(GUILD);
    slash_member.roles = vec![RoleId::new(ROLE)];
    let interaction: serenity::model::application::Interaction =
        serde_json::from_value(serde_json::json!({
            "id": "1",
            "application_id": "1",
            "type": 2,
            "token": "token",
            "version": 1,
            "guild_id": GUILD.to_string(),
            "channel_id": "2",
            "locale": "en-US",
            "app_permissions": "0",
            "entitlements": [],
            "authorizing_integration_owners": {},
            "attachment_size_limit": 10000000,
            "user": author,
            "member": slash_member,
            "data": {
                "id": "3",
                "name": "userinfo",
                "type": 1,
                "options": [{"name": "user", "type": 6, "value": TARGET.to_string()}]
            }
        }))
        .unwrap();

    let run = |label: &str| {
        captured.lock().unwrap().clear();
        let prefix_label = format!("{label} prefix");
        let slash_label = format!("{label} slash");
        let app = Arc::clone(&app);
        let http = Arc::clone(&http);
        let cache = Arc::clone(&cache);
        let cfg = Arc::clone(&cfg);
        let message = message.clone();
        let ctx = ctx.clone();
        let interaction = interaction.clone();
        let captured = Arc::clone(&captured);
        async move {
            crate::commands::handle_prefix(&app, http.as_ref(), &cache, cfg, &message, Some(&ctx))
                .await
                .unwrap();
            let prefix = report_content(&prefix_label, &captured);
            crate::commands::handle_interaction(&app, &ctx, interaction).await;
            let slash = report_content(&slash_label, &captured);
            (prefix, slash)
        }
    };

    install(false, false);
    {
        let guild = cache.guild(GuildId::new(GUILD)).unwrap();
        let roles = [RoleId::new(ROLE)];
        assert_eq!(
            crate::commands::permission_level(0, &guild.roles, ACTOR, [ROLE]),
            crate::commands::PermLevel::Mod
        );
        assert!(
            guild
                .threads
                .iter()
                .any(|thread| thread.id == ChannelId::new(PRIVATE)
                    && thread.kind == ChannelType::PrivateThread
                    && thread.member.is_some()),
            "the private thread must be cached with the bot's membership"
        );
        assert!(crate::commands::member_can_view_channel(
            &guild,
            UserId::new(ACTOR),
            &roles,
            ChannelId::new(PRIVATE),
        ));
        assert!(!crate::commands::private_thread_visible_to(
            &guild,
            UserId::new(ACTOR),
            &roles,
            ChannelId::new(PRIVATE),
        ));
        assert!(crate::commands::private_thread_visible_to(
            &guild,
            UserId::new(ACTOR),
            &roles,
            ChannelId::new(PUBLIC_ARCHIVED),
        ));
    }
    let (prefix, slash) = run("hidden").await;
    for content in [&prefix, &slash] {
        assert!(content.contains("800"), "{content}");
        assert!(content.contains("803"), "{content}");
        assert!(!content.contains("802"), "{content}");
        assert!(!content.contains("804"), "{content}");
        assert!(!content.contains("secret-thread"), "{content}");
        assert!(!content.contains("old-secret"), "{content}");
        assert!(content.contains("unavailable"), "{content}");
        let voice = voice_line_from_content(content);
        assert!(voice.contains("Lobby"), "{voice}");
        assert!(voice.contains("700"), "{voice}");
    }

    install(true, false);
    let (prefix, slash) = run("manage threads").await;
    for content in [&prefix, &slash] {
        assert!(content.contains("802"), "{content}");
        assert!(content.contains("804"), "{content}");
        assert!(content.contains("800"), "{content}");
        assert!(content.contains("803"), "{content}");
    }

    install(true, true);
    let (prefix, slash) = run("manage threads denied").await;
    for content in [&prefix, &slash] {
        assert!(content.contains("800"), "{content}");
        assert!(content.contains("803"), "{content}");
        assert!(!content.contains("802"), "{content}");
        assert!(!content.contains("804"), "{content}");
        assert!(!content.contains("secret-thread"), "{content}");
        assert!(!content.contains("old-secret"), "{content}");
    }

    server.abort();
    drop(app);
    crate::tenant::remove_sidecars(&path);
    let _ = std::fs::remove_file(path);
}

fn voice_line_from_content(content: &str) -> String {
    content
        .lines()
        .find(|line| line.contains("**Voice:**"))
        .unwrap_or_else(|| panic!("no voice line in {content}"))
        .to_string()
}

fn report_content(label: &str, captured: &Arc<std::sync::Mutex<Vec<String>>>) -> String {
    let requests = captured.lock().unwrap().clone();
    captured.lock().unwrap().clear();
    let report = requests
        .iter()
        .find(|raw| raw.contains("**Voice:**"))
        .unwrap_or_else(|| panic!("{label} produced no userinfo report: {requests:?}"));
    let (_, body) = report
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("{label} report had no body: {report}"));
    let payload: serde_json::Value =
        serde_json::from_str(body).unwrap_or_else(|error| panic!("{error} in {body}"));
    let message = payload
        .get("data")
        .filter(|data| data.is_object())
        .unwrap_or(&payload);
    message["content"]
        .as_str()
        .unwrap_or_else(|| panic!("{label} had no content in {payload}"))
        .to_string()
}

fn voice_line(report: &str) -> String {
    let (_, body) = report
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("report had no body: {report}"));
    let payload: serde_json::Value =
        serde_json::from_str(body).unwrap_or_else(|error| panic!("{error} in {body}"));
    let message = payload
        .get("data")
        .filter(|data| data.is_object())
        .unwrap_or(&payload);
    message["content"]
        .as_str()
        .unwrap_or_else(|| panic!("no content in {payload}"))
        .lines()
        .find(|line| line.contains("**Voice:**"))
        .unwrap_or_else(|| panic!("no voice line in {payload}"))
        .to_string()
}

// Real behavior execution, SQLite enrollment and REST serialization; only the
// Discord service is a local HTTP fixture. Prefix and slash share log_outcome.
async fn assert_command_mod_action(name: &str, args: &str, targeted: bool) {
    let cmd = jmodguard::defaults::default_commands()
        .into_iter()
        .find(|cmd| cmd.name == name)
        .unwrap();
    assert_configured_command_mod_action(cmd, args, targeted, 1).await;
}

async fn assert_configured_command_mod_action(
    cmd: crate::types::CommandDef,
    args: &str,
    targeted: bool,
    invocations: usize,
) {
    assert_command_report(cmd, args, targeted, invocations, "mod_action", None).await;
}

async fn assert_command_report(
    cmd: crate::types::CommandDef,
    args: &str,
    targeted: bool,
    invocations: usize,
    event: &str,
    custom: Option<(&str, &str)>,
) {
    let expected = custom.map(|(_, expected)| [expected]);
    assert_command_reports(
        cmd,
        args,
        targeted,
        invocations,
        event,
        custom.map(|(template, _)| (template, expected.as_ref().unwrap().as_slice())),
    )
    .await;
}

async fn assert_command_reports(
    cmd: crate::types::CommandDef,
    args: &str,
    targeted: bool,
    invocations: usize,
    event: &str,
    custom: Option<(&str, &[&str])>,
) {
    let reports_per_invocation = custom.map_or(1, |(_, expected)| expected.len());
    let name = &cmd.name;
    let path = std::env::temp_dir().join(format!("command_log_{:032x}.db", rand::random::<u128>()));
    let app = BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap();
    app.activation.mark_lock_held(true);
    let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
        .await
        .unwrap()
        .unwrap();
    app.tenant.activate(enrollment).await.unwrap();
    crate::db::upsert_log_channel(&app.db, 10, event, 999)
        .await
        .unwrap();
    if let Some((template, _)) = custom {
        crate::db::upsert_template(&app.db, 10, event, template)
            .await
            .unwrap();
    }
    let cache = Arc::new(Cache::new());
    remember_guild(&cache, 10, Some(target_member(10, 91, None)), None);
    let (http, requests, server) = capturing_proxy_for_commands(true).await;
    let ctx = test_context(cache, Arc::new(http)).await;
    let mut actor = target_member(10, 7, Some("Moderator"));
    actor.user.name = "Moderator".into();
    for invocation in 1..=invocations {
        let env = crate::commands::CmdEnv::new(
            &app,
            &ctx,
            &cmd,
            GuildId::new(10),
            ChannelId::new(1),
            UserId::new(7),
            actor.clone(),
            Args::new(args),
            false,
            None,
        );
        let outcome = crate::commands::run_command(env).await.unwrap();
        assert_eq!(
            outcome.vars["user"], "Moderator",
            "reply user must remain the actor"
        );
        crate::commands::log_outcome(
            &app,
            ctx.http.as_ref(),
            &ctx.cache,
            &cmd,
            GuildId::new(10),
            ChannelId::new(1),
            UserId::new(7),
            outcome,
        )
        .await;
        let count = requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| {
                request.lines().next().unwrap().starts_with("POST ")
                    && request.contains("/channels/999/messages")
            })
            .count();
        assert_eq!(
            count,
            invocation * reports_per_invocation,
            "{name}: expected reports without extra outcome/automatic duplicates"
        );
    }
    let payloads: Vec<serde_json::Value> = requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| {
            request.lines().next().unwrap().starts_with("POST ")
                && request.contains("/channels/999/messages")
        })
        .map(|request| serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap())
        .collect();
    assert_eq!(
        payloads.len(),
        invocations * reports_per_invocation,
        "{name}: expected reports without extra outcome/automatic duplicates; payloads={payloads:?}"
    );
    for (index, payload) in payloads.iter().enumerate() {
        let embed = &payload["embeds"][0];
        if let Some((_, expected)) = custom {
            let description = embed["description"].as_str().unwrap();
            assert_eq!(
                description,
                expected[index % reports_per_invocation],
                "{name}"
            );
            assert!(!description.contains(['{', '}']));
            continue;
        }
        let fields = embed["fields"].as_array().unwrap();
        // Shipped labels carry their map emoji in Discord.
        let field = |name: &str| {
            let label = format!("{} {name}", jmodguard::emoji::label(name).unwrap());
            fields.iter().find(|field| field["name"] == label).unwrap()["value"]
                .as_str()
                .unwrap()
        };
        if event == "plan_executed" {
            assert_eq!(field("Command"), format!("`{name}`"));
        } else if targeted {
            assert_eq!(
                field("User"),
                "targetuser · <@91>",
                "{name}: target, not moderator"
            );
            assert!(field("User").contains("91"));
        } else {
            assert_eq!(field("User"), "Moderator · <@7>");
        }
        if event != "plan_executed" {
            assert_eq!(field("Moderator"), "Moderator (<@7>)");
        }
        for text in fields.iter().map(|field| field["value"].as_str().unwrap()) {
            assert!(
                !text.contains(['{', '}']),
                "{name}: unresolved placeholder: {text}"
            );
        }
    }
    server.abort();
    drop(ctx);
    drop(app);
    crate::tenant::remove_sidecars(&path);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

#[tokio::test]
async fn command_mod_action_warn_once() {
    assert_command_mod_action("warn", "91 test reason", true).await;
}
#[tokio::test]
async fn command_mod_action_kick_once() {
    assert_command_mod_action("kick", "91 test reason", true).await;
}
#[tokio::test]
async fn command_mod_action_ban_once() {
    assert_command_mod_action("ban", "91 test reason", true).await;
}
#[tokio::test]
async fn command_mod_action_timeout_once() {
    assert_command_mod_action("timeout", "91 5 test reason", true).await;
}
#[tokio::test]
async fn command_mod_action_timeout_remove_once() {
    assert_command_mod_action("timeout", "91 0 test reason", true).await;
}
#[tokio::test]
async fn command_mod_action_warns_target() {
    assert_command_mod_action("warns", "91", true).await;
}
#[tokio::test]
async fn command_mod_action_userinfo_target() {
    assert_command_mod_action("userinfo", "91", true).await;
}
#[tokio::test]
async fn command_mod_action_without_target() {
    assert_command_mod_action("banlist", "", false).await;
}

#[tokio::test]
async fn command_mod_action_softban_once() {
    assert_command_mod_action("softban", "91 test reason", true).await;
}
#[tokio::test]
async fn command_mod_action_unban_once() {
    assert_command_mod_action("unban", "91 test reason", true).await;
}
#[tokio::test]
async fn command_mod_action_warnclear_once() {
    assert_command_mod_action("warnclear", "91", true).await;
}
#[tokio::test]
async fn command_mod_action_existing_ban_keeps_outcome_row() {
    let cmd = jmodguard::defaults::default_commands()
        .into_iter()
        .find(|cmd| cmd.name == "ban")
        .unwrap();
    assert_configured_command_mod_action(cmd, "91 test reason", true, 2).await;
}
#[tokio::test]
async fn command_mod_action_targeted_plan_with_and_without_log_step() {
    use crate::types::{Behavior, PlanStep};
    for explicit_log in [true, false] {
        let mut cmd = jmodguard::defaults::default_commands()
            .into_iter()
            .find(|cmd| cmd.name == "warn")
            .unwrap();
        cmd.name = "custom-plan".into();
        let mut steps = vec![PlanStep::AddRole { role_id: 50 }];
        if explicit_log {
            steps.push(PlanStep::Log {
                event: "mod_action".into(),
                template: "custom {user}".into(),
            });
        }
        cmd.behavior = Behavior::Plan { steps };
        assert_configured_command_mod_action(cmd, "91", true, 1).await;
    }
}

#[tokio::test]
async fn command_log_context_reply_plan_shipped_command() {
    use crate::types::{Behavior, PlanStep};
    let mut cmd = jmodguard::defaults::default_commands()
        .into_iter()
        .find(|cmd| cmd.name == "warn")
        .unwrap();
    cmd.name = "review-plan".into();
    cmd.log_event = "plan_executed".into();
    cmd.behavior = Behavior::Plan {
        steps: vec![PlanStep::Reply {
            template: "hello {user}".into(),
        }],
    };
    assert_command_report(cmd, "", false, 1, "plan_executed", None).await;
}

async fn assert_custom_command_log_context(name: &str, args: &str) {
    let mut cmd = jmodguard::defaults::default_commands()
        .into_iter()
        .find(|cmd| cmd.name == name)
        .unwrap();
    // The command alias must not replace the behavior's action.
    cmd.name = format!("review-{name}");
    let template = "{command}|{channel}|{target}|{target_id}|{user}|{mention}|{user_id}|{actor}|{actor_id}|{action}|{reason}";
    let expected = format!(
        "review-{name}|<#1>|targetuser|91|targetuser|<@91>|91|Moderator (<@7>)|7|{name}|test reason"
    );
    assert_command_report(
        cmd,
        args,
        true,
        1,
        "mod_action",
        Some((template, &expected)),
    )
    .await;
}

#[tokio::test]
async fn command_log_context_warn_custom() {
    assert_custom_command_log_context("warn", "91 test reason").await;
}
#[tokio::test]
async fn command_log_context_ban_custom() {
    assert_custom_command_log_context("ban", "91 test reason").await;
}
#[tokio::test]
async fn command_log_context_timeout_custom() {
    assert_custom_command_log_context("timeout", "91 5 test reason").await;
}

#[tokio::test]
async fn command_log_context_other_moderation_custom() {
    for name in ["kick", "softban", "unban"] {
        assert_custom_command_log_context(name, "91 test reason").await;
    }
}

#[tokio::test]
async fn command_log_context_warnclear_response_and_count() {
    let mut cmd = jmodguard::defaults::default_commands()
        .into_iter()
        .find(|cmd| cmd.name == "warnclear")
        .unwrap();
    cmd.name = "review-clear".into();
    assert_command_report(
        cmd, "91", true, 1, "mod_action",
        Some(("{command}|{channel}|{target}|{target_id}|{user}|{mention}|{actor}|{action}|{reason}|{count}|{response}",
        "review-clear|<#1>|targetuser|91|targetuser|<@91>|Moderator (<@7>)|warnclear|0|0|Cleared 0 warnings for targetuser.")),
    ).await;
}

#[tokio::test]
async fn command_log_context_timeout_events_and_duration() {
    for (minutes, duration, event) in [
        (5, "5m", "timeout_add"),
        (0, "0s", "timeout_remove"),
        (5, "5m", "mod_action"),
        (0, "0s", "mod_action"),
    ] {
        let mut cmd = jmodguard::defaults::default_commands()
            .into_iter()
            .find(|cmd| cmd.name == "timeout")
            .unwrap();
        cmd.name = "review-timeout".into();
        cmd.log_event = event.into();
        let expected = format!(
            "review-timeout|<#1>|targetuser|91|targetuser|<@91>|91|Moderator (<@7>)|7|{minutes}|{duration}|test reason"
        );
        assert_command_report(
            cmd, &format!("91 {minutes} test reason"), true, 1, event,
            Some(("{command}|{channel}|{target}|{target_id}|{user}|{mention}|{user_id}|{actor}|{actor_id}|{minutes}|{duration}|{reason}", &expected)),
        ).await;
    }
}

#[tokio::test]
async fn command_log_context_purge_count_and_slowmode_duration() {
    for (name, args, template, expected) in [
        (
            "purge",
            "2",
            "{command}|{channel}|{actor}|{action}|{reason}|{count}",
            "review-purge|<#1>|Moderator (<@7>)|purge|0 deleted|0",
        ),
        (
            "slowmode",
            "75",
            "{command}|{channel}|{actor}|{action}|{reason}|{seconds}|{duration}",
            "review-slowmode|<#1>|Moderator (<@7>)|slowmode|1m 15s|75|1m 15s",
        ),
    ] {
        let mut cmd = jmodguard::defaults::default_commands()
            .into_iter()
            .find(|cmd| cmd.name == name)
            .unwrap();
        cmd.name = format!("review-{name}");
        assert_command_report(
            cmd,
            args,
            false,
            1,
            "mod_action",
            Some((template, expected)),
        )
        .await;
    }
}

#[tokio::test]
async fn command_log_context_plan_explicit_events_once() {
    use crate::types::{Behavior, PlanStep};
    for event in ["plan_executed", "mod_action"] {
        let mut cmd = jmodguard::defaults::default_commands()
            .into_iter()
            .find(|cmd| cmd.name == "warn")
            .unwrap();
        cmd.name = "review-plan".into();
        cmd.log_event = event.into();
        cmd.behavior = Behavior::Plan {
            steps: vec![
                PlanStep::AddRole { role_id: 50 },
                PlanStep::Reply {
                    template: "hello {user}".into(),
                },
                PlanStep::Log {
                    event: event.into(),
                    template: "step {command}: {target}".into(),
                },
                PlanStep::Log {
                    event: event.into(),
                    template: "duplicate".into(),
                },
            ],
        };
        assert_command_reports(cmd, "91", true, 2, event,
            Some(("{command}|{channel}|{target}|{target_id}|{user}|{mention}|{actor}|{content}|{response}", &[
                "review-plan|<#1>|targetuser|91|targetuser|<@91>|Moderator (<@7>)|step review-plan: targetuser|hello Moderator",
                "review-plan|<#1>|targetuser|91|targetuser|<@91>|Moderator (<@7>)|duplicate|hello Moderator",
            ])),
        ).await;
    }
}

#[tokio::test]
async fn command_log_context_plan_same_event_distinct_templates() {
    use crate::types::{Behavior, PlanStep};
    for log_event in ["", "plan_executed"] {
        let mut cmd = jmodguard::defaults::default_commands()
            .into_iter()
            .find(|cmd| cmd.name == "warn")
            .unwrap();
        cmd.name = "review-plan".into();
        cmd.log_event = log_event.into();
        cmd.behavior = Behavior::Plan {
            steps: vec![
                PlanStep::Log {
                    event: "plan_executed".into(),
                    template: "FIRST {command}: {user} in {channel}".into(),
                },
                PlanStep::Reply {
                    template: "hello {user}".into(),
                },
                PlanStep::Log {
                    event: "plan_executed".into(),
                    template: "SECOND {command}: {response}".into(),
                },
            ],
        };
        assert_command_reports(
            cmd,
            "",
            false,
            1,
            "plan_executed",
            Some((
                "{content}|{command}|{channel}|{user}|{mention}|{actor}",
                &[
                    "FIRST review-plan: Moderator in <#1>|review-plan|<#1>|Moderator|<@7>|Moderator (<@7>)",
                    "SECOND review-plan: hello Moderator|review-plan|<#1>|Moderator|<@7>|Moderator (<@7>)",
                ],
            )),
        )
        .await;
    }
}

#[tokio::test]
async fn command_log_context_targeted_plan_automatic() {
    use crate::types::{Behavior, PlanStep};
    let mut cmd = jmodguard::defaults::default_commands()
        .into_iter()
        .find(|cmd| cmd.name == "warn")
        .unwrap();
    cmd.name = "review-plan".into();
    cmd.log_event = "plan_executed".into();
    cmd.behavior = Behavior::Plan {
        steps: vec![
            PlanStep::AddRole { role_id: 50 },
            PlanStep::Reply {
                template: "hello {user}".into(),
            },
        ],
    };
    assert_command_report(cmd, "91", true, 1, "plan_executed",
        Some(("{command}|{channel}|{target}|{target_id}|{user}|{mention}|{actor}|{content}|{response}",
        "review-plan|<#1>|targetuser|91|targetuser|<@91>|Moderator (<@7>)|hello Moderator|hello Moderator")),
    ).await;
}

/// One guild, one caller and the payload-capturing Discord proxy for the
/// real prefix (`handle_prefix`) and slash (`handle_interaction`) dispatch.
struct StatsDispatch {
    app: Arc<BotState>,
    path: std::path::PathBuf,
    cache: Arc<Cache>,
    http: Arc<serenity::http::Http>,
    captured: Arc<std::sync::Mutex<Vec<String>>>,
    server: tokio::task::JoinHandle<()>,
    ctx: serenity::client::Context,
    cfg: Arc<crate::state::CachedGuildConfig>,
}

impl StatsDispatch {
    const GUILD: u64 = 10;
    const ACTOR: u64 = 7;
    const ROLE: u64 = 50;

    /// `custom` adds guild commands (prefix config and stored rows for slash).
    async fn new(reject_embeds: bool, custom: Vec<crate::types::CommandDef>) -> Self {
        let path = std::env::temp_dir().join(format!(
            "stats_embed_dispatch_{:032x}.db",
            rand::random::<u128>()
        ));
        let app =
            BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap();
        app.activation.mark_lock_held(true);
        let mut commands = jmodguard::defaults::default_commands()
            .into_iter()
            .map(|cmd| (cmd.name.clone(), cmd))
            .collect::<std::collections::HashMap<_, _>>();
        for custom in custom {
            crate::db::upsert_command(&app.db, Self::GUILD as i64, &custom.name, &custom)
                .await
                .unwrap();
            commands.insert(custom.name.clone(), custom);
        }
        let cache = Arc::new(Cache::new());
        let (http, captured, server) = capturing_proxy_with(true, reject_embeds).await;
        let http = Arc::new(http);
        let ctx = test_context(Arc::clone(&cache), Arc::clone(&http)).await;
        let cfg = Arc::new(crate::state::CachedGuildConfig {
            version: 1,
            durable_version: 1,
            loaded_at: std::time::Instant::now(),
            settings: crate::types::GuildSettings {
                prefix: "!".into(),
                ..Default::default()
            },
            rules: Default::default(),
            commands,
            templates: Default::default(),
            log_channels: Default::default(),
            known_bad_hashes: Default::default(),
            known_spam: Default::default(),
            log_filters: Default::default(),
            regexes: Vec::new(),
            custom_patterns: Default::default(),
        });
        Self {
            app,
            path,
            cache,
            http,
            captured,
            server,
            ctx,
            cfg,
        }
    }

    fn install(&self, permissions: serenity::model::permissions::Permissions) {
        let mut guild = Guild::default();
        guild.id = GuildId::new(Self::GUILD);
        guild.owner_id = UserId::new(99);
        guild.name = "fixture".into();
        let mut everyone = Role::default();
        everyone.id = RoleId::new(Self::GUILD);
        everyone.guild_id = guild.id;
        everyone.permissions = serenity::model::permissions::Permissions::empty();
        guild.roles.insert(everyone.id, everyone);
        let mut role = Role::default();
        role.id = RoleId::new(Self::ROLE);
        role.guild_id = guild.id;
        role.name = "Staff".into();
        role.permissions = permissions;
        guild.roles.insert(role.id, role);
        let mut caller = target_member(Self::GUILD, Self::ACTOR, None);
        caller.user.name = "mod".into();
        caller.roles = vec![RoleId::new(Self::ROLE)];
        guild.members.insert(caller.user.id, caller);
        let mut event: GuildCreateEvent =
            serde_json::from_value(serde_json::to_value(guild).unwrap()).unwrap();
        self.cache.update(&mut event);
    }

    fn author() -> User {
        let mut author = User::default();
        author.id = UserId::new(Self::ACTOR);
        author.name = "mod".into();
        author
    }

    async fn prefix(&self, content: &str) {
        let partial: serenity::model::guild::PartialMember =
            serde_json::from_value(serde_json::json!({
                "deaf": false,
                "joined_at": null,
                "mute": false,
                "nick": null,
                "roles": [Self::ROLE.to_string()],
                "pending": false,
                "premium_since": null,
                "guild_id": Self::GUILD.to_string(),
                "user": null,
                "permissions": null,
                "unusual_dm_activity_until": null,
                "avatar": null,
                "banner": null,
                "avatar_decoration_data": null,
            }))
            .unwrap();
        let mut message = serenity::model::channel::Message::default();
        message.id = serenity::model::id::MessageId::new(9);
        message.content = content.into();
        message.guild_id = Some(GuildId::new(Self::GUILD));
        message.channel_id = ChannelId::new(2);
        message.author = Self::author();
        message.member = Some(Box::new(partial));
        crate::commands::handle_prefix(
            &self.app,
            self.http.as_ref(),
            &self.cache,
            Arc::clone(&self.cfg),
            &message,
            Some(&self.ctx),
        )
        .await
        .unwrap();
    }

    async fn slash(&self, name: &str) {
        let mut member = Member::default();
        member.user = Self::author();
        member.guild_id = GuildId::new(Self::GUILD);
        member.roles = vec![RoleId::new(Self::ROLE)];
        let interaction: serenity::model::application::Interaction =
            serde_json::from_value(serde_json::json!({
                "id": "1",
                "application_id": "1",
                "type": 2,
                "token": "token",
                "version": 1,
                "guild_id": Self::GUILD.to_string(),
                "channel_id": "2",
                "locale": "en-US",
                "app_permissions": "0",
                "entitlements": [],
                "authorizing_integration_owners": {},
                "attachment_size_limit": 10000000,
                "user": Self::author(),
                "member": member,
                "data": {"id": "3", "name": name, "type": 1, "options": []}
            }))
            .unwrap();
        crate::commands::handle_interaction(&self.app, &self.ctx, interaction).await;
    }

    /// Reply bodies sent to Discord (non-empty content or embeds), in order,
    /// with the HTTP route; the slash defer callback is left out.
    fn replies(&self) -> Vec<(String, serde_json::Value)> {
        std::mem::take(&mut *self.captured.lock().unwrap())
            .iter()
            .map(|raw| {
                let route = raw.lines().next().unwrap_or_default().to_string();
                (route, message_fields(raw))
            })
            .filter(|(_, payload)| {
                payload["content"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty())
                    || payload["embeds"].as_array().is_some_and(|e| !e.is_empty())
            })
            .collect()
    }

    fn sent(&self, label: &str) -> serde_json::Value {
        let replies = self.replies();
        assert_eq!(replies.len(), 1, "{label}: {replies:?}");
        replies.into_iter().next().unwrap().1
    }

    fn finish(self) {
        self.server.abort();
        drop(self.app);
        crate::tenant::remove_sidecars(&self.path);
        let _ = std::fs::remove_file(&self.path);
    }
}

/// `!status` (prefix alias) and `/stats` share one dispatch: the permission
/// denial is unchanged text, and an allowed caller gets one log-style embed.
#[tokio::test]
async fn stats_prefix_and_slash_send_the_status_embed_after_the_same_permission_check() {
    use serenity::model::permissions::Permissions;
    const DENIED: &str = "you don't have permission to use that command";
    let fx = StatsDispatch::new(false, Vec::new()).await;

    fx.install(Permissions::MANAGE_MESSAGES | Permissions::VIEW_CHANNEL);
    fx.prefix("!status").await;
    let denied = fx.sent("denied prefix");
    // Unchanged `{display name} — {reason}` text (the partial fixture member
    // carries no user name).
    let content = denied["content"].as_str().unwrap();
    assert!(content.ends_with(&format!(" — {DENIED}")), "{content}");
    assert!(
        denied["embeds"].as_array().is_none_or(Vec::is_empty),
        "{denied}"
    );
    fx.slash("stats").await;
    let denied = fx.sent("denied slash");
    assert_eq!(denied["content"], DENIED);
    assert!(
        denied["embeds"].as_array().is_none_or(Vec::is_empty),
        "{denied}"
    );

    fx.install(Permissions::MANAGE_GUILD | Permissions::VIEW_CHANNEL);
    fx.prefix("!status").await;
    let prefix = fx.sent("allowed prefix");
    fx.slash("stats").await;
    let slash = fx.sent("allowed slash");
    for payload in [&prefix, &slash] {
        assert!(
            payload["content"].as_str().is_none_or(str::is_empty),
            "{payload}"
        );
        assert_eq!(payload["allowed_mentions"]["parse"], serde_json::json!([]));
        let embeds = payload["embeds"].as_array().unwrap();
        assert_eq!(embeds.len(), 1, "{payload}");
        let embed = &embeds[0];
        assert_eq!(embed["title"], "Status");
        assert_eq!(embed["description"], "**Server stats**");
        // A fresh database has no release observations: attention accent.
        assert_eq!(embed["color"], jmodguard::log_embeds::YELLOW);
        let fields = embed["fields"].as_array().unwrap();
        let layout = fields
            .iter()
            .map(|f| (f["name"].as_str().unwrap(), f["inline"].as_bool().unwrap()))
            .collect::<Vec<_>>();
        assert_eq!(
            layout,
            [
                ("Uptime", true),
                ("Cases", true),
                ("Warns", true),
                ("Bans", true),
                ("Release status", false),
                ("Container use", false),
            ]
        );
        for field in &fields[1..4] {
            assert_eq!(
                field["value"],
                if field["name"] == "Cases" {
                    "`unavailable`"
                } else {
                    "`0`"
                }
            );
        }
        let value = |name: &str| {
            fields.iter().find(|f| f["name"] == name).unwrap()["value"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert!(value("Uptime").starts_with('`') && value("Uptime").ends_with('`'));
        let releases = value("Release status");
        for component in ["bot", "api", "webui"] {
            assert!(
                releases
                    .lines()
                    .any(|line| line.starts_with(&format!("**{component}:** "))),
                "{releases}"
            );
        }
        let containers = value("Container use");
        for label in [
            "Bot CPU",
            "Bot memory (real program use)",
            "API CPU",
            "API memory (real program use)",
        ] {
            assert!(
                containers.contains(&format!("**{label}:** `")),
                "{containers}"
            );
        }
        assert!(!containers.contains("pid"), "{containers}");
    }
    let without_uptime = |payload: &serde_json::Value| {
        let mut embed = payload["embeds"][0].clone();
        embed["fields"][0]["value"] = serde_json::Value::Null;
        embed
    };
    assert_eq!(without_uptime(&prefix), without_uptime(&slash));

    // A non-deferred slash answer and an oversized embed use the same bounds.
    let mut stats = jmodguard::defaults::default_commands()
        .into_iter()
        .find(|cmd| matches!(cmd.behavior, jmodguard::types::Behavior::Stats))
        .unwrap();
    stats.log_event.clear();
    let mut embed = crate::templates::stats_embed(0, Some(0), 0, 0, None, 0);
    embed.fields[4].value = format!("`{}`", "🦀".repeat(900));
    embed.fields.push(jmodguard::log_embeds::Field {
        name: "Container use".into(),
        value: "名".repeat(2000),
        inline: false,
    });
    let outcome = super::super::BehaviorOutcome {
        response: None,
        vars: std::collections::BTreeMap::new(),
        emitted_log_events: Vec::new(),
        suppress_response: false,
        embed: Some(embed),
    };
    let slash_ctx = super::super::SlashCtx {
        id: serenity::model::id::InteractionId::new(1),
        token: "token".into(),
        channel_id: ChannelId::new(2),
        guild_id: GuildId::new(StatsDispatch::GUILD),
        actor: UserId::new(StatsDispatch::ACTOR),
        member: None,
        name: "stats".into(),
        options: Vec::new(),
    };
    super::super::deliver_slash_success(
        &fx.app.activation,
        fx.http.as_ref(),
        &slash_ctx,
        super::super::SlashResponseState::Initial,
        &stats,
        &outcome,
    )
    .await;
    let initial = fx.sent("initial slash");
    super::super::send_outcome(
        &fx.app,
        fx.http.as_ref(),
        &fx.cache,
        &stats,
        GuildId::new(StatsDispatch::GUILD),
        ChannelId::new(2),
        UserId::new(StatsDispatch::ACTOR),
        outcome,
    )
    .await;
    let prefix = fx.sent("oversized prefix");
    let ephemeral = serenity::model::channel::MessageFlags::EPHEMERAL.bits();
    assert_eq!(
        initial["flags"].as_u64().unwrap_or(0) & ephemeral,
        0,
        "public answer"
    );
    assert_eq!(initial["embeds"], prefix["embeds"]);
    let embed = &prefix["embeds"][0];
    let units = |value: &serde_json::Value| value.as_str().unwrap().encode_utf16().count();
    let mut total = units(&embed["title"]) + units(&embed["description"]);
    for field in embed["fields"].as_array().unwrap() {
        assert!(units(&field["name"]) <= 256);
        assert!(units(&field["value"]) <= 1024, "{field}");
        total += units(&field["name"]) + units(&field["value"]);
    }
    assert!(total <= 6000);
    let releases = embed["fields"][4]["value"].as_str().unwrap();
    assert!(releases.ends_with('…'), "{releases}");
    assert!(
        releases
            .trim_start_matches('`')
            .trim_end_matches('…')
            .chars()
            .all(|c| c == '🦀'),
        "a surrogate pair was split: {releases}"
    );
    let containers = embed["fields"][5]["value"].as_str().unwrap();
    assert!(containers.trim_end_matches('…').chars().all(|c| c == '名'));

    fx.finish();
}

/// A custom command with the Stats behavior.
fn custom_stats_command(name: &str, response: &str) -> crate::types::CommandDef {
    let mut command = jmodguard::defaults::default_commands()
        .into_iter()
        .find(|cmd| matches!(cmd.behavior, jmodguard::types::Behavior::Stats))
        .unwrap();
    command.name = name.into();
    command.aliases.clear();
    command.usage = format!("!{name}");
    command.response = response.into();
    command
}

fn assert_plain_stats(text: &str) {
    for part in [
        "**📊 Stats (this server)**\n\n**⏱️ Uptime:** `",
        "\n**📋 Cases:** `unavailable`\n**⚠️ Warns:** `0`\n**🔨 Bans:** `0`",
        "\n\n**Release status**\n**bot:** ",
        "\n**api:** ",
        "\n**webui:** ",
        "\n\n**Container use**\n**Bot CPU:** `",
        "\n**Bot memory (real program use):** `",
        "\n**API CPU:** `unavailable`",
        "\n**API memory (real program use):** `unavailable`",
    ] {
        assert!(text.contains(part), "{part:?} missing from {text}");
    }
    assert!(!text.contains("pid"), "{text}");
}

fn has_embed(payload: &serde_json::Value) -> bool {
    payload["embeds"].as_array().is_some_and(|e| !e.is_empty())
}

/// A customized Stats response using `{response}` gets the plain-text stats,
/// as before the embed, and no embed; other custom text sits above the embed.
#[tokio::test]
async fn custom_stats_response_gets_plain_text_stats_through_prefix_and_slash() {
    use serenity::model::permissions::Permissions;
    let fx = StatsDispatch::new(
        false,
        vec![
            custom_stats_command("report", "Server report:\n{response}"),
            custom_stats_command("numbers", "Latest numbers:"),
        ],
    )
    .await;
    fx.install(Permissions::MANAGE_GUILD | Permissions::VIEW_CHANNEL);
    fx.prefix("!report").await;
    let prefix = fx.sent("prefix {response}");
    fx.slash("report").await;
    let slash = fx.sent("slash {response}");
    for payload in [&prefix, &slash] {
        let content = payload["content"].as_str().unwrap();
        assert!(
            content.starts_with("Server report:\n**📊 Stats (this server)**"),
            "{content}"
        );
        assert_plain_stats(content);
        assert!(!has_embed(payload), "stats shown twice: {payload}");
    }

    fx.prefix("!numbers").await;
    let prefix = fx.sent("prefix custom text");
    fx.slash("numbers").await;
    let slash = fx.sent("slash custom text");
    for payload in [&prefix, &slash] {
        assert_eq!(payload["content"], "Latest numbers:");
        assert_eq!(payload["embeds"][0]["title"], "Status");
        assert_eq!(payload["allowed_mentions"]["parse"], serde_json::json!([]));
    }
    fx.finish();
}

/// Without Embed Links Discord refuses the embed with 403/50013; the same
/// reply is then sent once as plain text, and the channel is not parked.
#[tokio::test]
async fn refused_stats_embed_falls_back_to_plain_text_for_prefix_and_slash() {
    use serenity::model::permissions::Permissions;
    let fx = StatsDispatch::new(true, Vec::new()).await;
    fx.install(Permissions::MANAGE_GUILD | Permissions::VIEW_CHANNEL);
    let assert_fallback = |label: &str, method: &str| {
        let replies = fx.replies();
        assert_eq!(replies.len(), 2, "{label}: {replies:?}");
        let (embed_route, embed) = &replies[0];
        let (text_route, text) = &replies[1];
        assert!(has_embed(embed), "{label}: {embed}");
        assert!(embed_route.starts_with(method), "{label}: {embed_route}");
        assert_eq!(embed_route, text_route, "{label}");
        assert!(!has_embed(text), "{label}: {text}");
        let content = text["content"].as_str().unwrap();
        assert!(
            content.starts_with("**📊 Stats (this server)**"),
            "{content}"
        );
        assert_plain_stats(content);
        text.clone()
    };
    // Twice: a fallback that was delivered must not silence the next reply.
    for round in ["first", "second"] {
        fx.prefix("!status").await;
        assert_fallback(&format!("{round} prefix"), "POST");
    }
    fx.slash("stats").await;
    assert_fallback("deferred slash", "PATCH");

    let stats = custom_stats_command("stats", "");
    let (embed, text) = {
        let embed = crate::templates::stats_embed(5, None, 0, 0, Some(&[]), 0);
        let text = crate::templates::stats_text_with_container_use(
            crate::templates::stats_response_with_update_availability(5, None, 0, 0, Some(&[]), 0),
            &jmodguard::cgroup::ContainerSample::unavailable(),
            None,
        );
        (
            crate::templates::stats_with_container_use(
                embed,
                &jmodguard::cgroup::ContainerSample::unavailable(),
                None,
            ),
            text,
        )
    };
    let outcome = super::super::BehaviorOutcome {
        response: Some(text),
        vars: std::collections::BTreeMap::new(),
        emitted_log_events: Vec::new(),
        suppress_response: false,
        embed: Some(embed),
    };
    let slash = super::super::SlashCtx {
        id: serenity::model::id::InteractionId::new(1),
        token: "token".into(),
        channel_id: ChannelId::new(2),
        guild_id: GuildId::new(StatsDispatch::GUILD),
        actor: UserId::new(StatsDispatch::ACTOR),
        member: None,
        name: "stats".into(),
        options: Vec::new(),
    };
    super::super::deliver_slash_success(
        &fx.app.activation,
        fx.http.as_ref(),
        &slash,
        super::super::SlashResponseState::Initial,
        &stats,
        &outcome,
    )
    .await;
    let text = assert_fallback("initial slash", "POST");
    let ephemeral = serenity::model::channel::MessageFlags::EPHEMERAL.bits();
    assert_eq!(text["flags"].as_u64().unwrap_or(0) & ephemeral, 0, "public");
    fx.finish();
}
