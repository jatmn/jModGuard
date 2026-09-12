use super::*;
use crate::config::{DbConfig, EvidenceConfig, GuildStoreConfig, GuildStoresConfig};
use crate::db::guild_data::{CaseAction, CaseData, CaseEvidence, CaseId, CaseOutcome, CaseSource};
use crate::db::guild_stores::guild_placement;
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
