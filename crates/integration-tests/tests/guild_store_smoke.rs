//! Run against a fresh disposable remote database, separate from the core smoke DB.
use jmodguard::db::guild_data::*;
use jmodguard::db::guild_stores::*;
use jmodguard::db::{Db, GuildEnrollment, test_drop_next_commit_response};

fn remote_options() -> GuildStoreOptions {
    GuildStoreOptions {
        path: "unused-remote-guild-store.db".into(),
        url: std::env::var("JMODGUARD_REMOTE_TEST_URL").expect("disposable empty libSQL URL"),
        token: std::env::var("JMODGUARD_REMOTE_TEST_TOKEN").expect("test token"),
        pool_size: 2,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires its own empty disposable libSQL server"]
async fn remote_store_identity_and_binding_survive_lost_commit_responses() {
    let path = std::env::temp_dir().join(format!(
        "guild_store_catalog_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let catalog = Db::open(&path, 2).unwrap();
    let registration = register_store(&catalog, StoreId::new(), StorePolicy::Shared)
        .await
        .unwrap();
    let placement = assign_initial_placement(&catalog, 10, registration.id().clone())
        .await
        .unwrap();
    assert!(GuildDataStore::open(remote_options(), registration.clone()).is_err());
    test_drop_next_commit_response();
    let store = GuildDataStore::initialize(remote_options(), registration.clone()).unwrap();
    assert!(store.verify_placement(placement.clone()).await.is_err());
    test_drop_next_commit_response();
    store
        .bind_initial_placement(placement.clone())
        .await
        .unwrap();
    store.verify_placement(placement.clone()).await.unwrap();
    let reopened = GuildDataStore::open(remote_options(), registration.clone()).unwrap();
    reopened.verify_placement(placement.clone()).await.unwrap();

    let second = assign_initial_placement(&catalog, 20, registration.id().clone())
        .await
        .unwrap();

    // Matching initialize/read/bind must not commit. The injected loss remains for
    // the actual second binding, whose normal receipt must establish success.
    test_drop_next_commit_response();
    GuildDataStore::initialize(remote_options(), registration.clone()).unwrap();
    reopened
        .bind_initial_placement(placement.clone())
        .await
        .unwrap();
    reopened.verify_placement(placement).await.unwrap();
    reopened
        .bind_initial_placement(second.clone())
        .await
        .unwrap();
    store.verify_placement(second).await.unwrap();
    let other = register_store(&catalog, StoreId::new(), StorePolicy::Shared)
        .await
        .unwrap();
    assert!(GuildDataStore::open(remote_options(), other.clone()).is_err());
    assert!(GuildDataStore::initialize(remote_options(), other.clone()).is_err());
    let wrong = assign_initial_placement(&catalog, 30, other.id().clone())
        .await
        .unwrap();
    assert!(store.bind_initial_placement(wrong.clone()).await.is_err());
    assert!(store.verify_placement(wrong).await.is_err());
    let scoped = GuildDataScope::new(
        guild_placement(&catalog, 10).await.unwrap().unwrap(),
        GuildEnrollment {
            guild_id: 10,
            id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            joined_at_ns: 100,
        },
    )
    .unwrap();
    assert!(store.log_channels(scoped.clone()).await.is_err());
    test_drop_next_commit_response();
    store.bind_initial_membership(scoped.clone()).await.unwrap();
    let snapshot = reopened.log_channels(scoped.clone()).await.unwrap();
    assert!(snapshot.channels().is_empty());
    test_drop_next_commit_response();
    // Matching initial binding and absent deletion must leave fault injection
    // for the real mapping mutation, without a no-op mutation receipt.
    reopened
        .bind_initial_membership(scoped.clone())
        .await
        .unwrap();
    assert_eq!(
        store
            .set_log_channel(scoped.clone(), snapshot.revision(), "system", None)
            .await
            .unwrap(),
        snapshot.revision()
    );
    let revision = store
        .set_log_channel(scoped.clone(), snapshot.revision(), "system", Some(12))
        .await
        .unwrap();
    let after = reopened.log_channels(scoped.clone()).await.unwrap();
    assert_eq!(after.revision(), revision);
    assert_eq!(after.channels(), &[("system".to_owned(), 12)]);
    assert!(
        reopened
            .set_log_channel(scoped.clone(), snapshot.revision(), "system", Some(13))
            .await
            .is_err()
    );
    let case_id = CaseId::new();
    let limits = CaseLimits::default();
    let facts = CaseData {
        source: CaseSource::DiscordAudit { entry_id: 123 },
        action: CaseAction::Ban,
        outcome: CaseOutcome::Uncertain,
        subject_id: 1234,
        actor_id: None,
        occurred_at_ms: 1000,
        reason: None,
        evidence: CaseEvidence::Unavailable,
    };
    test_drop_next_commit_response();
    assert_eq!(
        store
            .record_case(scoped.clone(), case_id.clone(), facts.clone(), limits)
            .await
            .unwrap(),
        RecordCase::Created(case_id.clone())
    );
    assert_eq!(
        reopened
            .case(scoped.clone(), case_id.clone(), limits)
            .await
            .unwrap(),
        Some(facts.clone())
    );
    let app_source = CaseSource::Application {
        operation_id: CaseId::new(),
    };
    let source_match = CaseSourceMatch {
        id: case_id.clone(),
        removed: false,
    };
    test_drop_next_commit_response();
    assert_eq!(
        store
            .link_case_source(
                scoped.clone(),
                case_id.clone(),
                facts.source.clone(),
                app_source.clone()
            )
            .await
            .unwrap(),
        LinkCaseSource::Linked(source_match.clone())
    );
    assert_eq!(
        reopened
            .case_for_source(scoped.clone(), app_source.clone())
            .await
            .unwrap(),
        Some(source_match.clone())
    );
    let evidence_limits = EvidenceLimits {
        max_items_per_case: 3,
        max_file_bytes: 10,
        max_case_bytes: 30,
        max_page_size: 2,
    };
    let evidence_id = CaseId::new();
    let evidence_request = EvidenceRequest {
        storage_id: CaseId::new(),
        bytes: 3,
        origin: EvidenceOrigin {
            channel_id: 1,
            message_id: 2,
            attachment_id: 3,
        },
    };
    test_drop_next_commit_response();
    assert!(
        store
            .reserve_case_evidence(
                scoped.clone(),
                case_id.clone(),
                evidence_id.clone(),
                evidence_request.clone(),
                evidence_limits
            )
            .await
            .unwrap()
            .created
    );
    test_drop_next_commit_response();
    assert!(
        !reopened
            .reserve_case_evidence(
                scoped.clone(),
                case_id.clone(),
                evidence_id.clone(),
                evidence_request,
                evidence_limits
            )
            .await
            .unwrap()
            .created
    );
    assert_eq!(
        reopened
            .evidence_jobs(
                scoped.clone(),
                EvidenceWork::Capture,
                None,
                1,
                evidence_limits
            )
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .settle_evidence_capture(
                scoped.clone(),
                case_id.clone(),
                evidence_id.clone(),
                EvidenceCapture::Stored {
                    bytes: 3,
                    sha256: "a".repeat(64)
                }
            )
            .await
            .unwrap(),
        EvidenceState::Available
    );
    assert!(
        reopened
            .available_case_evidence(scoped.clone(), case_id.clone(), evidence_id.clone())
            .await
            .unwrap()
            .is_some()
    );
    // Real local object I/O paired with the remote journal; a lost reservation
    // response must not cause a second upload or leave deletion unacknowledged.
    #[cfg(unix)]
    let local_evidence = {
        use jmodguard::evidence_transfer::{EvidenceTarget, EvidenceUpload, LocalEvidenceStore};
        let storage_id = CaseId::new();
        let root = std::env::temp_dir().join(format!("jmodguard-evidence-{}", storage_id.as_str()));
        let backend = LocalEvidenceStore::initialize(root.clone(), storage_id.clone(), 2).unwrap();
        let target = EvidenceTarget {
            scope: scoped.clone(),
            case_id: case_id.clone(),
            evidence_id: CaseId::new(),
        };
        test_drop_next_commit_response();
        assert_eq!(
            backend
                .capture(
                    store.clone(),
                    EvidenceUpload {
                        target: target.clone(),
                        request: EvidenceRequest {
                            storage_id,
                            bytes: 3,
                            origin: EvidenceOrigin {
                                channel_id: 1,
                                message_id: 2,
                                attachment_id: 4
                            },
                        },
                        limits: evidence_limits,
                    },
                    std::io::Cursor::new(b"abc".to_vec())
                )
                .await
                .unwrap(),
            EvidenceState::Available
        );
        (backend, target, root)
    };
    let history_limits = CaseHistoryLimits {
        entry: limits,
        max_entries: 10,
        max_total_bytes: 10000,
    };
    let history_id = CaseId::new();
    let history = CaseHistoryData {
        actor_id: None,
        recorded_at_ms: 1100,
        body: CaseHistoryBody::Correction {
            outcome: Some(CaseOutcome::Succeeded),
            reason: None,
            explanation: "later confirmation".into(),
        },
    };
    test_drop_next_commit_response();
    let history_ref = CaseHistoryRef {
        id: history_id.clone(),
        sequence: 1,
    };
    assert_eq!(
        store
            .append_case_history(
                scoped.clone(),
                case_id.clone(),
                history_id.clone(),
                history.clone(),
                history_limits
            )
            .await
            .unwrap(),
        AppendCaseHistory::Created(history_ref.clone())
    );
    assert_eq!(
        reopened
            .case_history_entry(
                scoped.clone(),
                case_id.clone(),
                history_id.clone(),
                history_limits
            )
            .await
            .unwrap(),
        Some(history.clone())
    );
    assert_eq!(
        reopened
            .case(scoped.clone(), case_id.clone(), limits)
            .await
            .unwrap(),
        Some(facts.clone())
    );
    test_drop_next_commit_response();
    // Reads and duplicate source/history retries leave the lost response for removal.
    assert_eq!(
        reopened
            .link_case_source(
                scoped.clone(),
                case_id.clone(),
                facts.source.clone(),
                app_source.clone()
            )
            .await
            .unwrap(),
        LinkCaseSource::Existing(source_match.clone())
    );
    assert_eq!(
        reopened
            .case_for_source(scoped.clone(), app_source.clone())
            .await
            .unwrap(),
        Some(source_match)
    );

    assert_eq!(
        reopened
            .append_case_history(
                scoped.clone(),
                case_id.clone(),
                history_id.clone(),
                history.clone(),
                history_limits
            )
            .await
            .unwrap(),
        AppendCaseHistory::Existing(history_ref.clone())
    );
    assert_eq!(
        reopened
            .case_history_ids(scoped.clone(), case_id.clone(), 0, 1, history_limits)
            .await
            .unwrap(),
        vec![history_ref]
    );
    assert_eq!(
        reopened
            .record_case(scoped.clone(), CaseId::new(), facts.clone(), limits)
            .await
            .unwrap(),
        RecordCase::Existing(case_id.clone())
    );
    assert_eq!(
        reopened
            .case_ids(scoped.clone(), None, 1, limits)
            .await
            .unwrap(),
        vec![case_id.clone()]
    );
    assert!(
        store
            .remove_case(scoped.clone(), case_id.clone())
            .await
            .unwrap()
    );
    assert_eq!(
        reopened
            .record_case(scoped.clone(), CaseId::new(), facts.clone(), limits)
            .await
            .unwrap(),
        RecordCase::Removed(case_id.clone())
    );
    assert!(
        reopened
            .case(scoped.clone(), case_id.clone(), limits)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        reopened
            .case_history_entry(
                scoped.clone(),
                case_id.clone(),
                history_id.clone(),
                history_limits
            )
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        reopened
            .append_case_history(
                scoped.clone(),
                case_id,
                history_id.clone(),
                history.clone(),
                history_limits
            )
            .await
            .is_err()
    );
    #[cfg(unix)]
    {
        let (backend, target, root) = local_evidence;
        test_drop_next_commit_response();
        assert!(
            backend
                .cleanup(store.clone(), target.clone())
                .await
                .unwrap()
        );
        assert!(!backend.cleanup(reopened.clone(), target).await.unwrap());
        assert!(!root.join("evidence").exists());
        drop(backend);
        std::fs::remove_dir_all(root).unwrap();
    }
    // Case removal hides metadata and retains the object until cleanup is acknowledged.
    let deletion = reopened
        .evidence_jobs(
            scoped.clone(),
            EvidenceWork::Delete,
            None,
            1,
            evidence_limits,
        )
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(deletion.id, evidence_id);
    assert!(
        reopened
            .available_case_evidence(
                scoped.clone(),
                deletion.case_id.clone(),
                evidence_id.clone()
            )
            .await
            .unwrap()
            .is_none()
    );
    test_drop_next_commit_response();
    assert!(
        store
            .confirm_evidence_deleted(scoped.clone(), deletion.case_id, evidence_id)
            .await
            .unwrap()
    );
    let alias_facts = CaseData {
        source: app_source.clone(),
        ..facts.clone()
    };
    let removed_match = reopened
        .case_for_source(scoped.clone(), app_source)
        .await
        .unwrap()
        .unwrap();
    assert!(removed_match.removed);
    assert_eq!(
        reopened
            .record_case(scoped.clone(), CaseId::new(), alias_facts, limits)
            .await
            .unwrap(),
        RecordCase::Removed(removed_match.id)
    );
    let live_case = CaseId::new();
    let live_facts = CaseData {
        source: CaseSource::DiscordAudit { entry_id: 124 },
        ..facts.clone()
    };
    store
        .record_case(scoped.clone(), live_case.clone(), live_facts, limits)
        .await
        .unwrap();
    store
        .append_case_history(
            scoped.clone(),
            live_case.clone(),
            history_id,
            history,
            history_limits,
        )
        .await
        .unwrap();
    let late_id = CaseId::new();
    let late_source = CaseSource::DiscordAudit { entry_id: 125 };
    let late_app = CaseSource::Application {
        operation_id: CaseId::new(),
    };
    store
        .record_case(
            scoped.clone(),
            late_id.clone(),
            CaseData {
                source: late_source.clone(),
                ..facts.clone()
            },
            limits,
        )
        .await
        .unwrap();
    store
        .remove_case(scoped.clone(), late_id.clone())
        .await
        .unwrap();
    test_drop_next_commit_response();
    assert_eq!(
        store
            .link_case_source(
                scoped.clone(),
                late_id.clone(),
                late_source,
                late_app.clone()
            )
            .await
            .unwrap(),
        LinkCaseSource::Linked(CaseSourceMatch {
            id: late_id.clone(),
            removed: true
        })
    );
    assert_eq!(
        reopened
            .case_for_source(scoped.clone(), late_app)
            .await
            .unwrap(),
        Some(CaseSourceMatch {
            id: late_id,
            removed: true
        })
    );
    let unfinished = CaseId::new();
    store
        .reserve_case_evidence(
            scoped.clone(),
            live_case.clone(),
            unfinished.clone(),
            EvidenceRequest {
                storage_id: CaseId::new(),
                bytes: 3,
                origin: EvidenceOrigin {
                    channel_id: 1,
                    message_id: 2,
                    attachment_id: 4,
                },
            },
            evidence_limits,
        )
        .await
        .unwrap();
    test_drop_next_commit_response();
    store.close_membership(scoped.clone()).await.unwrap();
    assert!(reopened.log_channels(scoped.clone()).await.is_err());
    assert!(
        reopened
            .set_log_channel(scoped.clone(), &revision, "system", Some(13))
            .await
            .is_err()
    );
    assert!(
        reopened
            .bind_initial_membership(scoped.clone())
            .await
            .is_err()
    );
    reopened.close_membership(scoped.clone()).await.unwrap();
    assert!(
        reopened
            .available_case_evidence(scoped.clone(), live_case.clone(), unfinished.clone())
            .await
            .is_err()
    );
    test_drop_next_commit_response();
    assert_eq!(
        store
            .cancel_closed_evidence(scoped.clone(), 1, evidence_limits)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        reopened
            .cancel_closed_evidence(scoped.clone(), 1, evidence_limits)
            .await
            .unwrap(),
        0
    );
    assert!(
        reopened
            .confirm_evidence_deleted(scoped.clone(), live_case.clone(), unfinished.clone())
            .await
            .is_err()
    );
    test_drop_next_commit_response();
    assert_eq!(
        store
            .settle_evidence_capture(
                scoped.clone(),
                live_case.clone(),
                unfinished.clone(),
                EvidenceCapture::Discarded
            )
            .await
            .unwrap(),
        EvidenceState::CleanupPending
    );
    test_drop_next_commit_response();
    assert!(
        store
            .confirm_evidence_deleted(scoped.clone(), live_case.clone(), unfinished.clone())
            .await
            .unwrap()
    );
    assert!(
        !reopened
            .confirm_evidence_deleted(scoped.clone(), live_case, unfinished)
            .await
            .unwrap()
    );

    test_drop_next_commit_response();
    assert_eq!(
        store
            .purge_closed_cases(scoped.clone(), 1, limits)
            .await
            .unwrap(),
        1
    );
    test_drop_next_commit_response();
    assert_eq!(
        reopened
            .purge_closed_cases(scoped.clone(), 1, limits)
            .await
            .unwrap(),
        1
    );
    test_drop_next_commit_response();
    assert_eq!(
        reopened
            .purge_closed_cases(scoped.clone(), 1, limits)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        reopened
            .purge_closed_cases(scoped, 1, limits)
            .await
            .unwrap(),
        0
    );
    drop((store, reopened, catalog));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}
