use super::*;
use crate::bot_actions::{Action, Limits};

struct Fixture {
    db: Db,
    path: PathBuf,
}
impl Fixture {
    async fn new() -> Self {
        let path = tmp_db_path("action_queue");
        let db = Db::open(&path, 2).unwrap();
        for guild in [10, 20, 21, 22, 23] {
            enroll_guild(&db, guild, 100).await.unwrap();
        }
        Self { db, path }
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
    let f = Fixture::new().await;
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
    let f = Fixture::new().await;
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
    assert!(
        !finish_bot_action(&f.db, job, "succeeded", "not_started")
            .await
            .unwrap()
    );
    assert!(begin_bot_action(&f.db, job, 1001).await.unwrap());
    assert!(!cancel_bot_action(&f.db, 10, &job.id).await.unwrap());
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
    let f = Fixture::new().await;
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
    // Running unban work occupies the target until a known finish; recovery
    // cancel must not pretend the Discord mutation never started.
    assert!(
        finish_bot_action(&reopened, &running, "failed", "confirmed_no_effect")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn move_fence_does_not_skip_overdue_expiry_when_no_unfenced_work_is_due() {
    let f = Fixture::new().await;
    let job = f.enqueue(10, "fenced_expiry", unban(99)).await;
    let destination = crate::db::guild_stores::StoreId::new();
    crate::db::guild_stores::register_store(
        &f.db,
        destination.clone(),
        crate::db::guild_stores::StorePolicy::Dedicated { guild_id: 10 },
    )
    .await
    .unwrap();
    crate::db::guild_stores::set_guild_write_fence(
        &f.db,
        10,
        destination,
        format!("{:032x}", 11u128),
        "freeze",
    )
    .await
    .unwrap();
    assert!(claim_bot_action(&f.db, 1001).await.unwrap().is_none());
    let pending: String =
        f.db.with_conn({
            let id = job.id.clone();
            move |conn| {
                conn.query_row(
                    "SELECT state FROM bot_actions WHERE id=?1",
                    params![id],
                    |row| Ok(row.get(0)?),
                )
            }
        })
        .await
        .unwrap();
    assert_eq!(pending, "pending");
    assert!(claim_bot_action(&f.db, 1300).await.unwrap().is_none());
    let expired: String =
        f.db.with_conn({
            let id = job.id.clone();
            move |conn| {
                conn.query_row(
                    "SELECT state FROM bot_actions WHERE id=?1",
                    params![id],
                    |row| Ok(row.get(0)?),
                )
            }
        })
        .await
        .unwrap();
    assert_eq!(expired, "expired");
}

#[tokio::test]
async fn modified_claim_identity_cannot_authorize_execution_or_completion() {
    let f = Fixture::new().await;
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
    let f = Fixture::new().await;
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
    let f = Fixture::new().await;
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
    let f = Fixture::new().await;
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
    let f = Fixture::new().await;
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

#[tokio::test]
async fn queued_log_clears_only_invalidate_changed_mappings_and_commit_atomically() {
    for present in [false, true] {
        let f = Fixture::new().await;
        if present {
            upsert_log_channel(&f.db, 10, "member_join", 100)
                .await
                .unwrap();
        }
        f.enqueue(10, "queued_clear", channel(None)).await;
        let job = claim_bot_action(&f.db, 1001).await.unwrap().unwrap();
        assert!(begin_bot_action(&f.db, &job, 1001).await.unwrap());
        let before = get_version_state(&f.db).await.unwrap();
        let generation = f.db.config_write_generation();
        f.db.with_conn(|conn| conn.execute_batch("CREATE TRIGGER reject_clear_version BEFORE UPDATE ON meta WHEN NEW.key='config_version' BEGIN SELECT RAISE(ABORT,'version write rejected'); END")).await.unwrap();
        if present {
            assert!(
                finish_bot_action(&f.db, &job, "succeeded", "cleared")
                    .await
                    .is_err()
            );
            assert_eq!(f.state(&job).await, "running");
            assert_eq!(list_log_channels(&f.db, 10).await.unwrap()[0].1, 100);
            assert_eq!(get_version_state(&f.db).await.unwrap(), before);
            assert_eq!(f.db.config_write_generation(), generation);
            f.db.with_conn(|conn| conn.execute_batch("DROP TRIGGER reject_clear_version"))
                .await
                .unwrap();
        }
        // An absent mapping completes even when any version write would fail.
        assert!(
            finish_bot_action(&f.db, &job, "succeeded", "cleared")
                .await
                .unwrap()
        );
        assert_eq!(f.state(&job).await, "succeeded");
        assert!(list_log_channels(&f.db, 10).await.unwrap().is_empty());
        let after = get_version_state(&f.db).await.unwrap();
        if present {
            assert_eq!(after.config, before.config + 1);
            assert_ne!(after.config_revision, before.config_revision);
            assert_eq!(after.slash, before.slash);
            assert_eq!(after.slash_revision, before.slash_revision);
            assert!(f.db.config_write_generation() > generation);
        } else {
            assert_eq!(after, before);
            assert_eq!(f.db.config_write_generation(), generation);
        }
        assert!(
            !finish_bot_action(&f.db, &job, "succeeded", "duplicate")
                .await
                .unwrap()
        );
        assert_eq!(get_version_state(&f.db).await.unwrap(), after);
    }
}

#[tokio::test]
async fn claimed_unban_can_be_cancelled_and_releases_the_target() {
    let f = Fixture::new().await;
    let job = f.enqueue(10, "claimed_unban", unban(99)).await;
    let claimed = claim_bot_action(&f.db, 1001).await.unwrap().unwrap();
    assert_eq!(claimed.id, job.id);
    assert!(cancel_bot_action(&f.db, 10, &claimed.id).await.unwrap());
    assert_eq!(f.state(&claimed).await, "cancelled");
    assert!(!begin_bot_action(&f.db, &claimed, 1001).await.unwrap());
    assert!(
        !finish_bot_action(&f.db, &claimed, "succeeded", "late")
            .await
            .unwrap()
    );
    let replacement = f.enqueue(10, "claimed_unban_retry", unban(99)).await;
    assert_eq!(replacement.state, "pending");
    assert_ne!(replacement.id, claimed.id);
}

#[tokio::test]
async fn running_and_uncertain_log_channels_can_be_force_cancelled() {
    let f = Fixture::new().await;
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
        assert!(cancel_bot_action(&f.db, 10, &job.id).await.unwrap());
        assert_eq!(f.state(&job).await, "cancelled");
        assert!(!begin_bot_action(&f.db, &job, 1001).await.unwrap());
        assert!(
            !finish_bot_action(&f.db, &job, "succeeded", "late")
                .await
                .unwrap()
        );
        f.enqueue(10, &format!("{state}_after"), channel(Some(200)))
            .await;
        delete_log_channel(&f.db, 10, "member_join").await.unwrap();
    }
}

#[tokio::test]
async fn full_guild_queue_rejects_log_enqueue_without_cancelling_unrelated_work() {
    let f = Fixture::new().await;
    let limits = Limits {
        pending_per_guild: 2,
        ..Limits::default()
    };
    let first = f.enqueue(10, "full_a", unban(98)).await;
    let second = f.enqueue(10, "full_b", unban(99)).await;
    assert_eq!(f.state(&first).await, "pending");
    assert_eq!(f.state(&second).await, "pending");
    assert!(
        enqueue_bot_action(
            &f.db,
            10,
            42,
            "1000_log_when_full",
            channel(Some(100)),
            1000,
            limits
        )
        .await
        .is_err()
    );
    assert_eq!(f.state(&first).await, "pending");
    assert_eq!(f.state(&second).await, "pending");
}

#[tokio::test]
async fn log_supersede_on_admission_frees_the_occupying_slot_before_insert() {
    let f = Fixture::new().await;
    let limits = Limits {
        pending_per_guild: 2,
        ..Limits::default()
    };
    let stuck = f.enqueue(10, "stuck_log", channel(Some(100))).await;
    let blocker = f.enqueue(10, "blocker_unban", unban(98)).await;
    assert_eq!(f.state(&stuck).await, "pending");
    assert_eq!(f.state(&blocker).await, "pending");
    let replacement = enqueue_bot_action(
        &f.db,
        10,
        42,
        "1000_log_replacement",
        channel(Some(200)),
        1000,
        limits,
    )
    .await
    .unwrap();
    assert_eq!(replacement.state, "pending");
    assert_eq!(f.state(&stuck).await, "cancelled");
    assert_eq!(f.state(&blocker).await, "pending");
}

#[tokio::test]
async fn queued_log_channel_admission_supersedes_occupying_work() {
    let f = Fixture::new().await;
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
        let replacement = f
            .enqueue(10, &format!("{state}_newer"), channel(Some(200)))
            .await;
        assert_eq!(replacement.state, "pending");
        assert_eq!(f.state(&job).await, "cancelled");
        let stored = bot_action(&f.db, 10, &job.id).await.unwrap().unwrap();
        assert_eq!(
            stored.outcome.as_deref(),
            Some("superseded_by_newer_request")
        );
        assert!(!begin_bot_action(&f.db, &job, 1001).await.unwrap());
        assert!(
            !finish_bot_action(&f.db, &job, "succeeded", "late")
                .await
                .unwrap()
        );
        delete_log_channel(&f.db, 10, "member_join").await.unwrap();
    }
    let blocking = f.enqueue(10, "unban_block", unban(99)).await;
    let claimed = claim_bot_action(&f.db, 1001).await.unwrap().unwrap();
    assert_eq!(claimed.id, blocking.id);
    assert!(
        enqueue_bot_action(
            &f.db,
            10,
            42,
            "1000_unban_conflict",
            unban(99),
            1000,
            Limits::default()
        )
        .await
        .is_err()
    );
    assert_eq!(f.state(&claimed).await, "claimed");
}

#[test]
fn action_channel_ids_follow_shared_snowflake_input_and_string_output() {
    for value in ["123", "\"123\"", "9007199254740993", "\"9007199254740993\""] {
        let input = format!(
            "{{\"kind\":\"set_log_channel\",\"event\":\"member_join\",\"channel_id\":{value}}}"
        );
        let action: Action = serde_json::from_str(&input).unwrap();
        action.validate().unwrap();
        let output = serde_json::to_value(&action).unwrap();
        assert!(output["channel_id"].is_string());
        assert_eq!(serde_json::from_value::<Action>(output).unwrap(), action);
    }
    let clear: Action = serde_json::from_str(
        r#"{"kind":"set_log_channel","event":"member_join","channel_id":null}"#,
    )
    .unwrap();
    assert_eq!(clear, channel(None));
    assert!(serde_json::to_value(clear).unwrap()["channel_id"].is_null());
    for value in ["0", "-1", "\"0\"", "\"-1\""] {
        let action: Action = serde_json::from_str(&format!(
            "{{\"kind\":\"set_log_channel\",\"event\":\"member_join\",\"channel_id\":{value}}}"
        ))
        .unwrap();
        assert!(action.validate().is_err());
    }
    for value in [
        "true",
        "1.5",
        "{}",
        "[]",
        "\"invalid\"",
        "9223372036854775808",
    ] {
        assert!(
            serde_json::from_str::<Action>(&format!(
                "{{\"kind\":\"set_log_channel\",\"event\":\"member_join\",\"channel_id\":{value}}}"
            ))
            .is_err()
        );
    }
}

#[tokio::test]
async fn numeric_action_channel_input_survives_storage_restart_and_retry() {
    let f = Fixture::new().await;
    let action: Action = serde_json::from_str(
        r#"{"kind":"set_log_channel","event":"member_join","channel_id":9007199254740993}"#,
    )
    .unwrap();
    let first = f.enqueue(10, "numeric_channel", action.clone()).await;
    let reopened = Db::open(&f.path, 2).unwrap();
    let retry = enqueue_bot_action(
        &reopened,
        10,
        42,
        "1000_numeric_channel",
        channel(Some(9_007_199_254_740_993)),
        1001,
        Limits::default(),
    )
    .await
    .unwrap();
    assert_eq!(first.id, retry.id);
    let job = claim_bot_action(&reopened, 1001).await.unwrap().unwrap();
    assert_eq!(job.action, action);
    assert_eq!(
        serde_json::to_value(&job).unwrap()["action"]["channel_id"],
        "9007199254740993"
    );
    assert!(begin_bot_action(&reopened, &job, 1001).await.unwrap());
    assert!(
        finish_bot_action(&reopened, &job, "succeeded", "configured")
            .await
            .unwrap()
    );
    assert_eq!(
        list_log_channels(&reopened, 10).await.unwrap()[0].1,
        9_007_199_254_740_993
    );
}
