use super::*;
use crate::config::{DbConfig, EvidenceConfig, GuildStoreConfig, GuildStoresConfig};
use crate::db::guild_data::{CaseAction, CaseData, CaseEvidence, CaseId, CaseOutcome, CaseSource};
use crate::db::guild_stores::{assign_initial_placement, guild_placement};
use crate::db::{self, Db, SqlExecutor};

struct Fixture {
    db: Db,
    path: PathBuf,
    tenant: TenantRuntime,
    extra_stores: Vec<PathBuf>,
}
impl Fixture {
    fn new() -> Self {
        Self::with_stores(
            GuildStoresConfig::default(),
            EvidenceConfig {
                backend: "none".into(),
                ..EvidenceConfig::default()
            },
        )
    }
    fn with_stores(stores: GuildStoresConfig, evidence: EvidenceConfig) -> Self {
        let extra_stores = stores
            .stores
            .iter()
            .map(|store| store.path.clone())
            .collect();
        let path = std::env::temp_dir().join(format!(
            "jmodguard_tenant_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = Db::open(&path, 2).unwrap();
        let db_cfg = DbConfig {
            path: path.clone(),
            pool_size: 2,
            ..DbConfig::default()
        };
        let tenant = TenantRuntime::open(&db, &db_cfg, &stores, &evidence).unwrap();
        Self {
            db,
            path,
            tenant,
            extra_stores,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
        }
        remove_sidecars(&self.path);
        for path in &self.extra_stores {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
            }
        }
    }
}

async fn enroll(f: &Fixture, guild: i64, joined: i64) -> crate::db::GuildEnrollment {
    let enrollment = db::enroll_guild(&f.db, guild, joined)
        .await
        .unwrap()
        .unwrap();
    f.tenant
        .activate(enrollment.clone())
        .await
        .unwrap()
        .unwrap();
    enrollment
}

fn case_data(subject: i64) -> CaseData {
    CaseData {
        source: CaseSource::Application {
            operation_id: CaseId::new(),
        },
        action: CaseAction::Warning,
        outcome: CaseOutcome::Succeeded,
        subject_id: subject,
        actor_id: None,
        occurred_at_ms: 1,
        reason: Some("test".into()),
        evidence: CaseEvidence::Unavailable,
    }
}

#[tokio::test]
async fn shared_and_dedicated_guilds_keep_independent_cases() {
    let dedicated = std::env::temp_dir().join(format!(
        "jmodguard_tenant_{:032x}.dedicated.db",
        rand::random::<u128>()
    ));
    let stores = GuildStoresConfig {
        max_handles: 4,
        stores: vec![GuildStoreConfig {
            dedicated_guild_id: 20,
            path: dedicated.clone(),
            ..GuildStoreConfig::default()
        }],
    };
    let f = Fixture::with_stores(
        stores,
        EvidenceConfig {
            backend: "none".into(),
            ..EvidenceConfig::default()
        },
    );
    let shared = enroll(&f, 10, 100).await;
    let owned = enroll(&f, 20, 100).await;
    f.tenant
        .record_selected_case(shared.clone(), case_data(1))
        .await
        .unwrap();
    f.tenant
        .record_selected_case(owned.clone(), case_data(2))
        .await
        .unwrap();
    let (shared_store, shared_scope) = f.tenant.live(shared.clone()).await.unwrap();
    let (owned_store, owned_scope) = f.tenant.live(owned.clone()).await.unwrap();
    assert_eq!(
        shared_store
            .case_count(shared_scope.clone(), case_limits())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        owned_store
            .case_count(owned_scope.clone(), case_limits())
            .await
            .unwrap(),
        1
    );
    let shared_ids = shared_store
        .case_ids(shared_scope, None, 10, case_limits())
        .await
        .unwrap();
    assert!(
        owned_store
            .case(owned_scope, shared_ids[0].clone(), case_limits())
            .await
            .unwrap()
            .is_none()
            || owned_store
                .case(
                    f.tenant.live(owned).await.unwrap().1,
                    shared_ids[0].clone(),
                    case_limits()
                )
                .await
                .is_err()
    );
    let _ = std::fs::remove_file(&dedicated);
}

#[tokio::test]
async fn stale_membership_does_not_read_or_write() {
    let f = Fixture::new();
    let live = enroll(&f, 10, 100).await;
    db::retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
    f.tenant.schedule_erasure(live.clone()).await.unwrap();
    f.tenant.drive_erasure().await.unwrap();
    assert!(
        f.tenant
            .record_selected_case(live.clone(), case_data(1))
            .await
            .unwrap()
            .is_none()
    );
    let next = enroll(&f, 10, 101).await;
    assert_ne!(next.id, live.id);
    f.tenant
        .record_selected_case(next.clone(), case_data(3))
        .await
        .unwrap();
    assert!(
        f.tenant
            .record_selected_case(live, case_data(4))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn erasure_clears_owned_rows_and_queues_backup_intent() {
    let f = Fixture::new();
    let enrollment = enroll(&f, 10, 100).await;
    f.tenant
        .record_selected_case(enrollment.clone(), case_data(9))
        .await
        .unwrap();
    db::patch_settings(&f.db, 10, Some(Some("?".into())), None, None)
        .await
        .unwrap();
    db::retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
    f.tenant.schedule_erasure(enrollment.clone()).await.unwrap();
    assert!(f.tenant.drive_erasure().await.unwrap());
    assert!(guild_placement(&f.db, 10).await.unwrap().is_none());
    assert_eq!(db::get_settings(&f.db, 10).await.unwrap().prefix, "!");
    assert!(
        f.tenant
            .record_selected_case(enrollment, case_data(9))
            .await
            .unwrap()
            .is_none()
    );
    let intents: i64 =
        f.db.with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM backup_cleanup_intents WHERE kind='guild' AND guild_id=10",
                (),
                |r| Ok(r.get(0)?),
            )
        })
        .await
        .unwrap();
    assert_eq!(intents, 1);
}

#[tokio::test]
async fn stale_erasure_does_not_wipe_a_rejoined_guilds_directory_data() {
    let f = Fixture::new();
    let departed = enroll(&f, 10, 100).await;
    f.tenant
        .record_selected_case(departed.clone(), case_data(9))
        .await
        .unwrap();
    db::retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
    f.tenant.schedule_erasure(departed).await.unwrap();

    let rejoined = db::enroll_guild(&f.db, 10, 101).await.unwrap().unwrap();
    assert!(
        f.tenant.activate(rejoined.clone()).await.unwrap().is_none(),
        "new tenant activation waits for the departed scope to be erased"
    );
    db::patch_settings(&f.db, 10, Some(Some("?".into())), None, None)
        .await
        .unwrap();

    assert!(f.tenant.drive_erasure().await.unwrap());
    assert_eq!(db::get_settings(&f.db, 10).await.unwrap().prefix, "?");
    assert!(guild_placement(&f.db, 10).await.unwrap().is_none());
    assert!(
        f.tenant.activate(rejoined).await.unwrap().is_some(),
        "the current membership can bind after stale erasure completes"
    );
}

#[tokio::test]
async fn offline_store_does_not_record_or_disclose() {
    let f = Fixture::new();
    let enrollment = db::enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let ghost = crate::db::guild_stores::StoreId::new();
    crate::db::guild_stores::register_store(
        &f.db,
        ghost.clone(),
        crate::db::guild_stores::StorePolicy::Shared,
    )
    .await
    .unwrap();
    crate::db::guild_stores::assign_initial_placement(&f.db, 10, ghost)
        .await
        .unwrap();
    let error = match f.tenant.live(enrollment.clone()).await {
        Ok(_) => panic!("offline store must not yield a live handle"),
        Err(error) => error,
    };
    assert!(matches!(
        error.downcast_ref::<TenantError>(),
        Some(TenantError::Offline)
    ));
    assert!(
        f.tenant
            .record_selected_case(enrollment, case_data(1))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn handle_cache_stays_within_configured_bound() {
    let dedicated: Vec<_> = (0..3)
        .map(|_| {
            std::env::temp_dir().join(format!(
                "jmodguard_tenant_{:032x}.dedicated.db",
                rand::random::<u128>()
            ))
        })
        .collect();
    let stores = GuildStoresConfig {
        max_handles: 2,
        stores: dedicated
            .iter()
            .enumerate()
            .map(|(index, path)| GuildStoreConfig {
                dedicated_guild_id: 20 + index as i64,
                path: path.clone(),
                ..GuildStoreConfig::default()
            })
            .collect(),
    };
    let f = Fixture::with_stores(
        stores,
        EvidenceConfig {
            backend: "none".into(),
            ..EvidenceConfig::default()
        },
    );
    for guild in [10, 20, 21, 22] {
        enroll(&f, guild, 100).await;
    }
    assert!(f.tenant.cached_handle_count().await <= 2);
    let shared = db::guild_enrollment(&f.db, 10).await.unwrap().unwrap();
    f.tenant
        .record_selected_case(shared, case_data(1))
        .await
        .unwrap();
    for path in dedicated {
        let _ = std::fs::remove_file(path);
    }
}

#[tokio::test]
async fn empty_store_ids_stay_stable_across_runtime_reopen() {
    let shared = std::env::temp_dir().join(format!(
        "jmodguard_tenant_{:032x}.explicit.db",
        rand::random::<u128>()
    ));
    let dedicated = std::env::temp_dir().join(format!(
        "jmodguard_tenant_{:032x}.dedicated.db",
        rand::random::<u128>()
    ));
    let stores = GuildStoresConfig {
        max_handles: 4,
        stores: vec![
            GuildStoreConfig {
                path: shared.clone(),
                ..GuildStoreConfig::default()
            },
            GuildStoreConfig {
                dedicated_guild_id: 20,
                path: dedicated.clone(),
                ..GuildStoreConfig::default()
            },
        ],
    };
    let f = Fixture::with_stores(
        stores.clone(),
        EvidenceConfig {
            backend: "none".into(),
            ..EvidenceConfig::default()
        },
    );
    let shared_enrollment = enroll(&f, 10, 100).await;
    let dedicated_enrollment = enroll(&f, 20, 100).await;
    let shared_placement = guild_placement(&f.db, 10).await.unwrap().unwrap();
    let dedicated_placement = guild_placement(&f.db, 20).await.unwrap().unwrap();
    f.tenant
        .record_selected_case(shared_enrollment.clone(), case_data(1))
        .await
        .unwrap()
        .unwrap();
    f.tenant
        .record_selected_case(dedicated_enrollment.clone(), case_data(2))
        .await
        .unwrap()
        .unwrap();

    let db_cfg = DbConfig {
        path: f.path.clone(),
        pool_size: 2,
        ..DbConfig::default()
    };
    let reopened = TenantRuntime::open(
        &f.db,
        &db_cfg,
        &stores,
        &EvidenceConfig {
            backend: "none".into(),
            ..EvidenceConfig::default()
        },
    )
    .unwrap();
    let shared_again = guild_placement(&f.db, 10).await.unwrap().unwrap();
    let dedicated_again = guild_placement(&f.db, 20).await.unwrap().unwrap();
    assert_eq!(shared_placement.store_id(), shared_again.store_id());
    assert_eq!(dedicated_placement.store_id(), dedicated_again.store_id());
    assert_ne!(shared_again.store_id(), dedicated_again.store_id());
    reopened.live(shared_enrollment.clone()).await.unwrap();
    reopened.live(dedicated_enrollment.clone()).await.unwrap();
    assert!(
        reopened
            .record_selected_case(shared_enrollment, case_data(3))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        reopened
            .record_selected_case(dedicated_enrollment, case_data(4))
            .await
            .unwrap()
            .is_some()
    );
    let catalog =
        f.db.with_conn(|c| {
            Ok(
                c.query_row("SELECT COUNT(*) FROM guild_store_catalog", (), |r| {
                    Ok(r.get::<i64>(0)?)
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(
        catalog, 2,
        "reopen must not mint additional catalog identities"
    );
}

#[tokio::test]
async fn empty_implicit_shared_store_id_stays_stable_across_runtime_reopen() {
    let f = Fixture::new();
    let enrollment = enroll(&f, 10, 100).await;
    let placement = guild_placement(&f.db, 10).await.unwrap().unwrap();
    let db_cfg = DbConfig {
        path: f.path.clone(),
        pool_size: 2,
        ..DbConfig::default()
    };
    let reopened = TenantRuntime::open(
        &f.db,
        &db_cfg,
        &GuildStoresConfig::default(),
        &EvidenceConfig {
            backend: "none".into(),
            ..EvidenceConfig::default()
        },
    )
    .unwrap();
    assert_eq!(
        guild_placement(&f.db, 10)
            .await
            .unwrap()
            .unwrap()
            .store_id(),
        placement.store_id()
    );
    reopened.live(enrollment).await.unwrap();
    let catalog =
        f.db.with_conn(|c| {
            Ok(
                c.query_row("SELECT COUNT(*) FROM guild_store_catalog", (), |r| {
                    Ok(r.get::<i64>(0)?)
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(catalog, 1);
}

#[tokio::test]
async fn erasure_resumes_after_core_failure_and_reopen() {
    for (operation, table) in [
        ("INSERT", "backup_cleanup_intents"),
        ("DELETE", "guild_settings"),
        ("DELETE", "guild_store_placements"),
        ("DELETE", "guild_erasure_jobs"),
    ] {
        let f = Fixture::new();
        let e = enroll(&f, 10, 100).await;
        db::patch_settings(&f.db, 10, Some(Some("old!".into())), None, None)
            .await
            .unwrap();
        db::retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
        f.tenant.schedule_erasure(e).await.unwrap();
        f.db.with_conn(move |c| { c.execute_batch(&format!("CREATE TRIGGER review_fail BEFORE {operation} ON {table} BEGIN SELECT RAISE(ABORT,'injected core failure'); END"))?; Ok(()) }).await.unwrap();
        let first = f.tenant.drive_erasure().await.unwrap_err();
        f.db.with_conn(|c| {
            c.execute_batch("DROP TRIGGER review_fail")?;
            Ok(())
        })
        .await
        .unwrap();
        assert!(first.to_string().contains("injected core failure"));
        assert!(db::guild_erasure_queued(&f.db, 10).await.unwrap());
        let db_cfg = DbConfig {
            path: f.path.clone(),
            pool_size: 2,
            ..Default::default()
        };
        let reopened = TenantRuntime::open(
            &f.db,
            &db_cfg,
            &GuildStoresConfig::default(),
            &EvidenceConfig {
                backend: "none".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(reopened.drive_erasure().await.unwrap());
        assert!(!db::guild_erasure_queued(&f.db, 10).await.unwrap());
        assert!(!reopened.drive_erasure().await.unwrap());
        let next = db::enroll_guild(&f.db, 10, 101).await.unwrap().unwrap();
        assert!(reopened.activate(next).await.unwrap().is_some());
    }
}
#[tokio::test]
async fn disabled_backend_keeps_owned_cleanup_pending() {
    use crate::db::guild_data::{EvidenceCapture, EvidenceOrigin, EvidenceRequest, EvidenceState};
    let f = Fixture::new();
    let e = enroll(&f, 10, 100).await;
    let (s, scope) = f.tenant.live(e).await.unwrap();
    let c = CaseId::new();
    let id = CaseId::new();
    s.record_case(scope.clone(), c.clone(), case_data(1), case_limits())
        .await
        .unwrap();
    s.reserve_case_evidence(
        scope.clone(),
        c.clone(),
        id.clone(),
        EvidenceRequest {
            storage_id: CaseId::new(),
            bytes: 3,
            origin: EvidenceOrigin {
                channel_id: 1,
                message_id: 2,
                attachment_id: 3,
            },
        },
        evidence_limits(),
    )
    .await
    .unwrap();
    s.settle_evidence_capture(
        scope.clone(),
        c.clone(),
        id.clone(),
        EvidenceCapture::Stored {
            bytes: 3,
            sha256: "a".repeat(64),
        },
    )
    .await
    .unwrap();
    s.remove_case(scope.clone(), c.clone()).await.unwrap();
    assert!(f.tenant.cleanup_owned_evidence(&s, &scope).await.is_err());
    let job = s.evidence_transfer_job(scope, c, id).await.unwrap();
    assert_ne!(job.state, EvidenceState::Erased);
}

#[tokio::test]
async fn directory_wipe_advances_config_observation() {
    let f = Fixture::new();
    let e = enroll(&f, 10, 100).await;
    db::patch_settings(&f.db, 10, Some(Some("old!".into())), None, None)
        .await
        .unwrap();
    let command = db::list_commands(&f.db, 0).await.unwrap().remove(0).1;
    db::upsert_command(&f.db, 10, "removed-command", &command)
        .await
        .unwrap();
    let before = db::get_primary_versions_sync(&f.db).unwrap();
    db::retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
    f.tenant.schedule_erasure(e).await.unwrap();
    f.tenant.drive_erasure().await.unwrap();
    let after = db::get_primary_versions_sync(&f.db).unwrap();
    assert!(after.config > before.config);
    assert_ne!(before.config_revision, after.config_revision);
    assert!(after.slash > before.slash);
    assert_ne!(after.slash_revision, before.slash_revision);
    let (version, revision, commands) = db::load_slash_commands_with_version_primary(&f.db, 10)
        .await
        .unwrap();
    assert_eq!(version, after.slash);
    assert_eq!(revision, after.slash_revision);
    assert!(!commands.contains_key("removed-command"));
    assert_eq!(db::get_settings(&f.db, 10).await.unwrap().prefix, "!");
}

#[tokio::test]
async fn retirement_and_erasure_admission_rollback_together() {
    let f = Fixture::new();
    let enrollment = enroll(&f, 10, 100).await;
    f.db.with_conn(|c| { c.execute_batch("CREATE TRIGGER fail_queue BEFORE INSERT ON guild_erasure_jobs BEGIN SELECT RAISE(ABORT,'queue unavailable'); END")?; Ok(()) }).await.unwrap();
    assert!(db::retire_guild_enrollment(&f.db, 10, 100).await.is_err());
    assert_eq!(
        db::guild_enrollment(&f.db, 10).await.unwrap(),
        Some(enrollment.clone())
    );
    assert!(!db::guild_erasure_queued(&f.db, 10).await.unwrap());
    f.db.with_conn(|c| {
        c.execute_batch("DROP TRIGGER fail_queue")?;
        Ok(())
    })
    .await
    .unwrap();
    db::retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
    assert_eq!(
        db::next_guild_erasure(&f.db)
            .await
            .unwrap()
            .unwrap()
            .enrollment,
        enrollment
    );
    db::retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
    assert!(f.tenant.drive_erasure().await.unwrap());
}

#[tokio::test]
async fn directory_wipe_preserves_active_guard_and_moderation_only_versions() {
    let f = Fixture::new();
    enroll(&f, 10, 100).await;
    db::patch_settings(&f.db, 10, Some(Some("keep!".into())), None, None)
        .await
        .unwrap();
    let before = db::get_primary_versions_sync(&f.db).unwrap();
    assert!(
        !db::wipe_departed_directory_guild_data(&f.db, 10)
            .await
            .unwrap()
    );
    let after = db::get_primary_versions_sync(&f.db).unwrap();
    assert_eq!(before.config_revision, after.config_revision);
    assert_eq!(db::get_settings(&f.db, 10).await.unwrap().prefix, "keep!");
    db::record_ban(&f.db, 20, 99, None, "test").await.unwrap();
    assert!(
        db::wipe_departed_directory_guild_data(&f.db, 20)
            .await
            .unwrap()
    );
    let after = db::get_primary_versions_sync(&f.db).unwrap();
    assert_eq!(before.config_revision, after.config_revision);
    assert_eq!(before.slash_revision, after.slash_revision);
}

#[tokio::test]
async fn departure_recovers_activation_before_membership_binding() {
    for bind in [false, true] {
        let f = Fixture::new();
        let enrollment = db::enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
        let placement = assign_initial_placement(&f.db, 10, f.tenant.store_for_guild(10).unwrap())
            .await
            .unwrap();
        let handle = f.tenant.handle(placement.store_id()).await.unwrap();
        if bind {
            handle.bind_initial_placement(placement).await.unwrap();
        }
        db::retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
        assert_eq!(
            db::next_guild_erasure(&f.db)
                .await
                .unwrap()
                .unwrap()
                .enrollment,
            enrollment
        );
        assert!(f.tenant.drive_erasure().await.unwrap());
        assert!(
            db::guild_stores::guild_placement(&f.db, 10)
                .await
                .unwrap()
                .is_none()
        );
        let next = db::enroll_guild(&f.db, 10, 101).await.unwrap().unwrap();
        assert!(f.tenant.activate(next).await.unwrap().is_some());
    }
}

#[tokio::test]
async fn newer_departure_preserves_cleanup_of_last_known_membership() {
    let f = Fixture::new();
    let old = enroll(&f, 10, 100).await;
    // A newer membership was not observed before its confirmed departure.
    db::retire_guild_enrollment(&f.db, 10, 101).await.unwrap();
    assert_eq!(
        db::next_guild_erasure(&f.db)
            .await
            .unwrap()
            .unwrap()
            .enrollment,
        old
    );
    assert!(f.tenant.drive_erasure().await.unwrap());
    assert!(!db::guild_erasure_queued(&f.db, 10).await.unwrap());
}

#[tokio::test]
async fn stale_activation_cannot_recreate_or_erase_membership() {
    for stage in 0..3 {
        let f = Fixture::new();
        let old = db::enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
        if stage > 0 {
            f.tenant.activate(old.clone()).await.unwrap();
        }
        db::retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
        if stage > 0 {
            assert!(f.tenant.drive_erasure().await.unwrap());
        }
        let current = if stage == 2 {
            let current = enroll(&f, 10, 101).await;
            f.tenant
                .record_selected_case(current.clone(), case_data(1))
                .await
                .unwrap();
            Some(current)
        } else {
            None
        };
        assert!(f.tenant.activate(old.clone()).await.unwrap().is_none());
        assert!(!db::guild_erasure_queued(&f.db, 10).await.unwrap());
        assert!(
            f.tenant
                .record_selected_case(old, case_data(2))
                .await
                .unwrap()
                .is_none()
        );
        if let Some(current) = current {
            let (store, scope) = f.tenant.live(current).await.unwrap();
            assert_eq!(store.case_count(scope, case_limits()).await.unwrap(), 1);
        } else {
            assert!(guild_placement(&f.db, 10).await.unwrap().is_none());
        }
    }
}

#[tokio::test]
async fn blocked_erasure_keeps_other_guilds_progressing_after_restart() {
    use crate::db::guild_data::{EvidenceCapture, EvidenceOrigin, EvidenceRequest};
    for offline in [false, true] {
        let f = Fixture::new();
        if offline {
            db::enroll_guild(&f.db, 10, 100).await.unwrap();
            let ghost = StoreId::new();
            register_store(
                &f.db,
                ghost.clone(),
                StorePolicy::Dedicated { guild_id: 10 },
            )
            .await
            .unwrap();
            assign_initial_placement(&f.db, 10, ghost).await.unwrap();
        } else {
            let old = enroll(&f, 10, 100).await;
            let (store, scope) = f.tenant.live(old).await.unwrap();
            let case = CaseId::new();
            let id = CaseId::new();
            store
                .record_case(scope.clone(), case.clone(), case_data(1), case_limits())
                .await
                .unwrap();
            store
                .reserve_case_evidence(
                    scope.clone(),
                    case.clone(),
                    id.clone(),
                    EvidenceRequest {
                        storage_id: CaseId::new(),
                        bytes: 3,
                        origin: EvidenceOrigin {
                            channel_id: 1,
                            message_id: 2,
                            attachment_id: 3,
                        },
                    },
                    evidence_limits(),
                )
                .await
                .unwrap();
            store
                .settle_evidence_capture(
                    scope,
                    case,
                    id,
                    EvidenceCapture::Stored {
                        bytes: 3,
                        sha256: "a".repeat(64),
                    },
                )
                .await
                .unwrap();
        }
        let healthy = enroll(&f, 20, 100).await;
        f.tenant
            .record_selected_case(healthy, case_data(2))
            .await
            .unwrap();
        db::retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
        db::retire_guild_enrollment(&f.db, 20, 100).await.unwrap();
        assert!(f.tenant.drive_erasure().await.is_err());
        // A fresh process may retry the blocked guild, but must then advance.
        let reopened = TenantRuntime::open(
            &f.db,
            &DbConfig {
                path: f.path.clone(),
                pool_size: 2,
                ..Default::default()
            },
            &GuildStoresConfig::default(),
            &EvidenceConfig {
                backend: "none".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(reopened.drive_erasure().await.is_err());
        assert!(reopened.drive_erasure().await.unwrap());
        assert!(!db::guild_erasure_queued(&f.db, 20).await.unwrap());
        assert!(guild_placement(&f.db, 20).await.unwrap().is_none());
        assert!(db::guild_erasure_queued(&f.db, 10).await.unwrap());
        let rejoined = db::enroll_guild(&f.db, 20, 101).await.unwrap().unwrap();
        assert!(reopened.activate(rejoined).await.unwrap().is_some());
        // Wrap to a newly queued lower ID, then retry the still-durable failure.
        let lower = db::enroll_guild(&f.db, 5, 100).await.unwrap().unwrap();
        reopened.activate(lower).await.unwrap();
        db::retire_guild_enrollment(&f.db, 5, 100).await.unwrap();
        assert!(reopened.drive_erasure().await.unwrap());
        assert!(guild_placement(&f.db, 5).await.unwrap().is_none());
        assert!(reopened.drive_erasure().await.is_err());
        assert!(db::guild_erasure_queued(&f.db, 10).await.unwrap());
    }
}

#[tokio::test]
async fn retirement_after_activation_admission_has_a_durable_obligation() {
    let f = Fixture::new();
    let old = db::enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let store = f.tenant.store_for_guild(10).unwrap();
    let placement = admit_tenant_placement(&f.db, old.clone(), store.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        admit_tenant_placement(&f.db, old.clone(), store.clone())
            .await
            .unwrap(),
        Some(placement.clone())
    );
    db::retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
    let job = db::next_guild_erasure(&f.db).await.unwrap().unwrap();
    assert_eq!(job.enrollment, old);
    assert_eq!(job.placement, placement);
    assert!(
        admit_tenant_placement(&f.db, old, store)
            .await
            .unwrap()
            .is_none()
    );
    assert!(f.tenant.drive_erasure().await.unwrap());
    assert!(!db::guild_erasure_queued(&f.db, 10).await.unwrap());
    assert!(guild_placement(&f.db, 10).await.unwrap().is_none());
}

#[tokio::test]
async fn departed_core_writers_cannot_restore_or_change_replacement_data() {
    let f = Fixture::new();
    let old = enroll(&f, 10, 100).await;
    db::record_enrolled_ban(&f.db, Some(old.clone()), 99, Some(42), "private")
        .await
        .unwrap();
    db::retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
    assert!(f.tenant.drive_erasure().await.unwrap());
    for enrollment in [Some(old.clone()), None] {
        assert!(
            db::record_enrolled_ban(&f.db, enrollment.clone(), 99, Some(42), "late")
                .await
                .is_err()
        );
        assert!(
            db::add_enrolled_warn(&f.db, enrollment.clone(), 99, 42, "late")
                .await
                .is_err()
        );
        assert!(
            db::insert_enrolled_event(
                &f.db,
                enrollment,
                "mod_action",
                Some(99),
                Some(42),
                &serde_json::json!({"reason":"late"})
            )
            .await
            .is_err()
        );
    }
    assert!(db::list_bans(&f.db, 10, 10).await.unwrap().is_empty());
    let current = enroll(&f, 10, 101).await;
    db::record_enrolled_ban(&f.db, Some(current.clone()), 99, Some(42), "new")
        .await
        .unwrap();
    assert!(
        db::mark_enrolled_unbanned(&f.db, Some(old.clone()), 99)
            .await
            .is_err()
    );
    let bans = db::list_enrolled_bans(&f.db, current.clone(), 10)
        .await
        .unwrap();
    assert_eq!(bans.len(), 1);
    assert!(bans[0].active);
    db::add_enrolled_warn(&f.db, Some(current.clone()), 99, 42, "current")
        .await
        .unwrap();
    assert!(
        db::clear_enrolled_warns(&f.db, Some(old), 99)
            .await
            .is_err()
    );
    assert_eq!(db::list_warns(&f.db, 10, 99).await.unwrap().len(), 1);
    db::insert_enrolled_event(
        &f.db,
        Some(current.clone()),
        "mod_action",
        Some(99),
        Some(42),
        &serde_json::json!({"reason":"current"}),
    )
    .await
    .unwrap();
    db::mark_enrolled_unbanned(&f.db, Some(current.clone()), 99)
        .await
        .unwrap();
    assert!(!db::list_enrolled_bans(&f.db, current, 10).await.unwrap()[0].active);
}

#[tokio::test]
async fn departure_without_placement_settles_directory_in_retirement() {
    let f = Fixture::new();
    let old = db::enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    db::patch_settings(&f.db, 10, Some(Some("old!".into())), None, None)
        .await
        .unwrap();
    db::add_enrolled_warn(&f.db, Some(old.clone()), 99, 42, "private")
        .await
        .unwrap();
    f.db.with_conn(|conn| { conn.execute_batch("CREATE TRIGGER reject_empty_cleanup BEFORE DELETE ON warns BEGIN SELECT RAISE(ABORT,'fixture'); END;")?; Ok(()) }).await.unwrap();
    assert!(db::retire_guild_enrollment(&f.db, 10, 100).await.is_err());
    assert_eq!(
        db::guild_enrollment(&f.db, 10).await.unwrap(),
        Some(old.clone())
    );
    f.db.with_conn(|conn| {
        conn.execute_batch("DROP TRIGGER reject_empty_cleanup;")?;
        Ok(())
    })
    .await
    .unwrap();
    db::retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
    assert!(db::list_warns(&f.db, 10, 99).await.unwrap().is_empty());
    assert_ne!(db::get_settings(&f.db, 10).await.unwrap().prefix, "old!");
    assert!(f.tenant.activate(old).await.unwrap().is_none());
    assert!(!f.tenant.drive_erasure().await.unwrap());
    let reminders: i64 =
        f.db.with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM backup_cleanup_intents WHERE guild_id=10",
                (),
                |r| Ok(r.get(0)?),
            )
        })
        .await
        .unwrap();
    assert_eq!(reminders, 1);
}

#[tokio::test]
async fn replacement_join_preserves_displaced_cleanup_before_activation() {
    let f = Fixture::new();
    let old = enroll(&f, 10, 100).await;
    f.tenant
        .record_selected_case(old.clone(), case_data(99))
        .await
        .unwrap();
    let replacement = db::enroll_guild(&f.db, 10, 101).await.unwrap().unwrap();
    assert_eq!(
        db::next_guild_erasure(&f.db)
            .await
            .unwrap()
            .unwrap()
            .enrollment,
        old
    );
    db::retire_guild_enrollment(&f.db, 10, 101).await.unwrap();
    assert_eq!(
        db::next_guild_erasure(&f.db)
            .await
            .unwrap()
            .unwrap()
            .enrollment,
        old
    );
    let reopened = TenantRuntime::open(
        &f.db,
        &DbConfig {
            path: f.path.clone(),
            pool_size: 2,
            ..Default::default()
        },
        &Default::default(),
        &EvidenceConfig {
            backend: "none".into(),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(reopened.drive_erasure().await.unwrap());
    assert!(reopened.activate(replacement).await.unwrap().is_none());
    let newest = db::enroll_guild(&f.db, 10, 102).await.unwrap().unwrap();
    assert!(reopened.activate(newest).await.unwrap().is_some());
}

#[tokio::test]
async fn settings_revision_composes_with_log_edits() {
    let f = Fixture::new();
    let enrollment = enroll(&f, 10, 100).await;
    let (store, scope) = f.tenant.live(enrollment).await.unwrap();
    let initial = store.tenant_settings(scope.clone()).await.unwrap();
    let roles = store
        .set_case_delete_roles(scope.clone(), &initial.revision, vec![1])
        .await
        .unwrap();
    let logs = store
        .set_log_channel(scope.clone(), &roles, "member_ban", Some(123))
        .await
        .unwrap();
    let fetched = store.tenant_settings(scope.clone()).await.unwrap();
    assert_eq!(fetched.revision, logs);
    assert_eq!(fetched.case_delete_roles, vec![1]);
    assert!(
        store
            .set_case_delete_roles(scope.clone(), &roles, vec![2])
            .await
            .is_err()
    );
    let next = store
        .set_case_delete_roles(scope.clone(), &fetched.revision, vec![2])
        .await
        .unwrap();
    assert_eq!(
        store
            .set_case_delete_roles(scope.clone(), &next, vec![2])
            .await
            .unwrap(),
        next
    );
    assert!(
        store
            .set_log_channel(scope, &next, "member_ban", Some(124))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn note_intent_retries_keep_one_entry_and_original_timestamp() {
    use crate::db::guild_data::AppendCaseHistory;
    let f = Fixture::new();
    let enrollment = enroll(&f, 10, 100).await;
    let (store, scope) = f.tenant.live(enrollment).await.unwrap();
    let case = CaseId::new();
    store
        .record_case(scope.clone(), case.clone(), case_data(99), case_limits())
        .await
        .unwrap();
    let id = CaseId::new();
    let append = || {
        store.append_case_note(
            scope.clone(),
            case.clone(),
            id.clone(),
            Some(42),
            "note".into(),
            history_limits(),
        )
    };
    let (a, b) = tokio::join!(append(), append());
    assert!(matches!(
        (a.unwrap(), b.unwrap()),
        (
            AppendCaseHistory::Created(_),
            AppendCaseHistory::Existing(_)
        ) | (
            AppendCaseHistory::Existing(_),
            AppendCaseHistory::Created(_)
        )
    ));
    let original = store
        .case_history_entry(scope.clone(), case.clone(), id.clone(), history_limits())
        .await
        .unwrap();
    assert!(matches!(
        append().await.unwrap(),
        AppendCaseHistory::Existing(_)
    ));
    assert_eq!(
        store
            .case_history_entry(scope.clone(), case.clone(), id.clone(), history_limits())
            .await
            .unwrap(),
        original
    );
    assert!(
        store
            .append_case_note(
                scope.clone(),
                case.clone(),
                id.clone(),
                Some(42),
                "changed".into(),
                history_limits()
            )
            .await
            .is_err()
    );
    assert!(
        store
            .append_case_note(
                scope.clone(),
                case.clone(),
                id,
                Some(43),
                "note".into(),
                history_limits()
            )
            .await
            .is_err()
    );
    store
        .append_case_note(
            scope.clone(),
            case.clone(),
            CaseId::new(),
            Some(42),
            "note".into(),
            history_limits(),
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .case_history_ids(scope, case, 0, 32, history_limits())
            .await
            .unwrap()
            .len(),
        2
    );
}

#[cfg(unix)]
#[tokio::test]
async fn cancelled_capture_cleanup_uses_backend_settlement_and_survives_restart() {
    use crate::db::guild_data::{EvidenceOrigin, EvidenceRequest, EvidenceState};
    for backend in ["local", "none"] {
        let f = Fixture::with_stores(
            Default::default(),
            EvidenceConfig {
                backend: backend.into(),
                ..Default::default()
            },
        );
        let e = enroll(&f, 10, 100).await;
        let (store, scope) = f.tenant.live(e).await.unwrap();
        let case = CaseId::new();
        let id = CaseId::new();
        store
            .record_case(scope.clone(), case.clone(), case_data(99), case_limits())
            .await
            .unwrap();
        let storage_id =
            CaseId::try_from(db::evidence_storage_id(&f.db, "").await.unwrap()).unwrap();
        store
            .reserve_case_evidence(
                scope.clone(),
                case.clone(),
                id.clone(),
                EvidenceRequest {
                    storage_id,
                    bytes: 3,
                    origin: EvidenceOrigin {
                        channel_id: 1,
                        message_id: 2,
                        attachment_id: 3,
                    },
                },
                evidence_limits(),
            )
            .await
            .unwrap();
        store
            .remove_case(scope.clone(), case.clone())
            .await
            .unwrap();
        let cleanup = f.tenant.cleanup_owned_evidence(&store, &scope).await;
        let job = store
            .evidence_transfer_job(scope.clone(), case, id)
            .await
            .unwrap();
        if backend == "local" {
            cleanup.unwrap();
            assert_eq!(job.state, EvidenceState::Erased);
        } else {
            assert!(cleanup.is_err());
            assert_eq!(job.state, EvidenceState::Capturing);
        }
        db::retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
        let reopened = TenantRuntime::open(
            &f.db,
            &DbConfig {
                path: f.path.clone(),
                pool_size: 2,
                ..Default::default()
            },
            &Default::default(),
            &EvidenceConfig {
                backend: backend.into(),
                ..Default::default()
            },
        )
        .unwrap();
        if backend == "local" {
            assert!(reopened.drive_erasure().await.unwrap());
            assert!(!db::guild_erasure_queued(&f.db, 10).await.unwrap());
        } else {
            assert!(reopened.drive_erasure().await.is_err());
            assert!(db::guild_erasure_queued(&f.db, 10).await.unwrap());
        }
    }
}

#[tokio::test]
async fn missing_owned_store_cannot_be_recreated_as_erasure_proof() {
    let f = Fixture::new();
    let e = enroll(&f, 10, 100).await;
    f.tenant
        .record_selected_case(e, case_data(99))
        .await
        .unwrap();
    db::retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
    let guild = PathBuf::from(format!("{}.guild.db", f.path.display()));
    f.tenant.inner.handles.lock().await.map.clear();
    let saved = guild.with_extension("saved");
    std::fs::rename(&guild, &saved).unwrap();
    let cfg = DbConfig {
        path: f.path.clone(),
        pool_size: 2,
        ..Default::default()
    };
    let evidence = EvidenceConfig {
        backend: "none".into(),
        ..Default::default()
    };
    assert!(TenantRuntime::open(&f.db, &cfg, &Default::default(), &evidence).is_err());
    assert!(!guild.exists(), "missing database must stay missing");
    std::fs::File::create(&guild).unwrap();
    assert!(
        TenantRuntime::open(&f.db, &cfg, &Default::default(), &evidence).is_err(),
        "an empty replacement is not cleanup proof either"
    );
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", guild.display()));
    }

    assert!(db::guild_erasure_queued(&f.db, 10).await.unwrap());
    assert!(guild_placement(&f.db, 10).await.unwrap().is_some());
    std::fs::rename(&saved, &guild).unwrap();
    let reopened = TenantRuntime::open(&f.db, &cfg, &Default::default(), &evidence).unwrap();
    assert!(reopened.drive_erasure().await.unwrap());
    assert!(!db::guild_erasure_queued(&f.db, 10).await.unwrap());
}
