use super::*;

struct Fixture {
    db: Db,
    path: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let path = tmp_db_path("nickname_enrollment");
        Self {
            db: Db::open(&path, 2).unwrap(),
            path,
        }
    }
    async fn request(&self, guild: i64, nickname: &str) -> NicknameReconcileJob {
        patch_settings(&self.db, guild, None, Some(Some(nickname.into())), None)
            .await
            .unwrap();
        pending_nickname_reconciles(&self.db, i64::MAX, 100)
            .await
            .unwrap()
            .into_iter()
            .find(|j| j.guild_id == guild)
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        cleanup_db(&self.path);
    }
}

#[tokio::test]
async fn new_join_never_adopts_old_work_and_late_receipts_cannot_touch_new_work() {
    for observed_departure in [false, true] {
        let f = Fixture::new();
        enroll_guild(&f.db, 10, 100).await.unwrap();
        let old = f.request(10, "old").await;
        if observed_departure {
            retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
        }
        enroll_guild(&f.db, 10, 101).await.unwrap();
        assert!(
            pending_nickname_reconciles(&f.db, i64::MAX, 10)
                .await
                .unwrap()
                .is_empty()
        );
        let new = f.request(10, "new").await;
        assert_ne!(old.enrollment_id, new.enrollment_id);
        assert!(!nickname_reconcile_can_dispatch(&f.db, &old).await.unwrap());
        acknowledge_nickname_reconcile(&f.db, &old).await.unwrap();
        schedule_nickname_reconcile_retry(&f.db, &old, 1000, "late", "late")
            .await
            .unwrap();
        finish_nickname_reconcile(&f.db, &old, "terminal", "late", "late", 1000)
            .await
            .unwrap();
        assert_eq!(
            pending_nickname_reconciles(&f.db, 0, 10).await.unwrap(),
            vec![new]
        );
    }
}

#[tokio::test]
async fn deleted_job_generation_reuse_does_not_reuse_request_identity() {
    let f = Fixture::new();
    enroll_guild(&f.db, 10, 100).await.unwrap();
    let old = f.request(10, "same").await;
    acknowledge_nickname_reconcile(&f.db, &old).await.unwrap();
    let new = f.request(10, "same").await;
    assert_eq!(old.generation, new.generation);
    assert_ne!(old.job_id, new.job_id);
    acknowledge_nickname_reconcile(&f.db, &old).await.unwrap();
    assert!(nickname_reconcile_can_dispatch(&f.db, &new).await.unwrap());
    let reopened = Db::open(&f.path, 2).unwrap();
    assert_eq!(
        pending_nickname_reconciles(&reopened, 0, 1).await.unwrap(),
        vec![new]
    );
}

#[tokio::test]
async fn modified_job_fields_and_stale_attempt_receipts_are_rejected() {
    let f = Fixture::new();
    enroll_guild(&f.db, 10, 100).await.unwrap();
    enroll_guild(&f.db, 20, 100).await.unwrap();
    let job = f.request(10, "intended").await;
    let other = f.request(20, "other").await;
    for field in 0..7 {
        let mut forged = job.clone();
        match field {
            0 => forged.guild_id = 20,
            1 => forged.job_id = "forged".into(),
            2 => forged.enrollment_id = other.enrollment_id.clone(),
            3 => forged.joined_at_ns += 1,
            4 => forged.generation += 1,
            5 => forged.nickname = "forged".into(),
            _ => forged.attempt_count += 1,
        }
        assert!(
            !nickname_reconcile_can_dispatch(&f.db, &forged)
                .await
                .unwrap()
        );
        acknowledge_nickname_reconcile(&f.db, &forged)
            .await
            .unwrap();
        schedule_nickname_reconcile_retry(&f.db, &forged, 1000, "forged", "forged")
            .await
            .unwrap();
        finish_nickname_reconcile(&f.db, &forged, "terminal", "forged", "forged", 1000)
            .await
            .unwrap();
    }
    assert_eq!(
        pending_nickname_reconciles(&f.db, 0, 10).await.unwrap(),
        vec![job.clone(), other]
    );
    schedule_nickname_reconcile_retry(&f.db, &job, 1000, "transport", "retry")
        .await
        .unwrap();
    assert!(!nickname_reconcile_can_dispatch(&f.db, &job).await.unwrap());
    acknowledge_nickname_reconcile(&f.db, &job).await.unwrap();
    let retried = pending_nickname_reconciles(&f.db, 2000, 10)
        .await
        .unwrap()
        .into_iter()
        .find(|j| j.guild_id == 10)
        .unwrap();
    assert_eq!(retried.attempt_count, 1);
    assert_eq!(retried.job_id, job.job_id);
    assert!(
        nickname_reconcile_can_dispatch(&f.db, &retried)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn unregistered_or_retired_requests_stay_unbound_until_explicit_replacement() {
    let f = Fixture::new();
    patch_settings(&f.db, 10, None, Some(Some("before join".into())), None)
        .await
        .unwrap();
    assert!(
        pending_nickname_reconciles(&f.db, 0, 10)
            .await
            .unwrap()
            .is_empty()
    );
    enroll_guild(&f.db, 10, 100).await.unwrap();
    assert!(
        pending_nickname_reconciles(&f.db, 0, 10)
            .await
            .unwrap()
            .is_empty()
    );
    let current = f.request(10, "after join").await;
    assert!(
        nickname_reconcile_can_dispatch(&f.db, &current)
            .await
            .unwrap()
    );
    retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
    request_nickname_reconcile(&f.db, 10).await.unwrap();
    enroll_guild(&f.db, 10, 101).await.unwrap();
    assert!(
        pending_nickname_reconciles(&f.db, i64::MAX, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn reconnect_resumes_same_enrollment_dormancy_without_resurrecting_old_attempt() {
    let f = Fixture::new();
    enroll_guild(&f.db, 10, 100).await.unwrap();
    let job = f.request(10, "name").await;
    finish_nickname_reconcile(&f.db, &job, "dormant", "unknown_guild", "unavailable", 1000)
        .await
        .unwrap();
    enroll_guild(&f.db, 10, 100).await.unwrap();
    let resumed = pending_nickname_reconciles(&f.db, 0, 1)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(job.enrollment_id, resumed.enrollment_id);
    assert!(resumed.generation > job.generation);
    assert!(!nickname_reconcile_can_dispatch(&f.db, &job).await.unwrap());
    assert!(
        nickname_reconcile_can_dispatch(&f.db, &resumed)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn nickname_and_enrollment_invalidation_rollback_together() {
    let f = Fixture::new();
    let enrollment = enroll_guild(&f.db, 10, 100).await.unwrap();
    let job = f.request(10, "name").await;
    f.db.with_conn(|conn| conn.execute_batch_bounded("CREATE TRIGGER fail_nickname BEFORE UPDATE ON nickname_reconcile BEGIN SELECT RAISE(ABORT,'injected failure'); END", 64 * 1024 * 1024)).await.unwrap();
    assert!(retire_guild_enrollment(&f.db, 10, 100).await.is_err());
    assert!(enroll_guild(&f.db, 10, 101).await.is_err());
    assert_eq!(guild_enrollment(&f.db, 10).await.unwrap(), enrollment);
    assert!(nickname_reconcile_can_dispatch(&f.db, &job).await.unwrap());
    assert!(
        patch_settings(&f.db, 10, None, Some(Some("rejected".into())), None)
            .await
            .is_err()
    );
    assert_eq!(get_settings(&f.db, 10).await.unwrap().bot_name, "name");
}

#[tokio::test]
async fn matching_observation_only_writes_when_resuming_bound_dormant_work() {
    let f = Fixture::new();
    let identity = enroll_guild(&f.db, 10, 100).await.unwrap();
    let job = f.request(10, "name").await;
    f.db.with_conn(|conn| conn.execute_batch_bounded("CREATE TRIGGER reject_nickname_write BEFORE UPDATE ON nickname_reconcile BEGIN SELECT RAISE(ABORT,'injected write failure'); END", 64 * 1024 * 1024)).await.unwrap();
    assert_eq!(enroll_guild(&f.db, 10, 100).await.unwrap(), identity);
    f.db.with_conn(|conn| {
        conn.execute_batch_bounded("DROP TRIGGER reject_nickname_write", 64 * 1024 * 1024)
    })
    .await
    .unwrap();
    finish_nickname_reconcile(&f.db, &job, "dormant", "unknown_guild", "unknown", 1)
        .await
        .unwrap();
    f.db.with_conn(|conn| conn.execute_batch_bounded("CREATE TRIGGER reject_nickname_write BEFORE UPDATE ON nickname_reconcile BEGIN SELECT RAISE(ABORT,'injected write failure'); END", 64 * 1024 * 1024)).await.unwrap();
    assert!(enroll_guild(&f.db, 10, 100).await.is_err());
    assert_eq!(guild_enrollment(&f.db, 10).await.unwrap(), identity);
    assert_eq!(
        nickname_reconcile_status(&f.db, 10)
            .await
            .unwrap()
            .unwrap()
            .state,
        "dormant"
    );
    f.db.with_conn(|conn| {
        conn.execute_batch_bounded("DROP TRIGGER reject_nickname_write", 64 * 1024 * 1024)
    })
    .await
    .unwrap();
    assert_eq!(enroll_guild(&f.db, 10, 100).await.unwrap(), identity);
    let resumed = pending_nickname_reconciles(&f.db, 0, 1)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(resumed.job_id, job.job_id);
    assert!(resumed.generation > job.generation);
    assert!(!nickname_reconcile_can_dispatch(&f.db, &job).await.unwrap());
}

#[tokio::test]
async fn version_eight_upgrade_does_not_bind_existing_nickname_to_active_membership() {
    let path = tmp_db_path("nickname_version_eight");
    let conn = Connection::open(&path).unwrap();
    for (index, sql) in MIGRATIONS[..8].iter().enumerate() {
        conn.execute_batch_bounded(sql, 64 * 1024 * 1024).unwrap();
        conn.pragma_update(None, "user_version", (index + 1) as i64)
            .unwrap();
    }
    for (guild, state) in [(10, "pending"), (20, "terminal"), (30, "dormant")] {
        conn.execute_bounded(
            "INSERT INTO guild_enrollments(guild_id,id,joined_at_ns,active) VALUES(?1,?2,100,1)",
            params![guild, format!("membership-{guild}")],
            64 * 1024 * 1024,
        )
        .unwrap();
        conn.execute_bounded("INSERT INTO nickname_reconcile(guild_id,generation,state,attempt_count,next_attempt_at) VALUES(?1,3,?2,2,99)", params![guild, state], 64 * 1024 * 1024).unwrap();
    }
    drop(conn);
    let db = Db::open(&path, 2).unwrap();
    for guild in [10, 20, 30] {
        enroll_guild(&db, guild, 100).await.unwrap();
        assert_eq!(
            nickname_reconcile_status(&db, guild)
                .await
                .unwrap()
                .unwrap()
                .state,
            "dormant"
        );
    }
    assert!(
        pending_nickname_reconciles(&db, i64::MAX, 25)
            .await
            .unwrap()
            .is_empty()
    );
    request_nickname_reconcile(&db, 10).await.unwrap();
    let current = pending_nickname_reconciles(&db, 0, 25)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(current.enrollment_id, "membership-10");
    assert_eq!(current.nickname, "");
    drop(db);
    cleanup_db(&path);
}
