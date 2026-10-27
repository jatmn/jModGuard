use super::*;
use crate::commands::Args;
use crate::db::guild_data::{
    CaseAction, CaseEvidence, CaseOutcome, MemberHistoryEntry, MemberHistoryPage,
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
    let snapshot = capture_member_snapshot(GuildId::new(10), UserId::new(91), &cache);
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
    let lines = case_lines_from_entries(&entries);
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

#[test]
fn rest_lookup_failure_still_renders_warns_and_cases() {
    let mut snapshot = MemberSnapshot {
        user_id: 91,
        username: None,
        global_name: None,
        nick: None,
        joined_at: None,
        roles: vec![],
        roles_cached: false,
        voice: VoiceSnapshot::NotCached,
        member_cached: false,
        guild_cached: false,
    };
    apply_missing_user_identity(&mut snapshot);
    let lines = vec![format_case_line(
        "warn",
        Some("succeeded"),
        Some(1_600_000_000_000),
        None,
    )];
    let body = render_userinfo_message(
        jmodguard::defaults::USER_INFO_TEMPLATE,
        &snapshot,
        &WarnsField::Count(3),
        lines,
    );
    assert!(body.contains(MISSING));
    assert!(body.contains("**Warnings:** 3"));
    assert!(body.contains("`warn`"));
    assert!(!body.contains("Discord API error"));
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
    let snapshot = capture_member_snapshot(GuildId::new(10), UserId::new(91), &cache);
    assert!(!snapshot.guild_cached);
    assert_eq!(format_voice(&snapshot.voice), VOICE_NOT_CACHED);
}

#[test]
fn cached_member_does_not_need_rest_lookup() {
    let cache = Cache::new();
    remember_guild(&cache, 10, Some(target_member(10, 91, None)), None);
    let snapshot = capture_member_snapshot(GuildId::new(10), UserId::new(91), &cache);
    assert!(snapshot_has_cached_user(&snapshot));
}

#[test]
fn uncached_user_requires_rest_lookup() {
    let cache = Cache::new();
    let snapshot = capture_member_snapshot(GuildId::new(10), UserId::new(91), &cache);
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

#[test]
fn omitted_user_argument_defaults_to_actor() {
    let mut args = Args::new("");
    assert!(args.next_arg().is_none());
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
    let ctx = test_context(cache).await;
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
        source_message: None,
    };
    let mut vars = crate::commands::behaviors::behaviors_base_vars(&env);
    let outcome = super::run(&env, Some(enrollment), &mut vars).await.unwrap();
    let body = outcome.response.unwrap();
    assert!(body.contains("**Warnings:** 1"));
    assert!(body.contains("`ban`"));
    assert!(body.contains("`91`"));
    drop(app);
    crate::tenant::remove_sidecars(&path);
    let _ = std::fs::remove_file(path);
}

async fn test_context(cache: Arc<Cache>) -> serenity::client::Context {
    use serenity::client::Context;
    use serenity::gateway::{
        Shard, ShardManager, ShardManagerOptions, ShardMessenger, ShardRunner, ShardRunnerOptions,
    };
    use serenity::model::gateway::{GatewayIntents, ShardInfo};
    use serenity::model::id::ShardId;
    let http = Arc::new(serenity::http::Http::new("fixture"));
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
