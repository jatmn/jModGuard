use super::*;

fn enrollment(guild_id: i64, joined_at_ns: i64) -> GuildEnrollment {
    GuildEnrollment {
        guild_id,
        joined_at_ns,
        id: format!("{:032x}", rand::random::<u128>()),
    }
}
pub(super) async fn scope(
    f: &Fixture,
    store: &GuildDataStore,
    reg: &StoreRegistration,
    guild: i64,
) -> GuildDataScope {
    let placement = f.placement(reg, guild).await;
    store
        .bind_initial_placement(placement.clone())
        .await
        .unwrap();
    let scope = GuildDataScope::new(placement, enrollment(guild, 100)).unwrap();
    store.bind_initial_membership(scope.clone()).await.unwrap();
    scope
}

#[tokio::test]
async fn log_mappings_are_membership_scoped_persistent_and_bounded_by_event_types() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let b = scope(&f, &store, &reg, 20).await;
    let snapshot = store.log_channels(a.clone()).await.unwrap();
    assert!(snapshot.channels().is_empty());
    let mut revision = snapshot.revision().to_owned();
    for event in crate::types::EVENT_TYPES {
        revision = store
            .set_log_channel(a.clone(), &revision, event, Some(123))
            .await
            .unwrap();
    }
    assert_eq!(
        store
            .log_channels(a.clone())
            .await
            .unwrap()
            .channels()
            .len(),
        crate::types::EVENT_TYPES.len()
    );
    assert!(
        store
            .log_channels(b.clone())
            .await
            .unwrap()
            .channels()
            .is_empty()
    );
    let reopened = GuildDataStore::open(f.options(), reg).unwrap();
    let before = reopened.log_channels(a.clone()).await.unwrap();
    assert_eq!(before.revision(), revision);
    for event in crate::types::EVENT_TYPES {
        revision = reopened
            .set_log_channel(a.clone(), &revision, event, None)
            .await
            .unwrap();
    }
    assert!(store.log_channels(a).await.unwrap().channels().is_empty());
    assert!(store.log_channels(b).await.unwrap().channels().is_empty());
}

#[tokio::test]
async fn invalid_missing_and_different_membership_never_read_or_mutate() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let p = f.placement(&reg, 10).await;
    let e = enrollment(10, 100);
    let s = GuildDataScope::new(p.clone(), e.clone()).unwrap();
    assert!(store.bind_initial_membership(s.clone()).await.is_err());
    store.bind_initial_placement(p.clone()).await.unwrap();
    assert!(store.log_channels(s.clone()).await.is_err());
    assert!(store.close_membership(s.clone()).await.is_err());
    store.bind_initial_membership(s.clone()).await.unwrap();
    let rev = store
        .log_channels(s.clone())
        .await
        .unwrap()
        .revision()
        .to_owned();
    for bad in [
        enrollment(20, 100),
        GuildEnrollment {
            id: "x".into(),
            ..e.clone()
        },
        GuildEnrollment {
            joined_at_ns: 0,
            ..e.clone()
        },
    ] {
        assert!(GuildDataScope::new(p.clone(), bad).is_err());
    }
    for changed in [
        enrollment(10, 100),
        GuildEnrollment {
            joined_at_ns: 101,
            ..e
        },
    ] {
        let wrong = GuildDataScope::new(p.clone(), changed).unwrap();
        assert!(store.bind_initial_membership(wrong.clone()).await.is_err());
        assert!(store.log_channels(wrong.clone()).await.is_err());
        assert!(
            store
                .set_log_channel(wrong.clone(), &rev, "system", Some(12))
                .await
                .is_err()
        );
        assert!(store.close_membership(wrong).await.is_err());
    }
    for (event, channel) in [
        ("unknown", Some(12)),
        ("system", Some(0)),
        ("system", Some(-1)),
    ] {
        assert!(
            store
                .set_log_channel(s.clone(), &rev, event, channel)
                .await
                .is_err()
        );
    }
    assert!(
        store
            .set_log_channel(s.clone(), "invalid", "system", None)
            .await
            .is_err()
    );
    assert!(store.log_channels(s).await.unwrap().channels().is_empty());
}

#[tokio::test]
async fn scoped_data_rechecks_store_and_placement_inside_transaction() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let rev = store
        .log_channels(a.clone())
        .await
        .unwrap()
        .revision()
        .to_owned();
    // Change an on-disk placement after capture/open, bypassing its guard as a restore fixture.
    store.db.with_conn(|c| c.execute_batch_bounded("DROP TRIGGER guild_binding_no_update; UPDATE guild_bindings SET revision='11111111111111111111111111111111'; CREATE TRIGGER guild_binding_no_update BEFORE UPDATE ON guild_bindings BEGIN SELECT RAISE(ABORT,'immutable'); END", 64 * 1024 * 1024)).await.unwrap();
    assert!(store.log_channels(a.clone()).await.is_err());
    assert!(
        store
            .set_log_channel(a.clone(), &rev, "system", Some(12))
            .await
            .is_err()
    );
    assert!(store.close_membership(a.clone()).await.is_err());
    // A different handle with an actual different store also refuses this scope.
    let dedicated = Fixture::new();
    let own = dedicated
        .registration(StoreId::new(), StorePolicy::Dedicated { guild_id: 10 })
        .await;
    let other = GuildDataStore::initialize(dedicated.options(), own).unwrap();
    assert!(other.log_channels(a.clone()).await.is_err());
    assert!(
        other
            .set_log_channel(a, &rev, "system", None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn concurrent_edits_use_revision_cas_and_noop_edits_write_nothing() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let rev = store
        .log_channels(a.clone())
        .await
        .unwrap()
        .revision()
        .to_owned();
    let (x, y) = tokio::join!(
        store.set_log_channel(a.clone(), &rev, "system", Some(12)),
        store.set_log_channel(a.clone(), &rev, "system", Some(13))
    );
    assert_eq!(usize::from(x.is_ok()) + usize::from(y.is_ok()), 1);
    let snapshot = store.log_channels(a.clone()).await.unwrap();
    assert_ne!(snapshot.revision(), rev);
    assert!(
        store
            .set_log_channel(a.clone(), &rev, "system", Some(snapshot.channels()[0].1))
            .await
            .is_err()
    );
    store.db.with_conn(|c| c.execute_batch_bounded("CREATE TRIGGER reject_membership_write BEFORE UPDATE ON guild_memberships BEGIN SELECT RAISE(ABORT,'unexpected revision write'); END; CREATE TRIGGER reject_log_write BEFORE INSERT ON guild_log_channels BEGIN SELECT RAISE(ABORT,'unexpected log write'); END; CREATE TRIGGER reject_receipt BEFORE INSERT ON mutation_receipts BEGIN SELECT RAISE(ABORT,'unexpected receipt'); END", 64 * 1024 * 1024)).await.unwrap();
    store.bind_initial_membership(a.clone()).await.unwrap();
    assert_eq!(
        store
            .set_log_channel(
                a.clone(),
                snapshot.revision(),
                "system",
                Some(snapshot.channels()[0].1)
            )
            .await
            .unwrap(),
        snapshot.revision()
    );
    assert_eq!(
        store
            .set_log_channel(a.clone(), snapshot.revision(), "member_join", None)
            .await
            .unwrap(),
        snapshot.revision()
    );
    assert!(
        store
            .set_log_channel(a.clone(), snapshot.revision(), "system", None)
            .await
            .is_err()
    );
    assert_eq!(
        store.log_channels(a).await.unwrap().channels(),
        snapshot.channels()
    );
}

#[tokio::test]
async fn closing_a_membership_removes_mappings_and_cannot_be_reopened() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let b = scope(&f, &store, &reg, 20).await;
    for s in [&a, &b] {
        let r = store.log_channels(s.clone()).await.unwrap();
        store
            .set_log_channel(s.clone(), r.revision(), "system", Some(12))
            .await
            .unwrap();
    }
    let rev = store
        .log_channels(a.clone())
        .await
        .unwrap()
        .revision()
        .to_owned();
    // The writer and closure contend for the same primary lock. If it wins first,
    // closure removes its row; if it loses, it must reject the closed membership.
    let (_, closed) = tokio::join!(
        store.set_log_channel(a.clone(), &rev, "system", Some(13)),
        store.close_membership(a.clone())
    );
    closed.unwrap();
    assert!(store.log_channels(a.clone()).await.is_err());
    assert!(
        store
            .set_log_channel(a.clone(), &rev, "system", Some(14))
            .await
            .is_err()
    );
    assert!(store.bind_initial_membership(a.clone()).await.is_err());
    let new = GuildDataScope::new(f.placement(&reg, 10).await, enrollment(10, 200)).unwrap();
    assert!(store.bind_initial_membership(new).await.is_err());
    store.close_membership(a.clone()).await.unwrap();
    assert_eq!(
        store.log_channels(b).await.unwrap().channels(),
        &[("system".to_owned(), 12)]
    );
    store
        .db
        .with_conn(|c| {
            assert_eq!(
                c.query_row(
                    "SELECT COUNT(*) FROM guild_log_channels WHERE guild_id=10",
                    (),
                    |r| Ok(r.get::<i64>(0)?)
                )?,
                0
            );
            Ok(())
        })
        .await
        .unwrap();
    let reopened = GuildDataStore::open(f.options(), reg).unwrap();
    assert!(reopened.log_channels(a).await.is_err());
}

#[tokio::test]
async fn sql_scope_guards_work_without_foreign_keys_and_events_match_shared_types() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let revision = store
        .log_channels(a.clone())
        .await
        .unwrap()
        .revision()
        .to_owned();
    store
        .set_log_channel(a.clone(), &revision, "system", Some(12))
        .await
        .unwrap();
    store.db.with_conn(|c| {
        c.execute_batch("PRAGMA foreign_keys=OFF; PRAGMA recursive_triggers=OFF")?;
        for sql in [
            "INSERT OR REPLACE INTO guild_memberships SELECT * FROM guild_memberships",
            "UPDATE guild_memberships SET enrollment_id='11111111111111111111111111111111'",
            "UPDATE guild_memberships SET joined_at_ns=200",
            "DELETE FROM guild_memberships",
            "INSERT INTO guild_log_channels VALUES(20,'11111111111111111111111111111111','system',12)",
            "INSERT OR REPLACE INTO guild_log_channels VALUES(10,'11111111111111111111111111111111','system',12)",
            "UPDATE guild_log_channels SET guild_id=20",
            "UPDATE guild_log_channels SET event_type='unknown'",
            "UPDATE guild_log_channels SET channel_id=1.5",
        ] { assert!(c.execute_batch_bounded(sql, 64 * 1024 * 1024).is_err(), "accepted {sql}"); }
        Ok(())
    }).await.unwrap();
    store.close_membership(a).await.unwrap();
    store.db.with_conn(|c| {
        assert!(c.execute_batch_bounded("UPDATE guild_memberships SET active=1", 64 * 1024 * 1024).is_err());
        assert!(c.execute_batch_bounded("INSERT INTO guild_log_channels SELECT guild_id,enrollment_id,'system',12 FROM guild_memberships", 64 * 1024 * 1024).is_err());
        Ok(())
    }).await.unwrap();
}

#[tokio::test]
async fn version_one_upgrade_preserves_identity_bindings_and_refuses_wrong_identity() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let p = f.placement(&reg, 10).await;
    {
        let raw = Connection::open(&f.store_path).unwrap();
        raw.execute_batch_bounded(SCHEMA, 64 * 1024 * 1024).unwrap();
        raw.execute_bounded(
            "INSERT INTO store_identity VALUES(1,?1,?2,NULL)",
            params![FAMILY, reg.id().as_str()],
            64 * 1024 * 1024,
        )
        .unwrap();
        raw.execute_bounded(
            "INSERT INTO guild_bindings VALUES(?1,?2)",
            params![p.guild_id(), p.revision()],
            64 * 1024 * 1024,
        )
        .unwrap();
        raw.pragma_update(None, "user_version", 1).unwrap();
    }
    assert!(GuildDataStore::open(f.options(), reg.clone()).is_err());
    let wrong = f.registration(StoreId::new(), StorePolicy::Shared).await;
    assert!(GuildDataStore::initialize(f.options(), wrong).is_err());
    {
        let raw = Connection::open(&f.store_path).unwrap();
        assert_eq!(
            raw.query_row("PRAGMA user_version", (), |r| Ok(r.get::<i64>(0)?))
                .unwrap(),
            1
        );
        assert!(
            raw.query_row("SELECT COUNT(*) FROM guild_memberships", (), |r| Ok(r
                .get::<i64>(
                0
            )?))
            .is_err()
        );
    }
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    store.verify_placement(p.clone()).await.unwrap();
    let a = GuildDataScope::new(p, enrollment(10, 100)).unwrap();
    assert!(
        store.log_channels(a.clone()).await.is_err(),
        "upgrade must not infer membership"
    );
    store.bind_initial_membership(a.clone()).await.unwrap();
    assert!(store.log_channels(a).await.unwrap().channels().is_empty());
    GuildDataStore::open(f.options(), reg).unwrap();
}

#[tokio::test]
async fn failed_close_rolls_back_mapping_removal_and_repeated_close_is_write_free() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let revision = store
        .log_channels(a.clone())
        .await
        .unwrap()
        .revision()
        .to_owned();
    let revision = store
        .set_log_channel(a.clone(), &revision, "system", Some(12))
        .await
        .unwrap();
    store.db.with_conn(|c| c.execute_batch_bounded("CREATE TRIGGER reject_close BEFORE UPDATE ON guild_memberships WHEN NEW.active=0 BEGIN SELECT RAISE(ABORT,'test close failure'); END", 64 * 1024 * 1024)).await.unwrap();
    assert!(store.close_membership(a.clone()).await.is_err());
    let after = store.log_channels(a.clone()).await.unwrap();
    assert_eq!(after.revision(), revision);
    assert_eq!(after.channels(), &[("system".to_owned(), 12)]);
    store
        .db
        .with_conn(|c| c.execute_batch_bounded("DROP TRIGGER reject_close", 64 * 1024 * 1024))
        .await
        .unwrap();
    store.close_membership(a.clone()).await.unwrap();
    store.db.with_conn(|c| c.execute_batch_bounded("CREATE TRIGGER reject_close BEFORE UPDATE ON guild_memberships BEGIN SELECT RAISE(ABORT,'test duplicate close'); END; CREATE TRIGGER reject_delete BEFORE DELETE ON guild_log_channels BEGIN SELECT RAISE(ABORT,'test duplicate delete'); END; CREATE TRIGGER reject_receipt BEFORE INSERT ON mutation_receipts BEGIN SELECT RAISE(ABORT,'test duplicate receipt'); END", 64 * 1024 * 1024)).await.unwrap();
    store.close_membership(a).await.unwrap();
}
