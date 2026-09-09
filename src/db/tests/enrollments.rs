use super::*;

struct Fixture {
    db: Db,
    path: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let path = tmp_db_path("enrollment_observation");
        Self {
            db: Db::open(&path, 2).unwrap(),
            path,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
        }
    }
}

#[tokio::test]
async fn reconnect_and_reopen_preserve_identity_but_rejoin_replaces_it() {
    let f = Fixture::new();
    let first = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    assert_eq!(
        enroll_guild(&f.db, 10, 100).await.unwrap(),
        Some(first.clone())
    );
    assert!(enroll_guild(&f.db, 10, 99).await.unwrap().is_none());
    let reopened = Db::open(&f.path, 2).unwrap();
    assert_eq!(
        guild_enrollment(&reopened, 10).await.unwrap(),
        Some(first.clone())
    );
    retire_guild_enrollment(&reopened, 10, 100).await.unwrap();
    assert!(guild_enrollment(&f.db, 10).await.unwrap().is_none());
    assert!(enroll_guild(&f.db, 10, 100).await.unwrap().is_none());
    let rejoined = enroll_guild(&f.db, 10, 101).await.unwrap().unwrap();
    assert_ne!(first.id, rejoined.id);
    retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
    assert_eq!(
        guild_enrollment(&f.db, 10).await.unwrap(),
        Some(rejoined.clone())
    );
    let wire = serde_json::to_value(rejoined).unwrap();
    assert_eq!(wire["guild_id"], "10");
    assert_eq!(wire["joined_at_ns"], "101");
}

#[tokio::test]
async fn reordered_and_concurrent_membership_observations_do_not_resurrect_departures() {
    let f = Fixture::new();
    retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
    assert!(enroll_guild(&f.db, 10, 100).await.unwrap().is_none());
    let (a, b) = tokio::join!(enroll_guild(&f.db, 10, 101), enroll_guild(&f.db, 10, 101));
    assert_eq!(a.unwrap(), b.unwrap());
    let (join, leave) = tokio::join!(
        enroll_guild(&f.db, 20, 100),
        retire_guild_enrollment(&f.db, 20, 100)
    );
    join.unwrap();
    leave.unwrap();
    assert!(guild_enrollment(&f.db, 20).await.unwrap().is_none());
    let (old, new) = tokio::join!(
        retire_guild_enrollment(&f.db, 10, 101),
        enroll_guild(&f.db, 10, 102)
    );
    old.unwrap();
    let newest = new.unwrap().unwrap();
    assert_eq!(guild_enrollment(&f.db, 10).await.unwrap(), Some(newest));
}

#[tokio::test]
async fn observations_preserve_settings_audits_and_unbound_work_without_noop_writes() {
    let f = Fixture::new();
    patch_settings(
        &f.db,
        10,
        Some(Some("?".into())),
        Some(Some("name".into())),
        None,
    )
    .await
    .unwrap();
    add_warn(&f.db, 10, 99, 42, "retained").await.unwrap();
    let version = get_config_version(&f.db).await.unwrap();
    let nickname = nickname_reconcile_status(&f.db, 10).await.unwrap();
    enroll_guild(&f.db, 10, 100).await.unwrap();
    retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
    let identity = enroll_guild(&f.db, 10, 101).await.unwrap();
    assert_eq!(version, get_config_version(&f.db).await.unwrap());
    assert_eq!(
        nickname,
        nickname_reconcile_status(&f.db, 10).await.unwrap()
    );
    assert_eq!(get_settings(&f.db, 10).await.unwrap().prefix, "?");
    assert_eq!(list_warns(&f.db, 10, 99).await.unwrap().len(), 1);
    f.db.with_conn(|conn|conn.execute_batch("CREATE TRIGGER block_membership BEFORE UPDATE ON guild_enrollments BEGIN SELECT RAISE(ABORT,'unexpected write'); END;
        CREATE TRIGGER block_receipt BEFORE INSERT ON mutation_receipts BEGIN SELECT RAISE(ABORT,'unexpected receipt'); END;")).await.unwrap();
    assert_eq!(enroll_guild(&f.db, 10, 101).await.unwrap(), identity);
    assert!(enroll_guild(&f.db, 10, 100).await.unwrap().is_none());
}

#[tokio::test]
async fn invalid_observations_and_failed_transactions_leave_identity_intact() {
    let f = Fixture::new();
    for (guild, joined) in [(0, 100), (-1, 100), (10, 0), (10, -1)] {
        assert!(enroll_guild(&f.db, guild, joined).await.is_err());
        assert!(retire_guild_enrollment(&f.db, guild, joined).await.is_err());
    }
    let first = enroll_guild(&f.db, 10, 100).await.unwrap();
    f.db.with_conn(|conn|conn.execute_batch("CREATE TRIGGER fail_membership AFTER UPDATE ON guild_enrollments BEGIN SELECT RAISE(ABORT,'test failure'); END")).await.unwrap();
    assert!(enroll_guild(&f.db, 10, 101).await.is_err());
    assert!(retire_guild_enrollment(&f.db, 10, 100).await.is_err());
    assert_eq!(guild_enrollment(&f.db, 10).await.unwrap(), first);
    assert!(guild_enrollment(&f.db, 20).await.unwrap().is_none());
}
