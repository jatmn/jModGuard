use super::cases::data;
use super::tenant_logs::scope;
use super::*;

#[tokio::test]
async fn bunny_attempts_are_single_use_scoped_and_guard_generic_settlement() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let b = scope(&f, &store, &reg, 20).await;
    let case = CaseId::new();
    let id = CaseId::new();
    let storage = CaseId::new();
    store
        .record_case(a.clone(), case.clone(), data(1), CaseLimits::default())
        .await
        .unwrap();
    let request = EvidenceRequest {
        storage_id: storage.clone(),
        bytes: 3,
        origin: EvidenceOrigin {
            channel_id: 1,
            message_id: 2,
            attachment_id: 3,
        },
    };
    let limits = EvidenceLimits {
        max_items_per_case: 3,
        max_file_bytes: 10,
        max_case_bytes: 30,
        max_page_size: 2,
    };
    store
        .reserve_case_evidence(a.clone(), case.clone(), id.clone(), request.clone(), limits)
        .await
        .unwrap();
    let binding = "https://storage.bunnycdn.com/evidence/".to_string();
    let (one, two) = tokio::join!(
        store.start_bunny_transfer(
            a.clone(),
            case.clone(),
            id.clone(),
            binding.clone(),
            "a".repeat(64)
        ),
        store.start_bunny_transfer(
            a.clone(),
            case.clone(),
            id.clone(),
            binding.clone(),
            "a".repeat(64)
        ),
    );
    assert_ne!(one.is_ok(), two.is_ok());
    assert!(
        store
            .bunny_transfer(b, case.clone(), id.clone())
            .await
            .is_err()
    );
    assert!(
        store
            .accept_bunny_transfer(a.clone(), CaseId::new(), id.clone())
            .await
            .is_err()
    );
    for result in [
        EvidenceCapture::Discarded,
        EvidenceCapture::Stored {
            bytes: 3,
            sha256: "a".repeat(64),
        },
    ] {
        assert!(
            store
                .settle_evidence_capture(a.clone(), case.clone(), id.clone(), result)
                .await
                .is_err()
        );
    }
    store
        .db
        .with_conn(|c| {
            c.execute_batch("PRAGMA foreign_keys=OFF; PRAGMA recursive_triggers=OFF;")?;
            for sql in [
                "DELETE FROM guild_bunny_evidence",
                "INSERT OR REPLACE INTO guild_bunny_evidence SELECT * FROM guild_bunny_evidence",
                "UPDATE guild_bunny_evidence SET binding='other'",
                "UPDATE guild_bunny_evidence SET sha256=NULL",
                "UPDATE guild_bunny_evidence SET sha256=printf('%064d',0)",
            ] {
                assert!(c.execute_batch(sql).is_err(), "accepted {sql}");
            }
            Ok(())
        })
        .await
        .unwrap();
    let other = CaseId::new();
    store
        .reserve_case_evidence(a.clone(), case.clone(), other.clone(), request, limits)
        .await
        .unwrap();
    assert!(
        store
            .start_bunny_transfer(
                a.clone(),
                case.clone(),
                other,
                binding + "different/",
                "a".repeat(64)
            )
            .await
            .is_err()
    );
    store.remove_case(a.clone(), case.clone()).await.unwrap();
    assert!(
        store
            .bunny_transfer(a.clone(), case.clone(), id.clone())
            .await
            .unwrap()
            .sha256
            .is_none()
    );
    store
        .accept_bunny_transfer(a.clone(), case.clone(), id.clone())
        .await
        .unwrap();
    let receipts = store
        .db
        .with_conn(|c| {
            c.query_row("SELECT COUNT(*) FROM mutation_receipts", (), |r| {
                Ok(r.get::<i64>(0)?)
            })
        })
        .await
        .unwrap();
    store
        .accept_bunny_transfer(a.clone(), case.clone(), id.clone())
        .await
        .unwrap();
    assert_eq!(
        store
            .db
            .with_conn(
                |c| c.query_row("SELECT COUNT(*) FROM mutation_receipts", (), |r| Ok(r
                    .get::<i64>(
                    0
                )?))
            )
            .await
            .unwrap(),
        receipts
    );
    assert_eq!(
        store
            .settle_evidence_capture(a, case, id, EvidenceCapture::Discarded)
            .await
            .unwrap(),
        EvidenceState::CleanupPending
    );
}

#[tokio::test]
async fn version_six_upgrade_preserves_ownership_and_rolls_back_failed_extension() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let p = f.placement(&reg, 10).await;
    let case = CaseId::new();
    let evidence = CaseId::new();
    let storage = CaseId::new();
    let raw = Connection::open(&f.store_path).unwrap();
    for schema in [
        SCHEMA,
        TENANT_SCHEMA,
        CASE_SCHEMA,
        HISTORY_SCHEMA,
        SOURCE_SCHEMA,
        EVIDENCE_SCHEMA,
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
    raw.execute(
        "INSERT INTO guild_cases VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',?1,'discord:1',?2,0)",
        params![case.as_str(), serde_json::to_string(&data(1)).unwrap()],
    )
    .unwrap();
    raw.execute("INSERT INTO guild_case_evidence VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',?1,?2,?3,3,0,0,1,2,3,NULL)",params![case.as_str(),evidence.as_str(),storage.as_str()]).unwrap();
    raw.pragma_update(None, "user_version", 6).unwrap();
    raw.execute_batch("CREATE INDEX guild_bunny_resource ON guild_case_evidence(storage_id)")
        .unwrap();
    assert!(GuildDataStore::initialize(f.options(), reg.clone()).is_err());
    assert_eq!(
        raw.query_row("PRAGMA user_version", (), |r| Ok(r.get::<i64>(0)?))
            .unwrap(),
        6
    );
    assert!(
        raw.query_row("SELECT COUNT(*) FROM guild_bunny_evidence", (), |r| Ok(
            r.get::<i64>(0)?
        ))
        .is_err()
    );
    raw.execute_batch("DROP INDEX guild_bunny_resource")
        .unwrap();
    assert!(GuildDataStore::open(f.options(), reg.clone()).is_err());
    let store = GuildDataStore::initialize(f.options(), reg).unwrap();
    assert_eq!(
        raw.query_row("PRAGMA user_version", (), |r| Ok(r.get::<i64>(0)?))
            .unwrap(),
        VERSION
    );
    assert_eq!(
        raw.query_row("SELECT COUNT(*) FROM guild_bunny_evidence", (), |r| Ok(
            r.get::<i64>(0)?
        ))
        .unwrap(),
        0
    );
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
            .evidence_transfer_job(a, case, evidence)
            .await
            .unwrap()
            .state,
        EvidenceState::Capturing
    );
}
