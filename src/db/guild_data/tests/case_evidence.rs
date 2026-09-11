use super::cases::data;
use super::tenant_logs::scope;
use super::*;

fn limits() -> EvidenceLimits {
    EvidenceLimits {
        max_items_per_case: 3,
        max_file_bytes: 100,
        max_case_bytes: 200,
        max_page_size: 2,
    }
}
fn request() -> EvidenceRequest {
    EvidenceRequest {
        storage_id: CaseId::new(),
        bytes: 3,
        origin: EvidenceOrigin {
            channel_id: 11,
            message_id: 12,
            attachment_id: 13,
        },
    }
}
fn stored() -> EvidenceCapture {
    EvidenceCapture::Stored {
        bytes: 3,
        sha256: "a".repeat(64),
    }
}
async fn parent(store: &GuildDataStore, a: &GuildDataScope, n: i64) -> CaseId {
    let id = CaseId::new();
    store
        .record_case(a.clone(), id.clone(), data(n), CaseLimits::default())
        .await
        .unwrap();
    id
}

#[tokio::test]
async fn reservation_ready_read_and_retries_preserve_one_owned_object() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let case = parent(&store, &a, 1).await;
    let id = CaseId::new();
    let req = request();
    let (one, two) = tokio::join!(
        store.reserve_case_evidence(a.clone(), case.clone(), id.clone(), req.clone(), limits()),
        store.reserve_case_evidence(a.clone(), case.clone(), id.clone(), req.clone(), limits())
    );
    let (one, two) = (one.unwrap(), two.unwrap());
    assert_ne!(one.created, two.created);
    assert_eq!(one.job, two.job);
    assert_eq!(
        one.job.object_key,
        format!(
            "evidence/10/{}/{}/{}",
            a.enrollment.id,
            case.as_str(),
            id.as_str()
        )
    );
    assert!(
        store
            .available_case_evidence(a.clone(), case.clone(), id.clone())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .confirm_evidence_deleted(a.clone(), case.clone(), id.clone())
            .await
            .is_err()
    );
    assert_eq!(
        store
            .settle_evidence_capture(a.clone(), case.clone(), id.clone(), stored())
            .await
            .unwrap(),
        EvidenceState::Available
    );
    let reopened = GuildDataStore::open(f.options(), reg).unwrap();
    let visible = reopened
        .available_case_evidence(a.clone(), case.clone(), id.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(visible.origin, req.origin);
    assert_eq!(visible.sha256, "a".repeat(64));
    assert_eq!(visible.job.storage_id, req.storage_id);
    assert!(
        store
            .settle_evidence_capture(
                a.clone(),
                case.clone(),
                id.clone(),
                EvidenceCapture::Discarded
            )
            .await
            .is_err()
    );
    assert!(
        store
            .settle_evidence_capture(
                a.clone(),
                case.clone(),
                id.clone(),
                EvidenceCapture::Stored {
                    bytes: 3,
                    sha256: "b".repeat(64)
                }
            )
            .await
            .is_err()
    );
    store.db.with_conn(|c|{c.execute_batch("CREATE TRIGGER no_receipt BEFORE INSERT ON mutation_receipts BEGIN SELECT RAISE(ABORT,'unexpected receipt'); END")?;Ok(())}).await.unwrap();
    assert!(
        !store
            .reserve_case_evidence(a.clone(), case.clone(), id.clone(), req, limits())
            .await
            .unwrap()
            .created
    );
    assert_eq!(
        store
            .settle_evidence_capture(a.clone(), case.clone(), id, stored())
            .await
            .unwrap(),
        EvidenceState::Available
    );
    assert!(
        store
            .evidence_jobs(a, EvidenceWork::Capture, None, 1, limits())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn removed_inflight_upload_must_settle_before_delete_and_parent_purge() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let case = parent(&store, &a, 1).await;
    let id = CaseId::new();
    let req = request();
    store
        .reserve_case_evidence(a.clone(), case.clone(), id.clone(), req.clone(), limits())
        .await
        .unwrap();
    store.remove_case(a.clone(), case.clone()).await.unwrap();
    let job = store
        .evidence_jobs(a.clone(), EvidenceWork::Capture, None, 1, limits())
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert!(job.cancelled);
    assert!(
        store
            .evidence_jobs(a.clone(), EvidenceWork::Delete, None, 1, limits())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .confirm_evidence_deleted(a.clone(), case.clone(), id.clone())
            .await
            .is_err()
    );
    assert!(
        store
            .reserve_case_evidence(a.clone(), case.clone(), id.clone(), req, limits())
            .await
            .is_err()
    );
    store.close_membership(a.clone()).await.unwrap();
    assert!(
        store
            .purge_closed_cases(a.clone(), 1, CaseLimits::default())
            .await
            .is_err()
    );
    // Simulate an upload that lands after SQL removal. This is a local ordering
    // regression, not a provider adapter or remote durability proof.
    let object = path("late_evidence_object");
    std::fs::write(&object, b"abc").unwrap();
    assert_eq!(
        store
            .settle_evidence_capture(a.clone(), case.clone(), id.clone(), stored())
            .await
            .unwrap(),
        EvidenceState::CleanupPending
    );
    assert!(
        store
            .purge_closed_cases(a.clone(), 1, CaseLimits::default())
            .await
            .is_err()
    );
    std::fs::remove_file(&object).unwrap();
    assert!(!object.exists());
    assert!(
        store
            .confirm_evidence_deleted(a.clone(), case.clone(), id.clone())
            .await
            .unwrap()
    );
    assert!(
        !store
            .confirm_evidence_deleted(a.clone(), case.clone(), id.clone())
            .await
            .unwrap()
    );
    assert_eq!(
        store
            .settle_evidence_capture(a.clone(), case.clone(), id, stored())
            .await
            .unwrap(),
        EvidenceState::Erased
    );
    assert_eq!(
        store
            .purge_closed_cases(a, 1, CaseLimits::default())
            .await
            .unwrap(),
        1
    );
    store
        .db
        .with_conn(|c| {
            assert_eq!(
                c.query_row("SELECT COUNT(*) FROM guild_case_evidence", (), |r| Ok(
                    r.get::<i64>(0)?
                ))?,
                0
            );
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn closure_preparation_is_bounded_and_keeps_neighbor_evidence() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let b = scope(&f, &store, &reg, 20).await;
    let case = parent(&store, &a, 1).await;
    let other = parent(&store, &b, 1).await;
    let mut ids = Vec::new();
    for _ in 0..3 {
        let id = CaseId::new();
        store
            .reserve_case_evidence(a.clone(), case.clone(), id.clone(), request(), limits())
            .await
            .unwrap();
        store
            .settle_evidence_capture(a.clone(), case.clone(), id.clone(), stored())
            .await
            .unwrap();
        ids.push(id);
    }
    let neighbor = CaseId::new();
    store
        .reserve_case_evidence(
            b.clone(),
            other.clone(),
            neighbor.clone(),
            request(),
            limits(),
        )
        .await
        .unwrap();
    store
        .settle_evidence_capture(b.clone(), other.clone(), neighbor.clone(), stored())
        .await
        .unwrap();
    assert!(
        store
            .cancel_closed_evidence(a.clone(), 1, limits())
            .await
            .is_err()
    );
    store.close_membership(a.clone()).await.unwrap();
    assert!(
        store
            .available_case_evidence(a.clone(), case.clone(), ids[0].clone())
            .await
            .is_err()
    );
    assert_eq!(
        store
            .cancel_closed_evidence(a.clone(), 1, limits())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .evidence_jobs(a.clone(), EvidenceWork::Delete, None, 2, limits())
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .cancel_closed_evidence(a.clone(), 2, limits())
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .cancel_closed_evidence(a.clone(), 2, limits())
            .await
            .unwrap(),
        0
    );
    let first = store
        .evidence_jobs(a.clone(), EvidenceWork::Delete, None, 2, limits())
        .await
        .unwrap();
    assert_eq!(first.len(), 2);
    let last = store
        .evidence_jobs(
            a.clone(),
            EvidenceWork::Delete,
            Some(first[1].id.clone()),
            2,
            limits(),
        )
        .await
        .unwrap();
    assert_eq!(last.len(), 1);
    assert!(first[1].id.as_str() < last[0].id.as_str());
    for id in ids {
        store
            .confirm_evidence_deleted(a.clone(), case.clone(), id)
            .await
            .unwrap();
    }
    assert_eq!(
        store
            .purge_closed_cases(a, 1, CaseLimits::default())
            .await
            .unwrap(),
        1
    );
    assert!(
        store
            .available_case_evidence(b, other, neighbor)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn budgets_and_identity_reject_churn_and_cross_case_reuse() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let case = parent(&store, &a, 1).await;
    let other = parent(&store, &a, 2).await;
    let id = CaseId::new();
    let req = request();
    let cap = EvidenceLimits {
        max_items_per_case: 1,
        max_case_bytes: 3,
        ..limits()
    };
    store
        .reserve_case_evidence(a.clone(), case.clone(), id.clone(), req.clone(), cap)
        .await
        .unwrap();
    for changed in [
        EvidenceRequest {
            storage_id: CaseId::new(),
            ..req.clone()
        },
        EvidenceRequest {
            bytes: 4,
            ..req.clone()
        },
        EvidenceRequest {
            origin: EvidenceOrigin {
                attachment_id: 14,
                ..req.origin.clone()
            },
            ..req.clone()
        },
    ] {
        assert!(
            store
                .reserve_case_evidence(a.clone(), case.clone(), id.clone(), changed, limits())
                .await
                .is_err()
        );
    }
    assert!(
        store
            .reserve_case_evidence(a.clone(), other, id.clone(), req.clone(), limits())
            .await
            .is_err()
    );
    for cap in [
        cap,
        EvidenceLimits {
            max_case_bytes: 5,
            ..limits()
        },
    ] {
        assert!(
            store
                .reserve_case_evidence(a.clone(), case.clone(), CaseId::new(), request(), cap)
                .await
                .is_err()
        );
    }
    assert_eq!(
        store
            .settle_evidence_capture(
                a.clone(),
                case.clone(),
                id.clone(),
                EvidenceCapture::Discarded
            )
            .await
            .unwrap(),
        EvidenceState::CleanupPending
    );
    store
        .confirm_evidence_deleted(a.clone(), case.clone(), id.clone())
        .await
        .unwrap();
    assert_eq!(
        store
            .reserve_case_evidence(a.clone(), case.clone(), id, req, cap)
            .await
            .unwrap()
            .job
            .state,
        EvidenceState::Erased
    );
    assert!(
        store
            .reserve_case_evidence(a.clone(), case.clone(), CaseId::new(), request(), cap)
            .await
            .is_err(),
        "erased identities still occupy admission slots"
    );
    assert!(
        store
            .reserve_case_evidence(
                a.clone(),
                case,
                CaseId::new(),
                EvidenceRequest {
                    bytes: u64::MAX,
                    ..request()
                },
                limits()
            )
            .await
            .is_err()
    );
    assert!(
        store
            .evidence_jobs(a, EvidenceWork::Delete, None, 3, limits())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn removal_racing_settlement_always_hides_evidence_and_schedules_cleanup() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let case = parent(&store, &a, 1).await;
    let id = CaseId::new();
    store
        .reserve_case_evidence(a.clone(), case.clone(), id.clone(), request(), limits())
        .await
        .unwrap();
    let (settled, removed) = tokio::join!(
        store.settle_evidence_capture(a.clone(), case.clone(), id.clone(), stored()),
        store.remove_case(a.clone(), case.clone())
    );
    assert!(matches!(
        settled.unwrap(),
        EvidenceState::Available | EvidenceState::CleanupPending
    ));
    assert!(removed.unwrap());
    assert!(
        store
            .available_case_evidence(a.clone(), case, id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .evidence_jobs(a, EvidenceWork::Delete, None, 1, limits())
            .await
            .unwrap()
            .len(),
        1
    );
    store.db.with_conn(|c|{assert_eq!(c.query_row("SELECT COUNT(*) FROM guild_case_evidence WHERE channel_id IS NOT NULL OR message_id IS NOT NULL OR attachment_id IS NOT NULL OR sha256 IS NOT NULL",(),|r|Ok(r.get::<i64>(0)?))?,0);Ok(())}).await.unwrap();
}

#[tokio::test]
async fn sql_guards_and_failed_cancellation_preserve_parent_and_metadata() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let case = parent(&store, &a, 1).await;
    let id = CaseId::new();
    store
        .reserve_case_evidence(a.clone(), case.clone(), id.clone(), request(), limits())
        .await
        .unwrap();
    store.db.with_conn(|c|{
        c.execute_batch("PRAGMA foreign_keys=OFF; PRAGMA recursive_triggers=OFF")?;
        for sql in ["DELETE FROM guild_case_evidence","INSERT OR REPLACE INTO guild_case_evidence SELECT * FROM guild_case_evidence","UPDATE guild_case_evidence SET reserved_bytes=5","UPDATE guild_case_evidence SET channel_id=99","UPDATE guild_case_evidence SET state=3,cancelled=1,channel_id=NULL,message_id=NULL,attachment_id=NULL","INSERT INTO guild_case_evidence SELECT 20,enrollment_id,case_id,evidence_id,storage_id,reserved_bytes,state,cancelled,channel_id,message_id,attachment_id,sha256 FROM guild_case_evidence"] {assert!(c.execute_batch(sql).is_err(),"accepted {sql}");}
        c.execute_batch("CREATE TRIGGER refuse_cancel BEFORE UPDATE ON guild_case_evidence BEGIN SELECT RAISE(ABORT,'keep'); END")?;Ok(())
    }).await.unwrap();
    assert!(store.remove_case(a.clone(), case.clone()).await.is_err());
    assert!(
        store
            .case(a.clone(), case.clone(), CaseLimits::default())
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        !store
            .evidence_jobs(a.clone(), EvidenceWork::Capture, None, 1, limits())
            .await
            .unwrap()[0]
            .cancelled
    );
    store
        .db
        .with_conn(|c| {
            c.execute_batch("DROP TRIGGER refuse_cancel")?;
            Ok(())
        })
        .await
        .unwrap();
    store
        .settle_evidence_capture(a.clone(), case.clone(), id.clone(), stored())
        .await
        .unwrap();
    store.remove_case(a.clone(), case.clone()).await.unwrap();
    store
        .confirm_evidence_deleted(a.clone(), case.clone(), id)
        .await
        .unwrap();
    store.close_membership(a.clone()).await.unwrap();
    store.db.with_conn(|c|{c.execute_batch("CREATE TRIGGER refuse_purge BEFORE DELETE ON guild_case_evidence BEGIN SELECT RAISE(ABORT,'keep'); END")?;Ok(())}).await.unwrap();
    assert!(
        store
            .purge_closed_cases(a.clone(), 1, CaseLimits::default())
            .await
            .is_err()
    );
    store
        .db
        .with_conn(|c| {
            assert_eq!(
                c.query_row("SELECT COUNT(*) FROM guild_cases", (), |r| Ok(
                    r.get::<i64>(0)?
                ))?,
                1
            );
            c.execute_batch("DROP TRIGGER refuse_purge")?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        store
            .purge_closed_cases(a, 1, CaseLimits::default())
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn empty_and_completion_paths_require_exact_scope_and_valid_inputs() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let b = scope(&f, &store, &reg, 20).await;
    let case = parent(&store, &a, 1).await;
    let id = CaseId::new();
    store
        .reserve_case_evidence(a.clone(), case.clone(), id.clone(), request(), limits())
        .await
        .unwrap();
    let wrong = GuildDataScope::new(
        a.placement.clone(),
        GuildEnrollment {
            id: "cccccccccccccccccccccccccccccccc".into(),
            ..a.enrollment.clone()
        },
    )
    .unwrap();
    assert!(
        store
            .evidence_jobs(wrong.clone(), EvidenceWork::Delete, None, 1, limits())
            .await
            .is_err()
    );
    assert!(
        store
            .cancel_closed_evidence(wrong.clone(), 1, limits())
            .await
            .is_err()
    );
    assert!(
        store
            .available_case_evidence(wrong, CaseId::new(), CaseId::new())
            .await
            .is_err()
    );
    assert!(
        store
            .evidence_jobs(b.clone(), EvidenceWork::Capture, None, 1, limits())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .settle_evidence_capture(b.clone(), case.clone(), id.clone(), stored())
            .await
            .is_err()
    );
    assert!(
        store
            .confirm_evidence_deleted(b, case.clone(), id.clone())
            .await
            .is_err()
    );
    for result in [
        EvidenceCapture::Stored {
            bytes: 4,
            sha256: "a".repeat(64),
        },
        EvidenceCapture::Stored {
            bytes: 3,
            sha256: "../bad".into(),
        },
    ] {
        assert!(
            store
                .settle_evidence_capture(a.clone(), case.clone(), id.clone(), result)
                .await
                .is_err()
        );
    }
    assert_eq!(
        store
            .evidence_jobs(a, EvidenceWork::Capture, None, 1, limits())
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn version_five_upgrade_preserves_cases_without_inventing_files() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let p = f.placement(&reg, 10).await;
    let id = CaseId::new();
    {
        let raw = Connection::open(&f.store_path).unwrap();
        for schema in [
            SCHEMA,
            TENANT_SCHEMA,
            CASE_SCHEMA,
            HISTORY_SCHEMA,
            SOURCE_SCHEMA,
        ] {
            raw.execute_batch(schema).unwrap();
        }
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
        raw.execute_batch("INSERT INTO guild_memberships VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',100,1,'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb')").unwrap();
        raw.execute("INSERT INTO guild_cases VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',?1,'discord:1',?2,0)",params![id.as_str(),serde_json::to_string(&data(1)).unwrap()]).unwrap();
        raw.pragma_update(None, "user_version", 5).unwrap();
    }
    assert!(GuildDataStore::open(f.options(), reg.clone()).is_err());
    let foreign = f.registration(StoreId::new(), StorePolicy::Shared).await;
    assert!(GuildDataStore::initialize(f.options(), foreign).is_err());
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
            .evidence_jobs(a.clone(), EvidenceWork::Capture, None, 1, limits())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .reserve_case_evidence(a, id, CaseId::new(), request(), limits())
            .await
            .unwrap()
            .created
    );
}
