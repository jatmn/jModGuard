use super::tenant_logs::scope;
use super::*;

pub(super) fn data(source: i64) -> CaseData {
    CaseData {
        ban_attempt_tag: None,
        source: CaseSource::DiscordAudit { entry_id: source },
        action: CaseAction::Ban,
        outcome: CaseOutcome::Succeeded,
        subject_id: i64::MAX,
        actor_id: None,
        occurred_at_ms: 1000,
        reason: Some("selected reason".into()),
        evidence: CaseEvidence::Text(vec![TextEvidence {
            channel_id: 10,
            message_id: 11,
            author_id: i64::MAX,
            occurred_at_ms: 900,
            text: "selected excerpt".into(),
        }]),
    }
}

#[tokio::test]
async fn cases_are_scoped_persistent_and_paginated_without_payloads() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let b = scope(&f, &store, &reg, 20).await;
    let limits = CaseLimits::default();
    let mut ids = Vec::new();
    for n in 1..=3 {
        let id = CaseId::new();
        assert_eq!(
            store
                .record_case(a.clone(), id.clone(), data(n), limits)
                .await
                .unwrap(),
            RecordCase::Created(id.clone())
        );
        ids.push(id);
    }
    ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    assert!(
        store
            .case(b.clone(), ids[0].clone(), limits)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .case_ids(b.clone(), None, 10, limits)
            .await
            .unwrap()
            .is_empty()
    );
    let first = store.case_ids(a.clone(), None, 2, limits).await.unwrap();
    assert_eq!(first, ids[..2]);
    assert_eq!(
        store
            .case_ids(a.clone(), Some(first[1].clone()), 2, limits)
            .await
            .unwrap(),
        ids[2..]
    );
    let reopened = GuildDataStore::open(f.options(), reg).unwrap();
    assert!(
        reopened
            .case(a.clone(), ids[0].clone(), limits)
            .await
            .unwrap()
            .is_some()
    );
    // Same local ID and source are allowed in another tenant; no shared payload identity.
    store
        .record_case(b.clone(), ids[0].clone(), data(1), limits)
        .await
        .unwrap();
    store.remove_case(a.clone(), ids[0].clone()).await.unwrap();
    assert!(
        store
            .case(b, ids[0].clone(), limits)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn source_retries_conflicts_and_removed_replays_preserve_canonical_identity() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let limits = CaseLimits::default();
    let id = CaseId::new();
    let facts = data(1);
    store
        .record_case(a.clone(), id.clone(), facts.clone(), limits)
        .await
        .unwrap();
    assert_eq!(
        store
            .record_case(a.clone(), CaseId::new(), facts.clone(), limits)
            .await
            .unwrap(),
        RecordCase::Existing(id.clone())
    );
    let mut changed = facts.clone();
    changed.outcome = CaseOutcome::Failed;
    assert!(
        store
            .record_case(a.clone(), id.clone(), changed.clone(), limits)
            .await
            .is_err()
    );
    assert!(
        store
            .record_case(a.clone(), id.clone(), data(2), limits)
            .await
            .is_err()
    );
    assert!(store.remove_case(a.clone(), id.clone()).await.unwrap());
    assert!(!store.remove_case(a.clone(), id.clone()).await.unwrap());
    assert!(!store.remove_case(a.clone(), CaseId::new()).await.unwrap());
    assert!(
        store
            .case(a.clone(), id.clone(), limits)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .record_case(a.clone(), CaseId::new(), changed, limits)
            .await
            .unwrap(),
        RecordCase::Removed(id)
    );
    assert!(
        store
            .case_ids(a, None, 100, limits)
            .await
            .unwrap()
            .is_empty()
    );
    store
        .db
        .with_conn(|c| {
            assert!(
                c.query_row("SELECT payload IS NULL FROM guild_cases", (), |r| Ok(
                    r.get::<bool>(0)?
                ))?
            );
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn concurrent_recording_has_one_row_and_noop_paths_are_write_free() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let limits = CaseLimits::default();
    let x = CaseId::new();
    let y = CaseId::new();
    let (one, two) = tokio::join!(
        store.record_case(a.clone(), x, data(1), limits),
        store.record_case(a.clone(), y, data(1), limits)
    );
    let id = match (one.unwrap(), two.unwrap()) {
        (RecordCase::Created(a), RecordCase::Existing(b))
        | (RecordCase::Existing(a), RecordCase::Created(b)) => {
            assert_eq!(a, b);
            a
        }
        other => panic!("unexpected results {other:?}"),
    };
    store.db.with_conn(|c| c.execute_batch_bounded("CREATE TRIGGER reject_case_insert BEFORE INSERT ON guild_cases BEGIN SELECT RAISE(ABORT,'unexpected insert'); END; CREATE TRIGGER reject_receipt BEFORE INSERT ON mutation_receipts BEGIN SELECT RAISE(ABORT,'unexpected receipt'); END", 64 * 1024 * 1024)).await.unwrap();
    assert_eq!(
        store
            .record_case(a.clone(), id.clone(), data(1), limits)
            .await
            .unwrap(),
        RecordCase::Existing(id.clone())
    );
    assert!(
        store
            .case(a.clone(), id.clone(), limits)
            .await
            .unwrap()
            .is_some()
    );
    assert!(!store.remove_case(a.clone(), CaseId::new()).await.unwrap());
    store.remove_case(a.clone(), id.clone()).await.unwrap();
    assert_eq!(
        store
            .record_case(a.clone(), id.clone(), data(1), limits)
            .await
            .unwrap(),
        RecordCase::Removed(id.clone())
    );
    assert!(!store.remove_case(a, id).await.unwrap());
}

#[tokio::test]
async fn record_budgets_reject_oversize_reads_without_erasing_records() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let limits = CaseLimits::default();
    let id = CaseId::new();
    let tiny = CaseLimits {
        max_record_bytes: 1,
        ..limits
    };
    assert!(
        store
            .record_case(a.clone(), id.clone(), data(1), tiny)
            .await
            .is_err()
    );
    store
        .record_case(a.clone(), id.clone(), data(1), limits)
        .await
        .unwrap();
    assert!(store.case(a.clone(), id.clone(), tiny).await.is_err());
    assert_eq!(
        store.case_ids(a.clone(), None, 1, tiny).await.unwrap(),
        vec![id.clone()]
    );
    assert!(store.case(a.clone(), id, limits).await.unwrap().is_some());
    assert!(store.case_ids(a.clone(), None, 0, limits).await.is_err());
    assert!(store.case_ids(a.clone(), None, 101, limits).await.is_err());
    for n in 2..=4 {
        let mut bad = data(n);
        match n {
            2 => bad.subject_id = 0,
            3 => bad.actor_id = Some(-1),
            _ => bad.source = CaseSource::DiscordAudit { entry_id: 0 },
        }
        assert!(
            store
                .record_case(a.clone(), CaseId::new(), bad, limits)
                .await
                .is_err()
        );
    }
    let mut bad = data(2);
    bad.evidence = CaseEvidence::Text(vec![]);
    assert!(
        store
            .record_case(a.clone(), CaseId::new(), bad, limits)
            .await
            .is_err()
    );
    let serialized = serde_json::to_value(data(1)).unwrap();
    assert_eq!(serialized["subject_id"], i64::MAX.to_string());
    assert_eq!(
        serde_json::from_value::<CaseData>(serialized).unwrap(),
        data(1)
    );
    assert!(serde_json::from_str::<CaseId>("\"invalid\"").is_err());
}

#[tokio::test]
async fn closed_memberships_deny_every_case_operation_and_cleanup_is_bounded() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let b = scope(&f, &store, &reg, 20).await;
    let limits = CaseLimits::default();
    let id = CaseId::new();
    for n in 1..=3 {
        store
            .record_case(a.clone(), CaseId::new(), data(n), limits)
            .await
            .unwrap();
    }
    store
        .record_case(b.clone(), id.clone(), data(1), limits)
        .await
        .unwrap();
    assert!(
        store
            .purge_closed_cases(a.clone(), 1, limits)
            .await
            .is_err()
    );
    let (_, closed) = tokio::join!(
        store.record_case(a.clone(), id.clone(), data(4), limits),
        store.close_membership(a.clone())
    );
    closed.unwrap();
    assert!(store.case(a.clone(), id.clone(), limits).await.is_err());
    assert!(store.case_ids(a.clone(), None, 1, limits).await.is_err());
    assert!(store.remove_case(a.clone(), id.clone()).await.is_err());
    assert!(
        store
            .record_case(a.clone(), id.clone(), data(1), limits)
            .await
            .is_err()
    );
    assert_eq!(
        store
            .purge_closed_cases(a.clone(), 2, limits)
            .await
            .unwrap(),
        2
    );
    let remaining = store
        .purge_closed_cases(a.clone(), 2, limits)
        .await
        .unwrap();
    assert!((1..=2).contains(&remaining));
    assert_eq!(
        store
            .purge_closed_cases(a.clone(), 2, limits)
            .await
            .unwrap(),
        0
    );
    assert!(store.bind_initial_membership(a).await.is_err());
    assert!(store.case(b, id, limits).await.unwrap().is_some());
}

#[tokio::test]
async fn sql_guards_prevent_overwrite_replacement_and_live_replay_key_deletion() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let limits = CaseLimits::default();
    let id = CaseId::new();
    store
        .record_case(a.clone(), id.clone(), data(1), limits)
        .await
        .unwrap();
    store.db.with_conn(|c| {
        c.execute_batch("PRAGMA foreign_keys=OFF; PRAGMA recursive_triggers=OFF")?;
        for sql in ["INSERT OR REPLACE INTO guild_cases SELECT guild_id,enrollment_id,case_id,source_key,payload,deleted FROM guild_cases", "UPDATE guild_cases SET payload='{}'", "DELETE FROM guild_cases", "UPDATE guild_cases SET guild_id=20", "INSERT INTO guild_cases SELECT 20,enrollment_id,case_id,source_key,payload,deleted FROM guild_cases"] {
            assert!(c.execute_batch_bounded(sql, 64 * 1024 * 1024).is_err(),"accepted {sql}");
        }
        c.execute_batch_bounded("CREATE TRIGGER reject_removal BEFORE UPDATE ON guild_cases BEGIN SELECT RAISE(ABORT,'test removal failure'); END", 64 * 1024 * 1024)?;
        Ok(())
    }).await.unwrap();
    assert!(store.remove_case(a.clone(), id.clone()).await.is_err());
    assert_eq!(
        store.case(a.clone(), id.clone(), limits).await.unwrap(),
        Some(data(1))
    );
    store
        .db
        .with_conn(|c| c.execute_batch_bounded("DROP TRIGGER reject_removal", 64 * 1024 * 1024))
        .await
        .unwrap();
    store.remove_case(a.clone(), id).await.unwrap();
    store
        .db
        .with_conn(|c| {
            assert!(
                c.execute_batch_bounded(
                    "UPDATE guild_cases SET deleted=0,payload='{}'",
                    64 * 1024 * 1024
                )
                .is_err()
            );
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn wrong_captured_membership_and_changed_identity_deny_empty_case_paths() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let wrong = GuildDataScope::new(
        f.placement(&reg, 10).await,
        GuildEnrollment {
            guild_id: 10,
            id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            joined_at_ns: 100,
        },
    )
    .unwrap();
    for s in [wrong] {
        assert!(
            store
                .case_summaries(s.clone(), vec![], CaseLimits::default())
                .await
                .is_err()
        );
        assert!(
            store
                .case_ids(s.clone(), None, 1, CaseLimits::default())
                .await
                .is_err()
        );
        assert!(
            store
                .case(s.clone(), CaseId::new(), CaseLimits::default())
                .await
                .is_err()
        );
        assert!(store.remove_case(s.clone(), CaseId::new()).await.is_err());
        assert!(
            store
                .record_case(s, CaseId::new(), data(1), CaseLimits::default())
                .await
                .is_err()
        );
    }
    store.db.with_conn(|c|c.execute_batch_bounded("DROP TRIGGER store_identity_no_update; UPDATE store_identity SET store_id='00000000000000000000000000000000'; CREATE TRIGGER store_identity_no_update BEFORE UPDATE ON store_identity BEGIN SELECT RAISE(ABORT,'immutable'); END", 64 * 1024 * 1024)).await.unwrap();
    assert!(
        store
            .case_ids(a.clone(), None, 1, CaseLimits::default())
            .await
            .is_err()
    );
    assert!(
        store
            .case_summaries(a.clone(), vec![], CaseLimits::default())
            .await
            .is_err()
    );
    assert!(store.remove_case(a, CaseId::new()).await.is_err());
}

#[tokio::test]
async fn upgrade_from_two_preserves_scoped_settings_without_adopting_cases() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let p = f.placement(&reg, 10).await;
    {
        let raw = Connection::open(&f.store_path).unwrap();
        apply_guild_fixture(&raw, 0, 2).unwrap();
        raw.execute_bounded(
            "INSERT INTO store_identity VALUES(1,?1,?2,NULL)",
            params![FAMILY, reg.id().as_str()],
            64 * 1024 * 1024,
        )
        .unwrap();
        raw.execute_bounded(
            "INSERT INTO guild_bindings VALUES(10,?1)",
            params![p.revision()],
            64 * 1024 * 1024,
        )
        .unwrap();
        raw.execute_batch_bounded("INSERT INTO guild_memberships VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',100,1,'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'); INSERT INTO guild_log_channels VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','system',12)", 64 * 1024 * 1024).unwrap();
        apply_guild_fixture(&raw, 2, GUILD_BASELINE_VERSION).unwrap();
    }
    assert!(GuildDataStore::open(f.options(), reg.clone()).is_err());
    let store = GuildDataStore::initialize(f.options(), reg).unwrap();
    let s = GuildDataScope::new(
        p,
        GuildEnrollment {
            guild_id: 10,
            id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            joined_at_ns: 100,
        },
    )
    .unwrap();
    assert_eq!(
        store.log_channels(s.clone()).await.unwrap().channels(),
        &[("system".into(), 12)]
    );
    assert!(
        store
            .case_ids(s, None, 1, CaseLimits::default())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn tagged_attempt_survives_reopen_and_cannot_be_retagged_or_exposed() {
    use crate::ban_reason::{BanAttemptTag, discord_reason};
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    // Exercise the immutable payload tag on a store upgraded from v0.2.0.
    GuildDataStore::create_historical_store(f.options(), reg.clone(), GUILD_BASELINE_VERSION)
        .unwrap();
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let captured = scope(&f, &store, &reg, 10).await;
    let limits = CaseLimits::default();
    let id = CaseId::new();
    let tag = BanAttemptTag::new();
    let copied = discord_reason("selected reason", Some(&BanAttemptTag::new()));
    let mut facts = data(1);
    facts.source = CaseSource::Application {
        operation_id: CaseId::new(),
    };
    facts.outcome = CaseOutcome::Attempted;
    facts.ban_attempt_tag = Some(tag.clone());
    facts.reason = Some(copied.clone());
    store
        .record_case(captured.clone(), id.clone(), facts.clone(), limits)
        .await
        .unwrap();
    assert_eq!(
        store
            .record_case(captured.clone(), id.clone(), facts, limits)
            .await
            .unwrap(),
        RecordCase::Existing(id.clone())
    );
    let id_for_update = id.as_str().to_owned();
    let error = store.db.with_conn(move |conn| {
        conn.execute_rows("UPDATE guild_cases SET payload=json_set(payload, '$.ban_attempt_tag', ?2) WHERE case_id=?1", params![id_for_update, "0".repeat(20)], 1)?;
        Ok(())
    }).await.unwrap_err();
    assert!(error.to_string().contains("immutable"), "{error:#}");
    drop(store);
    let store = GuildDataStore::open(f.options(), reg).unwrap();
    let saved = store
        .case(captured.clone(), id, limits)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.ban_attempt_tag.as_ref(), Some(&tag));
    assert_eq!(saved.reason.as_deref(), Some("selected reason"));
    let public = serde_json::to_value(&saved).unwrap();
    assert!(public.get("ban_attempt_tag").is_none());
    assert!(!public.to_string().contains(tag.as_str()));
    let sent = discord_reason(&copied, Some(&tag));
    assert!(
        store
            .settle_tagged_application_ban(captured.clone(), 42, &sent, limits)
            .await
            .unwrap()
            .is_empty()
    );
    let settled = store
        .settle_tagged_application_ban(captured.clone(), i64::MAX, &sent, limits)
        .await
        .unwrap();
    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0].1.outcome, CaseOutcome::Succeeded);
    assert_eq!(settled[0].1.ban_attempt_tag.as_ref(), Some(&tag));
    assert!(
        store
            .settle_tagged_application_ban(captured, i64::MAX, &sent, limits)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn matching_case_ids_filter_live_facts_and_keep_the_id_cursor() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let b = scope(&f, &store, &reg, 20).await;
    let limits = CaseLimits::default();
    // n: action, outcome, subject, actor
    let facts = [
        (CaseAction::Ban, CaseOutcome::Succeeded, 7, Some(42)),
        (CaseAction::Kick, CaseOutcome::Succeeded, 7, None),
        (CaseAction::Ban, CaseOutcome::Failed, 8, Some(42)),
        (CaseAction::Ban, CaseOutcome::Succeeded, 8, Some(43)),
        (
            CaseAction::Warning,
            CaseOutcome::Succeeded,
            i64::MAX,
            Some(i64::MAX),
        ),
    ];
    let mut by_fact = Vec::new();
    for (n, (action, outcome, subject, actor)) in facts.iter().enumerate() {
        let mut case = data(n as i64 + 1);
        case.action = action.clone();
        case.outcome = outcome.clone();
        case.subject_id = *subject;
        case.actor_id = *actor;
        let id = CaseId::new();
        store
            .record_case(a.clone(), id.clone(), case.clone(), limits)
            .await
            .unwrap();
        // The other tenant holds matching facts that must never leak into a's pages.
        store
            .record_case(b.clone(), CaseId::new(), case, limits)
            .await
            .unwrap();
        by_fact.push(id);
    }
    let sorted = |picks: &[usize]| {
        let mut ids: Vec<CaseId> = picks.iter().map(|i| by_fact[*i].clone()).collect();
        ids.sort_by(|x, y| x.as_str().cmp(y.as_str()));
        ids
    };
    let page = |filter: CaseFilter, after: Option<CaseId>, count: u32| {
        let store = &store;
        let a = a.clone();
        async move {
            store
                .matching_case_ids(a, after, count, filter, limits)
                .await
                .unwrap()
        }
    };
    let bans = CaseFilter {
        action: Some(CaseAction::Ban),
        ..CaseFilter::default()
    };
    assert_eq!(page(bans.clone(), None, 10).await, sorted(&[0, 2, 3]));
    assert_eq!(
        page(
            CaseFilter {
                outcome: Some(CaseOutcome::Failed),
                ..CaseFilter::default()
            },
            None,
            10
        )
        .await,
        sorted(&[2])
    );
    assert_eq!(
        page(
            CaseFilter {
                subject_id: Some(7),
                ..CaseFilter::default()
            },
            None,
            10
        )
        .await,
        sorted(&[0, 1])
    );
    // Snowflakes above 2^53 are stored as strings and still match exactly.
    assert_eq!(
        page(
            CaseFilter {
                actor_id: Some(i64::MAX),
                ..CaseFilter::default()
            },
            None,
            10
        )
        .await,
        sorted(&[4])
    );
    let combined = CaseFilter {
        action: Some(CaseAction::Ban),
        outcome: Some(CaseOutcome::Succeeded),
        actor_id: Some(42),
        ..CaseFilter::default()
    };
    assert_eq!(page(combined, None, 10).await, sorted(&[0]));
    // The ID cursor pages through matches only.
    let all_bans = sorted(&[0, 2, 3]);
    let first = page(bans.clone(), None, 2).await;
    assert_eq!(first, all_bans[..2]);
    assert_eq!(
        page(bans.clone(), Some(first[1].clone()), 2).await,
        all_bans[2..]
    );
    // An empty filter is the unfiltered page; removed records never match.
    assert_eq!(
        page(CaseFilter::default(), None, 10).await,
        store.case_ids(a.clone(), None, 10, limits).await.unwrap()
    );
    store
        .remove_case(a.clone(), by_fact[0].clone())
        .await
        .unwrap();
    assert_eq!(page(bans, None, 10).await, sorted(&[2, 3]));
    assert!(
        store
            .matching_case_ids(
                a,
                None,
                10,
                CaseFilter {
                    subject_id: Some(0),
                    ..CaseFilter::default()
                },
                limits
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn summary_pages_are_bounded_scoped_and_use_constant_database_calls() {
    let probe = std::sync::Arc::new(crate::db_compat::StartupProbe::default());
    let _guard = crate::db_compat::enter_startup_probe(Some(probe.clone()));
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let b = scope(&f, &store, &reg, 20).await;
    let limits = CaseLimits::default();
    for n in 1..=50 {
        store
            .record_case(a.clone(), CaseId::new(), data(n), limits)
            .await
            .unwrap();
    }
    let ids = store.case_ids(a.clone(), None, 50, limits).await.unwrap();
    let mut baseline = None;
    for count in [0, 1, 50] {
        let queries = probe.queries();
        let transactions = probe.transactions();
        let immediate = probe.immediate();
        let summaries = store
            .case_summaries(a.clone(), ids[..count].to_vec(), limits)
            .await
            .unwrap();
        assert_eq!(summaries.len(), count);
        assert_eq!(probe.transactions() - transactions, 1);
        assert_eq!(probe.immediate() - immediate, 0);
        let calls = probe.queries() - queries;
        assert!(calls > 0);
        assert_eq!(
            calls,
            *baseline.get_or_insert(calls),
            "calls must not grow with page size"
        );
        for summary in summaries.values() {
            assert_eq!(
                serde_json::to_value(summary).unwrap(),
                serde_json::json!({
                    "action": "ban", "outcome": "succeeded", "subject_id": i64::MAX.to_string(),
                    "actor_id": null, "source_kind": "discord_audit", "occurred_at_ms": 1000
                })
            );
        }
    }
    // Every source variant lists as its kind alone, never its identifier.
    let c = scope(&f, &store, &reg, 30).await;
    for (source, kind) in [
        (
            CaseSource::Application {
                operation_id: CaseId::new(),
            },
            "application",
        ),
        (
            CaseSource::GatewayObservation {
                observation_id: CaseId::new(),
            },
            "gateway_observation",
        ),
        (CaseSource::DiscordAudit { entry_id: 7 }, "discord_audit"),
    ] {
        let id = CaseId::new();
        store
            .record_case(
                c.clone(),
                id.clone(),
                CaseData { source, ..data(1) },
                limits,
            )
            .await
            .unwrap();
        let summaries = store
            .case_summaries(c.clone(), vec![id.clone()], limits)
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(&summaries[id.as_str()]).unwrap(),
            serde_json::json!({
                "action": "ban", "outcome": "succeeded", "subject_id": i64::MAX.to_string(),
                "actor_id": null, "source_kind": kind, "occurred_at_ms": 1000
            })
        );
    }
    assert!(
        store
            .case_summaries(a.clone(), vec![ids[0].clone(); 101], limits)
            .await
            .is_err()
    );
    assert!(
        store
            .case_summaries(b.clone(), ids.clone(), limits)
            .await
            .unwrap()
            .is_empty()
    );
    store
        .record_case(b.clone(), ids[0].clone(), data(99), limits)
        .await
        .unwrap();
    store.remove_case(a.clone(), ids[0].clone()).await.unwrap();
    let mut page = ids.clone();
    page.push(CaseId::new());
    let summaries = store.case_summaries(a.clone(), page, limits).await.unwrap();
    assert_eq!(summaries.len(), 49);
    assert!(!summaries.contains_key(ids[0].as_str()));
    assert_eq!(
        store
            .case_summaries(b, vec![ids[0].clone()], limits)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        store
            .case_ids(a.clone(), Some(ids[49].clone()), 50, limits)
            .await
            .unwrap()
            .is_empty()
    );
    let tiny = CaseLimits {
        max_record_bytes: 1,
        ..limits
    };
    assert!(
        store
            .case_summaries(a.clone(), ids.clone(), tiny)
            .await
            .is_err()
    );
    store.close_membership(a.clone()).await.unwrap();
    for page in [vec![], ids] {
        assert!(
            store.case_summaries(a.clone(), page, limits).await.is_err(),
            "even an empty page must check membership"
        );
    }
}

async fn check_case_listing_migration_with_snapshot_allocation(delete_snapshots: bool) {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let placement = f.placement(&reg, 10).await;
    GuildDataStore::create_historical_store(f.options(), reg.clone(), 20).unwrap();
    {
        let raw = Connection::open(&f.store_path).unwrap();
        raw.execute_bounded(
            "INSERT INTO guild_bindings VALUES(10,?1)",
            params![placement.revision()],
            32_000_000,
        )
        .unwrap();
        raw.execute_batch_bounded(
            "INSERT INTO guild_memberships VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',100,1,'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb');
             WITH RECURSIVE messages(id) AS (VALUES(1) UNION ALL SELECT id+1 FROM messages WHERE id < 9200)
             INSERT INTO guild_message_snapshots(
                 guild_id,enrollment_id,channel_id,message_id,content,content_supplied,created_at_ms,
                 first_observed_at_ms,last_observed_at_ms,expires_at_ms,coverage_epoch,encoded_bytes
             )
             SELECT 10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',20,id,printf('%032768d',0),1,1,1,1,86400001,0,32768
             FROM messages",
            340_000_000,
        )
        .unwrap();
        if delete_snapshots {
            raw.execute_batch_bounded("DELETE FROM guild_message_snapshots", 1_000_000)
                .unwrap();
        }
        let allocation = crate::capacity::sample(&raw).unwrap();
        assert!(
            (300_000_000..crate::capacity::GROWTH_STOP_BYTES).contains(&allocation.allocated_bytes),
            "{allocation:?}"
        );
        if delete_snapshots {
            let reusable_pages: u64 = raw
                .query_row("PRAGMA freelist_count", (), |r| Ok(r.get::<i64>(0)? as u64))
                .unwrap();
            assert!(reusable_pages * allocation.page_size >= 300_000_000);
        }
    }

    let store =
        GuildDataStore::initialize(f.options(), reg).expect("schema-20 store should upgrade");
    store
        .db
        .with_conn(|conn| {
            let version: i64 = conn.query_row("PRAGMA user_version", (), |r| Ok(r.get(0)?))?;
            assert_eq!(version, SCHEMA_VERSION);
            let indexes: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sqlite_schema WHERE type='index' AND tbl_name='guild_cases'
                 AND name IN ('guild_cases_list_date','guild_cases_list_action','guild_cases_list_outcome',
                              'guild_cases_list_subject','guild_cases_list_actor')",
                (),
                |r| Ok(r.get(0)?),
            )?;
            assert_eq!(indexes, 5);
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn case_listing_migration_admits_large_store_with_retained_snapshots() {
    check_case_listing_migration_with_snapshot_allocation(false).await;
}

#[tokio::test]
async fn case_listing_migration_admits_large_store_with_reusable_snapshot_pages() {
    check_case_listing_migration_with_snapshot_allocation(true).await;
}

#[tokio::test]
async fn case_listing_migration_indexes_existing_guild_rows_for_every_seek() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let placement = f.placement(&reg, 10).await;
    let enrollment = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    GuildDataStore::create_historical_store(f.options(), reg.clone(), 20).unwrap();
    {
        let raw = Connection::open(&f.store_path).unwrap();
        raw.execute_bounded(
            "INSERT INTO guild_bindings VALUES(10,?1)",
            params![placement.revision()],
            32_000_000,
        )
        .unwrap();
        raw.execute_bounded(
            "INSERT INTO guild_memberships VALUES(10,?1,100,1,'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb')",
            params![enrollment],
            32_000_000,
        )
        .unwrap();
        raw.execute_bounded(
            "WITH RECURSIVE numbers(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM numbers WHERE n < 8192)
             INSERT INTO guild_cases(guild_id,enrollment_id,case_id,source_key,payload,deleted)
             SELECT 10,?1,printf('%032x',n),'discord:' || n,
                    json_set(?2, '$.source.discord_audit.entry_id', CAST(n AS TEXT),
                                '$.occurred_at_ms', 1 + n % 4),0
             FROM numbers",
            params![enrollment, serde_json::to_string(&data(1)).unwrap()],
            32_000_000,
        )
        .unwrap();
    }
    let scope = GuildDataScope::new(
        placement,
        GuildEnrollment {
            guild_id: 10,
            id: enrollment.into(),
            joined_at_ns: 100,
        },
    )
    .unwrap();
    assert!(GuildDataStore::open(f.options(), reg.clone()).is_err());
    let store = GuildDataStore::initialize(f.options(), reg).unwrap();
    let limits = CaseLimits::default();
    for (sort, index, seek) in [
        (
            CaseSort::Date,
            "guild_cases_list_date",
            "(list_date,case_id)",
        ),
        (CaseSort::Case, "guild_cases_visible", "case_id"),
        (
            CaseSort::Action,
            "guild_cases_list_action",
            "(list_action,case_id)",
        ),
        (
            CaseSort::Subject,
            "guild_cases_list_subject",
            "(list_subject,case_id)",
        ),
        (
            CaseSort::Actor,
            "guild_cases_list_actor",
            "(list_actor,case_id)",
        ),
        (
            CaseSort::Outcome,
            "guild_cases_list_outcome",
            "(list_outcome,case_id)",
        ),
    ] {
        for descending in [false, true] {
            let query = |after| CaseListQuery {
                filter: CaseFilter::default(),
                search: None,
                from_ms: None,
                until_ms: None,
                sort,
                descending,
                after,
                include_total: false,
            };
            let first = store
                .list_cases(scope.clone(), query(None), 50, limits)
                .await
                .unwrap();
            assert_eq!(first.entries.len(), 50);
            assert!(first.next.is_some());
            for (has_cursor, query) in [(false, query(None)), (true, query(first.next))] {
                let (sql, params) = query.statement(&scope, 50, limits).unwrap();
                let plan = store
                    .db
                    .with_conn(move |conn| {
                        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
                        stmt.query_map(crate::db_compat::params_from_iter(params), |row| {
                            Ok(row.get::<String>(3)?)
                        })?
                        .collect::<Result<Vec<_>>>()
                    })
                    .await
                    .unwrap();
                let range = plan
                    .iter()
                    .find(|line| line.contains("SEARCH guild_cases"))
                    .unwrap_or_else(|| panic!("{plan:?}"));
                assert!(
                    range.contains(index) && range.contains("guild_id=? AND enrollment_id=?"),
                    "{plan:?}"
                );
                if has_cursor {
                    let comparison = if descending { '<' } else { '>' };
                    assert!(range.contains(&format!("{seek}{comparison}")), "{plan:?}");
                }
                assert!(
                    plan.iter().all(|line| !line.contains("TEMP B-TREE")),
                    "{plan:?}"
                );
            }
        }
    }
}
