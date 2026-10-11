use super::*;
use std::{collections::HashMap, sync::Mutex};
struct Fixture {
    db: Db,
    tenant: crate::tenant::TenantRuntime,
    path: std::path::PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_repair_{:032x}.db",
            rand::random::<u128>()
        ));
        Self {
            db: Db::open(&path, 2).unwrap(),
            tenant: {
                let db = Db::open(&path, 2).unwrap();
                let cfg = crate::config::BotProcessConfig::default();
                crate::tenant::TenantRuntime::open(&db, &cfg.db, &cfg.guild_stores, &cfg.evidence)
                    .unwrap()
            },
            path,
        }
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
struct Source {
    page: Vec<u64>,
    joins: HashMap<u64, i64>,
    calls: Mutex<Vec<(Option<u64>, u32)>>,
    member_calls: Mutex<Vec<u64>>,
    fail_page: bool,
    stall_member: bool,
}
impl Source {
    fn new(page: Vec<u64>) -> Self {
        Self {
            joins: page.iter().map(|id| (*id, 100)).collect(),
            page,
            calls: Mutex::default(),
            member_calls: Mutex::default(),
            fail_page: false,
            stall_member: false,
        }
    }
}
#[serenity::async_trait]
impl MembershipSource for Source {
    async fn guilds_after(&self, after: Option<u64>, limit: u32) -> Result<Vec<u64>> {
        self.calls.lock().unwrap().push((after, limit));
        anyhow::ensure!(!self.fail_page, "listing unavailable");
        Ok(self.page.clone())
    }
    async fn joined_at_ns(&self, guild: u64) -> Result<i64> {
        self.member_calls.lock().unwrap().push(guild);
        if self.stall_member {
            return std::future::pending().await;
        }
        self.joins
            .get(&guild)
            .copied()
            .context("membership unavailable")
    }
}
fn config() -> EnrollmentConfig {
    EnrollmentConfig {
        batch_size: 2,
        request_timeout_secs: 1,
        ..EnrollmentConfig::default()
    }
}

#[tokio::test]
async fn bounded_pages_repair_missing_joins_and_wait_between_sweeps() {
    let f = Fixture::new();
    let mut repair = EnrollmentRepair::new(config(), Instant::now()).unwrap();
    let source = Source::new(vec![20, 10]);
    let report = repair
        .step(&f.db, &f.tenant, &source)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.checked, 2);
    assert_eq!(report.confirmed, 2);
    assert!(!report.sweep_complete);
    assert_eq!(*source.calls.lock().unwrap(), vec![(None, 2)]);
    assert_eq!(*source.member_calls.lock().unwrap(), vec![10, 20]);
    assert_eq!(repair.after, Some(20));
    assert!(
        repair
            .step(&f.db, &f.tenant, &source)
            .await
            .unwrap()
            .is_none()
    );
    repair.next = Instant::now();
    let last = Source::new(vec![30]);
    let report = repair.step(&f.db, &f.tenant, &last).await.unwrap().unwrap();
    assert_eq!(*last.calls.lock().unwrap(), vec![(Some(20), 2)]);
    assert!(report.sweep_complete);
    assert_eq!(repair.after, None);
    assert!(repair.next > Instant::now() + Duration::from_secs(3500));
    for guild in [10, 20, 30] {
        assert!(db::guild_enrollment(&f.db, guild).await.unwrap().is_some());
    }
}

#[tokio::test]
async fn omitted_and_unreadable_memberships_are_never_retired() {
    let f = Fixture::new();
    let first = db::enroll_guild(&f.db, 10, 100).await.unwrap();
    let omitted = db::enroll_guild(&f.db, 30, 100).await.unwrap();
    let mut source = Source::new(vec![10, 20]);
    source.joins.remove(&10);
    let mut repair = EnrollmentRepair::new(config(), Instant::now()).unwrap();
    let report = repair
        .step(&f.db, &f.tenant, &source)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.failed, 1);
    assert_eq!(report.confirmed, 1);
    assert_eq!(db::guild_enrollment(&f.db, 10).await.unwrap(), first);
    assert_eq!(db::guild_enrollment(&f.db, 30).await.unwrap(), omitted);
    repair.next = Instant::now();
    assert!(
        repair
            .step(&f.db, &f.tenant, &Source::new(vec![]))
            .await
            .unwrap()
            .unwrap()
            .sweep_complete
    );
    assert_eq!(db::guild_enrollment(&f.db, 30).await.unwrap(), omitted);
}

#[tokio::test]
async fn invalid_pages_fail_before_any_observation_and_back_off() {
    let f = Fixture::new();
    for page in [
        vec![0],
        vec![10, 10],
        vec![10, 20, 30],
        vec![u64::MAX],
        vec![5],
    ] {
        let mut repair = EnrollmentRepair::new(config(), Instant::now()).unwrap();
        repair.after = Some(5);
        let source = Source::new(page);
        assert!(repair.step(&f.db, &f.tenant, &source).await.is_err());
        assert!(source.member_calls.lock().unwrap().is_empty());
        assert_eq!(repair.after, Some(5));
        assert!(
            repair
                .step(&f.db, &f.tenant, &source)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(source.calls.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn rest_repair_cannot_resurrect_retired_membership_but_recovers_new_join() {
    let f = Fixture::new();
    db::retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
    let mut repair = EnrollmentRepair::new(config(), Instant::now()).unwrap();
    let mut source = Source::new(vec![10]);
    assert_eq!(
        repair
            .step(&f.db, &f.tenant, &source)
            .await
            .unwrap()
            .unwrap()
            .refused,
        1
    );
    assert!(db::guild_enrollment(&f.db, 10).await.unwrap().is_none());
    source.joins.insert(10, 101);
    repair.next = Instant::now();
    assert_eq!(
        repair
            .step(&f.db, &f.tenant, &source)
            .await
            .unwrap()
            .unwrap()
            .confirmed,
        1
    );
    assert_eq!(
        db::guild_enrollment(&f.db, 10)
            .await
            .unwrap()
            .unwrap()
            .joined_at_ns,
        101
    );
}

#[tokio::test]
async fn healthy_repair_does_not_write_enrollment_or_nickname_state() {
    use crate::db::SqlExecutor;
    let f = Fixture::new();
    let before = db::enroll_guild(&f.db, 10, 100).await.unwrap();
    db::request_nickname_reconcile(&f.db, 10).await.unwrap();
    f.db.with_conn(|conn| {
        conn.execute_bounded("UPDATE nickname_reconcile SET state='terminal' WHERE guild_id=10",(),65_536)?;
        conn.execute_batch_bounded("CREATE TRIGGER block_enrollment BEFORE UPDATE ON guild_enrollments BEGIN SELECT RAISE(ABORT,'unexpected enrollment write'); END;
            CREATE TRIGGER block_nickname BEFORE UPDATE ON nickname_reconcile BEGIN SELECT RAISE(ABORT,'unexpected nickname write'); END",65_536)
    }).await.unwrap();
    let mut repair = EnrollmentRepair::new(config(), Instant::now()).unwrap();
    assert_eq!(
        repair
            .step(&f.db, &f.tenant, &Source::new(vec![10]))
            .await
            .unwrap()
            .unwrap()
            .confirmed,
        1
    );
    assert_eq!(db::guild_enrollment(&f.db, 10).await.unwrap(), before);
}

#[tokio::test]
async fn timeouts_and_listing_failures_are_bounded_and_non_destructive() {
    let f = Fixture::new();
    let mut repair = EnrollmentRepair::new(config(), Instant::now()).unwrap();
    let mut source = Source::new(vec![10]);
    source.fail_page = true;
    assert!(repair.step(&f.db, &f.tenant, &source).await.is_err());
    assert_eq!(repair.after, None);
    assert!(source.member_calls.lock().unwrap().is_empty());
    source.fail_page = false;
    source.stall_member = true;
    repair.next = Instant::now();
    let report = tokio::time::timeout(
        Duration::from_secs(3),
        repair.step(&f.db, &f.tenant, &source),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert_eq!(report.failed, 1);
    assert!(db::guild_enrollment(&f.db, 10).await.unwrap().is_none());
}

#[tokio::test]
async fn database_failure_preserves_cursor_then_a_later_pass_repairs_the_join() {
    let f = Fixture::new();
    f.db.with_conn(|conn| {
        conn.execute_batch_bounded(
            "CREATE TRIGGER fail_insert BEFORE INSERT ON guild_enrollments
        BEGIN SELECT RAISE(ABORT,'test database failure'); END",
            65_536,
        )
    })
    .await
    .unwrap();
    let mut repair = EnrollmentRepair::new(config(), Instant::now()).unwrap();
    let source = Source::new(vec![10, 20]);
    assert!(repair.step(&f.db, &f.tenant, &source).await.is_err());
    assert_eq!(repair.after, None);
    assert_eq!(source.member_calls.lock().unwrap().as_slice(), &[10]);
    assert!(repair.next > Instant::now() + Duration::from_secs(50));
    f.db.with_conn(|conn| conn.execute_batch_bounded("DROP TRIGGER fail_insert", 65_536))
        .await
        .unwrap();
    repair.next = Instant::now();
    assert_eq!(
        repair
            .step(&f.db, &f.tenant, &source)
            .await
            .unwrap()
            .unwrap()
            .confirmed,
        2
    );
}

#[tokio::test]
async fn gateway_observations_need_context_and_never_retire_on_unavailability() {
    let f = Fixture::new();
    observe_gateway_membership(&f.db, &f.tenant, 10, Some(100), false, true)
        .await
        .unwrap();
    let initial = db::guild_enrollment(&f.db, 10).await.unwrap();
    for present in [true, false] {
        observe_gateway_membership(&f.db, &f.tenant, 10, None, true, present)
            .await
            .unwrap();
        assert!(
            observe_gateway_membership(&f.db, &f.tenant, 10, None, false, present)
                .await
                .is_err()
        );
        assert!(
            observe_gateway_membership(&f.db, &f.tenant, 10, Some(0), false, present)
                .await
                .is_err()
        );
        assert!(
            observe_gateway_membership(&f.db, &f.tenant, u64::MAX, Some(100), false, present)
                .await
                .is_err()
        );
    }
    assert_eq!(db::guild_enrollment(&f.db, 10).await.unwrap(), initial);
    observe_gateway_membership(&f.db, &f.tenant, 10, Some(100), false, false)
        .await
        .unwrap();
    observe_gateway_membership(&f.db, &f.tenant, 10, Some(100), false, true)
        .await
        .unwrap();
    assert!(db::guild_enrollment(&f.db, 10).await.unwrap().is_none());
    observe_gateway_membership(&f.db, &f.tenant, 10, Some(101), false, true)
        .await
        .unwrap();
    assert_ne!(db::guild_enrollment(&f.db, 10).await.unwrap(), initial);
}

#[test]
fn discovery_waits_for_a_real_bot_identity_in_the_gateway_cache() {
    let cache = serenity::cache::Cache::default();
    let http = serenity::http::Http::new("test-token");
    assert_eq!(cache.current_user().id.get(), 1);
    assert!(DiscordMembership::from_cache(&http, &cache).is_none());
}

#[tokio::test]
async fn bot_cache_invalidation_tracks_committed_membership_changes() {
    use std::sync::atomic::Ordering;
    let f = Fixture::new();
    let mut cfg = crate::config::BotProcessConfig::default();
    cfg.db.path = f.path.clone();
    let app = crate::state::BotState::new(cfg, f.db.clone()).unwrap();
    let enrollment = db::enroll_guild(&app.db, 10, 100).await.unwrap().unwrap();
    app.tenant.activate(enrollment).await.unwrap();
    let epoch = app.member_scope_epoch.load(Ordering::SeqCst);
    let mut source = Source::new(vec![10]);
    let mut repair = EnrollmentRepair::new(config(), Instant::now()).unwrap();
    assert_eq!(
        repair
            .step_for_bot(&app, &source)
            .await
            .unwrap()
            .unwrap()
            .confirmed,
        1
    );
    assert_eq!(
        app.member_scope_epoch.load(Ordering::SeqCst),
        epoch,
        "unchanged confirmation must preserve cached scopes"
    );

    source.fail_page = true;
    repair.next = Instant::now();
    assert!(repair.step_for_bot(&app, &source).await.is_err());
    assert_eq!(app.member_scope_epoch.load(Ordering::SeqCst), epoch);
    source.fail_page = false;
    source.joins.clear();
    repair.next = Instant::now();
    assert_eq!(
        repair
            .step_for_bot(&app, &source)
            .await
            .unwrap()
            .unwrap()
            .failed,
        1
    );
    assert_eq!(app.member_scope_epoch.load(Ordering::SeqCst), epoch);

    app.db
        .with_conn(|conn| {
            conn.execute_batch_bounded(
                "CREATE TRIGGER fail_second_join BEFORE INSERT ON guild_enrollments
         WHEN NEW.guild_id=30 BEGIN SELECT RAISE(ABORT,'second join failed'); END",
                65_536,
            )
        })
        .await
        .unwrap();
    repair.next = Instant::now();
    assert!(
        repair
            .step_for_bot(&app, &Source::new(vec![20, 30]))
            .await
            .is_err()
    );
    assert!(db::guild_enrollment(&app.db, 20).await.unwrap().is_some());
    assert!(db::guild_enrollment(&app.db, 30).await.unwrap().is_none());
    assert_eq!(
        app.member_scope_epoch.load(Ordering::SeqCst),
        epoch + 1,
        "a committed prefix must invalidate even when a later join fails"
    );
}

// Shared with the local HTTP regression in commands: inspect the production
// scheduler state and make its time deadline eligible without sleeping.
pub(crate) fn assert_auth_stopped_after_deadline(repair: &mut EnrollmentRepair) {
    assert!(repair.retry.stopped, "401 must stop, not merely back off");
    assert!(
        !repair
            .retry
            .due(Instant::now() + Duration::from_secs(24 * 3600))
    );
    repair.next = Instant::now() - Duration::from_secs(1);
}
