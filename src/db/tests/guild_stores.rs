use super::*;
use crate::db::guild_stores::*;

struct Fixture {
    db: Db,
    path: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let path = tmp_db_path("guild_stores");
        Self {
            db: Db::open(&path, 2).unwrap(),
            path,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        cleanup_db(&self.path);
    }
}

#[tokio::test]
async fn catalog_is_empty_and_does_not_adopt_settings_or_memberships() {
    let f = Fixture::new();
    patch_settings(&f.db, 10, Some(Some("?".into())), None, None)
        .await
        .unwrap();
    enroll_guild(&f.db, 10, 100).await.unwrap();
    assert!(guild_placement(&f.db, 10).await.unwrap().is_none());
    let reopened = Db::open(&f.path, 2).unwrap();
    assert!(guild_placement(&reopened, 10).await.unwrap().is_none());
    let counts = reopened
        .with_conn(|c| {
            Ok((
                c.query_row("SELECT COUNT(*) FROM guild_store_catalog", (), |r| {
                    Ok(r.get::<i64>(0)?)
                })?,
                c.query_row("SELECT COUNT(*) FROM guild_store_placements", (), |r| {
                    Ok(r.get::<i64>(0)?)
                })?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(counts, (0, 0));
    assert_eq!(get_settings(&reopened, 10).await.unwrap().prefix, "?");
}

#[tokio::test]
async fn shared_and_dedicated_placements_persist_without_rerouting_or_rebinding() {
    let f = Fixture::new();
    let shared = StoreId::new();
    let dedicated = StoreId::new();
    register_store(&f.db, shared.clone(), StorePolicy::Shared)
        .await
        .unwrap();
    register_store(
        &f.db,
        dedicated.clone(),
        StorePolicy::Dedicated { guild_id: 30 },
    )
    .await
    .unwrap();
    let first = assign_initial_placement(&f.db, 10, shared.clone())
        .await
        .unwrap();
    let second = assign_initial_placement(&f.db, 20, shared.clone())
        .await
        .unwrap();
    let third = assign_initial_placement(&f.db, 30, dedicated.clone())
        .await
        .unwrap();
    assert_eq!(first.guild_id(), 10);
    assert_eq!(first.store_id(), &shared);
    assert_ne!(first.revision(), second.revision());
    assert_eq!(
        assign_initial_placement(&f.db, 10, shared.clone())
            .await
            .unwrap(),
        first
    );
    assert!(matches!(
        assign_initial_placement(&f.db, 20, dedicated.clone())
            .await
            .unwrap_err()
            .downcast_ref::<CatalogConflict>(),
        Some(CatalogConflict::DedicatedOwner)
    ));
    assert!(matches!(
        assign_initial_placement(&f.db, 30, shared.clone())
            .await
            .unwrap_err()
            .downcast_ref::<CatalogConflict>(),
        Some(CatalogConflict::Placement)
    ));
    assert!(matches!(
        register_store(
            &f.db,
            shared.clone(),
            StorePolicy::Dedicated { guild_id: 10 }
        )
        .await
        .unwrap_err()
        .downcast_ref::<CatalogConflict>(),
        Some(CatalogConflict::Registration)
    ));
    let reopened = Db::open(&f.path, 2).unwrap();
    for expected in [first, second, third] {
        assert_eq!(
            guild_placement(&reopened, expected.guild_id())
                .await
                .unwrap(),
            Some(expected)
        );
    }
    assert_eq!(
        store_registration(&reopened, &dedicated)
            .await
            .unwrap()
            .unwrap()
            .policy(),
        StorePolicy::Dedicated { guild_id: 30 }
    );
    assert!(
        guild_enrollment(&reopened, 30).await.unwrap().is_none(),
        "placement must not create membership"
    );
}

#[tokio::test]
async fn catalog_policy_lookups_return_matching_shared_or_dedicated_rows() {
    let f = Fixture::new();
    assert!(shared_store_registration(&f.db).await.unwrap().is_none());
    assert!(
        dedicated_store_registration(&f.db, 20)
            .await
            .unwrap()
            .is_none()
    );
    let shared = StoreId::new();
    let dedicated = StoreId::new();
    register_store(&f.db, shared.clone(), StorePolicy::Shared)
        .await
        .unwrap();
    register_store(
        &f.db,
        dedicated.clone(),
        StorePolicy::Dedicated { guild_id: 20 },
    )
    .await
    .unwrap();
    assert_eq!(
        shared_store_registration(&f.db)
            .await
            .unwrap()
            .unwrap()
            .id(),
        &shared
    );
    assert_eq!(
        dedicated_store_registration(&f.db, 20)
            .await
            .unwrap()
            .unwrap()
            .id(),
        &dedicated
    );
    assert!(
        dedicated_store_registration(&f.db, 21)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn catalog_retries_and_reads_are_receipt_free_and_preserve_versions() {
    let f = Fixture::new();
    let id = StoreId::new();
    let before = get_primary_versions(&f.db).await.unwrap();
    let registration = register_store(&f.db, id.clone(), StorePolicy::Shared)
        .await
        .unwrap();
    let placement = assign_initial_placement(&f.db, 10, id.clone())
        .await
        .unwrap();
    f.db.with_conn(|c| c.execute_batch("CREATE TRIGGER reject_catalog_receipt BEFORE INSERT ON mutation_receipts BEGIN SELECT RAISE(ABORT,'unexpected receipt'); END")).await.unwrap();
    assert_eq!(
        register_store(&f.db, id.clone(), StorePolicy::Shared)
            .await
            .unwrap(),
        registration
    );
    assert_eq!(
        assign_initial_placement(&f.db, 10, id.clone())
            .await
            .unwrap(),
        placement
    );
    assert_eq!(guild_placement(&f.db, 10).await.unwrap(), Some(placement));
    assert_eq!(
        store_registration(&f.db, &id).await.unwrap(),
        Some(registration)
    );
    assert!(guild_placement(&f.db, 11).await.unwrap().is_none());
    assert!(
        store_registration(&f.db, &StoreId::new())
            .await
            .unwrap()
            .is_none()
    );
    // Local SQLite needs no remote recovery receipts. Reject actual inserts to
    // exercise rollback; receipt creation failure is covered by remote smoke.
    f.db.with_conn(|c| c.execute_batch("CREATE TRIGGER reject_catalog_write BEFORE INSERT ON guild_store_catalog BEGIN SELECT RAISE(ABORT,'rejected write'); END; CREATE TRIGGER reject_placement_write BEFORE INSERT ON guild_store_placements BEGIN SELECT RAISE(ABORT,'rejected write'); END")).await.unwrap();
    let rejected = StoreId::new();
    assert!(
        register_store(&f.db, rejected.clone(), StorePolicy::Shared)
            .await
            .is_err()
    );
    assert!(
        store_registration(&f.db, &rejected)
            .await
            .unwrap()
            .is_none()
    );
    assert!(assign_initial_placement(&f.db, 11, id).await.is_err());
    assert!(guild_placement(&f.db, 11).await.unwrap().is_none());
    assert_eq!(get_primary_versions(&f.db).await.unwrap(), before);
}

#[tokio::test]
async fn concurrent_initial_placements_choose_one_store_and_matching_retries_agree() {
    let f = Fixture::new();
    let a = StoreId::new();
    let b = StoreId::new();
    for id in [&a, &b] {
        register_store(&f.db, id.clone(), StorePolicy::Shared)
            .await
            .unwrap();
    }
    let (one, two) = tokio::join!(
        assign_initial_placement(&f.db, 10, a),
        assign_initial_placement(&f.db, 10, b)
    );
    assert_eq!(usize::from(one.is_ok()) + usize::from(two.is_ok()), 1);
    let winner = guild_placement(&f.db, 10).await.unwrap().unwrap();
    let loser = if one.is_err() { one } else { two };
    assert!(matches!(
        loser.unwrap_err().downcast_ref::<CatalogConflict>(),
        Some(CatalogConflict::Placement)
    ));
    let (one, two) = tokio::join!(
        assign_initial_placement(&f.db, 10, winner.store_id().clone()),
        assign_initial_placement(&f.db, 10, winner.store_id().clone())
    );
    assert_eq!(one.unwrap(), winner);
    assert_eq!(two.unwrap(), winner);
}

#[tokio::test]
async fn catalog_rejects_invalid_ids_missing_stores_and_broken_references() {
    for text in [
        "",
        "https://db.example",
        "token-secret",
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "gggggggggggggggggggggggggggggggg",
        "0000000000000000000000000000000'",
    ] {
        assert!(StoreId::parse(text).is_err());
    }
    let f = Fixture::new();
    let id = StoreId::new();
    assert_eq!(StoreId::parse(id.as_str()).unwrap(), id);
    assert!(matches!(
        assign_initial_placement(&f.db, 10, id.clone())
            .await
            .unwrap_err()
            .downcast_ref::<CatalogConflict>(),
        Some(CatalogConflict::UnknownStore)
    ));
    for guild in [0, -1] {
        assert!(guild_placement(&f.db, guild).await.is_err());
        assert!(
            assign_initial_placement(&f.db, guild, id.clone())
                .await
                .is_err()
        );
        assert!(
            register_store(
                &f.db,
                id.clone(),
                StorePolicy::Dedicated { guild_id: guild }
            )
            .await
            .is_err()
        );
    }
    register_store(&f.db, id.clone(), StorePolicy::Shared)
        .await
        .unwrap();
    assign_initial_placement(&f.db, 10, id).await.unwrap();
    f.db.with_conn(|c| {
        c.execute_batch(
            "DROP TRIGGER guild_store_catalog_referenced; DELETE FROM guild_store_catalog",
        )
    })
    .await
    .unwrap();
    assert!(
        guild_placement(&f.db, 10).await.is_err(),
        "broken references must not look unplaced"
    );
}

#[tokio::test]
async fn catalog_sql_constraints_do_not_depend_on_foreign_key_pragmas() {
    let f = Fixture::new();
    let id = StoreId::parse("11111111111111111111111111111111").unwrap();
    register_store(&f.db, id.clone(), StorePolicy::Dedicated { guild_id: 10 })
        .await
        .unwrap();
    assign_initial_placement(&f.db, 10, id).await.unwrap();
    f.db.with_conn(|c| {
        c.execute_batch("PRAGMA foreign_keys=OFF")?;
        for sql in [
            "INSERT INTO guild_store_catalog VALUES('bad',NULL)",
            "INSERT INTO guild_store_catalog VALUES('22222222222222222222222222222222','bad')",
            "INSERT INTO guild_store_placements VALUES(20,'11111111111111111111111111111111','22222222222222222222222222222222')",
            "INSERT INTO guild_store_placements VALUES(20,'33333333333333333333333333333333','22222222222222222222222222222222')",
            "UPDATE guild_store_placements SET guild_id=20 WHERE guild_id=10",
            "UPDATE guild_store_placements SET store_id='33333333333333333333333333333333' WHERE guild_id=10",
            "UPDATE guild_store_placements SET revision='bad' WHERE guild_id=10",
            "UPDATE guild_store_catalog SET dedicated_guild_id=NULL",
            "INSERT OR REPLACE INTO guild_store_catalog VALUES('11111111111111111111111111111111',NULL)",
            "DELETE FROM guild_store_catalog",
        ] {
            assert!(c.execute_batch(sql).is_err(), "accepted invalid SQL: {sql}");
        }
        Ok(())
    }).await.unwrap();
    assert_eq!(
        guild_placement(&f.db, 10)
            .await
            .unwrap()
            .unwrap()
            .guild_id(),
        10
    );
}

#[tokio::test]
async fn catalog_migration_keeps_version_nine_data_and_starts_empty() {
    let path = tmp_db_path("catalog_v9");
    {
        let c = Connection::open(&path).unwrap();
        for (index, sql) in MIGRATIONS[..9].iter().enumerate() {
            c.execute_batch(sql).unwrap();
            c.pragma_update(None, "user_version", (index + 1) as i64)
                .unwrap();
        }
        c.execute_batch(
            "INSERT INTO events(guild_id,event_type,data,created_at) VALUES(10,'member_ban','{}','fixture')",
        )
        .unwrap();
    }
    let db = Db::open(&path, 2).unwrap();
    assert_eq!(
        query_events(&db, 10, None, None, 50).await.unwrap().len(),
        1
    );
    assert!(guild_placement(&db, 10).await.unwrap().is_none());
    drop(db);
    cleanup_db(&path);
}

#[tokio::test]
async fn tenant_runtime_migration_keeps_version_ten_catalog_and_starts_empty() {
    let path = tmp_db_path("catalog_v10");
    {
        let c = Connection::open(&path).unwrap();
        for (index, sql) in MIGRATIONS[..10].iter().enumerate() {
            c.execute_batch(sql).unwrap();
            c.pragma_update(None, "user_version", (index + 1) as i64)
                .unwrap();
        }
        c.execute_batch(
            "INSERT INTO events(guild_id,event_type,data,created_at) VALUES(10,'member_ban','{}','fixture')",
        )
        .unwrap();
    }
    let db = Db::open(&path, 2).unwrap();
    assert_eq!(
        query_events(&db, 10, None, None, 50).await.unwrap().len(),
        1
    );
    let jobs: i64 = db
        .with_conn(|c| {
            c.query_row("SELECT COUNT(*) FROM guild_erasure_jobs", (), |r| {
                Ok(r.get(0)?)
            })
        })
        .await
        .unwrap();
    let intents: i64 = db
        .with_conn(|c| {
            c.query_row("SELECT COUNT(*) FROM backup_cleanup_intents", (), |r| {
                Ok(r.get(0)?)
            })
        })
        .await
        .unwrap();
    assert_eq!(jobs, 0);
    assert_eq!(intents, 0);
    drop(db);
    cleanup_db(&path);
}

#[tokio::test]
async fn guild_move_migration_starts_with_empty_fences() {
    let path = tmp_db_path("catalog_v11");
    {
        let c = Connection::open(&path).unwrap();
        for (index, sql) in MIGRATIONS[..11].iter().enumerate() {
            c.execute_batch(sql).unwrap();
            c.pragma_update(None, "user_version", (index + 1) as i64)
                .unwrap();
        }
    }
    let db = Db::open(&path, 2).unwrap();
    let fences: i64 = db
        .with_conn(|c| {
            c.query_row("SELECT COUNT(*) FROM guild_write_fences", (), |r| {
                Ok(r.get(0)?)
            })
        })
        .await
        .unwrap();
    assert_eq!(fences, 0);
    drop(db);
    cleanup_db(&path);
}

#[tokio::test]
async fn fenced_rebind_moves_placement_and_blocks_unfenced_cutover() {
    let f = Fixture::new();
    let shared = StoreId::new();
    let dedicated = StoreId::new();
    register_store(&f.db, shared.clone(), StorePolicy::Shared)
        .await
        .unwrap();
    register_store(
        &f.db,
        dedicated.clone(),
        StorePolicy::Dedicated { guild_id: 10 },
    )
    .await
    .unwrap();
    let first = assign_initial_placement(&f.db, 10, shared.clone())
        .await
        .unwrap();
    let dest_revision = format!("{:032x}", 7u128);
    assert!(
        rebind_placement(
            &f.db,
            first.clone(),
            dedicated.clone(),
            dest_revision.clone()
        )
        .await
        .is_err()
    );
    let generation = set_guild_write_fence(
        &f.db,
        10,
        dedicated.clone(),
        dest_revision.clone(),
        "freeze",
    )
    .await
    .unwrap();
    assert!(guild_is_write_fenced(&f.db, 10).await.unwrap());
    let moved = rebind_placement(
        &f.db,
        first.clone(),
        dedicated.clone(),
        dest_revision.clone(),
    )
    .await
    .unwrap();
    assert_eq!(moved.store_id(), &dedicated);
    assert_eq!(moved.revision(), dest_revision);
    let current = guild_placement(&f.db, 10).await.unwrap().unwrap();
    assert_eq!(current.store_id(), &dedicated);
    clear_guild_write_fence(&f.db, 10, &generation)
        .await
        .unwrap();
    assert!(!guild_is_write_fenced(&f.db, 10).await.unwrap());
}

#[tokio::test]
async fn guild_configuration_writers_fail_inside_the_move_fence_transaction() {
    let f = Fixture::new();
    upsert_template(&f.db, 10, "ban", "before").await.unwrap();
    upsert_log_channel(&f.db, 10, "automod", 55).await.unwrap();
    let destination = StoreId::new();
    register_store(
        &f.db,
        destination.clone(),
        StorePolicy::Dedicated { guild_id: 10 },
    )
    .await
    .unwrap();
    set_guild_write_fence(&f.db, 10, destination, format!("{:032x}", 8u128), "freeze")
        .await
        .unwrap();

    assert!(upsert_template(&f.db, 10, "ban", "blocked").await.is_err());
    assert!(delete_template(&f.db, 10, "ban").await.is_err());
    assert!(upsert_log_channel(&f.db, 10, "automod", 66).await.is_err());
    assert!(delete_log_channel(&f.db, 10, "automod").await.is_err());
    upsert_template(&f.db, 0, "ban", "global remains writable")
        .await
        .unwrap();
}
