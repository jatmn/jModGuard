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
fn profile_lookup_error_skips_ban_history_only_note() {
    assert_eq!(build_rejoin_note(prior(vec![], false, true), None), None);
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
async fn legacy_guild_case_without_index_is_not_listed_until_backfill() {
    const GUILD: i64 = 10;
    const SUBJECT: i64 = 88;
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
    let case_id = record_application(
        &f.app,
        Some(enrollment.clone()),
        CaseAction::Ban,
        CaseOutcome::Succeeded,
        SUBJECT as u64,
        Some(1),
        Some("legacy-only".into()),
        CaseEvidence::Unavailable,
    )
    .await
    .expect("indexed case");
    store
        .remove_case_index_for_test(scope.clone(), case_id)
        .await
        .unwrap();
    let cases = collect_prior_cases(&store, scope, SUBJECT).await.unwrap();
    assert!(!cases.index_complete);
    assert!(cases.entries.is_empty());
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
async fn delivered_rejoin_note_is_deduped_for_same_join_token() {
    let f = Fixture::new();
    f.app.activation.mark_lock_held(true);
    const GUILD: u64 = 789;
    const USER: u64 = 42;
    const STAY: &str = "stay-delivered-a";
    let note = RejoinNote::IndexNotReady;
    let vars = member_rejoin_vars(&user(USER), &note);
    db::upsert_log_channel(&f.app.db, GUILD as i64, "member_rejoin", 200)
        .await
        .unwrap();
    f.app.invalidate_configs();
    let sink = Sink::default();
    assert!(
        f.app
            .rejoin_note_deliveries
            .begin_attempt(GUILD, USER, STAY)
    );
    let sent = emit_to(
        &f.app,
        &sink,
        GuildId::new(GUILD),
        "member_rejoin",
        vars.clone(),
    )
    .await
        == Delivery::Sent;
    f.app
        .rejoin_note_deliveries
        .finish_attempt(GUILD, USER, STAY, sent);
    assert!(sent);
    assert_eq!(sink.messages.lock().unwrap().len(), 1);
    assert!(
        !f.app
            .rejoin_note_deliveries
            .begin_attempt(GUILD, USER, STAY),
        "second join handler pass must not post again for the same stay"
    );
    assert_eq!(sink.messages.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn delivered_rejoin_note_is_deduped_after_membership_refresh() {
    let f = Fixture::new();
    f.app.activation.mark_lock_held(true);
    const GUILD: u64 = 789;
    const USER: u64 = 42;
    const STAY: &str = "stay-delivered-b";
    let note = RejoinNote::IndexNotReady;
    let vars = member_rejoin_vars(&user(USER), &note);
    db::upsert_log_channel(&f.app.db, GUILD as i64, "member_rejoin", 200)
        .await
        .unwrap();
    f.app.invalidate_configs();
    let sink = Sink::default();
    assert!(
        f.app
            .rejoin_note_deliveries
            .begin_attempt(GUILD, USER, STAY)
    );
    let sent = emit_to(
        &f.app,
        &sink,
        GuildId::new(GUILD),
        "member_rejoin",
        vars.clone(),
    )
    .await
        == Delivery::Sent;
    f.app
        .rejoin_note_deliveries
        .finish_attempt(GUILD, USER, STAY, sent);
    assert!(sent);
    assert!(
        !f.app
            .rejoin_note_deliveries
            .begin_attempt(GUILD, USER, STAY),
        "membership refresh must not bypass a delivered rejoin receipt"
    );
}

#[test]
fn case_text_inline_more_substrings_do_not_satisfy_overflow_marker() {
    let lines = vec![
        "- `ban` · succeeded · `2026-01-01T00:00:00Z` — reason mentions +3 more inline".into(),
    ];
    let note = RejoinNote::Cases {
        lines,
        truncated: false,
        index_incomplete: false,
    };
    let vars = member_rejoin_vars(&user(42), &note);
    let last_line = vars["cases"].lines().next_back().expect("cases text");
    let overflow_count = last_line
        .strip_prefix('+')
        .and_then(|rest| rest.strip_suffix(" more"))
        .and_then(|count| count.parse::<usize>().ok())
        .filter(|&count| count > 0);
    assert!(
        overflow_count.is_none(),
        "inline +N more text must not count as the overflow marker: {}",
        vars["cases"]
    );
}

#[test]
fn case_text_budget_appends_more_suffix() {
    let lines = vec!["- `ban` · succeeded · `2026-01-01T00:00:00Z` — x".repeat(400); 6];
    let note = RejoinNote::Cases {
        lines,
        truncated: false,
        index_incomplete: false,
    };
    let vars = member_rejoin_vars(&user(42), &note);
    let last_line = vars["cases"]
        .lines()
        .next_back()
        .expect("budgeted cases text");
    assert!(
        last_line
            .strip_prefix('+')
            .and_then(|rest| rest.strip_suffix(" more"))
            .and_then(|count| count.parse::<usize>().ok())
            .is_some_and(|count| count > 0),
        "cases must end with a +N more overflow marker, got: {}",
        vars["cases"]
    );
}

#[test]
fn near_full_case_budget_still_shows_omission_marker() {
    let reason = "\u{1f600}".repeat(113);
    let lines = (0..8)
        .map(|_| format!("- `ban` · succeeded · `2026-01-01T00:00:00Z` — {reason}"))
        .collect();
    let note = RejoinNote::Cases {
        lines,
        truncated: false,
        index_incomplete: false,
    };
    let vars = member_rejoin_vars(&user(42), &note);
    let last_line = vars["cases"].lines().next_back().expect("cases text");
    assert!(
        last_line
            .strip_prefix('+')
            .and_then(|rest| rest.strip_suffix(" more"))
            .and_then(|count| count.parse::<usize>().ok())
            .is_some_and(|count| count >= 2),
        "near-full output must still include +N more, got: {}",
        vars["cases"]
    );
}

#[test]
fn canceled_rejoin_attempt_allows_redelivery() {
    let deliveries = crate::logging::RejoinNoteDeliveries::default();
    let guard = deliveries
        .begin_in_flight(1, 2, "stay-canceled")
        .expect("first attempt must start");
    drop(guard);
    assert!(
        deliveries.begin_attempt(1, 2, "stay-canceled"),
        "canceled attempt must clear in-flight state"
    );
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
        body.contains("rejoin_note_deliveries") && body.contains("begin_in_flight"),
        "join handler must reserve rejoin delivery before awaiting Discord"
    );
    assert!(
        body.contains("in_flight.complete"),
        "join handler must record successful rejoin delivery"
    );
    assert_eq!(
        body.matches("maybe_emit_member_rejoin").count(),
        1,
        "join handler must not double-post the prior-case note"
    );
    assert!(
        body.contains("pending_redelivery"),
        "stale join events must still retry an undelivered rejoin note"
    );
    assert!(
        body.contains("membership_refresh || retry_undelivered || durable_pending"),
        "fresh stays, durable receipts, or failed-delivery retries may emit rejoin notes"
    );
    assert!(
        body.contains("clear_pending_rejoin_note"),
        "settled rejoin delivery must clear the durable receipt"
    );
    assert!(
        body.contains("retain_current_stay") && body.contains("current_member_stay_id"),
        "in-memory retries must key to the current stay"
    );
}

#[tokio::test]
async fn durable_pending_rejoin_note_survives_stale_member_add_without_refresh() {
    const GUILD: i64 = 10;
    const USER: i64 = 42;
    let f = Fixture::new();
    let enrollment = db::enroll_guild(&f.app.db, GUILD, 100)
        .await
        .unwrap()
        .unwrap();
    f.app.tenant.activate(enrollment.clone()).await.unwrap();
    let (store, scope) = ready_directory(&f.app, &enrollment).await;
    store
        .observe_member(
            scope.clone(),
            MemberObservation {
                user_id: USER,
                username: Some(Some("member".into())),
                global_name: None,
                nickname: None,
                joined_at_ms: Some(1_000),
                observed_at_ms: 1_000,
                is_join: true,
            },
        )
        .await
        .unwrap();
    store
        .depart_member(scope.clone(), USER, 2_000)
        .await
        .unwrap();
    store
        .observe_member(
            scope.clone(),
            MemberObservation {
                user_id: USER,
                username: Some(Some("member".into())),
                global_name: None,
                nickname: None,
                joined_at_ms: Some(3_000),
                observed_at_ms: 3_000,
                is_join: true,
            },
        )
        .await
        .unwrap();
    assert!(
        store
            .has_pending_rejoin_note(scope.clone(), USER)
            .await
            .unwrap()
    );
    let stale = store
        .observe_member(
            scope.clone(),
            MemberObservation {
                user_id: USER,
                username: Some(Some("member".into())),
                global_name: None,
                nickname: None,
                joined_at_ms: Some(3_000),
                observed_at_ms: 3_500,
                is_join: true,
            },
        )
        .await
        .unwrap();
    assert!(!stale.membership_refresh);
    assert!(crate::member_directory::pending_rejoin_note(&f.app, GUILD as u64, USER as u64).await);
}

#[tokio::test]
async fn failed_rejoin_retries_after_stale_member_add_without_refresh() {
    let f = Fixture::new();
    f.app.activation.mark_lock_held(true);
    const GUILD: u64 = 789;
    const USER: u64 = 42;
    const STAY: &str = "stay-retry";
    let note = RejoinNote::IndexNotReady;
    let vars = member_rejoin_vars(&user(USER), &note);
    db::upsert_log_channel(&f.app.db, GUILD as i64, "member_rejoin", 200)
        .await
        .unwrap();
    f.app.invalidate_configs();
    let sink = Sink::default();
    let refresh_attempt = f
        .app
        .rejoin_note_deliveries
        .begin_in_flight(GUILD, USER, STAY)
        .expect("fresh stay reserves delivery");
    refresh_attempt.complete(false);
    assert!(
        f.app
            .rejoin_note_deliveries
            .pending_redelivery(GUILD, USER, STAY),
        "failed delivery must remain retryable"
    );
    let membership_refresh = false;
    let retry_undelivered = f
        .app
        .rejoin_note_deliveries
        .pending_redelivery(GUILD, USER, STAY);
    assert!(retry_undelivered);
    assert!(!membership_refresh);
    let stale_retry = f
        .app
        .rejoin_note_deliveries
        .begin_in_flight(GUILD, USER, STAY)
        .expect("stale add must retry the undelivered stay");
    let sent =
        emit_to(&f.app, &sink, GuildId::new(GUILD), "member_rejoin", vars).await == Delivery::Sent;
    stale_retry.complete(sent);
    assert!(sent);
    assert_eq!(sink.messages.lock().unwrap().len(), 1);
    assert!(
        !f.app
            .rejoin_note_deliveries
            .begin_attempt(GUILD, USER, STAY),
        "successful delivery must not post again"
    );
}

#[tokio::test]
async fn member_rejoin_emit_retries_without_clearing_durable_pending() {
    const GUILD: i64 = 10;
    const USER: i64 = 42;
    let f = Fixture::new();
    let enrollment = db::enroll_guild(&f.app.db, GUILD, 100)
        .await
        .unwrap()
        .unwrap();
    f.app.tenant.activate(enrollment.clone()).await.unwrap();
    let (store, scope) = ready_directory(&f.app, &enrollment).await;
    store
        .observe_member(
            scope.clone(),
            MemberObservation {
                user_id: USER,
                username: Some(Some("member".into())),
                global_name: None,
                nickname: None,
                joined_at_ms: Some(1_000),
                observed_at_ms: 1_000,
                is_join: true,
            },
        )
        .await
        .unwrap();
    store
        .depart_member(scope.clone(), USER, 2_000)
        .await
        .unwrap();
    store
        .observe_member(
            scope.clone(),
            MemberObservation {
                user_id: USER,
                username: Some(Some("member".into())),
                global_name: None,
                nickname: None,
                joined_at_ms: Some(3_000),
                observed_at_ms: 3_000,
                is_join: true,
            },
        )
        .await
        .unwrap();
    assert!(
        store
            .has_pending_rejoin_note(scope.clone(), USER)
            .await
            .unwrap()
    );
    let mut vars = BTreeMap::new();
    assert_eq!(
        enrich_member_rejoin_vars(&f.app, GuildId::new(999_999), &user(USER as u64), &mut vars)
            .await,
        MemberRejoinEnrich::Retry
    );
    assert!(
        store
            .has_pending_rejoin_note(scope.clone(), USER)
            .await
            .unwrap()
    );
}

#[test]
fn superseded_stay_does_not_inherit_undelivered_retry() {
    let deliveries = crate::logging::RejoinNoteDeliveries::default();
    deliveries.finish_attempt(1, 2, "stay-old", false);
    assert!(deliveries.pending_redelivery(1, 2, "stay-old"));
    assert!(!deliveries.pending_redelivery(1, 2, "stay-new"));
    deliveries.retain_current_stay(1, 2, "stay-new");
    assert!(!deliveries.pending_redelivery(1, 2, "stay-old"));
    deliveries.finish_attempt(1, 2, "stay-new", false);
    assert!(deliveries.pending_redelivery(1, 2, "stay-new"));
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
    assert_eq!(
        enrich_member_rejoin_vars(&f.app, GuildId::new(GUILD as u64), &user, &mut vars).await,
        MemberRejoinEnrich::Ready
    );
    assert!(vars["notice"].contains("no case details"));
    assert!(!vars["notice"].contains("not ready"));
    assert!(vars["cases"].is_empty());
}
