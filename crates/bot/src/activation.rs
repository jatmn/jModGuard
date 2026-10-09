//! One active bot during a rollout overlap.
//!
//! The lease is a short meta row taken under the writer lock. It is not a
//! process-lifetime transaction. Discord connect, the started notice, and rule
//! triggers stay closed until this process both holds that lease and has
//! received Discord Ready.

use crate::db::{self, Db};
use crate::{set_process_ready, shutdown_signal};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const RENEW_EVERY: Duration = Duration::from_secs(5);
/// In-flight rule work must finish inside the observed shutdown grace.
pub const HANDOFF_DEADLINE: Duration = Duration::from_secs(8);

pub struct Activation {
    instance: String,
    lock_held: AtomicBool,
    discord_connected: AtomicBool,
    shutting_down: Arc<AtomicBool>,
    cut_off: AtomicBool,
    cutoff_wait: tokio::sync::Notify,
    in_flight: std::sync::atomic::AtomicUsize,
    moderation_depth: std::sync::atomic::AtomicUsize,
    recovery: AtomicBool,
    #[cfg(test)]
    renewal_fault: AtomicBool,
}

pub struct RuleHold {
    activation: Arc<Activation>,
}

impl Drop for RuleHold {
    fn drop(&mut self) {
        self.activation
            .moderation_depth
            .fetch_sub(1, Ordering::Release);
        self.activation.in_flight.fetch_sub(1, Ordering::Release);
    }
}

/// Control work is in flight, but it does not block moderation.
pub struct ControlHold {
    activation: Arc<Activation>,
}

impl Drop for ControlHold {
    fn drop(&mut self) {
        self.activation.in_flight.fetch_sub(1, Ordering::Release);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterLockPoll {
    Waiting,
    Ready,
}

impl Activation {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            instance: fresh_instance(),
            lock_held: AtomicBool::new(false),
            discord_connected: AtomicBool::new(false),
            shutting_down: Arc::new(AtomicBool::new(false)),
            cut_off: AtomicBool::new(false),
            cutoff_wait: tokio::sync::Notify::new(),
            in_flight: std::sync::atomic::AtomicUsize::new(0),
            moderation_depth: std::sync::atomic::AtomicUsize::new(0),
            recovery: AtomicBool::new(false),
            #[cfg(test)]
            renewal_fault: AtomicBool::new(false),
        })
    }

    pub fn recovery_in_progress(&self) -> bool {
        self.recovery.load(Ordering::Acquire)
    }

    pub fn set_recovery_in_progress(&self, active: bool) {
        self.recovery.store(active, Ordering::Release);
    }

    #[cfg(test)]
    pub fn fail_next_renewal(&self) {
        self.renewal_fault.store(true, Ordering::Release);
    }

    pub fn instance(&self) -> &str {
        &self.instance
    }

    pub fn allows_discord_connect(&self) -> bool {
        self.lock_held.load(Ordering::Acquire) && !self.is_shutting_down()
    }

    pub fn allows_started_notice(&self) -> bool {
        self.allows_rule_trigger()
    }

    pub fn allows_rule_trigger(&self) -> bool {
        self.lock_held.load(Ordering::Acquire)
            && self.discord_connected.load(Ordering::Acquire)
            && !self.is_shutting_down()
    }

    /// In-flight work may still record until cut-off or lease loss.
    /// New intake uses [`Self::allows_rule_trigger`] and stops at shutdown.
    pub fn may_record(&self) -> bool {
        self.lock_held.load(Ordering::Acquire) && !self.cut_off.load(Ordering::Acquire)
    }

    pub fn hold_rule(self: &Arc<Self>) -> RuleHold {
        self.in_flight.fetch_add(1, Ordering::Release);
        self.moderation_depth.fetch_add(1, Ordering::Release);
        RuleHold {
            activation: Arc::clone(self),
        }
    }

    /// Admit one control job. `None` means recording has stopped, and `in_flight`
    /// is left unchanged. This does not increment moderation depth.
    pub fn hold_control(self: &Arc<Self>) -> Option<ControlHold> {
        if !self.may_record() {
            return None;
        }
        self.in_flight.fetch_add(1, Ordering::Release);
        if !self.may_record() {
            self.in_flight.fetch_sub(1, Ordering::Release);
            return None;
        }
        Some(ControlHold {
            activation: Arc::clone(self),
        })
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }

    pub fn moderation_depth(&self) -> usize {
        self.moderation_depth.load(Ordering::Acquire)
    }

    pub fn cut_off(&self) {
        self.cut_off.store(true, Ordering::Release);
        self.cutoff_wait.notify_waiters();
    }

    /// Resolves when [`Self::cut_off`] has run. The waiter is registered before
    /// the flag is read again, and the flag stays set for a waiter that arrives
    /// after cutoff.
    pub async fn until_cutoff(&self) {
        let notified = self.cutoff_wait.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.cut_off.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }

    pub fn note_discord_connected(&self) {
        self.discord_connected.store(true, Ordering::Release);
    }

    pub fn mark_lock_held(&self, held: bool) {
        self.lock_held.store(held, Ordering::Release);
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }

    /// Stop new intake. In-flight work may finish until [`Self::cut_off`].
    pub fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
    }

    /// Lease loss or a killed renewal. No further record, ban, or log.
    pub fn note_lease_lost(&self) {
        self.mark_lock_held(false);
        self.cut_off();
    }

    pub fn publish_process_ready(&self) {
        set_process_ready(self.allows_rule_trigger());
    }

    fn refuse_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shutting_down)
    }
}

/// One attempt of the production wait. Does not sleep.
pub async fn poll_writer_lock(db: &Db, activation: &Activation) -> anyhow::Result<WriterLockPoll> {
    if activation.is_shutting_down() {
        activation.mark_lock_held(false);
        return Ok(WriterLockPoll::Waiting);
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    if db::claim_active_bot(db, activation.instance(), now_ms, activation.refuse_flag()).await? {
        activation.mark_lock_held(true);
        Ok(WriterLockPoll::Ready)
    } else {
        activation.mark_lock_held(false);
        Ok(WriterLockPoll::Waiting)
    }
}

/// Production wait used before the Discord client is constructed.
///
/// Returns `Ok(false)` when shutdown wins. Returns `Ok(true)` as soon as the
/// lease is free, without sleeping. The sleep is only the startup backoff
/// while another instance still holds the lease.
pub async fn wait_for_writer_lock(db: &Db, activation: &Arc<Activation>) -> anyhow::Result<bool> {
    let budget = db::startup_retry_budget(db.is_remote());
    let started = Instant::now();
    let deadline = started + budget.budget;
    let mut attempt = 0_u32;
    loop {
        if activation.is_shutting_down() {
            return Ok(false);
        }
        attempt += 1;
        match poll_writer_lock(db, activation).await {
            Ok(WriterLockPoll::Ready) => {
                let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                tracing::info!(
                    attempt,
                    retried = attempt > 1,
                    elapsed_ms,
                    "active-bot lease held"
                );
                return Ok(true);
            }
            Ok(WriterLockPoll::Waiting) => {}
            Err(error) if db::is_startup_retryable(&error, db.is_remote()) => {
                tracing::info!(attempt, "active-bot lease not ready; retrying: {error:#}");
            }
            Err(error) => return Err(error),
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("timed out waiting for the active-bot lease");
        }
        let sleep_for =
            db::startup_backoff(attempt, budget.initial_backoff, budget.max_backoff).min(remaining);
        if sleep_for.is_zero() {
            anyhow::bail!("timed out waiting for the active-bot lease");
        }
        tokio::select! {
            _ = shutdown_signal() => return Ok(false),
            _ = tokio::time::sleep(sleep_for) => {}
        }
    }
}

pub fn spawn_renewal(db: Db, activation: Arc<Activation>) -> tokio::task::JoinHandle<()> {
    spawn_renewal_every(db, activation, RENEW_EVERY)
}

fn spawn_renewal_every(
    db: Db,
    activation: Arc<Activation>,
    every: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(every);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            if activation.is_shutting_down() {
                break;
            }
            #[cfg(test)]
            let fault = activation.renewal_fault.swap(false, Ordering::AcqRel);
            #[cfg(not(test))]
            let fault = false;
            let now_ms = chrono::Utc::now().timestamp_millis();
            let result = if fault {
                Err(anyhow::anyhow!("active-bot lease renewal failed"))
            } else {
                db::claim_active_bot(&db, activation.instance(), now_ms, activation.refuse_flag())
                    .await
            };
            if !settle_renewal(&activation, &result) {
                break;
            }
        }
    })
}

/// One path for a lost lease. `true` means keep renewing.
/// Shutdown leaves cutoff to the notice drain so those notices still send.
fn settle_renewal(activation: &Activation, result: &anyhow::Result<bool>) -> bool {
    if matches!(result, Ok(true)) {
        activation.mark_lock_held(true);
        return true;
    }
    if activation.is_shutting_down() {
        return false;
    }
    match result {
        Err(error) => tracing::warn!("active-bot lease renewal failed: {error:#}"),
        Ok(false) => tracing::warn!("active-bot lease lost"),
        Ok(true) => return true,
    }
    activation.note_lease_lost();
    set_process_ready(false);
    false
}

pub async fn finish_in_flight(activation: &Activation) {
    let deadline = Instant::now() + HANDOFF_DEADLINE;
    while activation.in_flight() > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A missing or short control secret only disables the listener. The lease and
/// moderation do not wait on it.
pub async fn claim_writer_lease(
    secret: &str,
    db: &Db,
    activation: &Arc<Activation>,
) -> anyhow::Result<bool> {
    if let Err(error) = jmodguard::control::load_writer_key(secret) {
        tracing::error!("control listener disabled: {error}");
    }
    // Refuse control jobs for this lease term before the claim returns. The
    // listener starts only after this function, and recovery's first read is
    // still ahead of it.
    activation.set_recovery_in_progress(true);
    let claimed = match wait_for_writer_lock(db, activation).await {
        Ok(claimed) => claimed,
        Err(error) => {
            activation.set_recovery_in_progress(false);
            return Err(error);
        }
    };
    if !claimed {
        activation.set_recovery_in_progress(false);
        return Ok(false);
    }
    let db = db.clone();
    let activation = Arc::clone(activation);
    tokio::spawn(async move {
        if let Err(error) = crate::recovery::run(&db, &activation).await {
            tracing::error!("maintenance recovery stopped: {error:#}");
        }
    });
    Ok(true)
}

/// Drain in-flight work, run shutdown notices while recording is still allowed,
/// then clear the lock and cancel in-flight Discord work. Notices stay inside
/// [`HANDOFF_DEADLINE`].
pub async fn drain_for_shutdown_notices(
    activation: &Activation,
    notices: impl std::future::Future<Output = ()> + Send,
) {
    let deadline = Instant::now() + HANDOFF_DEADLINE;
    while activation.in_flight() > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    if activation.may_record() && !remaining.is_zero() {
        let _ = tokio::time::timeout(remaining, notices).await;
    }
    activation.note_lease_lost();
}

fn fresh_instance() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let tick = COUNTER.fetch_add(1, Ordering::Relaxed) as u128;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let value = nanos ^ (tick << 64) ^ (std::process::id() as u128);
    format!("{value:032x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use jmodguard::db::SqlExecutor;
    use std::sync::Arc;
    use std::time::Instant;

    fn temp_db(label: &str) -> (std::path::PathBuf, Db) {
        let path = std::env::temp_dir().join(format!(
            "active_bot_{label}_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let db = Db::open(&path, 2).unwrap();
        (path, db)
    }

    fn cleanup(path: &std::path::Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn unready_bot_does_not_connect_post_started_or_act() {
        let idle = Activation::new();
        assert!(
            !idle.allows_discord_connect(),
            "a new bot must not connect before it holds the lease"
        );
        assert!(
            !idle.allows_started_notice(),
            "a new bot must not post started before it is ready"
        );
        assert!(
            !idle.allows_rule_trigger(),
            "a new bot must not act on a rule trigger before it is ready"
        );

        let (path, db) = temp_db("overlap");
        let holder = Activation::new();
        let started = Instant::now();
        assert!(wait_for_writer_lock(&db, &holder).await.unwrap());
        assert!(
            started.elapsed() < Duration::from_millis(800),
            "a free lease must return without the retry sleep"
        );
        assert!(holder.allows_discord_connect());
        assert!(!holder.allows_started_notice());
        assert!(!holder.allows_rule_trigger());

        let challenger = Activation::new();
        assert_eq!(
            poll_writer_lock(&db, &challenger).await.unwrap(),
            WriterLockPoll::Waiting
        );
        assert!(!challenger.allows_discord_connect());
        challenger.note_discord_connected();
        assert!(
            !challenger.allows_started_notice(),
            "discord alone must not post started"
        );
        assert!(
            !challenger.allows_rule_trigger(),
            "discord alone must not act on a rule"
        );

        db::release_active_bot(&db, holder.instance())
            .await
            .unwrap();
        assert_eq!(
            poll_writer_lock(&db, &challenger).await.unwrap(),
            WriterLockPoll::Ready
        );
        assert!(challenger.allows_discord_connect());
        assert!(challenger.allows_started_notice());
        assert!(challenger.allows_rule_trigger());

        assert!(challenger.may_record());
        challenger.begin_shutdown();
        assert!(!challenger.allows_discord_connect());
        assert!(!challenger.allows_started_notice());
        assert!(!challenger.allows_rule_trigger());
        assert!(
            challenger.may_record(),
            "shutdown stops intake and still lets in-flight work record"
        );
        let hold = challenger.hold_rule();
        assert_eq!(challenger.in_flight(), 1);
        drop(hold);
        assert_eq!(challenger.in_flight(), 0);
        challenger.cut_off();
        assert!(!challenger.may_record());
        assert!(challenger.hold_control().is_none());
        assert_eq!(challenger.in_flight(), 0);
        drop(db);
        cleanup(&path);
    }

    #[tokio::test]
    async fn missing_control_secret_still_claims_the_lease_and_records_a_ban() {
        let (path, db) = temp_db("secret");
        let activation = Activation::new();
        assert!(claim_writer_lease("", &db, &activation).await.unwrap());
        assert!(activation.lock_held.load(Ordering::Acquire));
        assert!(activation.may_record());
        let enrollment = db::enroll_guild(&db, 10, 100).await.unwrap().unwrap();
        let id = db::record_enrolled_ban_if_absent(
            &db,
            Some(enrollment),
            55,
            None,
            None,
            Some(1),
            None,
            "spam",
            None,
            db::BanRecord::Fresh,
        )
        .await
        .unwrap();
        assert!(
            id.is_some(),
            "a ban must be recorded without a control secret"
        );
        drop(db);
        cleanup(&path);
    }

    async fn wait_until_polled(task_started: &tokio::sync::Notify) {
        let notified = task_started.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(2)) => {
                panic!("the discord future must be polled while it is still pending");
            }
            _ = &mut notified => {}
        }
    }

    struct PolledPending {
        started: Arc<tokio::sync::Notify>,
    }

    impl std::future::Future for PolledPending {
        type Output = ();
        fn poll(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<()> {
            self.started.notify_waiters();
            std::task::Poll::Pending
        }
    }

    fn start_polled_discord(
        activation: &Arc<Activation>,
    ) -> (
        tokio::task::JoinHandle<crate::discord::DiscordCall<()>>,
        Arc<tokio::sync::Notify>,
    ) {
        let started = Arc::new(tokio::sync::Notify::new());
        let activation = Arc::clone(activation);
        let pending = PolledPending {
            started: Arc::clone(&started),
        };
        let task =
            tokio::spawn(
                async move { crate::discord::race_discord_call(&activation, pending).await },
            );
        (task, started)
    }

    async fn assert_cancelled(task: tokio::task::JoinHandle<crate::discord::DiscordCall<()>>) {
        let raced = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("lease loss must cancel the discord future")
            .unwrap();
        assert!(matches!(raced, crate::discord::DiscordCall::Cancelled));
    }

    #[tokio::test]
    async fn failed_renewal_cancels_an_inflight_discord_call() {
        let (path, db) = temp_db("renew-err");
        let activation = Activation::new();
        assert!(wait_for_writer_lock(&db, &activation).await.unwrap());
        let (task, started) = start_polled_discord(&activation);
        wait_until_polled(&started).await;
        activation.fail_next_renewal();
        let renewal = spawn_renewal_every(
            db.clone(),
            Arc::clone(&activation),
            Duration::from_millis(30),
        );
        assert_cancelled(task).await;
        assert!(!activation.may_record());
        renewal.abort();
        drop(db);
        cleanup(&path);
    }

    #[tokio::test]
    async fn expired_lease_cancels_an_inflight_discord_call() {
        let (path, db) = temp_db("renew-expire");
        let activation = Activation::new();
        let now = chrono::Utc::now().timestamp_millis();
        assert!(
            db::claim_active_bot(&db, activation.instance(), now, activation.refuse_flag())
                .await
                .unwrap()
        );
        activation.mark_lock_held(true);
        let (task, started) = start_polled_discord(&activation);
        wait_until_polled(&started).await;
        let challenger = Activation::new();
        assert!(
            db::claim_active_bot(
                &db,
                challenger.instance(),
                now.saturating_add(21_000),
                challenger.refuse_flag(),
            )
            .await
            .unwrap()
        );
        let renewal = spawn_renewal_every(
            db.clone(),
            Arc::clone(&activation),
            Duration::from_millis(30),
        );
        assert_cancelled(task).await;
        assert!(!activation.may_record());
        renewal.abort();
        drop(db);
        cleanup(&path);
    }

    #[tokio::test]
    async fn takeover_cancels_an_inflight_discord_call() {
        let (path, db) = temp_db("renew-takeover");
        let activation = Activation::new();
        assert!(wait_for_writer_lock(&db, &activation).await.unwrap());
        let (task, started) = start_polled_discord(&activation);
        wait_until_polled(&started).await;
        db::release_active_bot(&db, activation.instance())
            .await
            .unwrap();
        let challenger = Activation::new();
        assert!(
            db::claim_active_bot(
                &db,
                challenger.instance(),
                chrono::Utc::now().timestamp_millis(),
                challenger.refuse_flag(),
            )
            .await
            .unwrap()
        );
        let renewal = spawn_renewal_every(
            db.clone(),
            Arc::clone(&activation),
            Duration::from_millis(30),
        );
        assert_cancelled(task).await;
        assert!(!activation.may_record());
        renewal.abort();
        drop(db);
        cleanup(&path);
    }

    #[tokio::test]
    async fn shutdown_cancellation_cancels_an_inflight_discord_call_after_notices() {
        let (path, db) = temp_db("renew-shutdown");
        let activation = Activation::new();
        assert!(wait_for_writer_lock(&db, &activation).await.unwrap());
        let (task, started) = start_polled_discord(&activation);
        wait_until_polled(&started).await;
        activation.begin_shutdown();
        let renewal = spawn_renewal_every(
            db.clone(),
            Arc::clone(&activation),
            Duration::from_millis(20),
        );
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            activation.may_record(),
            "shutdown notices must still be allowed before the cutoff path"
        );
        let saw_notice = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&saw_notice);
        let during = Arc::clone(&activation);
        drain_for_shutdown_notices(&activation, async move {
            assert!(during.may_record());
            flag.store(true, Ordering::SeqCst);
        })
        .await;
        assert!(saw_notice.load(Ordering::SeqCst));
        assert_cancelled(task).await;
        assert!(!activation.may_record());
        renewal.abort();
        drop(db);
        cleanup(&path);
    }

    #[tokio::test]
    async fn release_cannot_clear_another_owners_lease() {
        let (path, db) = temp_db("release");
        let holder = Activation::new();
        assert!(wait_for_writer_lock(&db, &holder).await.unwrap());
        let other = Activation::new();
        db::release_active_bot(&db, other.instance()).await.unwrap();
        assert!(
            holder.may_record()
                || poll_writer_lock(&db, &holder).await.unwrap() == WriterLockPoll::Ready,
            "a different instance must not clear the held lease"
        );
        assert_eq!(
            poll_writer_lock(&db, &other).await.unwrap(),
            WriterLockPoll::Waiting
        );
        assert!(!other.may_record());
        db::release_active_bot(&db, holder.instance())
            .await
            .unwrap();
        holder.note_lease_lost();
        assert!(!holder.may_record());
        assert_eq!(
            poll_writer_lock(&db, &other).await.unwrap(),
            WriterLockPoll::Ready
        );
        assert!(other.may_record());
        drop(db);
        cleanup(&path);
    }

    #[tokio::test]
    async fn expired_lease_takes_no_further_action() {
        let (path, db) = temp_db("expire");
        let holder = Activation::new();
        let now = chrono::Utc::now().timestamp_millis();
        assert!(
            db::claim_active_bot(&db, holder.instance(), now, holder.refuse_flag())
                .await
                .unwrap()
        );
        holder.mark_lock_held(true);
        let challenger = Activation::new();
        let later = now.saturating_add(21_000);
        assert!(
            db::claim_active_bot(&db, challenger.instance(), later, challenger.refuse_flag())
                .await
                .unwrap()
        );
        holder.note_lease_lost();
        assert!(!holder.may_record());
        assert!(!holder.allows_rule_trigger());
        db::release_active_bot(&db, holder.instance())
            .await
            .unwrap();
        let stored = db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT value FROM meta WHERE key = 'active_bot_lease'",
                    db::params![],
                    |row| {
                        let value: String =
                            row.get(0).map_err(|error| anyhow::anyhow!("{error}"))?;
                        Ok(value)
                    },
                )
            })
            .await
            .unwrap();
        assert!(
            stored.starts_with(&format!("{} ", challenger.instance())),
            "a late release must leave the new owner's lease in place"
        );
        assert!(
            db::claim_active_bot(&db, challenger.instance(), later, challenger.refuse_flag())
                .await
                .unwrap(),
            "the expired owner must not have cleared the new lease"
        );
        challenger.mark_lock_held(true);
        challenger.note_discord_connected();
        assert!(challenger.allows_rule_trigger());
        drop(db);
        cleanup(&path);
    }

    #[test]
    fn control_hold_does_not_raise_moderation_depth() {
        let activation = Activation::new();
        assert!(activation.hold_control().is_none());
        assert_eq!(activation.in_flight(), 0);
        activation.mark_lock_held(true);
        let hold = activation.hold_control().expect("control admission");
        assert_eq!(activation.in_flight(), 1);
        assert_eq!(activation.moderation_depth(), 0);
        drop(hold);
        assert_eq!(activation.in_flight(), 0);
        activation.note_lease_lost();
        assert!(activation.hold_control().is_none());
        assert_eq!(activation.in_flight(), 0);
        assert_eq!(activation.moderation_depth(), 0);
    }
}
