use super::*;
use crate::db::guild_stores::*;

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
        assert_eq!(count, 21, "store contains prior tenant tables and ten member-directory tables");
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
        raw.execute_batch("CREATE TABLE sentinel(value TEXT); INSERT INTO sentinel VALUES('keep')")
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
        raw.execute_batch("DROP TABLE sentinel").unwrap();
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
        .with_conn(|c| c.execute_batch("DROP TRIGGER guild_binding_owner"))
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
        c.execute_batch("DROP TRIGGER store_identity_no_update; UPDATE store_identity SET store_id='00000000000000000000000000000000'; CREATE TRIGGER store_identity_no_update BEFORE UPDATE ON store_identity BEGIN SELECT RAISE(ABORT,'immutable'); END")
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
    store.db.with_conn(|c| c.execute_batch("CREATE TRIGGER reject_binding_insert BEFORE INSERT ON guild_bindings BEGIN SELECT RAISE(ABORT,'unexpected binding'); END; CREATE TRIGGER reject_receipt_insert BEFORE INSERT ON mutation_receipts BEGIN SELECT RAISE(ABORT,'unexpected receipt'); END")).await.unwrap();
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
        ] { assert!(c.execute_batch(sql).is_err(),"accepted {sql}"); }
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

mod tenant_logs;

mod cases;

mod case_history;

mod case_sources;

mod case_evidence;

mod member_directory;

#[cfg(unix)]
mod bunny_transfer;
