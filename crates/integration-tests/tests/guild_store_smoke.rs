//! Run against a fresh disposable remote database, separate from the core smoke DB.
use jmodguard::db::guild_data::{GuildDataScope, GuildDataStore, GuildStoreOptions};
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
    reopened.close_membership(scoped).await.unwrap();
    drop((store, reopened, catalog));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}
