use super::*;
use crate::cases::record_application;
use crate::config::BotProcessConfig;
use crate::db;
use crate::db::Db;
use crate::db::guild_data::{
    CaseAction, CaseEvidence, CaseOutcome, MemberHistoryEntry, MemberHistoryPage,
};
use crate::logging::{Delivery, LogDelivery, emit_to};
use crate::state::BotState;
use jmodguard::db::SqlExecutor;
use serenity::async_trait;
use serenity::builder::CreateMessage;
use serenity::model::id::{ChannelId, GuildId, UserId};
use serenity::model::user::User;
use std::sync::Arc;
use std::sync::Mutex;

fn user(id: u64) -> User {
    let mut user = User::default();
    user.id = UserId::new(id);
    user.name = "member".into();
    user
}

#[test]
fn no_cases_and_not_ever_banned_skips_note() {
    let page = MemberHistoryPage {
        entries: vec![MemberHistoryEntry {
            kind: "join".into(),
            occurred_at_ms: Some(1),
            observed_at_ms: None,
            stay_id: Some("stay".into()),
            observation_id: None,
            case_id: None,
            joined_at_ms: Some(1),
            departed_at_ms: None,
            moderator_id: None,
            reason: None,
            outcome: None,
            source_kind: None,
        }],
        next_cursor: None,
    };
    let (cases, truncated) = collect_prior_cases_from_pages(&[page]);
    assert_eq!(build_rejoin_note(cases, false, truncated), None);
}

#[test]
fn history_error_skips_note_without_not_ready() {
    assert_eq!(build_rejoin_note(vec![], false, false), None);
}

#[test]
fn ever_banned_without_index_uses_not_ready_wording() {
    let page = MemberHistoryPage {
        entries: vec![],
        next_cursor: None,
    };
    let (cases, truncated) = collect_prior_cases_from_pages(&[page]);
    let note = build_rejoin_note(cases, true, truncated).unwrap();
    assert_eq!(note, RejoinNote::IndexNotReady);
    let vars = member_rejoin_vars(&user(42), &note);
    assert!(vars["notice"].contains("not ready"));
    assert!(vars["cases"].is_empty());
}

#[test]
fn three_cases_newest_first_with_tag_stripped() {
    let entries = [
        (
            "ban",
            "succeeded",
            3_000,
            Some("[jmg:0123456789abcdef0123] spam"),
        ),
        ("kick", "failed", 2_000, None),
        ("timeout", "succeeded", 1_000, Some("rule break")),
    ];
    let page = MemberHistoryPage {
        entries: entries
            .iter()
            .map(|(kind, outcome, at, reason)| MemberHistoryEntry {
                kind: (*kind).into(),
                occurred_at_ms: Some(*at),
                observed_at_ms: None,
                stay_id: None,
                observation_id: None,
                case_id: Some(format!("case-{at}")),
                joined_at_ms: None,
                departed_at_ms: None,
                moderator_id: None,
                reason: reason.map(str::to_string),
                outcome: Some((*outcome).into()),
                source_kind: Some("application".into()),
            })
            .collect(),
        next_cursor: None,
    };
    let (cases, truncated) = collect_prior_cases_from_pages(&[page]);
    let note = build_rejoin_note(cases, false, truncated).unwrap();
    let RejoinNote::Cases { lines, truncated } = note else {
        panic!("expected cases");
    };
    assert!(!truncated);
    assert_eq!(lines.len(), 3);
    assert!(lines[0].contains("`ban`"));
    assert!(lines[0].contains("succeeded"));
    assert!(lines[0].contains("spam"));
    assert!(!lines[0].contains("[jmg:"));
    assert!(lines[1].contains("`kick`"));
    assert!(lines[2].contains("`timeout`"));
}

#[test]
fn truncation_notice_when_more_than_eight_cases_exist() {
    let entries = (0..9)
        .map(|index| MemberHistoryEntry {
            kind: "ban".into(),
            occurred_at_ms: Some(10_000 - index as i64),
            observed_at_ms: None,
            stay_id: None,
            observation_id: None,
            case_id: Some(format!("case-{index}")),
            joined_at_ms: None,
            departed_at_ms: None,
            moderator_id: None,
            reason: None,
            outcome: Some("succeeded".into()),
            source_kind: None,
        })
        .collect();
    let page = MemberHistoryPage {
        entries,
        next_cursor: None,
    };
    let (cases, truncated) = collect_prior_cases_from_pages(&[page]);
    assert!(truncated);
    assert_eq!(cases.len(), PRIOR_CASE_LIMIT as usize);
    let note = build_rejoin_note(cases, false, truncated).unwrap();
    let vars = member_rejoin_vars(&user(42), &note);
    assert!(vars["notice"].contains("omitted"));
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

#[test]
fn paginate_past_stay_rows_on_first_page() {
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
        entries: vec![case_row(1_000, "case-two"), case_row(500, "case-three")],
        next_cursor: None,
    };
    let (cases, truncated) = collect_prior_cases_from_pages(&[page1, page2]);
    assert_eq!(cases.len(), 3);
    assert!(!truncated);
    let note = build_rejoin_note(cases, false, truncated).unwrap();
    let vars = member_rejoin_vars(&user(42), &note);
    assert!(vars["notice"].is_empty());
}

#[test]
fn timeline_cursor_after_stays_does_not_imply_case_truncation() {
    let page1 = MemberHistoryPage {
        entries: vec![
            stay_row(3_000, "stay-a"),
            stay_row(2_000, "stay-b"),
            case_row(1_000, "only-case"),
        ],
        next_cursor: Some("1000:ban:only-case".into()),
    };
    let page2 = MemberHistoryPage {
        entries: vec![stay_row(500, "stay-c")],
        next_cursor: None,
    };
    let (cases, truncated) = collect_prior_cases_from_pages(&[page1, page2]);
    assert_eq!(cases.len(), 1);
    assert!(!truncated);
}

#[test]
fn template_with_eight_long_reasons_stays_within_discord_limit() {
    let long = format!("[jmg:0123456789abcdef0123] {}", "x".repeat(480));
    let entries = (0..8)
        .map(|index| MemberHistoryEntry {
            kind: "ban".into(),
            occurred_at_ms: Some(10_000 - index as i64),
            observed_at_ms: None,
            stay_id: None,
            observation_id: None,
            case_id: Some(format!("case-{index}")),
            joined_at_ms: None,
            departed_at_ms: None,
            moderator_id: None,
            reason: Some(long.clone()),
            outcome: Some("succeeded".into()),
            source_kind: None,
        })
        .collect();
    let (cases, truncated) = collect_prior_cases_from_pages(&[MemberHistoryPage {
        entries,
        next_cursor: None,
    }]);
    let note = build_rejoin_note(cases, false, truncated).unwrap();
    let vars = member_rejoin_vars(&user(42), &note);
    let template = crate::defaults::activity_log_templates()
        .into_iter()
        .find(|(event, _)| *event == "member_rejoin")
        .unwrap()
        .1;
    let rendered = templates::render_bounded(template, &vars, 2000);
    assert!(rendered.encode_utf16().count() <= 2000);
}

struct Fixture {
    app: Arc<BotState>,
    path: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("member_rejoin_{:032x}.db", rand::random::<u128>()));
        let mut cfg = BotProcessConfig::default();
        cfg.log.discord_concurrency = 1;
        cfg.log.discord_timeout_secs = 1;
        Self {
            app: BotState::new(cfg, Db::open(&path, 2).unwrap()).unwrap(),
            path,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
        }
        crate::tenant::remove_sidecars(&self.path);
    }
}

#[derive(Default)]
struct Sink {
    messages: Mutex<Vec<(ChannelId, String)>>,
}

#[async_trait]
impl LogDelivery for Sink {
    async fn send(&self, channel: ChannelId, message: CreateMessage) -> anyhow::Result<()> {
        self.messages.lock().unwrap().push((
            channel,
            serde_json::to_value(message)?["content"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        ));
        Ok(())
    }
}

async fn ready_directory(
    app: &BotState,
    enrollment: &db::GuildEnrollment,
) -> (
    db::guild_data::GuildDataStore,
    db::guild_data::GuildDataScope,
) {
    let (store, scope) = app.tenant.live(enrollment.clone()).await.unwrap();
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    store
        .backfill_member_case_index(scope.clone(), 42, 100)
        .await
        .unwrap();
    (store, scope)
}

#[tokio::test]
async fn ninth_case_is_truncated_not_a_second_post() {
    const GUILD: i64 = 10;
    const SUBJECT: u64 = 77;
    let f = Fixture::new();
    f.app.activation.mark_lock_held(true);
    let enrollment = db::enroll_guild(&f.app.db, GUILD, 100)
        .await
        .unwrap()
        .unwrap();
    f.app.tenant.activate(enrollment.clone()).await.unwrap();
    let (store, scope) = ready_directory(&f.app, &enrollment).await;
    for index in 0..9 {
        record_application(
            &f.app,
            Some(enrollment.clone()),
            CaseAction::Kick,
            CaseOutcome::Succeeded,
            SUBJECT,
            Some(1),
            Some(format!("case {index}")),
            CaseEvidence::Unavailable,
        )
        .await;
    }
    let (cases, truncated) = collect_prior_cases(&store, scope, SUBJECT as i64)
        .await
        .unwrap();
    assert!(truncated);
    assert_eq!(cases.len(), PRIOR_CASE_LIMIT as usize);
    let note = build_rejoin_note(cases, false, truncated).unwrap();
    let vars = member_rejoin_vars(&user(SUBJECT), &note);
    let sink = Sink::default();
    db::upsert_log_channel(&f.app.db, GUILD, "member_join", 100)
        .await
        .unwrap();
    f.app.invalidate_configs();
    assert_eq!(
        emit_to(
            &f.app,
            &sink,
            GuildId::new(GUILD as u64),
            "member_rejoin",
            vars,
        )
        .await,
        Delivery::Sent
    );
    assert_eq!(sink.messages.lock().unwrap().len(), 1);
    assert!(sink.messages.lock().unwrap()[0].1.contains("omitted"));
}

#[tokio::test]
async fn member_rejoin_falls_back_to_member_join_channel() {
    let f = Fixture::new();
    f.app.activation.mark_lock_held(true);
    let note = RejoinNote::Cases {
        lines: vec!["- `ban` · succeeded · `2020-01-01T00:00:00+00:00`".into()],
        truncated: false,
    };
    let vars = member_rejoin_vars(&user(42), &note);
    let sink = Sink::default();
    assert_eq!(
        emit_to(
            &f.app,
            &sink,
            GuildId::new(789),
            "member_rejoin",
            vars.clone(),
        )
        .await,
        Delivery::Unconfigured
    );
    db::upsert_log_channel(&f.app.db, 789, "member_join", 200)
        .await
        .unwrap();
    f.app.invalidate_configs();
    assert_eq!(
        emit_to(&f.app, &sink, GuildId::new(789), "member_rejoin", vars).await,
        Delivery::Sent
    );
    assert_eq!(sink.messages.lock().unwrap()[0].0, ChannelId::new(200));
}

#[tokio::test]
async fn ignored_user_skips_member_rejoin_note() {
    let f = Fixture::new();
    f.app.activation.mark_lock_held(true);
    let note = RejoinNote::IndexNotReady;
    let vars = member_rejoin_vars(&user(42), &note);
    db::upsert_log_channel(&f.app.db, 789, "member_rejoin", 200)
        .await
        .unwrap();
    f.app
        .db
        .with_conn(|conn| {
            conn.execute_bounded(
                "INSERT INTO log_filters(guild_id,event_type,data) VALUES(789,'member_rejoin',?1)",
                db::params![r#"{"users":["42"],"channels":[]}"#],
                65_536,
            )?;
            Ok(())
        })
        .await
        .unwrap();
    f.app.invalidate_configs();
    let sink = Sink::default();
    assert_eq!(
        emit_to(&f.app, &sink, GuildId::new(789), "member_rejoin", vars).await,
        Delivery::Unconfigured
    );
    assert!(sink.messages.lock().unwrap().is_empty());
}

#[test]
fn guild_member_addition_emits_at_most_one_rejoin_note() {
    let source = include_str!("../handlers.rs");
    let start = source
        .find("async fn guild_member_addition")
        .expect("guild_member_addition");
    let body = &source[start..start + 3_000];
    assert_eq!(
        body.matches("maybe_emit_member_rejoin").count(),
        1,
        "join handler must not double-post the prior-case note"
    );
}
