use super::*;
use crate::bot_actions::{Action, Limits};

struct Fixture {
    db: Db,
    path: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let path = tmp_db_path("action_queue");
        Self {
            db: Db::open(&path, 2).unwrap(),
            path,
        }
    }
    async fn enqueue(&self, guild: i64, nonce: &str, action: Action) -> BotAction {
        enqueue_bot_action(
            &self.db,
            guild,
            42,
            &format!("1000_{nonce}"),
            action,
            1000,
            Limits::default(),
        )
        .await
        .unwrap()
    }
    async fn state(&self, job: &BotAction) -> String {
        bot_action(&self.db, job.guild_id, &job.id)
            .await
            .unwrap()
            .unwrap()
            .state
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
        }
    }
}
fn unban(user_id: i64) -> Action {
    Action::Unban {
        user_id,
        reason: "appeal accepted".into(),
    }
}
fn channel(id: Option<i64>) -> Action {
    Action::SetLogChannel {
        event: "member_join".into(),
        channel_id: id,
    }
}

#[tokio::test]
async fn admission_deduplicates_scopes_and_bounds_pending_work() {
    let f = Fixture::new();
    let version = get_config_version(&f.db).await.unwrap();
    let (a, b) = tokio::join!(
        enqueue_bot_action(
            &f.db,
            10,
            42,
            "1000_retry",
            unban(99),
            1000,
            Limits::default()
        ),
        enqueue_bot_action(
            &f.db,
            10,
            42,
            "1000_retry",
            unban(99),
            1000,
            Limits::default()
        )
    );
    let a = a.unwrap();
    assert_eq!(a.id, b.unwrap().id);
    for (actor, action) in [(43, unban(99)), (42, unban(98))] {
        assert!(
            enqueue_bot_action(
                &f.db,
                10,
                actor,
                "1000_retry",
                action,
                1000,
                Limits::default()
            )
            .await
            .is_err()
        );
    }
    assert!(
        enqueue_bot_action(
            &f.db,
            10,
            42,
            "1000_target",
            unban(99),
            1000,
            Limits::default()
        )
        .await
        .is_err()
    );
    let limits = Limits {
        pending_per_guild: 1,
        ..Limits::default()
    };
    assert!(
        enqueue_bot_action(&f.db, 10, 42, "1000_full", unban(98), 1000, limits)
            .await
            .is_err()
    );
    let other = f.enqueue(20, "retry", unban(99)).await;
    assert_ne!(a.id, other.id);
    assert!(bot_action(&f.db, 20, &a.id).await.unwrap().is_none());
    assert!(!cancel_bot_action(&f.db, 20, &a.id).await.unwrap());
    assert_eq!(recent_bot_actions(&f.db, 10).await.unwrap().len(), 1);
    for (guild, actor, action) in [
        (0, 42, unban(99)),
        (10, 0, unban(99)),
        (10, 42, unban(0)),
        (10, 42, channel(Some(-1))),
    ] {
        assert!(
            enqueue_bot_action(
                &f.db,
                guild,
                actor,
                "1000_bad",
                action,
                1000,
                Limits::default()
            )
            .await
            .is_err()
        );
    }
    assert_eq!(version, get_config_version(&f.db).await.unwrap());
}

#[tokio::test]
async fn claims_are_exclusive_and_execution_is_consumed_once() {
    let f = Fixture::new();
    let pending = f.enqueue(10, "claim", unban(99)).await;
    assert!(
        !finish_bot_action(&f.db, &pending, "succeeded", "not_started")
            .await
            .unwrap()
    );
    let (a, b) = tokio::join!(claim_bot_action(&f.db, 1001), claim_bot_action(&f.db, 1001));
    let claims: Vec<_> = [a.unwrap(), b.unwrap()].into_iter().flatten().collect();
    assert_eq!(claims.len(), 1);
    let job = &claims[0];
    assert!(job.is_claimed());
    assert!(!cancel_bot_action(&f.db, 10, &job.id).await.unwrap());
    assert!(
        !finish_bot_action(&f.db, job, "succeeded", "not_started")
            .await
            .unwrap()
    );
    assert!(begin_bot_action(&f.db, job, 1001).await.unwrap());
    assert!(!begin_bot_action(&f.db, job, 1001).await.unwrap());
    assert!(
        finish_bot_action(&f.db, job, "failed", "permission_denied")
            .await
            .unwrap()
    );
    assert!(
        !finish_bot_action(&f.db, job, "succeeded", "duplicate")
            .await
            .unwrap()
    );
    assert!(
        query_events(&f.db, 10, None, None, 20)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn expiry_never_retries_running_work_and_pending_cancellation_releases_target() {
    let f = Fixture::new();
    let pending = f.enqueue(10, "cancel", unban(99)).await;
    assert!(cancel_bot_action(&f.db, 10, &pending.id).await.unwrap());
    assert!(!cancel_bot_action(&f.db, 10, &pending.id).await.unwrap());
    f.enqueue(10, "expired_claim", unban(99)).await;
    let claimed = claim_bot_action(&f.db, 1001).await.unwrap().unwrap();
    assert!(!begin_bot_action(&f.db, &claimed, 1300).await.unwrap());
    assert!(claim_bot_action(&f.db, 1300).await.unwrap().is_none());
    assert_eq!(f.state(&claimed).await, "expired");
    f.enqueue(10, "running", unban(99)).await;
    let running = claim_bot_action(&f.db, 1001).await.unwrap().unwrap();
    assert!(begin_bot_action(&f.db, &running, 1001).await.unwrap());
    assert!(claim_bot_action(&f.db, 1300).await.unwrap().is_none());
    assert_eq!(f.state(&running).await, "uncertain");
    let reopened = Db::open(&f.path, 2).unwrap();
    assert!(claim_bot_action(&reopened, 1400).await.unwrap().is_none());
    assert!(
        enqueue_bot_action(
            &reopened,
            10,
            42,
            "1400_duplicate",
            unban(99),
            1400,
            Limits::default()
        )
        .await
        .is_err()
    );
    assert!(!cancel_bot_action(&reopened, 10, &running.id).await.unwrap());
    assert!(
        finish_bot_action(&reopened, &running, "failed", "confirmed_no_effect")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn modified_claim_identity_cannot_authorize_execution_or_completion() {
    let f = Fixture::new();
    f.enqueue(10, "identity", unban(99)).await;
    let original = claim_bot_action(&f.db, 1001).await.unwrap().unwrap();
    let mut variants = Vec::new();
    let mut job = original.clone();
    job.guild_id = 20;
    variants.push(job);
    let mut job = original.clone();
    job.actor_id = 43;
    variants.push(job);
    let mut job = original.clone();
    job.action = unban(98);
    variants.push(job);
    let mut job = original.clone();
    job.expires_at += 1;
    variants.push(job);
    let mut job = original.clone();
    job.claim = Some("forged".into());
    variants.push(job);
    let mut job = original.clone();
    job.claim = None;
    variants.push(job);
    for job in &variants {
        assert!(!begin_bot_action(&f.db, job, 1001).await.unwrap());
    }
    assert!(begin_bot_action(&f.db, &original, 1001).await.unwrap());
    for job in &variants {
        assert!(
            !finish_bot_action(&f.db, job, "succeeded", "forged")
                .await
                .unwrap()
        );
    }
    assert_eq!(f.state(&original).await, "running");
    assert!(
        query_events(&f.db, 10, None, None, 20)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        query_events(&f.db, 20, None, None, 20)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        finish_bot_action(&f.db, &original, "succeeded", "unbanned")
            .await
            .unwrap()
    );
    let event = query_events(&f.db, 10, Some("member_unban"), None, 20)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(event.user_id, Some(99));
    assert_eq!(event.actor_id, Some(42));
    let wire = serde_json::to_value(original).unwrap();
    assert!(wire.get("claim").is_none());
    assert_eq!(wire["guild_id"], "10");
    assert_eq!(wire["action"]["user_id"], "99");
}

#[tokio::test]
async fn completion_rolls_back_its_state_ban_change_and_audit_together() {
    let f = Fixture::new();
    record_ban(&f.db, 10, 99, Some(42), "spam").await.unwrap();
    record_ban(&f.db, 20, 99, Some(42), "other guild")
        .await
        .unwrap();
    f.enqueue(10, "atomic", unban(99)).await;
    let job = claim_bot_action(&f.db, 1001).await.unwrap().unwrap();
    assert!(begin_bot_action(&f.db, &job, 1001).await.unwrap());
    f.db.with_conn(|conn| conn.execute_batch("CREATE TRIGGER fail_audit BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT,'test audit failure'); END")).await.unwrap();
    assert!(
        finish_bot_action(&f.db, &job, "succeeded", "unbanned")
            .await
            .is_err()
    );
    assert_eq!(f.state(&job).await, "running");
    f.db.with_conn(|conn| {
        let active: i64 = conn.query_row("SELECT count(*) FROM bans WHERE active=1", (), |r| {
            Ok(r.get(0)?)
        })?;
        assert_eq!(active, 2);
        conn.execute_batch("DROP TRIGGER fail_audit")
    })
    .await
    .unwrap();
    let version = get_config_version(&f.db).await.unwrap();
    assert!(
        finish_bot_action(&f.db, &job, "succeeded", "unbanned")
            .await
            .unwrap()
    );
    assert!(
        !finish_bot_action(&f.db, &job, "succeeded", "duplicate")
            .await
            .unwrap()
    );
    assert_eq!(
        query_events(&f.db, 10, Some("member_unban"), None, 20)
            .await
            .unwrap()
            .len(),
        1
    );
    f.db.with_conn(|conn| {
        let active: i64 = conn.query_row(
            "SELECT count(*) FROM bans WHERE active=1 AND guild_id=10",
            (),
            |r| Ok(r.get(0)?),
        )?;
        assert_eq!(active, 0);
        let active: i64 = conn.query_row(
            "SELECT count(*) FROM bans WHERE active=1 AND guild_id=20",
            (),
            |r| Ok(r.get(0)?),
        )?;
        assert_eq!(active, 1);
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(version, get_config_version(&f.db).await.unwrap());
}

#[tokio::test]
async fn newer_direct_log_settings_supersede_old_work_and_cannot_be_overwritten() {
    let f = Fixture::new();
    for clear in [false, true] {
        f.enqueue(10, if clear { "clear" } else { "set" }, channel(Some(100)))
            .await;
        let job = claim_bot_action(&f.db, 1001).await.unwrap().unwrap();
        assert!(begin_bot_action(&f.db, &job, 1001).await.unwrap());
        let version = get_config_version(&f.db).await.unwrap();
        if clear {
            delete_log_channel(&f.db, 10, "member_join").await.unwrap();
        } else {
            upsert_log_channel(&f.db, 10, "member_join", 200)
                .await
                .unwrap();
        }
        assert_eq!(get_config_version(&f.db).await.unwrap(), version + 1);
        assert_eq!(f.state(&job).await, "cancelled");
        assert!(
            !finish_bot_action(&f.db, &job, "succeeded", "late")
                .await
                .unwrap()
        );
        let rows = list_log_channels(&f.db, 10).await.unwrap();
        if clear {
            assert!(rows.is_empty());
        } else {
            assert_eq!(rows[0].1, 200);
        }
    }
    let pending = f.enqueue(10, "pending", channel(Some(100))).await;
    let other = f.enqueue(20, "other", channel(Some(100))).await;
    upsert_log_channel(&f.db, 10, "member_join", 300)
        .await
        .unwrap();
    assert_eq!(f.state(&pending).await, "cancelled");
    assert_eq!(f.state(&other).await, "pending");
    let job = claim_bot_action(&f.db, 1001).await.unwrap().unwrap();
    assert!(begin_bot_action(&f.db, &job, 1001).await.unwrap());
    let version = get_config_version(&f.db).await.unwrap();
    assert!(
        finish_bot_action(&f.db, &job, "succeeded", "validated")
            .await
            .unwrap()
    );
    assert_eq!(get_config_version(&f.db).await.unwrap(), version + 1);
    assert_eq!(list_log_channels(&f.db, 20).await.unwrap()[0].1, 100);
    f.enqueue(20, "remove", channel(None)).await;
    let job = claim_bot_action(&f.db, 1001).await.unwrap().unwrap();
    assert!(begin_bot_action(&f.db, &job, 1001).await.unwrap());
    assert!(
        finish_bot_action(&f.db, &job, "succeeded", "removed")
            .await
            .unwrap()
    );
    assert!(list_log_channels(&f.db, 20).await.unwrap().is_empty());
}

#[tokio::test]
async fn absent_log_deletes_preserve_versions_but_supersede_queued_work() {
    let f = Fixture::new();
    let versions = get_version_state(&f.db).await.unwrap();
    let generation = f.db.config_write_generation();
    delete_log_channel(&f.db, 10, "member_join").await.unwrap();
    assert_eq!(get_version_state(&f.db).await.unwrap(), versions);
    assert_eq!(f.db.config_write_generation(), generation);

    for state in ["pending", "claimed", "running", "uncertain"] {
        let mut job = f.enqueue(10, state, channel(Some(100))).await;
        if state != "pending" {
            job = claim_bot_action(&f.db, 1001).await.unwrap().unwrap();
        }
        if matches!(state, "running" | "uncertain") {
            assert!(begin_bot_action(&f.db, &job, 1001).await.unwrap());
        }
        if state == "uncertain" {
            assert!(
                finish_bot_action(&f.db, &job, "uncertain", "unknown")
                    .await
                    .unwrap()
            );
        }
        assert_eq!(f.state(&job).await, state);
        delete_log_channel(&f.db, 10, "member_join").await.unwrap();
        assert_eq!(f.state(&job).await, "cancelled");
        assert!(!begin_bot_action(&f.db, &job, 1001).await.unwrap());
        assert!(
            !finish_bot_action(&f.db, &job, "succeeded", "late")
                .await
                .unwrap()
        );
        assert!(list_log_channels(&f.db, 10).await.unwrap().is_empty());
        assert_eq!(get_version_state(&f.db).await.unwrap(), versions);
        assert_eq!(f.db.config_write_generation(), generation);
    }
    // Repeating the clear after supersession is a no-op as well.
    delete_log_channel(&f.db, 10, "member_join").await.unwrap();
    assert_eq!(get_version_state(&f.db).await.unwrap(), versions);
}

#[tokio::test]
async fn failed_direct_config_write_does_not_supersede_pending_work() {
    let f = Fixture::new();
    upsert_log_channel(&f.db, 10, "member_join", 300)
        .await
        .unwrap();
    let pending = f.enqueue(10, "rollback", channel(Some(100))).await;
    f.db.with_conn(|conn|conn.execute_batch("CREATE TRIGGER fail_config_version BEFORE UPDATE ON meta WHEN NEW.key='config_version' BEGIN SELECT RAISE(ABORT,'test failure'); END")).await.unwrap();
    assert!(
        upsert_log_channel(&f.db, 10, "member_join", 200)
            .await
            .is_err()
    );
    assert!(delete_log_channel(&f.db, 10, "member_join").await.is_err());
    assert_eq!(list_log_channels(&f.db, 10).await.unwrap()[0].1, 300);
    assert_eq!(f.state(&pending).await, "pending");
}

#[test]
fn action_payloads_reject_unknown_operations_and_fields() {
    for json in [
        r#"{"kind":"execute","sql":"DELETE FROM bans"}"#,
        r#"{"kind":"unban","user_id":"99","reason":"ok","url":"https://example.test"}"#,
    ] {
        assert!(serde_json::from_str::<Action>(json).is_err());
    }
    assert!(
        Action::Unban {
            user_id: 99,
            reason: "bad\nreason".into()
        }
        .validate()
        .is_err()
    );
    assert!(
        Action::SetLogChannel {
            event: "unknown".into(),
            channel_id: None
        }
        .validate()
        .is_err()
    );
    let action = channel(Some(9_007_199_254_740_993));
    let json = serde_json::to_string(&action).unwrap();
    assert!(json.contains("\"9007199254740993\""));
    assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), action);
}
