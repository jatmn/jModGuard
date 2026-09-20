use super::*;
use crate::bot_actions::Limits;
use crate::db::SqlExecutor;
use crate::db::params;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture {
    db: Db,
    path: std::path::PathBuf,
    request_time: i64,
}
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_actions_{:032x}.db",
            rand::random::<u128>()
        ));
        Self {
            db: Db::open(&path, 2).unwrap(),
            path,
            request_time: now(),
        }
    }
    fn request_key(&self, label: &str) -> String {
        format!("{}_{}", self.request_time, label)
    }
    async fn enqueue(&self, guild: i64, request: &str, action: Action) -> BotAction {
        db::enroll_guild(&self.db, guild, 1).await.unwrap();
        db::enqueue_bot_action(
            &self.db,
            guild,
            42,
            &self.request_key(request),
            action,
            now(),
            Limits::default(),
        )
        .await
        .unwrap()
    }
    async fn status(&self, job: &BotAction) -> BotAction {
        db::bot_action(&self.db, job.guild_id, &job.id)
            .await
            .unwrap()
            .unwrap()
    }
    async fn persisted_status(&self, job: &BotAction) -> (String, Option<String>) {
        let id = job.id.clone();
        self.db
            .with_conn(move |conn| {
                conn.query_row(
                    "SELECT state,outcome FROM bot_actions WHERE id=?1",
                    params![id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
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
        crate::tenant::remove_sidecars(&self.path);
    }
}
fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
fn unban(user_id: i64) -> Action {
    Action::Unban {
        user_id,
        reason: "test reason".into(),
    }
}
fn log_channel(channel_id: Option<i64>) -> Action {
    Action::SetLogChannel {
        event: "message_edit".into(),
        channel_id,
    }
}
struct MockDiscord {
    authorized: bool,
    membership_matches: bool,
    valid_channel: bool,
    outcome: Outcome,
    calls: AtomicUsize,
    checks: AtomicUsize,
    slow_authorization: bool,
    slow_unban: bool,
    check_error: Option<&'static str>,
}
impl Default for MockDiscord {
    fn default() -> Self {
        Self {
            authorized: true,
            membership_matches: true,
            valid_channel: true,
            outcome: Outcome::succeeded("unbanned"),
            calls: AtomicUsize::new(0),
            checks: AtomicUsize::new(0),
            slow_authorization: false,
            slow_unban: false,
            check_error: None,
        }
    }
}
#[serenity::async_trait]
impl DiscordActions for MockDiscord {
    async fn membership_matches(&self, _: &BotAction) -> Result<bool> {
        anyhow::ensure!(
            self.check_error != Some("membership"),
            "membership unavailable"
        );
        Ok(self.membership_matches)
    }
    async fn authorized(&self, _: &BotAction) -> Result<bool> {
        self.checks.fetch_add(1, Ordering::SeqCst);
        if self.slow_authorization {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
        anyhow::ensure!(
            self.check_error != Some("permission"),
            "permission unavailable"
        );
        Ok(self.authorized)
    }
    async fn valid_log_channel(&self, _: i64, _: i64) -> Result<bool> {
        anyhow::ensure!(self.check_error != Some("channel"), "channel unavailable");
        Ok(self.valid_channel)
    }
    async fn unban(&self, _: i64, _: i64, _: &str) -> Outcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.slow_unban {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
        self.outcome
    }
}

struct FenceDuringChannelValidation<'a> {
    db: &'a Db,
    destination: db::guild_stores::StoreId,
}
#[serenity::async_trait]
impl DiscordActions for FenceDuringChannelValidation<'_> {
    async fn membership_matches(&self, _: &BotAction) -> Result<bool> {
        Ok(true)
    }
    async fn authorized(&self, _: &BotAction) -> Result<bool> {
        Ok(true)
    }
    async fn valid_log_channel(&self, guild: i64, _: i64) -> Result<bool> {
        db::guild_stores::set_guild_write_fence(
            self.db,
            guild,
            self.destination.clone(),
            format!("{:032x}", 9u128),
            "freeze",
        )
        .await?;
        Ok(true)
    }
    async fn unban(&self, _: i64, _: i64, _: &str) -> Outcome {
        panic!("local log-channel action must not call unban")
    }
}

struct FenceDuringUnban<'a> {
    db: &'a Db,
    destination: db::guild_stores::StoreId,
}
#[serenity::async_trait]
impl DiscordActions for FenceDuringUnban<'_> {
    async fn membership_matches(&self, _: &BotAction) -> Result<bool> {
        Ok(true)
    }
    async fn authorized(&self, _: &BotAction) -> Result<bool> {
        Ok(true)
    }
    async fn valid_log_channel(&self, _: i64, _: i64) -> Result<bool> {
        panic!("unban action must not validate a log channel")
    }
    async fn unban(&self, guild: i64, _: i64, _: &str) -> Outcome {
        db::guild_stores::set_guild_write_fence(
            self.db,
            guild,
            self.destination.clone(),
            format!("{:032x}", 10u128),
            "freeze",
        )
        .await
        .unwrap();
        Outcome::succeeded("unbanned")
    }
}

#[tokio::test]
async fn move_fence_reports_its_own_pre_dispatch_outcome() {
    let f = Fixture::new();
    let job = f.enqueue(10, "fenced_unban", unban(99)).await;
    let claim = db::claim_bot_action(&f.db, now()).await.unwrap().unwrap();
    let destination = db::guild_stores::StoreId::new();
    db::guild_stores::register_store(
        &f.db,
        destination.clone(),
        db::guild_stores::StorePolicy::Dedicated { guild_id: 10 },
    )
    .await
    .unwrap();
    db::guild_stores::set_guild_write_fence(
        &f.db,
        10,
        destination,
        format!("{:032x}", 9u128),
        "freeze",
    )
    .await
    .unwrap();

    let discord = MockDiscord::default();
    execute(&f.db, &discord, &claim, &WorkerLimits::default())
        .await
        .unwrap();
    assert_eq!(discord.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        f.persisted_status(&job).await,
        ("failed".into(), Some("guild_move_fenced".into()))
    );
}

#[tokio::test]
async fn move_fence_racing_local_completion_blocks_the_write_transaction() {
    let f = Fixture::new();
    let job = f.enqueue(10, "fenced_log", log_channel(Some(123))).await;
    let claim = db::claim_bot_action(&f.db, now()).await.unwrap().unwrap();
    let destination = db::guild_stores::StoreId::new();
    db::guild_stores::register_store(
        &f.db,
        destination.clone(),
        db::guild_stores::StorePolicy::Dedicated { guild_id: 10 },
    )
    .await
    .unwrap();
    let discord = FenceDuringChannelValidation {
        db: &f.db,
        destination,
    };

    execute(&f.db, &discord, &claim, &WorkerLimits::default())
        .await
        .unwrap();
    assert_eq!(
        f.persisted_status(&job).await,
        ("failed".into(), Some("guild_move_fenced".into()))
    );
    assert!(db::list_log_channels(&f.db, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn move_fence_racing_unban_completion_blocks_local_mirror_writes() {
    let f = Fixture::new();
    f.db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO bans(guild_id,user_id,created_at) VALUES(10,99,'now')",
            (),
        )?;
        Ok(())
    })
    .await
    .unwrap();
    let job = f.enqueue(10, "fenced_unban_completion", unban(99)).await;
    let claim = db::claim_bot_action(&f.db, now()).await.unwrap().unwrap();
    let destination = db::guild_stores::StoreId::new();
    db::guild_stores::register_store(
        &f.db,
        destination.clone(),
        db::guild_stores::StorePolicy::Dedicated { guild_id: 10 },
    )
    .await
    .unwrap();
    let discord = FenceDuringUnban {
        db: &f.db,
        destination,
    };

    execute(&f.db, &discord, &claim, &WorkerLimits::default())
        .await
        .unwrap();
    assert_eq!(
        f.persisted_status(&job).await,
        ("succeeded".into(), Some("unbanned".into()))
    );
    let (active, events): (i64, i64) =
        f.db.with_conn(|conn| {
            Ok((
                conn.query_row(
                    "SELECT active FROM bans WHERE guild_id=10 AND user_id=99",
                    (),
                    |row| Ok(row.get(0)?),
                )?,
                conn.query_row(
                    "SELECT COUNT(*) FROM events WHERE guild_id=10 AND event_type='member_unban'",
                    (),
                    |row| Ok(row.get(0)?),
                )?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(active, 1);
    assert_eq!(events, 0);
}

#[tokio::test]
async fn copied_claim_can_dispatch_only_once_and_audit_is_atomic() {
    let f = Fixture::new();
    f.db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO bans(guild_id,user_id,created_at) VALUES(10,99,'now'),(20,99,'now')",
            params![],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    let initial_version = db::get_config_version(&f.db).await.unwrap();
    let job = f.enqueue(10, "unban", unban(99)).await;
    let (a, b) = tokio::join!(
        db::claim_bot_action(&f.db, now()),
        db::claim_bot_action(&f.db, now())
    );
    let claims: Vec<_> = [a.unwrap(), b.unwrap()].into_iter().flatten().collect();
    assert_eq!(claims.len(), 1);
    let mock = MockDiscord::default();
    let limits = WorkerLimits::default();
    let (a, b) = tokio::join!(
        execute(&f.db, &mock, &claims[0], &limits),
        execute(&f.db, &mock, &claims[0], &limits)
    );
    a.unwrap();
    b.unwrap();
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.status(&job).await.state, "succeeded");
    assert!(
        !db::finish_bot_action(&f.db, &claims[0], "succeeded", "unbanned")
            .await
            .unwrap()
    );
    assert!(!db::list_bans(&f.db, 10, 10).await.unwrap()[0].active);
    assert!(db::list_bans(&f.db, 20, 10).await.unwrap()[0].active);
    assert_eq!(
        db::prune_bot_action_history(
            &f.db,
            now() + i64::from(Limits::default().history_retention_secs) + 1,
            Limits::default()
        )
        .await
        .unwrap(),
        1
    );
    assert!(db::bot_action(&f.db, 10, &job.id).await.unwrap().is_none());
    assert!(!db::list_bans(&f.db, 10, 10).await.unwrap()[0].active);
    let audits =
        f.db.with_conn(|conn| {
            conn.query_row(
                "SELECT count(*) FROM events WHERE guild_id=10 AND event_type='member_unban'",
                params![],
                |row| Ok(row.get::<i64>(0)?),
            )
        })
        .await
        .unwrap();
    assert_eq!(audits, 1);
    assert_eq!(
        initial_version,
        db::get_config_version(&f.db).await.unwrap()
    );
}

#[tokio::test]
async fn production_drain_records_one_stable_truthful_case_for_a_queued_unban() {
    for (label, outcome, expected) in [
        (
            "succeeded",
            Outcome::succeeded("unbanned"),
            crate::db::guild_data::CaseOutcome::Succeeded,
        ),
        (
            "failed",
            Outcome::failed("discord_rejected"),
            crate::db::guild_data::CaseOutcome::Failed,
        ),
        (
            "uncertain",
            Outcome::uncertain("discord_outcome_unknown"),
            crate::db::guild_data::CaseOutcome::Uncertain,
        ),
    ] {
        let f = Fixture::new();
        let job = f.enqueue(10, label, unban(99)).await;
        let enrollment = db::guild_enrollment(&f.db, 10).await.unwrap().unwrap();
        let tenant = crate::tenant::TenantRuntime::open(
            &f.db,
            &crate::config::DbConfig {
                path: f.path.clone(),
                pool_size: 2,
                ..Default::default()
            },
            &crate::config::GuildStoresConfig::default(),
            &crate::config::EvidenceConfig {
                backend: "none".into(),
                ..Default::default()
            },
        )
        .unwrap();
        tenant.activate(enrollment.clone()).await.unwrap().unwrap();
        let discord = MockDiscord {
            outcome,
            ..Default::default()
        };

        drain_with_tenant(&f.db, &tenant, &discord, &WorkerLimits::default())
            .await
            .unwrap();

        let (store, scope) = tenant.live(enrollment).await.unwrap();
        let ids = store
            .case_ids(scope.clone(), None, 10, crate::tenant::case_limits())
            .await
            .unwrap();
        assert_eq!(ids.len(), 1, "{label}");
        let case = store
            .case(scope, ids[0].clone(), crate::tenant::case_limits())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            case.source,
            crate::db::guild_data::CaseSource::Application {
                operation_id: crate::db::guild_data::CaseId::try_from(job.id).unwrap(),
            },
            "{label}"
        );
        assert_eq!(case.action, crate::db::guild_data::CaseAction::Unban);
        assert_eq!(case.outcome, expected, "{label}");
        assert_eq!(case.actor_id, Some(42));
        assert_eq!(case.reason.as_deref(), Some("test reason"));
    }
}

#[tokio::test]
async fn revoked_permissions_never_dispatch() {
    let f = Fixture::new();
    let job = f.enqueue(10, "unban", unban(99)).await;
    let mock = MockDiscord {
        authorized: false,
        ..Default::default()
    };
    drain(&f.db, &mock, &WorkerLimits::default()).await.unwrap();
    assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
    let result = f.status(&job).await;
    assert_eq!(result.state, "failed");
    assert_eq!(result.outcome.as_deref(), Some("permission_revoked"));
}

#[tokio::test]
async fn cancelled_and_expired_work_never_dispatches() {
    let f = Fixture::new();
    let cancelled = f.enqueue(10, "cancel", unban(99)).await;
    assert!(
        db::cancel_bot_action(&f.db, 10, &cancelled.id)
            .await
            .unwrap()
    );
    let old = db::enqueue_bot_action(
        &f.db,
        10,
        42,
        &format!("{}_old", now() - 1000),
        unban(100),
        now() - 1000,
        Limits::default(),
    )
    .await
    .unwrap();
    let claimed = f.enqueue(10, "claim", unban(101)).await;
    let claim = db::claim_bot_action(&f.db, now()).await.unwrap().unwrap();
    assert_eq!(claim.id, claimed.id);
    assert!(db::cancel_bot_action(&f.db, 10, &claim.id).await.unwrap());
    let mock = MockDiscord::default();
    execute(&f.db, &mock, &claim, &WorkerLimits::default())
        .await
        .unwrap();
    assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
    assert_eq!(mock.checks.load(Ordering::SeqCst), 0);
    assert_eq!(f.status(&claimed).await.state, "cancelled");
    assert_eq!(
        f.status(&claimed).await.outcome.as_deref(),
        Some("cancelled_before_execution")
    );
    assert!(
        db::claim_bot_action(&f.db, now() + 1000)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(f.status(&old).await.state, "expired");
}

#[tokio::test]
async fn uncertain_and_failed_discord_results_do_not_change_ban_mirror() {
    for outcome in [
        Outcome::uncertain("discord_outcome_unknown"),
        Outcome::failed("discord_rejected"),
    ] {
        let f = Fixture::new();
        f.db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO bans(guild_id,user_id,created_at) VALUES(10,99,'now')",
                params![],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        let job = f.enqueue(10, "unban", unban(99)).await;
        let mock = MockDiscord {
            outcome,
            ..Default::default()
        };
        drain(&f.db, &mock, &WorkerLimits::default()).await.unwrap();
        drain(&f.db, &mock, &WorkerLimits::default()).await.unwrap();
        assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
        assert_eq!(f.status(&job).await.state, outcome.state);
        assert!(db::list_bans(&f.db, 10, 10).await.unwrap()[0].active);
        if outcome.state == "uncertain" {
            assert!(
                db::enqueue_bot_action(
                    &f.db,
                    10,
                    42,
                    &f.request_key("retry"),
                    unban(99),
                    now(),
                    Limits::default()
                )
                .await
                .is_err()
            );
        }
    }
}

#[tokio::test]
async fn failed_receipt_rolls_back_local_effects_without_resending_discord() {
    let f = Fixture::new();
    f.db.with_conn(|conn| {
        conn.execute("INSERT INTO bans(guild_id,user_id,created_at) VALUES(10,99,'now')", params![])?;
        conn.execute("CREATE TRIGGER reject_action_audit BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT,'injected receipt failure'); END", params![])?;
        Ok(())
    }).await.unwrap();
    let job = f.enqueue(10, "unban", unban(99)).await;
    let mock = MockDiscord::default();
    drain(&f.db, &mock, &WorkerLimits::default()).await.unwrap();
    assert_eq!(f.status(&job).await.state, "running");
    assert!(db::list_bans(&f.db, 10, 10).await.unwrap()[0].active);
    assert!(
        db::claim_bot_action(&f.db, now() + 1000)
            .await
            .unwrap()
            .is_none()
    );
    drain(&f.db, &mock, &WorkerLimits::default()).await.unwrap();
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.status(&job).await.state, "uncertain");
}

#[tokio::test]
async fn log_channel_validation_and_revision_are_bot_owned() {
    let f = Fixture::new();
    let initial = db::get_config_version(&f.db).await.unwrap();
    let invalid = f.enqueue(10, "invalid", log_channel(Some(123))).await;
    drain(
        &f.db,
        &MockDiscord {
            valid_channel: false,
            ..Default::default()
        },
        &WorkerLimits::default(),
    )
    .await
    .unwrap();
    assert_eq!(f.status(&invalid).await.state, "failed");
    assert!(db::list_log_channels(&f.db, 10).await.unwrap().is_empty());
    assert_eq!(initial, db::get_config_version(&f.db).await.unwrap());
    let valid = f.enqueue(10, "valid", log_channel(Some(456))).await;
    drain(&f.db, &MockDiscord::default(), &WorkerLimits::default())
        .await
        .unwrap();
    assert_eq!(f.status(&valid).await.state, "succeeded");
    assert_eq!(
        db::list_log_channels(&f.db, 10).await.unwrap(),
        vec![("message_edit".into(), 456)]
    );
    assert_eq!(initial + 1, db::get_config_version(&f.db).await.unwrap());
    f.enqueue(10, "clear", log_channel(None)).await;
    drain(&f.db, &MockDiscord::default(), &WorkerLimits::default())
        .await
        .unwrap();
    assert!(db::list_log_channels(&f.db, 10).await.unwrap().is_empty());
    assert_eq!(initial + 2, db::get_config_version(&f.db).await.unwrap());
}

#[tokio::test]
async fn direct_config_write_supersedes_in_flight_log_action() {
    let f = Fixture::new();
    f.enqueue(10, "queued", log_channel(Some(123))).await;
    let claim = db::claim_bot_action(&f.db, now()).await.unwrap().unwrap();
    assert!(db::begin_bot_action(&f.db, &claim, now()).await.unwrap());
    db::upsert_log_channel(&f.db, 10, "message_edit", 456)
        .await
        .unwrap();
    assert!(
        !db::finish_bot_action(&f.db, &claim, "succeeded", "log_channel_set")
            .await
            .unwrap()
    );
    assert_eq!(f.status(&claim).await.state, "cancelled");
    assert_eq!(
        db::list_log_channels(&f.db, 10).await.unwrap(),
        vec![("message_edit".into(), 456)]
    );
    f.enqueue(10, "clear", log_channel(Some(789))).await;
    db::delete_log_channel(&f.db, 10, "message_edit")
        .await
        .unwrap();
    drain(&f.db, &MockDiscord::default(), &WorkerLimits::default())
        .await
        .unwrap();
    assert!(db::list_log_channels(&f.db, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn timeout_before_discord_mutation_is_failed_without_dispatch() {
    let f = Fixture::new();
    let job = f.enqueue(10, "unban", unban(99)).await;
    let mock = MockDiscord {
        slow_authorization: true,
        ..Default::default()
    };
    let limits = WorkerLimits {
        request_timeout_secs: 1,
        ..WorkerLimits::default()
    };
    drain(&f.db, &mock, &limits).await.unwrap();
    assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.status(&job).await.state, "failed");
    assert_eq!(
        f.status(&job).await.outcome.as_deref(),
        Some("validation_timed_out")
    );
}

#[tokio::test]
async fn timeout_after_discord_dispatch_is_uncertain_and_never_resent() {
    let f = Fixture::new();
    let job = f.enqueue(10, "unban", unban(99)).await;
    let mock = MockDiscord {
        slow_unban: true,
        ..Default::default()
    };
    let limits = WorkerLimits {
        request_timeout_secs: 1,
        ..WorkerLimits::default()
    };
    drain(&f.db, &mock, &limits).await.unwrap();
    assert_eq!(f.status(&job).await.state, "uncertain");
    assert_eq!(
        f.status(&job).await.outcome.as_deref(),
        Some("request_timed_out")
    );
    drain(&f.db, &mock, &limits).await.unwrap();
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn changed_discord_membership_never_dispatches_old_queued_work() {
    let f = Fixture::new();
    let job = f.enqueue(10, "membership", unban(99)).await;
    let mock = MockDiscord {
        membership_matches: false,
        ..MockDiscord::default()
    };
    drain(&f.db, &mock, &WorkerLimits::default()).await.unwrap();
    assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
    assert_eq!(mock.checks.load(Ordering::SeqCst), 0);
    assert_eq!(
        f.status(&job).await.outcome.as_deref(),
        Some("bot_membership_changed")
    );
}

#[tokio::test]
async fn discord_timestamp_precision_skew_keeps_current_membership_valid() {
    struct CoarseDiscord {
        joined_at_ns: i64,
    }
    #[serenity::async_trait]
    impl DiscordActions for CoarseDiscord {
        async fn membership_matches(&self, _: &BotAction) -> Result<bool> {
            panic!("the live timestamp path must be used")
        }
        async fn membership_joined_at_ns(&self, _: &BotAction) -> Result<Option<i64>> {
            Ok(Some(self.joined_at_ns))
        }
        async fn authorized(&self, _: &BotAction) -> Result<bool> {
            Ok(true)
        }
        async fn valid_log_channel(&self, _: i64, _: i64) -> Result<bool> {
            Ok(true)
        }
        async fn unban(&self, _: i64, _: i64, _: &str) -> Outcome {
            panic!("this test dispatches only a local log-channel action")
        }
    }

    const PRECISE_JOIN: i64 = 1_591_971_316_123_000_000;
    const SECOND_ONLY_JOIN: i64 = 1_591_971_316_000_000_000;
    let f = Fixture::new();
    db::enroll_guild(&f.db, 10, PRECISE_JOIN).await.unwrap();
    let job = f
        .enqueue(10, "timestamp_precision", log_channel(Some(123)))
        .await;

    drain(
        &f.db,
        &CoarseDiscord {
            joined_at_ns: SECOND_ONLY_JOIN,
        },
        &WorkerLimits::default(),
    )
    .await
    .unwrap();

    assert_eq!(
        f.persisted_status(&job).await,
        ("succeeded".into(), Some("log_channel_set".into()))
    );
    assert_eq!(
        db::list_log_channels(&f.db, 10).await.unwrap(),
        vec![("message_edit".into(), 123)]
    );
}

#[tokio::test]
async fn later_same_second_membership_repairs_missed_rejoin_before_consuming_claim() {
    struct RejoinedDiscord;
    #[serenity::async_trait]
    impl DiscordActions for RejoinedDiscord {
        async fn membership_matches(&self, _: &BotAction) -> Result<bool> {
            Ok(false)
        }
        async fn membership_joined_at_ns(&self, _: &BotAction) -> Result<Option<i64>> {
            Ok(Some(2))
        }
        async fn authorized(&self, _: &BotAction) -> Result<bool> {
            panic!("a stale action must not reach actor authorization")
        }
        async fn valid_log_channel(&self, _: i64, _: i64) -> Result<bool> {
            panic!("a stale action must not validate its target channel")
        }
        async fn unban(&self, _: i64, _: i64, _: &str) -> Outcome {
            panic!("a stale action must not dispatch")
        }
    }

    let f = Fixture::new();
    let job = f
        .enqueue(10, "repair_membership", log_channel(Some(123)))
        .await;
    let app = crate::state::BotState::new(Default::default(), f.db.clone()).unwrap();
    let old = db::guild_enrollment(&f.db, 10).await.unwrap().unwrap();
    app.tenant.activate(old).await.unwrap();

    drain_with_state(&app, &RejoinedDiscord, &WorkerLimits::default())
        .await
        .unwrap();

    let current = db::guild_enrollment(&f.db, 10).await.unwrap().unwrap();
    assert_eq!(current.joined_at_ns, 2);
    assert_ne!(current.id.as_str(), job.enrollment_id.as_deref().unwrap());
    assert_eq!(
        f.persisted_status(&job).await,
        ("cancelled".into(), Some("enrollment_ended".into()))
    );
    assert!(db::list_log_channels(&f.db, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn move_fence_racing_membership_repair_finishes_the_claim_without_dispatch() {
    struct FencedRepair<'a> {
        db: &'a Db,
        destination: db::guild_stores::StoreId,
    }
    #[serenity::async_trait]
    impl DiscordActions for FencedRepair<'_> {
        async fn membership_matches(&self, _: &BotAction) -> Result<bool> {
            Ok(false)
        }
        async fn membership_joined_at_ns(&self, job: &BotAction) -> Result<Option<i64>> {
            db::guild_stores::set_guild_write_fence(
                self.db,
                job.guild_id,
                self.destination.clone(),
                format!("{:032x}", 11u128),
                "freeze",
            )
            .await?;
            Ok(Some(1_000_000_001))
        }
        async fn authorized(&self, _: &BotAction) -> Result<bool> {
            panic!("a fenced repair must not reach actor authorization")
        }
        async fn valid_log_channel(&self, _: i64, _: i64) -> Result<bool> {
            panic!("a fenced repair must not validate its target channel")
        }
        async fn unban(&self, _: i64, _: i64, _: &str) -> Outcome {
            panic!("a fenced repair must not dispatch")
        }
    }

    let f = Fixture::new();
    let job = f
        .enqueue(10, "fenced_membership_repair", log_channel(Some(123)))
        .await;
    let app = crate::state::BotState::new(Default::default(), f.db.clone()).unwrap();
    let old = db::guild_enrollment(&f.db, 10).await.unwrap().unwrap();
    app.tenant.activate(old).await.unwrap();
    let destination = db::guild_stores::StoreId::new();
    db::guild_stores::register_store(
        &f.db,
        destination.clone(),
        db::guild_stores::StorePolicy::Dedicated { guild_id: 10 },
    )
    .await
    .unwrap();

    drain_with_state(
        &app,
        &FencedRepair {
            db: &f.db,
            destination,
        },
        &WorkerLimits::default(),
    )
    .await
    .unwrap();

    assert_eq!(
        f.persisted_status(&job).await,
        ("failed".into(), Some("guild_move_fenced".into()))
    );
    assert_eq!(
        db::guild_enrollment(&f.db, 10)
            .await
            .unwrap()
            .unwrap()
            .joined_at_ns,
        1
    );
    assert!(db::list_log_channels(&f.db, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn departure_during_authorization_blocks_dispatch() {
    struct DepartDuringCheck<'a>(&'a Db);
    #[serenity::async_trait]
    impl DiscordActions for DepartDuringCheck<'_> {
        async fn membership_matches(&self, _: &BotAction) -> Result<bool> {
            Ok(true)
        }
        async fn authorized(&self, job: &BotAction) -> Result<bool> {
            db::retire_guild_enrollment(self.0, job.guild_id, job.joined_at_ns.unwrap()).await?;
            Ok(true)
        }
        async fn valid_log_channel(&self, _: i64, _: i64) -> Result<bool> {
            panic!("unexpected channel validation")
        }
        async fn unban(&self, _: i64, _: i64, _: &str) -> Outcome {
            panic!("must not dispatch after retirement")
        }
    }
    let f = Fixture::new();
    f.enqueue(10, "departure", unban(99)).await;
    drain(&f.db, &DepartDuringCheck(&f.db), &WorkerLimits::default())
        .await
        .unwrap();
    assert!(db::recent_bot_actions(&f.db, 10).await.unwrap().is_empty());
}

#[test]
fn unban_results_distinguish_unknown_ban_from_other_rejections() {
    assert_eq!(
        classify_unban_response(404, 10026),
        Outcome::succeeded("already_unbanned")
    );
    for (status, code) in [(403, 50013), (404, 10004), (404, 10003), (429, 0), (400, 0)] {
        assert_eq!(
            classify_unban_response(status, code),
            Outcome::failed("discord_rejected")
        );
    }
    for status in [408, 500, 502, 503] {
        assert_eq!(
            classify_unban_response(status, 0),
            Outcome::uncertain("discord_outcome_unknown")
        );
    }
}

#[tokio::test]
async fn verification_errors_do_not_dispatch_or_change_log_settings() {
    for (phase, expected) in [
        ("membership", "bot_membership_check_unavailable"),
        ("permission", "permission_check_unavailable"),
        ("channel", "channel_check_unavailable"),
    ] {
        let f = Fixture::new();
        let job = f.enqueue(10, phase, log_channel(Some(123))).await;
        let mock = MockDiscord {
            check_error: Some(phase),
            ..Default::default()
        };
        drain(&f.db, &mock, &WorkerLimits::default()).await.unwrap();
        let result = f.status(&job).await;
        assert_eq!(result.state, "failed");
        assert_eq!(result.outcome.as_deref(), Some(expected));
        assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
        assert!(db::list_log_channels(&f.db, 10).await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn invalid_worker_limits_leave_pending_work_untouched_and_batches_are_bounded() {
    let f = Fixture::new();
    let job = f.enqueue(10, "first", unban(98)).await;
    f.enqueue(10, "second", unban(99)).await;
    let mock = MockDiscord::default();
    for limits in [
        WorkerLimits {
            batch_size: 0,
            ..Default::default()
        },
        WorkerLimits {
            request_timeout_secs: 0,
            ..Default::default()
        },
        WorkerLimits {
            batch_size: 65,
            ..Default::default()
        },
        WorkerLimits {
            request_timeout_secs: 121,
            ..Default::default()
        },
    ] {
        assert!(drain(&f.db, &mock, &limits).await.is_err());
        assert_eq!(f.status(&job).await.state, "pending");
    }
    drain(
        &f.db,
        &mock,
        &WorkerLimits {
            batch_size: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        db::recent_bot_actions(&f.db, 10)
            .await
            .unwrap()
            .iter()
            .filter(|j| j.state == "pending")
            .count(),
        1
    );
    drain(&f.db, &mock, &WorkerLimits::default()).await.unwrap();
    assert_eq!(mock.calls.load(Ordering::SeqCst), 2);
    drain(&f.db, &mock, &WorkerLimits::default()).await.unwrap();
    assert_eq!(mock.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn modified_claim_payload_cannot_reach_discord() {
    let f = Fixture::new();
    f.enqueue(10, "claim", unban(99)).await;
    let original = db::claim_bot_action(&f.db, now()).await.unwrap().unwrap();
    let mut forged = original.clone();
    forged.action = unban(100);
    let mock = MockDiscord::default();
    execute(&f.db, &mock, &forged, &WorkerLimits::default())
        .await
        .unwrap();
    assert_eq!(mock.checks.load(Ordering::SeqCst), 0);
    assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.status(&original).await.state, "claimed");
    execute(&f.db, &mock, &original, &WorkerLimits::default())
        .await
        .unwrap();
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn rest_actor_permissions_use_current_roles_and_action_specific_authority() {
    use serenity::model::guild::{Member, PartialGuild, Role};
    use serenity::model::id::RoleId;
    let mut guild: PartialGuild = serde_json::from_value(serde_json::json!({
        "id":"10", "name":"fixture", "owner_id":"11", "verification_level":0,
        "default_message_notifications":0, "explicit_content_filter":0, "roles":[],
        "emojis":[], "features":[], "mfa_level":0, "system_channel_flags":0,
        "premium_tier":0, "preferred_locale":"en-US", "nsfw_level":0,
        "stickers":[], "premium_progress_bar_enabled":false
    }))
    .unwrap();
    let mut member = Member::default();
    member.user.id = UserId::new(42);
    let mut role = Role::default();
    role.id = RoleId::new(20);
    member.roles = vec![role.id];
    for (permissions, can_unban, can_configure) in [
        (Permissions::empty(), false, false),
        (Permissions::MANAGE_GUILD, false, true),
        (Permissions::BAN_MEMBERS, true, false),
        (Permissions::ADMINISTRATOR, true, true),
    ] {
        role.permissions = permissions;
        guild.roles.insert(role.id, role.clone());
        assert_eq!(actor_authorized(&guild, &member, &unban(99)), can_unban);
        assert_eq!(
            actor_authorized(&guild, &member, &log_channel(None)),
            can_configure
        );
    }
    member.roles.clear(); // A removed role cannot keep cached administrator authority.
    assert!(!actor_authorized(&guild, &member, &unban(99)));

    member.roles = vec![role.id];
    role.permissions = Permissions::BAN_MEMBERS | Permissions::MANAGE_GUILD;
    guild.roles.insert(role.id, role.clone());
    member.communication_disabled_until =
        Some(serenity::model::Timestamp::from_unix_timestamp(now() + 3600).unwrap());
    for action in [unban(99), log_channel(Some(123)), log_channel(None)] {
        assert!(!actor_authorized(&guild, &member, &action));
    }
    for timeout in [
        None,
        Some(serenity::model::Timestamp::from_unix_timestamp(now() - 1).unwrap()),
    ] {
        member.communication_disabled_until = timeout;
        assert!(actor_authorized(&guild, &member, &unban(99)));
        assert!(actor_authorized(&guild, &member, &log_channel(None)));
    }
    member.communication_disabled_until =
        Some(serenity::model::Timestamp::from_unix_timestamp(now() + 3600).unwrap());
    role.permissions = Permissions::ADMINISTRATOR;
    guild.roles.insert(role.id, role);
    assert!(actor_authorized(&guild, &member, &unban(99)));
    assert!(actor_authorized(&guild, &member, &log_channel(None)));
    member.roles.clear();
    guild.owner_id = member.user.id;
    assert!(actor_authorized(&guild, &member, &unban(99)));
    assert!(actor_authorized(&guild, &member, &log_channel(None)));
}

#[tokio::test]
async fn local_completion_deadline_does_not_discard_a_known_remote_outcome() {
    for action in [log_channel(Some(123)), log_channel(None), unban(99)] {
        let f = Fixture::new();
        f.enroll_old_action(action.clone()).await;
        let claim = db::claim_bot_action(&f.db, now() - 1000)
            .await
            .unwrap()
            .unwrap();
        assert!(
            db::begin_bot_action(&f.db, &claim, now() - 1000)
                .await
                .unwrap()
        );
        let version = db::get_config_version(&f.db).await.unwrap();
        assert!(
            db::finish_dispatched_bot_action(&f.db, &claim, "succeeded", "confirmed")
                .await
                .unwrap()
        );
        let status = f.status(&claim).await;
        if matches!(action, Action::Unban { .. }) {
            assert_eq!(status.state, "succeeded");
            assert_eq!(status.outcome.as_deref(), Some("confirmed"));
        } else {
            assert_eq!(status.state, "failed");
            assert_eq!(status.outcome.as_deref(), Some("expired_before_execution"));
            assert!(db::list_log_channels(&f.db, 10).await.unwrap().is_empty());
        }
        assert_eq!(db::get_config_version(&f.db).await.unwrap(), version);
    }
}

impl Fixture {
    async fn enroll_old_action(&self, action: Action) {
        db::enroll_guild(&self.db, 10, 1).await.unwrap();
        db::enqueue_bot_action(
            &self.db,
            10,
            42,
            &format!("{}_old", now() - 1000),
            action,
            now() - 1000,
            Limits::default(),
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn configuration_or_membership_change_during_channel_validation_wins() {
    use std::sync::Arc;
    struct PausedChannel {
        entered: Arc<tokio::sync::Notify>,
        resume: Arc<tokio::sync::Notify>,
    }
    #[serenity::async_trait]
    impl DiscordActions for PausedChannel {
        async fn membership_matches(&self, _: &BotAction) -> Result<bool> {
            Ok(true)
        }
        async fn authorized(&self, _: &BotAction) -> Result<bool> {
            Ok(true)
        }
        async fn valid_log_channel(&self, _: i64, _: i64) -> Result<bool> {
            self.entered.notify_one();
            self.resume.notified().await;
            Ok(true)
        }
        async fn unban(&self, _: i64, _: i64, _: &str) -> Outcome {
            panic!("not an unban")
        }
    }
    for retire in [false, true] {
        let f = Fixture::new();
        let original = f.enqueue(10, "channel", log_channel(Some(123))).await;
        let claim = db::claim_bot_action(&f.db, now()).await.unwrap().unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let resume = Arc::new(tokio::sync::Notify::new());
        let discord = PausedChannel {
            entered: entered.clone(),
            resume: resume.clone(),
        };
        let worker_db = f.db.clone();
        let worker = tokio::spawn(async move {
            execute(&worker_db, &discord, &claim, &WorkerLimits::default()).await
        });
        entered.notified().await;
        if retire {
            db::retire_guild_enrollment(&f.db, 10, 1).await.unwrap();
            db::enroll_guild(&f.db, 10, 2).await.unwrap();
        }
        db::upsert_log_channel(&f.db, 10, "message_edit", 456)
            .await
            .unwrap();
        let before = db::get_primary_versions(&f.db).await.unwrap();
        resume.notify_one();
        worker.await.unwrap().unwrap();
        assert_eq!(
            db::list_log_channels(&f.db, 10).await.unwrap(),
            vec![("message_edit".into(), 456)]
        );
        assert_eq!(
            db::get_primary_versions(&f.db)
                .await
                .unwrap()
                .config_revision,
            before.config_revision
        );
        if retire {
            assert!(
                db::bot_action(&f.db, 10, &original.id)
                    .await
                    .unwrap()
                    .is_none()
            );
        } else {
            assert_eq!(f.status(&original).await.state, "cancelled");
        }
    }
}

#[tokio::test]
async fn aborted_worker_does_not_reclaim_a_dispatched_mutation() {
    use std::sync::Arc;
    struct PendingUnban(Arc<tokio::sync::Notify>);
    #[serenity::async_trait]
    impl DiscordActions for PendingUnban {
        async fn membership_matches(&self, _: &BotAction) -> Result<bool> {
            Ok(true)
        }
        async fn authorized(&self, _: &BotAction) -> Result<bool> {
            Ok(true)
        }
        async fn valid_log_channel(&self, _: i64, _: i64) -> Result<bool> {
            panic!("not a log change")
        }
        async fn unban(&self, _: i64, _: i64, _: &str) -> Outcome {
            self.0.notify_one();
            std::future::pending().await
        }
    }
    let f = Fixture::new();
    let original = f.enqueue(10, "abort", unban(99)).await;
    let claim = db::claim_bot_action(&f.db, now()).await.unwrap().unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let discord = PendingUnban(entered.clone());
    let worker_db = f.db.clone();
    let worker = tokio::spawn(async move {
        execute(&worker_db, &discord, &claim, &WorkerLimits::default()).await
    });
    entered.notified().await;
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    assert_eq!(f.status(&original).await.state, "running");
    assert!(
        db::claim_bot_action(&f.db, original.expires_at + 1)
            .await
            .unwrap()
            .is_none()
    );
    let mock = MockDiscord::default();
    drain(&f.db, &mock, &WorkerLimits::default()).await.unwrap();
    assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.status(&original).await.state, "uncertain");
}

#[tokio::test]
async fn rejected_completion_reports_outcome_without_overwriting_or_resending() {
    // Other tests exercise the same tracing callsite without a subscriber.
    // Capture diagnostics in an isolated process rather than installing a global
    // subscriber that would alter logging for the entire parallel test suite.
    const CHILD: &str = "JMODGUARD_RECEIPT_LOG_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "action_worker::tests::rejected_completion_reports_outcome_without_overwriting_or_resending",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tracing::instrument::WithSubscriber;

    #[derive(Clone)]
    struct CapturedLog(Arc<Mutex<Vec<u8>>>);
    impl Write for CapturedLog {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    struct SupersedeDuringDispatch<'a> {
        db: &'a Db,
        calls: AtomicUsize,
    }
    #[serenity::async_trait]
    impl DiscordActions for SupersedeDuringDispatch<'_> {
        async fn membership_matches(&self, _: &BotAction) -> Result<bool> {
            Ok(true)
        }
        async fn authorized(&self, _: &BotAction) -> Result<bool> {
            Ok(true)
        }
        async fn valid_log_channel(&self, guild: i64, _: i64) -> Result<bool> {
            db::upsert_log_channel(self.db, guild, "message_edit", 456).await?;
            Ok(true)
        }
        async fn unban(&self, guild: i64, _: i64, _: &str) -> Outcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            db::retire_guild_enrollment(self.db, guild, 1)
                .await
                .unwrap();
            db::enroll_guild(self.db, guild, 2).await.unwrap();
            Outcome::succeeded("unbanned")
        }
    }

    let log = CapturedLog(Arc::new(Mutex::new(Vec::new())));
    let writer = log.clone();
    let subscriber = tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish(),
    );
    for action in [unban(99), log_channel(Some(123))] {
        log.0.lock().unwrap().clear();
        let f = Fixture::new();
        let remote = matches!(action, Action::Unban { .. });
        // Keep the retired receipt until the placed-guild erasure worker runs.
        let store = db::guild_stores::StoreId::new();
        db::guild_stores::register_store(
            &f.db,
            store.clone(),
            db::guild_stores::StorePolicy::Shared,
        )
        .await
        .unwrap();
        db::guild_stores::assign_initial_placement(&f.db, 10, store)
            .await
            .unwrap();
        let job = f.enqueue(10, "rejected_receipt", action).await;
        let claim = db::claim_bot_action(&f.db, now()).await.unwrap().unwrap();
        let discord = SupersedeDuringDispatch {
            db: &f.db,
            calls: AtomicUsize::new(0),
        };
        execute(&f.db, &discord, &claim, &WorkerLimits::default())
            .with_subscriber(subscriber.clone())
            .await
            .unwrap();
        let output = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
        assert!(
            output.contains("bot action completion rejected"),
            "{output}"
        );
        assert!(output.contains(&job.id), "{output}");
        assert!(output.contains("reported_state=\"succeeded\""), "{output}");
        let outcome = if remote {
            "unbanned"
        } else {
            "log_channel_set"
        };
        assert!(output.contains(outcome), "{output}");
        assert!(
            output.contains(&format!("mutation_started={remote}")),
            "{output}"
        );
        execute(&f.db, &discord, &claim, &WorkerLimits::default())
            .await
            .unwrap();
        let id = job.id.clone();
        let (state, outcome): (String, String) =
            f.db.with_conn(move |conn| {
                conn.query_row(
                    "SELECT state,outcome FROM bot_actions WHERE id=?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
            })
            .await
            .unwrap();
        if remote {
            assert_eq!(
                (state.as_str(), outcome.as_str()),
                ("uncertain", "enrollment_ended")
            );
            assert_eq!(discord.calls.load(Ordering::SeqCst), 1);
        } else {
            assert_eq!(
                (state.as_str(), outcome.as_str()),
                ("cancelled", "superseded_by_config_write")
            );
            assert_eq!(
                db::list_log_channels(&f.db, 10).await.unwrap(),
                vec![("message_edit".into(), 456)]
            );
            assert_eq!(discord.calls.load(Ordering::SeqCst), 0);
        }
        assert!(
            db::query_events(&f.db, 10, None, None, 10)
                .await
                .unwrap()
                .is_empty()
        );
        let reopened = Db::open(&f.path, 2).unwrap();
        assert!(
            db::claim_bot_action(&reopened, now() + 1000)
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn known_noop_unban_does_not_consume_a_future_external_echo() {
    for (outcome, expect_echo) in [
        (Outcome::succeeded("already_unbanned"), false),
        (Outcome::succeeded("unbanned"), true),
        (Outcome::failed("discord_rejected"), false),
        (Outcome::uncertain("discord_outcome_unknown"), true),
    ] {
        let f = Fixture::new();
        let app = crate::state::BotState::new(Default::default(), f.db.clone()).unwrap();
        let job = f.enqueue(10, "echo", unban(99)).await;
        let e = db::guild_enrollment(&f.db, 10).await.unwrap().unwrap();
        app.tenant.activate(e).await.unwrap();
        let mock = MockDiscord {
            outcome,
            ..Default::default()
        };
        drain_with_state(&app, &mock, &WorkerLimits::default())
            .await
            .unwrap();
        assert_eq!(f.status(&job).await.state, outcome.state);
        assert_eq!(
            app.consume_gateway_case_suppression(crate::state::GatewayCaseAction::Unban, 10, 99),
            expect_echo
        );
        assert!(!app.consume_gateway_case_suppression(
            crate::state::GatewayCaseAction::Unban,
            10,
            99
        ));
    }
}

#[tokio::test]
async fn queued_unban_links_a_bot_audit_without_subject_search() {
    let f = Fixture::new();
    let app = crate::state::BotState::new(Default::default(), f.db.clone()).unwrap();
    let job = f.enqueue(10, "audit", unban(99)).await;
    let enrollment = db::guild_enrollment(&f.db, 10).await.unwrap().unwrap();
    app.tenant.activate(enrollment.clone()).await.unwrap();
    drain_with_state(
        &app,
        &MockDiscord {
            outcome: Outcome::succeeded("unbanned"),
            ..Default::default()
        },
        &WorkerLimits::default(),
    )
    .await
    .unwrap();
    crate::cases::await_discord_audit(
        &app,
        7,
        8,
        crate::db::guild_data::CaseAction::Unban,
        10,
        99,
        321,
    )
    .await;
    crate::cases::await_discord_audit(
        &app,
        7,
        7,
        crate::db::guild_data::CaseAction::Unban,
        10,
        99,
        321,
    )
    .await;
    let (store, scope) = app.tenant.live(enrollment).await.unwrap();
    let by_application = store
        .case_for_source(
            scope.clone(),
            crate::db::guild_data::CaseSource::Application {
                operation_id: crate::db::guild_data::CaseId::try_from(job.id).unwrap(),
            },
        )
        .await
        .unwrap()
        .unwrap();
    let by_audit = store
        .case_for_source(
            scope,
            crate::db::guild_data::CaseSource::DiscordAudit { entry_id: 321 },
        )
        .await
        .unwrap()
        .unwrap();
    assert!(!by_audit.removed);
    assert_eq!(by_application.id, by_audit.id);
}

#[tokio::test]
async fn already_unbanned_does_not_bind_a_later_real_unban_audit() {
    let f = Fixture::new();
    let app = crate::state::BotState::new(Default::default(), f.db.clone()).unwrap();
    let noop_job = f.enqueue(10, "noop", unban(99)).await;
    let enrollment = db::guild_enrollment(&f.db, 10).await.unwrap().unwrap();
    app.tenant.activate(enrollment.clone()).await.unwrap();
    drain_with_state(
        &app,
        &MockDiscord {
            outcome: Outcome::succeeded("already_unbanned"),
            ..Default::default()
        },
        &WorkerLimits::default(),
    )
    .await
    .unwrap();
    let real_job = f.enqueue(10, "real", unban(99)).await;
    drain_with_state(
        &app,
        &MockDiscord {
            outcome: Outcome::succeeded("unbanned"),
            ..Default::default()
        },
        &WorkerLimits::default(),
    )
    .await
    .unwrap();
    crate::cases::await_discord_audit(
        &app,
        7,
        7,
        crate::db::guild_data::CaseAction::Unban,
        10,
        99,
        555,
    )
    .await;
    let (store, scope) = app.tenant.live(enrollment).await.unwrap();
    let noop_case = store
        .case_for_source(
            scope.clone(),
            crate::db::guild_data::CaseSource::Application {
                operation_id: crate::db::guild_data::CaseId::try_from(noop_job.id).unwrap(),
            },
        )
        .await
        .unwrap()
        .unwrap();
    let real_case = store
        .case_for_source(
            scope.clone(),
            crate::db::guild_data::CaseSource::Application {
                operation_id: crate::db::guild_data::CaseId::try_from(real_job.id).unwrap(),
            },
        )
        .await
        .unwrap()
        .unwrap();
    let audit_case = store
        .case_for_source(
            scope,
            crate::db::guild_data::CaseSource::DiscordAudit { entry_id: 555 },
        )
        .await
        .unwrap();
    assert_ne!(noop_case.id, real_case.id);
    assert_eq!(
        audit_case.as_ref().map(|m| m.id.as_str()),
        Some(real_case.id.as_str())
    );
}
