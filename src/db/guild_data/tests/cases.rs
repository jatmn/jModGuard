use super::tenant_logs::scope;
use super::*;

fn data(source: i64) -> CaseData {
    CaseData {
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
            assert_eq!(
                c.query_row("SELECT payload IS NULL FROM guild_cases", (), |r| Ok(
                    r.get::<bool>(0)?
                ))?,
                true
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
    store.db.with_conn(|c| c.execute_batch("CREATE TRIGGER reject_case_insert BEFORE INSERT ON guild_cases BEGIN SELECT RAISE(ABORT,'unexpected insert'); END; CREATE TRIGGER reject_receipt BEFORE INSERT ON mutation_receipts BEGIN SELECT RAISE(ABORT,'unexpected receipt'); END")).await.unwrap();
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
        for sql in ["INSERT OR REPLACE INTO guild_cases SELECT * FROM guild_cases", "UPDATE guild_cases SET payload='{}'", "DELETE FROM guild_cases", "UPDATE guild_cases SET guild_id=20", "INSERT INTO guild_cases SELECT 20,enrollment_id,case_id,source_key,payload,deleted FROM guild_cases"] {
            assert!(c.execute_batch(sql).is_err(),"accepted {sql}");
        }
        c.execute_batch("CREATE TRIGGER reject_removal BEFORE UPDATE ON guild_cases BEGIN SELECT RAISE(ABORT,'test removal failure'); END")?;
        Ok(())
    }).await.unwrap();
    assert!(store.remove_case(a.clone(), id.clone()).await.is_err());
    assert_eq!(
        store.case(a.clone(), id.clone(), limits).await.unwrap(),
        Some(data(1))
    );
    store
        .db
        .with_conn(|c| c.execute_batch("DROP TRIGGER reject_removal"))
        .await
        .unwrap();
    store.remove_case(a.clone(), id).await.unwrap();
    store
        .db
        .with_conn(|c| {
            assert!(
                c.execute_batch("UPDATE guild_cases SET deleted=0,payload='{}'")
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
    store.db.with_conn(|c|c.execute_batch("DROP TRIGGER store_identity_no_update; UPDATE store_identity SET store_id='00000000000000000000000000000000'; CREATE TRIGGER store_identity_no_update BEFORE UPDATE ON store_identity BEGIN SELECT RAISE(ABORT,'immutable'); END")).await.unwrap();
    assert!(
        store
            .case_ids(a.clone(), None, 1, CaseLimits::default())
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
        raw.execute_batch(SCHEMA).unwrap();
        raw.execute_batch(TENANT_SCHEMA).unwrap();
        raw.execute(
            "INSERT INTO store_identity VALUES(1,?1,?2,NULL)",
            params![FAMILY, reg.id().as_str()],
        )
        .unwrap();
        raw.execute(
            "INSERT INTO guild_bindings VALUES(10,?1)",
            params![p.revision()],
        )
        .unwrap();
        raw.execute_batch("INSERT INTO guild_memberships VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',100,1,'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'); INSERT INTO guild_log_channels VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','system',12)").unwrap();
        raw.pragma_update(None, "user_version", 2).unwrap();
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
