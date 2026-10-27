use super::*;
use crate::cases::record_application;
use crate::config::BotProcessConfig;
use crate::db;
use crate::db::Db;
use crate::db::guild_data::{
    CaseAction, CaseEvidence, CaseId, CaseOutcome, MemberHistoryEntry, MemberObservation,
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

fn prior(entries: Vec<MemberHistoryEntry>, truncated: bool, index_complete: bool) -> PriorCases {
    PriorCases {
        entries,
        truncated,
        index_complete,
    }
}

fn case_row(at: i64, case_id: &str, reason: Option<&str>) -> MemberHistoryEntry {
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
        reason: reason.map(str::to_string),
        outcome: Some("succeeded".into()),
        source_kind: None,
    }
}

#[test]
fn no_cases_and_complete_index_skips_note() {
    assert_eq!(
        build_rejoin_note(prior(vec![], false, true), Some(false)),
        None
    );
}

#[test]
fn incomplete_index_without_ban_history_skips_note() {
    assert_eq!(
        build_rejoin_note(prior(vec![], false, false), Some(false)),
        None
    );
}

#[test]
fn incomplete_index_without_cases_uses_not_ready_wording() {
    let note = build_rejoin_note(prior(vec![], false, false), Some(true)).unwrap();
    assert_eq!(note, RejoinNote::IndexNotReady);
    let vars = member_rejoin_vars(&user(42), &note);
    assert!(vars["notice"].contains("not ready"));
    assert!(vars["cases"].is_empty());
}

#[test]
fn complete_index_with_ban_history_only_uses_neutral_notice() {
    let note = build_rejoin_note(prior(vec![], false, true), Some(true)).unwrap();
    assert_eq!(note, RejoinNote::BanHistoryOnly);
    let vars = member_rejoin_vars(&user(42), &note);
    assert!(vars["notice"].contains("no case details"));
    assert!(!vars["notice"].contains("not ready"));
}

#[test]
fn three_cases_newest_first_with_tag_stripped() {
    let entries = [
        case_row(3_000, "case-3", Some("[jmg:0123456789abcdef0123] spam")),
        case_row(2_000, "case-2", None),
        case_row(1_000, "case-1", Some("rule break")),
    ];
    let note = build_rejoin_note(prior(entries.to_vec(), false, true), Some(false)).unwrap();
    let RejoinNote::Cases {
        lines,
        truncated,
        index_incomplete,
    } = note
    else {
        panic!("expected cases");
    };
    assert!(!truncated);
    assert!(!index_incomplete);
    assert_eq!(lines.len(), 3);
    assert!(lines[0].contains("`ban`"));
    assert!(lines[0].contains("spam"));
    assert!(!lines[0].contains("[jmg:"));
}

#[test]
fn truncation_notice_when_more_than_eight_cases_exist() {
    let entries = (0..9)
        .map(|index| case_row(10_000 - index as i64, &format!("case-{index}"), None))
        .collect();
    let note = build_rejoin_note(prior(entries, true, true), Some(false)).unwrap();
    let vars = member_rejoin_vars(&user(42), &note);
    assert!(vars["notice"].contains("omitted"));
}

#[test]
fn incomplete_index_with_partial_cases_adds_backfill_notice() {
    let note = build_rejoin_note(
        prior(vec![case_row(1_000, "case-one", None)], false, false),
        Some(false),
    )
    .unwrap();
    let vars = member_rejoin_vars(&user(42), &note);
    assert!(vars["notice"].contains("backfill"));
    assert!(vars["cases"].contains("`ban`"));
}

#[test]
fn case_reason_newlines_and_markdown_are_sanitized() {
    let note = build_rejoin_note(
        prior(
            vec![case_row(1_000, "case-one", Some("line1\n- fake\n`break`"))],
            false,
            true,
        ),
        Some(false),
    )
    .unwrap();
    let vars = member_rejoin_vars(&user(42), &note);
    assert!(!vars["cases"].contains('\n'));
    assert!(vars["cases"].contains("\\`"));
}

#[tokio::test]
async fn collect_prior_cases_reads_index_after_backfill() {
    const GUILD: i64 = 10;
    const SUBJECT: u64 = 77;
    let f = Fixture::new();
    let enrollment = db::enroll_guild(&f.app.db, GUILD, 100)
        .await
        .unwrap()
        .unwrap();
    f.app.tenant.activate(enrollment.clone()).await.unwrap();
    let (store, scope) = ready_directory(&f.app, &enrollment).await;
    for index in 0..3 {
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
    let cases = collect_prior_cases(&store, scope, SUBJECT as i64)
        .await
        .unwrap();
    assert!(cases.index_complete);
    assert_eq!(cases.entries.len(), 3);
}

#[tokio::test]
async fn collect_prior_cases_reads_store_rows_before_backfill_completes() {
    const GUILD: i64 = 10;
    const SUBJECT: u64 = 88;
    let f = Fixture::new();
    let enrollment = db::enroll_guild(&f.app.db, GUILD, 100)
        .await
        .unwrap()
        .unwrap();
    f.app.tenant.activate(enrollment.clone()).await.unwrap();
    let (store, scope) = f.app.tenant.live(enrollment.clone()).await.unwrap();
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    record_application(
        &f.app,
        Some(enrollment.clone()),
        CaseAction::Ban,
        CaseOutcome::Succeeded,
        SUBJECT,
        Some(1),
        Some("pre-backfill".into()),
        CaseEvidence::Unavailable,
    )
    .await;
    let cases = collect_prior_cases(&store, scope, SUBJECT as i64)
        .await
        .unwrap();
    assert!(!cases.index_complete);
    assert_eq!(cases.entries.len(), 1);
    assert_eq!(cases.entries[0].reason.as_deref(), Some("pre-backfill"));
}

#[tokio::test]
async fn indexed_cases_survive_stay_rows_in_store() {
    const GUILD: i64 = 10;
    const SUBJECT: i64 = 99;
    let f = Fixture::new();
    let enrollment = db::enroll_guild(&f.app.db, GUILD, 100)
        .await
        .unwrap()
        .unwrap();
    f.app.tenant.activate(enrollment.clone()).await.unwrap();
    let (store, scope) = ready_directory(&f.app, &enrollment).await;
    record_application(
        &f.app,
        Some(enrollment.clone()),
        CaseAction::Ban,
        CaseOutcome::Succeeded,
        SUBJECT as u64,
        Some(1),
        Some("indexed ban".into()),
        CaseEvidence::Unavailable,
    )
    .await;
    for offset in 0..5 {
        store
            .observe_member(
                scope.clone(),
                MemberObservation {
                    user_id: SUBJECT,
                    username: Some(Some("member".into())),
                    global_name: None,
                    nickname: None,
                    joined_at_ms: Some(5_000 + offset),
                    observed_at_ms: 6_000 + offset,
                    is_join: true,
                },
            )
            .await
            .unwrap();
        store
            .depart_member(scope.clone(), SUBJECT, 7_000 + offset)
            .await
            .unwrap();
    }
    let cases = collect_prior_cases(&store, scope, SUBJECT).await.unwrap();
    assert_eq!(cases.entries.len(), 1);
    assert_eq!(cases.entries[0].reason.as_deref(), Some("indexed ban"));
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
    let cases = collect_prior_cases(&store, scope, SUBJECT as i64)
        .await
        .unwrap();
    let note = build_rejoin_note(cases, Some(false)).unwrap();
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
        index_incomplete: false,
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
fn guild_member_addition_retries_rejoin_without_double_posting() {
    let source = include_str!("../handlers.rs");
    let start = source
        .find("async fn guild_member_addition")
        .expect("guild_member_addition");
    let body = &source[start..start + 3_200];
    assert!(
        body.contains("rejoin_note_deliveries.should_attempt"),
        "join handler must admit rejoin notes via delivery state, not only membership_refresh"
    );
    assert!(
        body.contains("mark_delivered"),
        "join handler must record successful rejoin delivery"
    );
    assert_eq!(
        body.matches("maybe_emit_member_rejoin").count(),
        1,
        "join handler must not double-post the prior-case note"
    );
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
async fn removed_last_case_after_backfill_uses_ban_history_notice() {
    const GUILD: i64 = 10;
    const SUBJECT: u64 = 55;
    let f = Fixture::new();
    let enrollment = db::enroll_guild(&f.app.db, GUILD, 100)
        .await
        .unwrap()
        .unwrap();
    f.app.tenant.activate(enrollment.clone()).await.unwrap();
    let (store, scope) = ready_directory(&f.app, &enrollment).await;
    record_application(
        &f.app,
        Some(enrollment.clone()),
        CaseAction::Ban,
        CaseOutcome::Succeeded,
        SUBJECT,
        Some(1),
        Some("remove me".into()),
        CaseEvidence::Unavailable,
    )
    .await;
    let recorded = store
        .member_case_entries(scope.clone(), SUBJECT as i64, 8)
        .await
        .unwrap()
        .0;
    assert_eq!(recorded.len(), 1);
    let removed = CaseId::try_from(recorded[0].case_id.clone().unwrap()).unwrap();
    store.remove_case(scope.clone(), removed).await.unwrap();
    assert!(
        store
            .member_case_entries(scope.clone(), SUBJECT as i64, 8)
            .await
            .unwrap()
            .0
            .is_empty()
    );
    let user = user(SUBJECT);
    let mut vars = BTreeMap::new();
    assert!(enrich_member_rejoin_vars(&f.app, GuildId::new(GUILD as u64), &user, &mut vars).await);
    assert!(vars["notice"].contains("no case details"));
    assert!(!vars["notice"].contains("not ready"));
    assert!(vars["cases"].is_empty());
}
