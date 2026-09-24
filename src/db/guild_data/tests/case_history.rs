use super::cases::data;
use super::tenant_logs::scope;
use super::*;

fn limits() -> CaseHistoryLimits {
    CaseHistoryLimits {
        entry: CaseLimits::default(),
        max_entries: 10,
        max_total_bytes: 10000,
    }
}
fn note(text: &str) -> CaseHistoryData {
    CaseHistoryData {
        actor_id: Some(i64::MAX),
        recorded_at_ms: 100,
        body: CaseHistoryBody::Note { text: text.into() },
    }
}
async fn parent(store: &GuildDataStore, scope: &GuildDataScope, source: i64) -> CaseId {
    let id = CaseId::new();
    store
        .record_case(
            scope.clone(),
            id.clone(),
            data(source),
            CaseLimits::default(),
        )
        .await
        .unwrap();
    id
}
fn created(result: AppendCaseHistory) -> CaseHistoryRef {
    match result {
        AppendCaseHistory::Created(r) => r,
        _ => panic!("expected new history"),
    }
}

#[tokio::test]
async fn ordered_history_preserves_original_and_isolates_parent_and_guild() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let b = scope(&f, &store, &reg, 20).await;
    let case = parent(&store, &a, 1).await;
    let other = parent(&store, &a, 2).await;
    let first = created(
        store
            .append_case_history(
                a.clone(),
                case.clone(),
                CaseId::new(),
                note("context"),
                limits(),
            )
            .await
            .unwrap(),
    );
    let correction = CaseHistoryData {
        body: CaseHistoryBody::Correction {
            outcome: Some(CaseOutcome::Uncertain),
            reason: Some(CaseReasonCorrection::Clear),
            explanation: "late evidence".into(),
        },
        ..note("unused")
    };
    let second = created(
        store
            .append_case_history(
                a.clone(),
                case.clone(),
                CaseId::new(),
                correction.clone(),
                limits(),
            )
            .await
            .unwrap(),
    );
    assert_eq!((first.sequence, second.sequence), (1, 2));
    assert_eq!(
        store
            .case(a.clone(), case.clone(), CaseLimits::default())
            .await
            .unwrap(),
        Some(data(1))
    );
    assert_eq!(
        store
            .case_history_ids(a.clone(), case.clone(), 0, 1, limits())
            .await
            .unwrap(),
        vec![first.clone()]
    );
    assert_eq!(
        store
            .case_history_ids(a.clone(), case.clone(), first.sequence, 1, limits())
            .await
            .unwrap(),
        vec![second.clone()]
    );
    let reopened = GuildDataStore::open(f.options(), reg).unwrap();
    assert_eq!(
        reopened
            .case_history_entry(a.clone(), case.clone(), second.id.clone(), limits())
            .await
            .unwrap(),
        Some(correction)
    );
    assert!(
        store
            .case_history_entry(a.clone(), other.clone(), first.id.clone(), limits())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .case_history_ids(b.clone(), case.clone(), 0, 1, limits())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .append_case_history(b, case.clone(), CaseId::new(), note("foreign"), limits())
            .await
            .is_err()
    );
    // Same opaque entry ID belongs to another case independently.
    store
        .append_case_history(a, other, first.id, note("other case"), limits())
        .await
        .unwrap();
}

#[tokio::test]
async fn concurrent_retry_and_conflict_preserve_sequence_without_noop_writes() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let case = parent(&store, &a, 1).await;
    let id = CaseId::new();
    let (one, two) = tokio::join!(
        store.append_case_history(a.clone(), case.clone(), id.clone(), note("same"), limits()),
        store.append_case_history(a.clone(), case.clone(), id.clone(), note("same"), limits())
    );
    match (one.unwrap(), two.unwrap()) {
        (AppendCaseHistory::Created(a), AppendCaseHistory::Existing(b))
        | (AppendCaseHistory::Existing(a), AppendCaseHistory::Created(b)) => assert_eq!(a, b),
        _ => panic!("duplicate append"),
    }
    assert!(
        store
            .append_case_history(
                a.clone(),
                case.clone(),
                id.clone(),
                note("different"),
                limits()
            )
            .await
            .is_err()
    );
    store.db.with_conn(|c|c.execute_batch_bounded("CREATE TRIGGER reject_history_write BEFORE INSERT ON guild_case_history BEGIN SELECT RAISE(ABORT,'unexpected append'); END; CREATE TRIGGER reject_receipt BEFORE INSERT ON mutation_receipts BEGIN SELECT RAISE(ABORT,'unexpected receipt'); END", 64 * 1024 * 1024)).await.unwrap();
    assert_eq!(
        store
            .append_case_history(a.clone(), case.clone(), id.clone(), note("same"), limits())
            .await
            .unwrap(),
        AppendCaseHistory::Existing(CaseHistoryRef {
            id: id.clone(),
            sequence: 1
        })
    );
    assert_eq!(
        store
            .case_history_entry(a.clone(), case.clone(), id, limits())
            .await
            .unwrap(),
        Some(note("same"))
    );
    assert_eq!(
        store
            .case_history_ids(a, case, 0, 10, limits())
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn budgets_cover_count_aggregate_bytes_and_read_size_without_eviction() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let case = parent(&store, &a, 1).await;
    let id = CaseId::new();
    let one = CaseHistoryLimits {
        max_entries: 1,
        ..limits()
    };
    store
        .append_case_history(a.clone(), case.clone(), id.clone(), note("one"), one)
        .await
        .unwrap();
    assert!(
        store
            .append_case_history(a.clone(), case.clone(), CaseId::new(), note("two"), one)
            .await
            .is_err()
    );
    assert!(matches!(
        store
            .append_case_history(a.clone(), case.clone(), id.clone(), note("one"), one)
            .await
            .unwrap(),
        AppendCaseHistory::Existing(_)
    ));
    let bytes = serde_json::to_string(&note("one")).unwrap().len() as u64;
    let aggregate = CaseHistoryLimits {
        max_total_bytes: bytes,
        ..limits()
    };
    assert!(
        store
            .append_case_history(
                a.clone(),
                case.clone(),
                CaseId::new(),
                note("two"),
                aggregate
            )
            .await
            .is_err()
    );
    let tiny = CaseHistoryLimits {
        entry: CaseLimits {
            max_record_bytes: 1,
            ..CaseLimits::default()
        },
        ..limits()
    };
    assert!(
        store
            .case_history_entry(a.clone(), case.clone(), id.clone(), tiny)
            .await
            .is_err()
    );
    assert_eq!(
        store
            .case_history_ids(a.clone(), case.clone(), 0, 1, tiny)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .case_history_entry(a.clone(), case.clone(), id, limits())
            .await
            .unwrap(),
        Some(note("one"))
    );
    assert!(
        store
            .case_history_ids(a.clone(), case.clone(), -1, 1, limits())
            .await
            .is_err()
    );
    assert!(
        store
            .case_history_ids(a, case, 0, 101, limits())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn deletion_racing_append_erases_history_and_prevents_replay() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let case = parent(&store, &a, 1).await;
    let other = parent(&store, &a, 2).await;
    let id = CaseId::new();
    store
        .append_case_history(a.clone(), case.clone(), id.clone(), note("erase"), limits())
        .await
        .unwrap();
    store
        .append_case_history(a.clone(), other.clone(), id.clone(), note("keep"), limits())
        .await
        .unwrap();
    let (_, removed) = tokio::join!(
        store.append_case_history(
            a.clone(),
            case.clone(),
            CaseId::new(),
            note("race"),
            limits()
        ),
        store.remove_case(a.clone(), case.clone())
    );
    assert!(removed.unwrap());
    assert!(
        store
            .case_history_entry(a.clone(), case.clone(), id.clone(), limits())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .case_history_ids(a.clone(), case.clone(), 0, 10, limits())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .append_case_history(a.clone(), case.clone(), id.clone(), note("erase"), limits())
            .await
            .is_err()
    );
    assert_eq!(
        store
            .record_case(a.clone(), case.clone(), data(1), CaseLimits::default())
            .await
            .unwrap(),
        RecordCase::Removed(case.clone())
    );
    let sql_id = case.as_str().to_owned();
    store
        .db
        .with_conn(move |c| {
            assert_eq!(
                c.query_row(
                    "SELECT COUNT(*) FROM guild_case_history WHERE case_id=?1",
                    params![sql_id],
                    |r| Ok(r.get::<i64>(0)?)
                )?,
                0
            );
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        store
            .case_history_entry(a, other, id, limits())
            .await
            .unwrap(),
        Some(note("keep"))
    );
}

#[tokio::test]
async fn failed_child_removal_rolls_back_parent_and_closed_cleanup_cascades() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let b = scope(&f, &store, &reg, 20).await;
    let case = parent(&store, &a, 1).await;
    let other = parent(&store, &b, 1).await;
    let id = CaseId::new();
    for (s, c) in [(&a, &case), (&b, &other)] {
        store
            .append_case_history(s.clone(), c.clone(), id.clone(), note("retained"), limits())
            .await
            .unwrap();
    }
    store.db.with_conn(|c|c.execute_batch_bounded("CREATE TRIGGER reject_history_delete BEFORE DELETE ON guild_case_history BEGIN SELECT RAISE(ABORT,'test failed removal'); END", 64 * 1024 * 1024)).await.unwrap();
    assert!(store.remove_case(a.clone(), case.clone()).await.is_err());
    assert!(
        store
            .case(a.clone(), case.clone(), CaseLimits::default())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        store
            .case_history_entry(a.clone(), case.clone(), id.clone(), limits())
            .await
            .unwrap(),
        Some(note("retained"))
    );
    store.close_membership(a.clone()).await.unwrap();
    assert!(
        store
            .purge_closed_cases(a.clone(), 1, CaseLimits::default())
            .await
            .is_err()
    );
    assert!(
        store
            .case_history_ids(a.clone(), case.clone(), 0, 1, limits())
            .await
            .is_err()
    );
    store
        .db
        .with_conn(|c| {
            c.execute_batch_bounded("DROP TRIGGER reject_history_delete", 64 * 1024 * 1024)
        })
        .await
        .unwrap();
    assert_eq!(
        store
            .purge_closed_cases(a.clone(), 1, CaseLimits::default())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .purge_closed_cases(a, 1, CaseLimits::default())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .case_history_entry(b, other, id, limits())
            .await
            .unwrap(),
        Some(note("retained"))
    );
    store
        .db
        .with_conn(|c| {
            assert_eq!(
                c.query_row(
                    "SELECT COUNT(*) FROM guild_case_history WHERE guild_id=10",
                    (),
                    |r| Ok(r.get::<i64>(0)?)
                )?,
                0
            );
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn sql_guards_validation_and_missing_scopes_refuse_history_changes() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let case = parent(&store, &a, 1).await;
    for body in [
        CaseHistoryBody::Note { text: " ".into() },
        CaseHistoryBody::Correction {
            outcome: None,
            reason: None,
            explanation: "no change".into(),
        },
        CaseHistoryBody::Policy {
            rule_id: "".into(),
            revision: "rev".into(),
            description: "bad".into(),
        },
    ] {
        assert!(
            store
                .append_case_history(
                    a.clone(),
                    case.clone(),
                    CaseId::new(),
                    CaseHistoryData { body, ..note("") },
                    limits()
                )
                .await
                .is_err()
        );
    }
    let policy = CaseHistoryData {
        body: CaseHistoryBody::Policy {
            rule_id: "attachment_repeat".into(),
            revision: "rev1".into(),
            description: "3 channels in 60 seconds".into(),
        },
        ..note("")
    };
    let id = CaseId::new();
    store
        .append_case_history(
            a.clone(),
            case.clone(),
            id.clone(),
            policy.clone(),
            limits(),
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&policy).unwrap()["actor_id"],
        i64::MAX.to_string()
    );
    store.db.with_conn(|c|{
        c.execute_batch("PRAGMA foreign_keys=OFF; PRAGMA recursive_triggers=OFF")?;
        for sql in ["UPDATE guild_case_history SET payload='{}'","DELETE FROM guild_case_history","INSERT OR REPLACE INTO guild_case_history SELECT * FROM guild_case_history","INSERT INTO guild_case_history SELECT 20,enrollment_id,case_id,entry_id,sequence,payload FROM guild_case_history"] {assert!(c.execute_batch_bounded(sql, 64 * 1024 * 1024).is_err(),"accepted {sql}");}
        Ok(())
    }).await.unwrap();
    let wrong = GuildDataScope::new(
        f.placement(&reg, 10).await,
        GuildEnrollment {
            guild_id: 10,
            id: "cccccccccccccccccccccccccccccccc".into(),
            joined_at_ns: 100,
        },
    )
    .unwrap();
    assert!(
        store
            .case_history_ids(wrong.clone(), CaseId::new(), 0, 1, limits())
            .await
            .is_err()
    );
    assert!(
        store
            .case_history_entry(wrong.clone(), CaseId::new(), id.clone(), limits())
            .await
            .is_err()
    );
    assert!(
        store
            .append_case_history(wrong, case, id, policy, limits())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn version_three_upgrade_preserves_cases_and_removal_markers() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let p = f.placement(&reg, 10).await;
    let id = CaseId::new();
    let removed = CaseId::new();
    {
        let raw = Connection::open(&f.store_path).unwrap();
        for schema in [SCHEMA, TENANT_SCHEMA, CASE_SCHEMA] {
            raw.execute_batch_bounded(schema, 64 * 1024 * 1024).unwrap();
        }
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
        raw.execute_batch_bounded("INSERT INTO guild_memberships VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',100,1,'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb')", 64 * 1024 * 1024).unwrap();
        raw.execute_bounded("INSERT INTO guild_cases VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',?1,'discord:1',?2,0)",params![id.as_str(),serde_json::to_string(&data(1)).unwrap()], 64 * 1024 * 1024).unwrap();
        raw.execute_bounded("INSERT INTO guild_cases VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',?1,'discord:2',NULL,1)",params![removed.as_str()], 64 * 1024 * 1024).unwrap();
        raw.pragma_update(None, "user_version", 3).unwrap();
    }
    assert!(GuildDataStore::open(f.options(), reg.clone()).is_err());
    let store = GuildDataStore::initialize(f.options(), reg).unwrap();
    let a = GuildDataScope::new(
        p,
        GuildEnrollment {
            guild_id: 10,
            id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            joined_at_ns: 100,
        },
    )
    .unwrap();
    assert_eq!(
        store
            .case(a.clone(), id.clone(), CaseLimits::default())
            .await
            .unwrap(),
        Some(data(1))
    );
    assert!(
        store
            .case_history_ids(a.clone(), id.clone(), 0, 1, limits())
            .await
            .unwrap()
            .is_empty()
    );
    store
        .append_case_history(
            a.clone(),
            id,
            CaseId::new(),
            note("after upgrade"),
            limits(),
        )
        .await
        .unwrap();
    assert!(
        store
            .append_case_history(
                a.clone(),
                removed.clone(),
                CaseId::new(),
                note("refused"),
                limits()
            )
            .await
            .is_err()
    );
    assert_eq!(
        store
            .record_case(a, removed.clone(), data(2), CaseLimits::default())
            .await
            .unwrap(),
        RecordCase::Removed(removed)
    );
}
