use super::*;
use crate::bot_actions::{Action, Limits, RequestKeyError};

struct Fixture {
    db: Db,
    path: PathBuf,
}
impl Fixture {
    async fn new() -> Self {
        let path = tmp_db_path("action_history");
        let db = Db::open(&path, 2).unwrap();
        for guild in [10, 20, 21, 22, 23] {
            enroll_guild(&db, guild, 100).await.unwrap();
        }
        Self { db, path }
    }
    async fn enqueue(&self, nonce: &str, now: i64) -> BotAction {
        enqueue_bot_action(
            &self.db,
            10,
            42,
            &format!("{now}_{nonce}"),
            action(),
            now,
            Limits::default(),
        )
        .await
        .unwrap()
    }
    async fn mark(&self, job: &BotAction, state: &str, finished: i64) {
        let (id, state) = (job.id.clone(), state.to_owned());
        self.db
            .with_conn(move |conn| {
                conn.execute(
                    "UPDATE bot_actions SET state=?1,finished_at=?2 WHERE id=?3",
                    params![state, finished, id],
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }
    async fn floor(&self) -> i64 {
        self.db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT CAST(value AS INTEGER) FROM meta WHERE key='action_request_floor'",
                    (),
                    |r| Ok(r.get(0)?),
                )
            })
            .await
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
        }
    }
}
fn action() -> Action {
    Action::Unban {
        user_id: 99,
        reason: "appeal".into(),
    }
}
fn limits() -> Limits {
    Limits {
        history_retention_secs: 100,
        cleanup_batch_size: 2,
        ..Limits::default()
    }
}

#[tokio::test]
async fn pruning_is_bounded_preserves_unresolved_and_uses_completion_age() {
    let f = Fixture::new().await;
    let mut eligible = Vec::new();
    for state in ["succeeded", "failed", "cancelled", "expired"] {
        let job = f.enqueue(state, 1000).await;
        f.mark(&job, state, 1100).await;
        eligible.push(job);
    }
    let recent = f.enqueue("recent", 1000).await;
    f.mark(&recent, "succeeded", 1101).await;
    // A forward-skewed request key must not be deleted until that key expires.
    let future = enqueue_bot_action(
        &f.db,
        10,
        42,
        "1101_future",
        action(),
        1000,
        Limits::default(),
    )
    .await
    .unwrap();
    f.mark(&future, "failed", 1100).await;
    let mut unresolved = Vec::new();
    for (i, state) in ["pending", "claimed", "running", "uncertain"]
        .into_iter()
        .enumerate()
    {
        let job = enqueue_bot_action(
            &f.db,
            20 + i as i64,
            42,
            &format!("1000_{state}"),
            action(),
            1000,
            Limits::default(),
        )
        .await
        .unwrap();
        // Even an old finished_at does not make uncertain work safe to discard.
        f.mark(&job, state, 1001).await;
        unresolved.push(job);
    }
    let version = get_config_version(&f.db).await.unwrap();
    assert_eq!(
        prune_bot_action_history(&f.db, 1200, limits())
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        prune_bot_action_history(&f.db, 1200, limits())
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        prune_bot_action_history(&f.db, 1200, limits())
            .await
            .unwrap(),
        0
    );
    for job in eligible {
        assert!(
            bot_action(&f.db, job.guild_id, &job.id)
                .await
                .unwrap()
                .is_none()
        );
    }
    for job in unresolved.into_iter().chain([recent, future]) {
        assert!(
            bot_action(&f.db, job.guild_id, &job.id)
                .await
                .unwrap()
                .is_some()
        );
    }
    assert_eq!(version, get_config_version(&f.db).await.unwrap());
    assert_eq!(f.floor().await, 1100);
}

#[tokio::test]
async fn old_keys_cannot_replay_after_cleanup_restart_or_retention_increase() {
    let f = Fixture::new().await;
    let job = f.enqueue("retry", 1000).await;
    f.mark(&job, "succeeded", 1001).await;
    // An extant receipt can still acknowledge a retry after its admission window.
    assert_eq!(
        enqueue_bot_action(&f.db, 10, 42, "1000_retry", action(), 1200, limits())
            .await
            .unwrap()
            .id,
        job.id
    );
    assert_eq!(
        prune_bot_action_history(&f.db, 1200, limits())
            .await
            .unwrap(),
        1
    );
    let reopened = Db::open(&f.path, 2).unwrap();
    let error = enqueue_bot_action(
        &reopened,
        10,
        42,
        "1000_retry",
        action(),
        1200,
        Limits::default(),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<RequestKeyError>(),
        Some(RequestKeyError::Expired)
    ));
    // Neither a clock moving backwards nor a longer retention setting lowers the floor.
    assert_eq!(
        prune_bot_action_history(&reopened, 1050, Limits::default())
            .await
            .unwrap(),
        0
    );
    for key in ["1099_other", "1100_boundary"] {
        assert!(
            enqueue_bot_action(&reopened, 10, 42, key, action(), 1050, Limits::default())
                .await
                .is_err()
        );
    }
    assert_eq!(f.floor().await, 1100);
    assert!(
        enqueue_bot_action(
            &reopened,
            10,
            42,
            "1200_new",
            action(),
            1200,
            Limits::default()
        )
        .await
        .is_ok()
    );
}

#[tokio::test]
async fn cleanup_and_its_replay_fence_roll_back_together_and_idle_cleanup_does_not_write() {
    let f = Fixture::new().await;
    f.db.with_conn(|conn| {
        conn.execute_batch("CREATE TRIGGER reject_floor_update BEFORE UPDATE ON meta
            WHEN NEW.key='action_request_floor' BEGIN SELECT RAISE(ABORT,'test fence failure'); END")
    }).await.unwrap();
    assert_eq!(
        prune_bot_action_history(&f.db, 1200, limits())
            .await
            .unwrap(),
        0
    );
    let job = f.enqueue("rollback", 1000).await;
    f.mark(&job, "succeeded", 1001).await;
    assert!(
        prune_bot_action_history(&f.db, 1200, limits())
            .await
            .is_err()
    );
    assert!(bot_action(&f.db, 10, &job.id).await.unwrap().is_some());
    assert_eq!(f.floor().await, 0);
}

#[tokio::test]
async fn racing_cleanup_and_duplicate_admission_never_create_new_work() {
    let f = Fixture::new().await;
    let job = f.enqueue("race", 1000).await;
    f.mark(&job, "succeeded", 1001).await;
    let (cleanup, retry) = tokio::join!(
        prune_bot_action_history(&f.db, 1200, limits()),
        enqueue_bot_action(
            &f.db,
            10,
            42,
            "1000_race",
            action(),
            1200,
            Limits::default()
        )
    );
    assert_eq!(cleanup.unwrap(), 1);
    match retry {
        Ok(receipt) => assert_eq!(receipt.id, job.id),
        Err(error) => assert!(matches!(
            error.downcast_ref::<RequestKeyError>(),
            Some(RequestKeyError::Expired)
        )),
    }
    assert!(recent_bot_actions(&f.db, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn malformed_future_and_expired_ids_are_rejected_and_cleanup_is_indexed() {
    let f = Fixture::new().await;
    for key in [
        "no-timestamp",
        "",
        "0_zero",
        "01000_alias",
        "1000_",
        "-1_negative",
        "1301_future",
        "9223372036854775808_overflow",
        "900_expired",
    ] {
        assert!(
            enqueue_bot_action(&f.db, 10, 42, key, action(), 1000, limits())
                .await
                .is_err(),
            "accepted {key}"
        );
    }
    f.db.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "EXPLAIN QUERY PLAN SELECT id FROM bot_actions INDEXED BY bot_actions_finished
            WHERE state IN ('succeeded','failed','cancelled','expired') AND request_created_at>0
            AND max(finished_at,request_created_at)<=1100 ORDER BY max(finished_at,request_created_at),id LIMIT 2",
        )?;
        let details = stmt
            .query_map((), |r| Ok(r.get::<String>(3)?))?
            .collect::<Result<Vec<_>>>()?;
        assert!(
            details
                .iter()
                .any(|line| line.contains("bot_actions_finished")),
            "{details:?}"
        );
        assert!(
            !details.iter().any(|line| line.contains("TEMP B-TREE")),
            "{details:?}"
        );
        Ok(())
    })
    .await
    .unwrap();
}

#[test]
fn action_history_settings_have_defaults_and_reject_unbounded_batches() {
    let defaults: Limits = toml::from_str("").unwrap();
    assert_eq!(defaults.history_retention_secs, 604800);
    for settings in [
        "history_retention_secs=0",
        "cleanup_batch_size=0",
        "cleanup_batch_size=4097",
    ] {
        assert!(
            toml::from_str::<Limits>(settings)
                .unwrap()
                .validate()
                .is_err()
        );
    }
    toml::from_str::<Limits>("history_retention_secs=4294967295\ncleanup_batch_size=4096")
        .unwrap()
        .validate()
        .unwrap();
}
