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
        suppress_response: false,
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
        true,
        &userinfo,
        &outcome,
    )
    .await;
    super::super::deliver_slash_success(
        &app.activation,
        http.as_ref(),
        &slash,
        false,
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
            suppress_response: false,
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let slot = Arc::clone(&captured);
    let server = tokio::spawn(async move {
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
            slot.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&request).into_owned());
            let body = "{}";
            let response = format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
