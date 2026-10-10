use super::*;
use crate::db::guild_stores::*;
use crate::db_compat::OptionalExtension;

struct Fixture {
    catalog: Db,
    catalog_path: PathBuf,
    store_path: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let catalog_path = path("catalog");
        Self {
            catalog: Db::open(&catalog_path, 2).unwrap(),
            catalog_path,
            store_path: path("store"),
        }
    }
    fn options(&self) -> GuildStoreOptions {
        options(&self.store_path)
    }
    async fn registration(&self, id: StoreId, policy: StorePolicy) -> StoreRegistration {
        register_store(&self.catalog, id, policy).await.unwrap()
    }
    async fn placement(&self, registration: &StoreRegistration, guild: i64) -> GuildPlacement {
        assign_initial_placement(&self.catalog, guild, registration.id().clone())
            .await
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        clean(&self.catalog_path);
        clean(&self.store_path);
    }
}
fn path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "jmodguard_guild_{label}_{:032x}.db",
        rand::random::<u128>()
    ))
}
fn clean(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}
fn options(path: &Path) -> GuildStoreOptions {
    GuildStoreOptions {
        read_only: false,
        path: path.to_owned(),
        url: String::new(),
        token: String::new(),
        pool_size: 2,
    }
}

#[tokio::test]
async fn separate_store_initialization_reopen_and_no_core_tables() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    assert!(GuildDataStore::open(f.options(), reg.clone()).is_err());
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let placement = f.placement(&reg, 10).await;
    assert!(matches!(
        store
            .verify_placement(placement.clone())
            .await
            .unwrap_err()
            .downcast_ref::<StoreCheckError>(),
        Some(StoreCheckError::Unbound)
    ));
    store
        .bind_initial_placement(placement.clone())
        .await
        .unwrap();
    store.verify_placement(placement.clone()).await.unwrap();
    let reopened = GuildDataStore::open(f.options(), reg.clone()).unwrap();
    reopened.verify_placement(placement.clone()).await.unwrap();
    GuildDataStore::initialize(f.options(), reg)
        .unwrap()
        .verify_placement(placement)
        .await
        .unwrap();
    store.db.with_conn(|c| {
        let count: i64 = c.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT GLOB 'sqlite_*'", (), |r| Ok(r.get(0)?))?;
        assert_eq!(
            count,
            26,
            "store contains tenant tables, transient snapshots, member-directory tables, the local write fence, and the writer epoch"
        );
        Ok(())
    }).await.unwrap();
    assert!(
        Db::open(&f.store_path, 2).is_err(),
        "core bootstrap must not adopt a guild store"
    );
}

#[tokio::test]
async fn existing_registration_reads_stamped_identity_without_creating() {
    let f = Fixture::new();
    assert!(
        GuildDataStore::existing_registration(f.options())
            .unwrap()
            .is_none()
    );
    let reg = f
        .registration(StoreId::new(), StorePolicy::Dedicated { guild_id: 20 })
        .await;
    GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let stamped = GuildDataStore::existing_registration(f.options())
        .unwrap()
        .expect("stamped identity");
    assert_eq!(stamped, reg);
    assert!(
        GuildDataStore::existing_registration(options(&f.catalog_path)).is_err(),
        "core databases are not empty guild stores"
    );
}

#[tokio::test]
async fn store_refuses_foreign_core_newer_or_unmarked_schema_without_adoption() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    assert!(GuildDataStore::initialize(options(&f.catalog_path), reg.clone()).is_err());
    assert_eq!(
        store_registration(&f.catalog, reg.id()).await.unwrap(),
        Some(reg.clone())
    );
    {
        let raw = Connection::open(&f.store_path).unwrap();
        raw.execute_batch_bounded(
            "CREATE TABLE sentinel(value TEXT); INSERT INTO sentinel VALUES('keep')",
            64 * 1024 * 1024,
        )
        .unwrap();
    }
    assert!(GuildDataStore::initialize(f.options(), reg.clone()).is_err());
    {
        let raw = Connection::open(&f.store_path).unwrap();
        assert_eq!(
            raw.query_row(
                "SELECT value FROM sentinel",
                (),
                |r| Ok(r.get::<String>(0)?)
            )
            .unwrap(),
            "keep"
        );
        raw.execute_batch_bounded("DROP TABLE sentinel", 64 * 1024 * 1024)
            .unwrap();
    }
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    for version in [0, VERSION + 1] {
        store
            .db
            .with_conn(move |c| c.pragma_update(None, "user_version", version))
            .await
            .unwrap();
        assert!(GuildDataStore::open(f.options(), reg.clone()).is_err());
        assert!(GuildDataStore::initialize(f.options(), reg.clone()).is_err());
    }
    store
        .db
        .with_conn(|c| c.pragma_update(None, "user_version", VERSION))
        .await
        .unwrap();
    store
        .db
        .with_conn(|c| {
            c.execute_batch_bounded("DROP TRIGGER guild_binding_owner", 64 * 1024 * 1024)
        })
        .await
        .unwrap();
    assert!(
        GuildDataStore::open(f.options(), reg).is_err(),
        "missing schema guard must fail verification"
    );
}

#[tokio::test]
async fn expected_identity_and_policy_are_checked_again_after_open() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let p = f.placement(&reg, 10).await;
    store.bind_initial_placement(p.clone()).await.unwrap();
    let other = f.registration(StoreId::new(), StorePolicy::Shared).await;
    assert!(GuildDataStore::open(f.options(), other.clone()).is_err());
    assert!(GuildDataStore::initialize(f.options(), other).is_err());
    let alternate = Fixture::new();
    let different_policy = alternate
        .registration(reg.id().clone(), StorePolicy::Dedicated { guild_id: 10 })
        .await;
    assert!(GuildDataStore::open(f.options(), different_policy).is_err());
    // Simulate a restored different identity, bypassing the immutable trigger explicitly.
    store.db.with_conn(|c| {
        c.execute_batch_bounded("DROP TRIGGER store_identity_no_update; UPDATE store_identity SET store_id='00000000000000000000000000000000'; CREATE TRIGGER store_identity_no_update BEFORE UPDATE ON store_identity BEGIN SELECT RAISE(ABORT,'immutable'); END", 64 * 1024 * 1024)
    }).await.unwrap();
    assert!(matches!(
        store
            .verify_placement(p.clone())
            .await
            .unwrap_err()
            .downcast_ref::<StoreCheckError>(),
        Some(StoreCheckError::Identity)
    ));
    assert!(store.bind_initial_placement(p).await.is_err());
}

#[tokio::test]
async fn shared_dedicated_and_revision_boundaries_refuse_rebinding() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    for guild in [10, 20] {
        let p = f.placement(&reg, guild).await;
        store.bind_initial_placement(p.clone()).await.unwrap();
        store.verify_placement(p).await.unwrap();
    }
    let other_catalog = Fixture::new();
    let same_store = other_catalog
        .registration(reg.id().clone(), StorePolicy::Shared)
        .await;
    let stale = other_catalog.placement(&same_store, 10).await;
    assert!(store.verify_placement(stale.clone()).await.is_err());
    assert!(store.bind_initial_placement(stale).await.is_err());
    let dedicated = Fixture::new();
    let dedicated_reg = dedicated
        .registration(StoreId::new(), StorePolicy::Dedicated { guild_id: 30 })
        .await;
    let dedicated_store =
        GuildDataStore::initialize(dedicated.options(), dedicated_reg.clone()).unwrap();
    let own = dedicated.placement(&dedicated_reg, 30).await;
    dedicated_store
        .bind_initial_placement(own.clone())
        .await
        .unwrap();
    assert!(store.verify_placement(own).await.is_err());
    // A stale directory claiming this dedicated store is shared must not admit guild31.
    let alias = other_catalog
        .registration(dedicated_reg.id().clone(), StorePolicy::Shared)
        .await;
    let foreign = other_catalog.placement(&alias, 31).await;
    assert!(
        dedicated_store
            .bind_initial_placement(foreign.clone())
            .await
            .is_err()
    );
    assert!(dedicated_store.verify_placement(foreign).await.is_err());
}

#[tokio::test]
async fn matching_bindings_and_initialization_do_not_write() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let p = f.placement(&reg, 10).await;
    store.bind_initial_placement(p.clone()).await.unwrap();
    store.db.with_conn(|c| c.execute_batch_bounded("CREATE TRIGGER reject_binding_insert BEFORE INSERT ON guild_bindings BEGIN SELECT RAISE(ABORT,'unexpected binding'); END; CREATE TRIGGER reject_receipt_insert BEFORE INSERT ON mutation_receipts BEGIN SELECT RAISE(ABORT,'unexpected receipt'); END", 64 * 1024 * 1024)).await.unwrap();
    store.bind_initial_placement(p.clone()).await.unwrap();
    store.verify_placement(p.clone()).await.unwrap();
    GuildDataStore::initialize(f.options(), reg.clone())
        .unwrap()
        .verify_placement(p)
        .await
        .unwrap();
    let new = f.placement(&reg, 20).await;
    assert!(store.bind_initial_placement(new.clone()).await.is_err());
    assert!(matches!(
        store
            .verify_placement(new)
            .await
            .unwrap_err()
            .downcast_ref::<StoreCheckError>(),
        Some(StoreCheckError::Unbound)
    ));
}

#[tokio::test]
async fn concurrent_conflicting_bindings_choose_one_revision() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let other = Fixture::new();
    let alias = other
        .registration(reg.id().clone(), StorePolicy::Shared)
        .await;
    let a = f.placement(&reg, 10).await;
    let b = other.placement(&alias, 10).await;
    let (one, two) = tokio::join!(
        store.bind_initial_placement(a.clone()),
        store.bind_initial_placement(b.clone())
    );
    assert_eq!(usize::from(one.is_ok()) + usize::from(two.is_ok()), 1);
    assert_ne!(
        store.verify_placement(a).await.is_ok(),
        store.verify_placement(b).await.is_ok()
    );
}

#[tokio::test]
async fn store_sql_protects_identity_bindings_and_dedicated_ownership_without_foreign_keys() {
    let f = Fixture::new();
    let reg = f
        .registration(StoreId::new(), StorePolicy::Dedicated { guild_id: 10 })
        .await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let p = f.placement(&reg, 10).await;
    store.bind_initial_placement(p.clone()).await.unwrap();
    store
        .bind_initial_membership(
            GuildDataScope::new(
                p.clone(),
                crate::db::GuildEnrollment {
                    guild_id: 10,
                    id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                    joined_at_ns: 100,
                },
            )
            .unwrap(),
        )
        .await
        .unwrap();
    store.db.with_conn(|c| {
        c.execute_batch("PRAGMA foreign_keys=OFF; PRAGMA recursive_triggers=OFF")?;
        for sql in [
            "UPDATE store_identity SET dedicated_guild_id=NULL",
            "DELETE FROM store_identity",
            "INSERT OR REPLACE INTO store_identity SELECT * FROM store_identity",
            "UPDATE guild_bindings SET revision='11111111111111111111111111111111'",
            "DELETE FROM guild_bindings",
            "INSERT OR REPLACE INTO guild_bindings VALUES(10,'11111111111111111111111111111111')",
            "INSERT INTO guild_bindings VALUES(20,'11111111111111111111111111111111')",
        ] { assert!(c.execute_batch_bounded(sql, 64 * 1024 * 1024).is_err(),"accepted {sql}"); }
        Ok(())
    }).await.unwrap();
    store.verify_placement(p).await.unwrap();
}

#[tokio::test]
async fn remote_option_mistakes_never_fall_back_to_local() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let mut opts = f.options();
    opts.token = "secret".into();
    assert!(GuildDataStore::initialize(opts, reg.clone()).is_err());
    let mut opts = f.options();
    opts.url = "https://invalid.example".into();
    assert!(GuildDataStore::initialize(opts, reg).is_err());
    assert!(!f.store_path.exists());
}

#[tokio::test]
async fn version_seven_upgrade_adds_erasure_and_tenant_settings() {
    use super::{
        BUNNY_SCHEMA, CASE_SCHEMA, EVIDENCE_SCHEMA, FAMILY, HISTORY_SCHEMA, SCHEMA, SOURCE_SCHEMA,
        TENANT_SCHEMA, VERSION,
    };
    use crate::db_compat::Connection;
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let p = f.placement(&reg, 10).await;
    let raw = Connection::open(&f.store_path).unwrap();
    for schema in [
        SCHEMA,
        TENANT_SCHEMA,
        CASE_SCHEMA,
        HISTORY_SCHEMA,
        SOURCE_SCHEMA,
        EVIDENCE_SCHEMA,
        BUNNY_SCHEMA,
    ] {
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
    raw.pragma_update(None, "user_version", 7).unwrap();
    let store = GuildDataStore::initialize(f.options(), reg).unwrap();
    assert_eq!(
        raw.query_row("PRAGMA user_version", (), |r| Ok(r.get::<i64>(0)?))
            .unwrap(),
        VERSION
    );
    let scope = GuildDataScope::new(
        p,
        crate::db::GuildEnrollment {
            guild_id: 10,
            id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            joined_at_ns: 100,
        },
    )
    .unwrap();
    let settings = store.tenant_settings(scope).await.unwrap();
    assert!(settings.case_delete_roles.is_empty());
}

mod snapshots;
mod tenant_logs;

mod cases;

mod case_history;

mod case_sources;

mod case_evidence;

mod member_directory;

#[cfg(unix)]
mod bunny_transfer;

#[tokio::test]
async fn identity_check_runs_once_and_again_after_reconnect_or_schema_change() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let probe = std::sync::Arc::new(crate::db_compat::StartupProbe::default());
    let _guard = crate::db_compat::enter_startup_probe(Some(probe.clone()));
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let conn = crate::db_compat::Connection::open(&f.store_path).unwrap();

    let mut catalog = probe.catalog();
    let mut imm = probe.immediate();
    verify_identity(&conn, &reg).unwrap();
    assert_eq!(probe.catalog() - catalog, 1);
    assert_eq!(probe.immediate() - imm, 0);
    catalog = probe.catalog();
    imm = probe.immediate();

    verify_identity(&conn, &reg).unwrap();
    assert_eq!(probe.catalog() - catalog, 0);
    assert_eq!(probe.immediate() - imm, 0);
    catalog = probe.catalog();
    imm = probe.immediate();

    // Remote checkout resets the stream before every operation. That reset is
    // not a reconnect and must keep the memo.
    conn.reset_remote_stream().unwrap();
    verify_identity(&conn, &reg).unwrap();
    assert_eq!(probe.catalog() - catalog, 0);
    assert_eq!(probe.immediate() - imm, 0);

    conn.execute_batch_bounded("CREATE TABLE identity_cache_probe (id INTEGER)", 32_000_000)
        .unwrap();
    catalog = probe.catalog();
    verify_identity(&conn, &reg).unwrap();
    assert_eq!(probe.catalog() - catalog, 1);
    catalog = probe.catalog();
    verify_identity(&conn, &reg).unwrap();
    assert_eq!(probe.catalog() - catalog, 0);

    let pool_probe = probe.clone();
    let pool_reg = reg.clone();
    store
        .db
        .with_conn(move |conn| {
            verify_identity(conn, &pool_reg)?;
            conn.reset_remote_stream()?;
            let cat = pool_probe.catalog();
            let start_imm = pool_probe.immediate();
            verify_identity(conn, &pool_reg)?;
            assert_eq!(pool_probe.catalog() - cat, 0);
            assert_eq!(pool_probe.immediate() - start_imm, 0);
            Ok(())
        })
        .await
        .unwrap();

    conn.execute_batch_bounded("DROP TRIGGER store_identity_no_update", 32_000_000)
        .unwrap();
    catalog = probe.catalog();
    assert!(verify_identity(&conn, &reg).is_err());
    assert_eq!(probe.catalog() - catalog, 1);

    drop(conn);
    catalog = probe.catalog();
    let conn = crate::db_compat::Connection::open(&f.store_path).unwrap();
    assert!(verify_identity(&conn, &reg).is_err());
    assert_eq!(probe.catalog() - catalog, 1);
}

#[tokio::test]
async fn identity_memo_rereads_store_row_and_rejects_swapped_identity() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let other = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let probe = std::sync::Arc::new(crate::db_compat::StartupProbe::default());
    let _guard = crate::db_compat::enter_startup_probe(Some(probe.clone()));
    GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let conn = crate::db_compat::Connection::open(&f.store_path).unwrap();

    verify_identity(&conn, &reg).unwrap();
    verify_identity(&conn, &reg).unwrap();

    let (schema_before, user_before): (i64, i64) = conn
        .query_row(
            "SELECT (SELECT schema_version FROM pragma_schema_version), (SELECT user_version FROM pragma_user_version)",
            (),
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    // Disable triggers only on a separate connection so the warmed connection's
    // memo stays eligible: no schema objects or version cookies change.
    {
        struct SwapConnection(*mut libsql::ffi::sqlite3);
        impl Drop for SwapConnection {
            fn drop(&mut self) {
                // SAFETY: this scope owns the handle and leaves no statements open.
                unsafe { libsql::ffi::sqlite3_close_v2(self.0) };
            }
        }
        let path = std::ffi::CString::new(f.store_path.to_str().unwrap()).unwrap();
        let mut swap = SwapConnection(std::ptr::null_mut());
        let update = std::ffi::CString::new(format!(
            "UPDATE store_identity SET store_id='{}' WHERE singleton=1",
            other.id().as_str()
        ))
        .unwrap();
        // SAFETY: all C strings live through their calls; the handle is owned by
        // swap, used on this thread only, and closed before verification resumes.
        unsafe {
            assert_eq!(
                libsql::ffi::sqlite3_open_v2(
                    path.as_ptr(),
                    &mut swap.0,
                    libsql::ffi::SQLITE_OPEN_READWRITE,
                    std::ptr::null(),
                ),
                libsql::ffi::SQLITE_OK
            );
            assert_eq!(
                libsql::ffi::sqlite3_db_config(
                    swap.0,
                    libsql::ffi::SQLITE_DBCONFIG_ENABLE_TRIGGER,
                    0_i32,
                    std::ptr::null_mut::<std::ffi::c_int>(),
                ),
                libsql::ffi::SQLITE_OK
            );
            assert_eq!(
                libsql::ffi::sqlite3_exec(
                    swap.0,
                    update.as_ptr(),
                    None,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                ),
                libsql::ffi::SQLITE_OK
            );
        }
    }
    let (schema_after, user_after): (i64, i64) = conn
        .query_row(
            "SELECT (SELECT schema_version FROM pragma_schema_version), (SELECT user_version FROM pragma_user_version)",
            (),
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(schema_after, schema_before);
    assert_eq!(user_after, user_before);

    let catalog = probe.catalog();
    let imm = probe.immediate();
    let error = verify_identity(&conn, &reg).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<StoreCheckError>(),
        Some(StoreCheckError::Identity)
    ));
    assert_eq!(
        probe.catalog() - catalog,
        0,
        "memo hit must re-read store_identity without scanning sqlite_master"
    );
    assert_eq!(probe.immediate() - imm, 0);

    let catalog = probe.catalog();
    let imm = probe.immediate();
    let error = verify_identity(&conn, &reg).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<StoreCheckError>(),
        Some(StoreCheckError::Identity)
    ));
    assert_eq!(probe.catalog() - catalog, 1);
    assert_eq!(probe.immediate() - imm, 0);
}

#[tokio::test]
async fn identity_memo_hit_still_avoids_catalog_reads() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let probe = std::sync::Arc::new(crate::db_compat::StartupProbe::default());
    let _guard = crate::db_compat::enter_startup_probe(Some(probe.clone()));
    GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let conn = crate::db_compat::Connection::open(&f.store_path).unwrap();

    verify_identity(&conn, &reg).unwrap();
    let catalog = probe.catalog();
    let imm = probe.immediate();
    verify_identity(&conn, &reg).unwrap();
    assert_eq!(probe.catalog() - catalog, 0);
    assert_eq!(probe.immediate() - imm, 0);
}

#[tokio::test]
async fn identity_memo_miss_for_different_expected_registration_rescans_catalog() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let other = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let probe = std::sync::Arc::new(crate::db_compat::StartupProbe::default());
    let _guard = crate::db_compat::enter_startup_probe(Some(probe.clone()));
    GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let conn = crate::db_compat::Connection::open(&f.store_path).unwrap();

    verify_identity(&conn, &reg).unwrap();
    let catalog = probe.catalog();
    assert!(verify_identity(&conn, &other).is_err());
    assert_eq!(probe.catalog() - catalog, 1);
}

#[tokio::test]
async fn identity_memo_survives_pooled_checkout_reset() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let probe = std::sync::Arc::new(crate::db_compat::StartupProbe::default());
    let _guard = crate::db_compat::enter_startup_probe(Some(probe.clone()));
    let mut store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    crate::db::test_mark_connection_pool_remote(&mut store.db);

    store
        .db
        .with_conn_sync(|conn| verify_identity(conn, &reg))
        .unwrap();
    let catalog = probe.catalog();
    let imm = probe.immediate();

    store
        .db
        .with_conn_sync(|conn| verify_identity(conn, &reg))
        .unwrap();
    assert_eq!(probe.catalog() - catalog, 0);
    assert_eq!(probe.immediate() - imm, 0);
}

#[tokio::test]
async fn identity_verification_uses_one_catalog_read_and_rechecks_schema() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let probe = std::sync::Arc::new(crate::db_compat::StartupProbe::default());
    let _guard = crate::db_compat::enter_startup_probe(Some(probe.clone()));
    let conn = crate::db_compat::Connection::open(&f.store_path).unwrap();
    let before = probe.queries();
    verify_identity(&conn, &reg).unwrap();
    assert!(
        probe.queries() - before <= 4,
        "identity must use one catalog read"
    );
    assert_eq!(
        probe.immediate(),
        0,
        "identity reads must not reserve the writer"
    );
    conn.execute_batch_bounded("DROP TRIGGER store_identity_no_update", 32_000_000)
        .unwrap();
    assert!(verify_identity(&conn, &reg).is_err());
    drop(store);
    assert!(GuildDataStore::open(f.options(), reg).is_err());
}

#[tokio::test]
async fn guild_mutation_commit_rechecks_the_core_lease() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone())
        .unwrap()
        .with_core_write_fence(f.catalog.clone());
    let open = f.placement(&reg, 10).await;
    let live = f.placement(&reg, 11).await;
    let released = f.placement(&reg, 12).await;
    let expired = f.placement(&reg, 13).await;
    let empty = f.placement(&reg, 14).await;
    store.bind_initial_placement(open.clone()).await.unwrap();
    store.verify_placement(open).await.unwrap();

    let owner = "ab".repeat(16);
    assert!(
        claim_active_bot(
            &f.catalog,
            &owner,
            crate::control::now_ms(),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await
        .unwrap()
    );
    raise_held(&store, &f.catalog).await;
    store.bind_initial_placement(live.clone()).await.unwrap();
    store.verify_placement(live).await.unwrap();

    release_active_bot(&f.catalog, &owner).await.unwrap();
    let error = store
        .bind_initial_placement(released.clone())
        .await
        .unwrap_err();
    assert!(lease_rejected(&error), "{error:#}");
    assert!(matches!(
        store
            .verify_placement(released)
            .await
            .unwrap_err()
            .downcast_ref::<StoreCheckError>(),
        Some(StoreCheckError::Unbound)
    ));

    f.catalog.clear_lease_owner();
    write_core_lease(
        &f.catalog,
        Some(format!("{owner} {}", crate::control::now_ms() - 1_000)),
    )
    .await;
    f.catalog.arm_lease_owner(&owner);
    let error = store
        .bind_initial_placement(expired.clone())
        .await
        .unwrap_err();
    assert!(lease_rejected(&error), "{error:#}");
    assert!(matches!(
        store
            .verify_placement(expired)
            .await
            .unwrap_err()
            .downcast_ref::<StoreCheckError>(),
        Some(StoreCheckError::Unbound)
    ));

    f.catalog.clear_lease_owner();
    write_core_lease(&f.catalog, None).await;
    store.bind_initial_placement(empty.clone()).await.unwrap();
    store.verify_placement(empty).await.unwrap();
}

#[tokio::test]
async fn guild_commit_does_not_land_when_the_lease_moves_before_the_final_reread() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone())
        .unwrap()
        .with_core_write_fence(f.catalog.clone());
    let placement = f.placement(&reg, 21).await;
    let owner = "ab".repeat(16);
    assert!(
        claim_active_bot(
            &f.catalog,
            &owner,
            crate::control::now_ms(),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await
        .unwrap()
    );
    let catalog = f.catalog.clone();
    f.catalog.arm_before_final_lease_reread(move || {
        catalog
            .with_conn_sync(|conn| {
                conn.set_expected_lease_owner(None);
                let mut tx = conn.transaction_primary_read()?;
                tx.ensure_mutation_receipt()?;
                let moved = format!(
                    "{} {}",
                    "cd".repeat(16),
                    crate::control::now_ms().saturating_add(60_000)
                );
                tx.execute_bounded(
                    "INSERT INTO meta (key, value) VALUES ('active_bot_lease', ?1)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![moved],
                    4096,
                )?;
                tx.commit()
            })
            .unwrap();
    });
    let error = store
        .bind_initial_placement(placement.clone())
        .await
        .unwrap_err();
    assert!(lease_rejected(&error), "{error:#}");
    assert!(matches!(
        store
            .verify_placement(placement)
            .await
            .unwrap_err()
            .downcast_ref::<StoreCheckError>(),
        Some(StoreCheckError::Unbound)
    ));
}

#[tokio::test]
async fn slow_guild_commit_still_lets_the_same_owner_renew() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone())
        .unwrap()
        .with_core_write_fence(f.catalog.clone());
    let placement = f.placement(&reg, 22).await;
    let owner = "ab".repeat(16);
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    assert!(
        claim_active_bot(
            &f.catalog,
            &owner,
            started,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await
        .unwrap()
    );
    raise_held(&store, &f.catalog).await;
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    f.catalog.arm_during_leased_guild_commit(move || {
        let _ = ready_tx.send(());
        // Longer than the 5s renewal interval and shorter than the 20s TTL.
        std::thread::sleep(std::time::Duration::from_secs(6));
    });
    let catalog = f.catalog.clone();
    let renewal = tokio::spawn(async move {
        ready_rx.await.unwrap();
        let began = std::time::Instant::now();
        let renewed = claim_active_bot(
            &catalog,
            &owner,
            crate::control::now_ms(),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await
        .unwrap();
        (renewed, began.elapsed())
    });
    store
        .bind_initial_placement(placement.clone())
        .await
        .unwrap();
    let (renewed, elapsed) = renewal.await.unwrap();
    assert!(renewed);
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "renewal waited {elapsed:?} for the guild commit"
    );
    store.verify_placement(placement).await.unwrap();
}

#[tokio::test]
async fn guild_commit_stays_ok_when_only_core_cleanup_fails() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone())
        .unwrap()
        .with_core_write_fence(f.catalog.clone());
    let placement = f.placement(&reg, 23).await;
    let owner = "ab".repeat(16);
    assert!(
        claim_active_bot(
            &f.catalog,
            &owner,
            crate::control::now_ms(),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await
        .unwrap()
    );
    raise_held(&store, &f.catalog).await;
    f.catalog.arm_lease_cleanup_failure();
    store
        .bind_initial_placement(placement.clone())
        .await
        .unwrap();
    store.verify_placement(placement).await.unwrap();
}

#[tokio::test]
async fn replacement_epoch_rejects_a_stale_guild_commit() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let placement = f.placement(&reg, 31).await;
    let old_core = Db::open(&f.catalog_path, 2).unwrap();
    let new_core = Db::open(&f.catalog_path, 2).unwrap();
    let old_store = GuildDataStore::initialize(f.options(), reg.clone())
        .unwrap()
        .with_core_write_fence(old_core.clone());
    let new_store = GuildDataStore::open(f.options(), reg.clone())
        .unwrap()
        .with_core_write_fence(new_core.clone());
    let old_owner = "ab".repeat(16);
    let new_owner = "cd".repeat(16);
    let now = crate::control::now_ms();
    let refuse = || std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    assert!(
        claim_active_bot(&old_core, &old_owner, now, refuse())
            .await
            .unwrap()
    );
    raise_held(&old_store, &old_core).await;
    assert!(
        claim_active_bot(&new_core, &new_owner, now.saturating_add(21_000), refuse(),)
            .await
            .unwrap()
    );
    raise_held(&new_store, &new_core).await;
    assert_eq!(epoch_generation(&new_store).await, 2);
    // BEGIN IMMEDIATE already holds the guild writer lock when the core
    // re-read hooks run, so the replacement cannot raise until that
    // transaction ends. Start the stale commit after the raise, and put the
    // lease row back so the core re-read passes and the epoch rejects it.
    write_core_lease(
        &old_core,
        Some(format!(
            "{old_owner} {}",
            crate::control::now_ms().saturating_add(60_000)
        )),
    )
    .await;
    let error = old_store
        .bind_initial_placement(placement.clone())
        .await
        .unwrap_err();
    assert!(lease_rejected(&error), "{error:#}");
    assert!(
        error
            .chain()
            .any(|cause| cause.to_string().contains("active bot lease")),
        "{error:#}"
    );
    assert!(matches!(
        old_store
            .verify_placement(placement.clone())
            .await
            .unwrap_err()
            .downcast_ref::<StoreCheckError>(),
        Some(StoreCheckError::Unbound)
    ));
    write_core_lease(
        &new_core,
        Some(format!(
            "{new_owner} {}",
            crate::control::now_ms().saturating_add(60_000)
        )),
    )
    .await;
    new_store
        .bind_initial_placement(placement.clone())
        .await
        .unwrap();
    new_store.verify_placement(placement).await.unwrap();
}

#[tokio::test]
async fn older_epoch_raise_does_not_lower_the_stored_generation() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg)
        .unwrap()
        .with_core_write_fence(f.catalog.clone());
    let now = crate::control::now_ms();
    let refuse = || std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    assert!(
        claim_active_bot(&f.catalog, &"ab".repeat(16), now, refuse())
            .await
            .unwrap()
    );
    raise_held(&store, &f.catalog).await;
    assert!(
        claim_active_bot(
            &f.catalog,
            &"cd".repeat(16),
            now.saturating_add(21_000),
            refuse(),
        )
        .await
        .unwrap()
    );
    raise_held(&store, &f.catalog).await;
    assert_eq!(epoch_generation(&store).await, 2);
    let error = store.raise_writer_epoch(1).await.unwrap_err();
    assert!(lease_rejected(&error), "{error:#}");
    assert_eq!(epoch_generation(&store).await, 2);
}

#[tokio::test]
async fn renewal_keeps_the_generation_and_a_fresh_claim_increments_it() {
    let f = Fixture::new();
    let owner = "ab".repeat(16);
    let now = crate::control::now_ms();
    let refuse = || std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    assert!(
        claim_active_bot(&f.catalog, &owner, now, refuse())
            .await
            .unwrap()
    );
    assert_eq!(f.catalog.leased_generation(), Some(1));
    assert_eq!(stored_generation(&f.catalog).await, 1);
    assert!(
        claim_active_bot(&f.catalog, &owner, now.saturating_add(1_000), refuse())
            .await
            .unwrap()
    );
    assert_eq!(f.catalog.leased_generation(), Some(1));
    assert_eq!(stored_generation(&f.catalog).await, 1);
    assert!(
        claim_active_bot(
            &f.catalog,
            &"cd".repeat(16),
            now.saturating_add(21_000),
            refuse(),
        )
        .await
        .unwrap()
    );
    assert_eq!(f.catalog.leased_generation(), Some(2));
    assert_eq!(stored_generation(&f.catalog).await, 2);
}

async fn raise_held(store: &GuildDataStore, catalog: &Db) {
    store
        .raise_writer_epoch(
            catalog
                .leased_generation()
                .expect("claim arms a generation"),
        )
        .await
        .unwrap();
}

fn lease_rejected(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<crate::db_compat::ActiveBotLeaseRejected>()
            .is_some()
    })
}

async fn epoch_generation(store: &GuildDataStore) -> i64 {
    store
        .db
        .with_conn(|conn| {
            let tx = conn.transaction_primary_snapshot()?;
            let value: Option<i64> = tx
                .query_row(
                    "SELECT generation FROM guild_writer_epoch WHERE singleton=1",
                    (),
                    |row| Ok(row.get(0)?),
                )
                .optional()?;
            tx.finish_read()?;
            Ok(value.unwrap_or(0))
        })
        .await
        .unwrap()
}

async fn stored_generation(db: &Db) -> i64 {
    db.with_conn(|conn| {
        let value: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key='active_bot_generation'",
                (),
                |row| Ok(row.get(0)?),
            )
            .optional()?;
        Ok(value.as_deref().unwrap_or("0").parse::<i64>().unwrap_or(0))
    })
    .await
    .unwrap()
}

async fn write_core_lease(db: &Db, value: Option<String>) {
    db.with_conn(move |conn| {
        conn.set_expected_lease_owner(None);
        let mut tx = conn.transaction_primary_read()?;
        tx.ensure_mutation_receipt()?;
        if let Some(value) = value {
            tx.execute_bounded(
                "INSERT INTO meta (key, value) VALUES ('active_bot_lease', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![value],
                4096,
            )?;
        } else {
            tx.execute_bounded(
                "DELETE FROM meta WHERE key='active_bot_lease'",
                params![],
                4096,
            )?;
        }
        tx.commit()
    })
    .await
    .unwrap();
}
