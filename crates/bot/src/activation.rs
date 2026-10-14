//! One active bot during a rollout overlap.
//!
//! The lease is a short meta row taken under the writer lock. It is not a
//! process-lifetime transaction. Discord connect, the started notice, and rule
//! triggers stay closed until this process both holds that lease and has
//! received Discord Ready.

use crate::db::{self, Db};
use crate::{set_process_ready, shutdown_signal};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const RENEW_EVERY: Duration = Duration::from_secs(5);
const RENEW_START_MARGIN: Duration = Duration::from_millis(100);
/// Host clocks can disagree. The local deadline ends this far before the lease row.
const LEASE_SAFETY_MARGIN: Duration = Duration::from_secs(5);
/// In-flight rule work must finish inside the observed shutdown grace.
pub const HANDOFF_DEADLINE: Duration = Duration::from_secs(8);
/// Lost-lease exits release the row before terminating so a replacement need not
/// wait for TTL expiry.
const LEASE_RELEASE_BEFORE_EXIT: Duration = Duration::from_secs(2);

pub struct Activation {
    instance: String,
    lock_held: AtomicBool,
    discord_connected: AtomicBool,
    shutting_down: Arc<AtomicBool>,
    shutdown_wait: tokio::sync::Notify,
    cut_off: AtomicBool,
    cutoff_wait: tokio::sync::Notify,
    in_flight: std::sync::atomic::AtomicUsize,
    moderation_depth: std::sync::atomic::AtomicUsize,
    recovery: AtomicBool,
    /// `None` until a claim succeeds. Callers that only mark the flag stay open.
    authority_deadline: Mutex<Option<Instant>>,
    exit_hook: fn(i32),
    #[cfg(test)]
    startup_delay: Mutex<Duration>,
    #[cfg(test)]
    renewal_fault: AtomicBool,
    #[cfg(test)]
    renewal_fault_delay: Mutex<Duration>,
    #[cfg(test)]
    renewal_attempts: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    renewal_stopped_for_authority: AtomicBool,
    #[cfg(test)]
    renewal_loop_stopped_at: Mutex<Option<Instant>>,
    #[cfg(test)]
    renewal_stall: AtomicBool,
    #[cfg(test)]
    lease_ttl: Mutex<Duration>,
    #[cfg(test)]
    lease_margin: Mutex<Duration>,
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
            shutdown_wait: tokio::sync::Notify::new(),
            cut_off: AtomicBool::new(false),
            cutoff_wait: tokio::sync::Notify::new(),
            in_flight: std::sync::atomic::AtomicUsize::new(0),
            moderation_depth: std::sync::atomic::AtomicUsize::new(0),
            recovery: AtomicBool::new(false),
            authority_deadline: Mutex::new(None),
            #[cfg(not(test))]
            exit_hook: |code| std::process::exit(code),
            #[cfg(test)]
            exit_hook: |_| {},
            #[cfg(test)]
            startup_delay: Mutex::new(Duration::ZERO),
            #[cfg(test)]
            renewal_fault: AtomicBool::new(false),
            #[cfg(test)]
            renewal_fault_delay: Mutex::new(Duration::ZERO),
            #[cfg(test)]
            renewal_attempts: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            renewal_stopped_for_authority: AtomicBool::new(false),
            #[cfg(test)]
            renewal_loop_stopped_at: Mutex::new(None),
            #[cfg(test)]
            renewal_stall: AtomicBool::new(false),
            #[cfg(test)]
            lease_ttl: Mutex::new(Duration::from_millis(
                u64::try_from(db::ACTIVE_BOT_LEASE_TTL_MS).unwrap_or(20_000),
            )),
            #[cfg(test)]
            lease_margin: Mutex::new(LEASE_SAFETY_MARGIN),
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

    #[cfg(test)]
    pub fn stall_next_renewal(&self) {
        self.renewal_stall.store(true, Ordering::Release);
    }

    #[cfg(test)]
    pub fn renewal_stopped_for_authority(&self) -> bool {
        self.renewal_stopped_for_authority.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub fn renewal_loop_stopped_at(&self) -> Option<Instant> {
        *self
            .renewal_loop_stopped_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[cfg(test)]
    pub fn set_lease_window(&self, ttl: Duration, margin: Duration) {
        *self
            .lease_ttl
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = ttl;
        *self
            .lease_margin
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = margin;
    }

    pub fn instance(&self) -> &str {
        &self.instance
    }

    pub fn allows_discord_connect(&self) -> bool {
        self.lock_held.load(Ordering::Acquire)
            && !self.is_shutting_down()
            && !self.cut_off.load(Ordering::Acquire)
            && self.authority_current()
    }

    pub fn allows_started_notice(&self) -> bool {
        self.allows_rule_trigger()
    }

    pub fn allows_rule_trigger(&self) -> bool {
        self.lock_held.load(Ordering::Acquire)
            && self.discord_connected.load(Ordering::Acquire)
            && !self.is_shutting_down()
            && self.authority_current()
    }

    /// In-flight work may still record until cut-off, lease loss, or the
    /// confirmed authority deadline.
    /// New intake uses [`Self::allows_rule_trigger`] and stops at shutdown.
    pub fn may_record(&self) -> bool {
        self.lock_held.load(Ordering::Acquire)
            && !self.cut_off.load(Ordering::Acquire)
            && self.authority_current()
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
        // One renewal waiter; retain a permit if it has not entered backoff yet.
        self.shutdown_wait.notify_one();
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

    fn lease_ttl(&self) -> Duration {
        #[cfg(test)]
        {
            *self
                .lease_ttl
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }
        #[cfg(not(test))]
        {
            Duration::from_millis(u64::try_from(db::ACTIVE_BOT_LEASE_TTL_MS).unwrap_or(20_000))
        }
    }

    fn lease_margin(&self) -> Duration {
        #[cfg(test)]
        {
            *self
                .lease_margin
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }
        #[cfg(not(test))]
        {
            LEASE_SAFETY_MARGIN
        }
    }

    fn authority_deadline(&self) -> Option<Instant> {
        *self
            .authority_deadline
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A successful claim starts at `requested_at`, which is captured before the
    /// request. A process that has already cut off does not extend that horizon.
    fn note_authority(&self, requested_at: Instant) {
        if self.cut_off.load(Ordering::Acquire) {
            return;
        }
        let window = self.lease_ttl().saturating_sub(self.lease_margin());
        let deadline = requested_at.checked_add(window).unwrap_or(requested_at);
        *self
            .authority_deadline
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(deadline);
    }

    fn authority_current(&self) -> bool {
        match self.authority_deadline() {
            None => true,
            Some(deadline) => Instant::now() < deadline,
        }
    }
}

/// One attempt of the production wait. Does not sleep.
pub async fn poll_writer_lock(db: &Db, activation: &Activation) -> anyhow::Result<WriterLockPoll> {
    if activation.is_shutting_down() {
        activation.mark_lock_held(false);
        return Ok(WriterLockPoll::Waiting);
    }
    let requested_at = Instant::now();
    let now_ms = chrono::Utc::now().timestamp_millis();
    if db::claim_active_bot(db, activation.instance(), now_ms, activation.refuse_flag()).await? {
        activation.note_authority(requested_at);
        activation.mark_lock_held(true);
        Ok(WriterLockPoll::Ready)
    } else {
        activation.mark_lock_held(false);
        Ok(WriterLockPoll::Waiting)
    }
}

fn writer_lease_lost(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string().contains("active bot lease"))
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

fn spawn_renewal(
    db: Db,
    activation: Arc<Activation>,
    tenant: Option<crate::tenant::TenantRuntime>,
) -> tokio::task::JoinHandle<()> {
    spawn_renewal_every(db, activation, tenant, RENEW_EVERY)
}

fn spawn_renewal_every(
    db: Db,
    activation: Arc<Activation>,
    tenant: Option<crate::tenant::TenantRuntime>,
    every: Duration,
) -> tokio::task::JoinHandle<()> {
    use tracing::instrument::WithSubscriber;
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    tokio::spawn(async move {
        let mut armed = db.leased_generation();
        let mut interval = tokio::time::interval(every);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        let mut last_error = None;
        let mut loss_reason = "active-bot lease expired before renewal";
        loop {
            let lost_at_deadline = wait_for_renewal_tick(&activation, &mut interval, every).await;
            if lost_at_deadline {
                // Shutdown notices still record until the notice drain cuts off.
                if !activation.is_shutting_down() {
                    lose_lease(&activation);
                }
                break;
            }
            if activation.is_shutting_down() {
                break;
            }
            if !activation.authority_current() {
                lose_lease(&activation);
                break;
            }
            let mut backoff = Duration::from_millis(250);
            let started_at = Instant::now();
            let deadline = activation.authority_deadline().unwrap_or(started_at);
            // A hung stream must leave room for a fresh CAS. Keep this cap
            // fixed across retries, including windows shorter than six seconds;
            // the confirmed deadline still bounds their combined budget.
            let attempt_budget = deadline.saturating_duration_since(started_at) / 2;
            let (requested_at, result) = loop {
                let requested_at = Instant::now();
                if requested_at >= deadline {
                    loss_reason = "active-bot lease renewal exhausted its authority window";
                    break (requested_at, Err(anyhow::anyhow!(loss_reason)));
                }
                let attempt_deadline = (requested_at + attempt_budget).min(deadline);
                #[cfg(test)]
                activation.renewal_attempts.fetch_add(1, Ordering::AcqRel);
                #[cfg(test)]
                let fault = activation.renewal_fault.swap(false, Ordering::AcqRel);
                #[cfg(not(test))]
                let fault = false;
                #[cfg(test)]
                let stall = activation.renewal_stall.swap(false, Ordering::AcqRel);
                #[cfg(not(test))]
                let stall = false;
                let claim = async {
                    if fault {
                        #[cfg(test)]
                        {
                            let delay = *activation.renewal_fault_delay.lock().unwrap();
                            tokio::time::sleep(delay).await;
                        }
                        Err(anyhow::anyhow!("active-bot lease renewal failed: connection reset"))
                    } else if stall {
                        std::future::pending::<anyhow::Result<bool>>().await
                    } else {
                        db::renew_active_bot(
                            &db,
                            activation.instance(),
                            chrono::Utc::now().timestamp_millis(),
                            deadline,
                            activation.refuse_flag(),
                        )
                        .await
                    }
                };
                let result = match tokio::time::timeout_at(
                    tokio::time::Instant::from_std(attempt_deadline), claim,
                ).await {
                    Ok(result) => result,
                    Err(_) => Err(anyhow::anyhow!("active-bot lease renewal timed out")),
                };
                match result {
                    Err(error) => {
                        last_error = Some(format!("{error:#}"));
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if activation.is_shutting_down()
                            || remaining <= backoff
                            || db::is_database_operation_timeout(&error)
                        {
                            if remaining <= backoff && !activation.is_shutting_down() {
                                #[cfg(test)]
                                activation
                                    .renewal_stopped_for_authority
                                    .store(true, Ordering::Release);
                            }
                            loss_reason = if remaining <= backoff || db::is_database_operation_timeout(&error) {
                                "active-bot lease renewal exhausted its authority window"
                            } else {
                                "active-bot lease renewal interrupted by shutdown"
                            };
                            break (requested_at, Err(error));
                        }
                        tokio::select! {
                            biased;
                            _ = activation.shutdown_wait.notified() => {
                                loss_reason = "active-bot lease renewal interrupted by shutdown";
                                break (requested_at, Err(error));
                            }
                            _ = tokio::time::sleep(backoff) => {}
                        }
                        backoff = backoff.saturating_mul(2).min(Duration::from_secs(1));
                    }
                    Ok(false) => {
                        loss_reason = "active-bot lease renewal matched no row: another instance holds it, it expired, or a restore is in progress";
                        break (requested_at, Ok(false));
                    }
                    Ok(true) => break (requested_at, Ok(true)),
                }
            };
            if matches!(result, Ok(true)) {
                if !activation.authority_current() {
                    if !activation.is_shutting_down() {
                        loss_reason = "active-bot lease renewal completed too late";
                        lose_lease(&activation);
                    }
                    break;
                }
                last_error = None;
                activation.note_authority(requested_at);
                let generation = db.leased_generation();
                if generation != armed {
                    if let Some(tenant) = &tenant
                        && let Err(error) = tenant.raise_held_writer_epoch().await
                    {
                        if writer_lease_lost(&error) {
                            let _ = db::release_active_bot(&db, activation.instance()).await;
                            loss_reason = "active bot lease is not held";
                            last_error = Some(format!("{error:#}"));
                            lose_lease(&activation);
                        } else {
                            loss_reason = "guild writer epoch raise failed";
                            last_error = Some(format!("{error:#}"));
                            lose_lease(&activation);
                        }
                        break;
                    }
                    armed = generation;
                }
            }
            if !settle_renewal(&activation, &result) {
                if activation.is_shutting_down() {
                    tracing::info!(reason = loss_reason, "active-bot lease renewal stopped");
                }
                break;
            }
        }
        #[cfg(test)]
        {
            *activation
                .renewal_loop_stopped_at
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now());
        }
        if !activation.is_shutting_down() {
            // Cut off Discord work before exiting, even if the task ended after
            // an already-observed cutoff. A running, unleased bot is unhealthy.
            activation.note_lease_lost();
            set_process_ready(false);
            let instance = activation.instance();
            release_held_active_bot_before_exit(&db, instance).await;
            if activation.is_shutting_down() {
                return;
            }
            tracing::error!(
                reason = loss_reason,
                last_error = last_error.as_deref().unwrap_or("none"),
                "active-bot lease lost outside shutdown; exiting for restart"
            );
            (activation.exit_hook)(1);
        }
    }.with_subscriber(dispatch))
}

/// `true` when the confirmed deadline arrives before the next renewal tick.
async fn wait_for_renewal_tick(
    activation: &Activation,
    interval: &mut tokio::time::Interval,
    every: Duration,
) -> bool {
    let tick = interval.tick();
    tokio::pin!(tick);
    let Some(deadline) = activation.authority_deadline() else {
        tick.await;
        return false;
    };
    // A slow claim (or startup work) may leave less than one normal tick.
    // Wake early enough to start a renewal while confirmed authority remains.
    let renew_at = deadline
        .checked_sub(every.saturating_add(RENEW_START_MARGIN))
        .unwrap_or(deadline);
    tokio::select! {
        biased;
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => true,
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(renew_at)) => false,
        _ = &mut tick => false,
    }
}

fn lose_lease(activation: &Activation) {
    activation.note_lease_lost();
    set_process_ready(false);
}

async fn release_held_active_bot_before_exit(db: &Db, instance: &str) {
    let release = db::release_active_bot(db, instance);
    match tokio::time::timeout(LEASE_RELEASE_BEFORE_EXIT, release).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!("active-bot lease release before exit failed: {error:#}");
        }
        Err(_) => tracing::warn!("active-bot lease release before exit timed out"),
    }
}

/// One path for a lost lease. `true` means keep renewing.
/// Shutdown leaves cutoff to the notice drain so those notices still send.
fn settle_renewal(activation: &Activation, result: &anyhow::Result<bool>) -> bool {
    if matches!(result, Ok(true)) {
        if activation.cut_off.load(Ordering::Acquire) {
            return false;
        }
        activation.mark_lock_held(true);
        return true;
    }
    if activation.is_shutting_down() {
        return false;
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
    tenant: Option<&crate::tenant::TenantRuntime>,
) -> anyhow::Result<bool> {
    if let Err(error) = jmodguard::control::load_writer_key(secret) {
        tracing::error!("control listener disabled: {error}");
    }
    // Refuse control jobs for this lease term before the claim returns. The
    // listener starts only after this function, and recovery's first read is
    // still ahead of it.
    activation.set_recovery_in_progress(true);
    let previous = db.leased_generation();
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
    // Renew before any guild-store startup work can consume this claim's
    // authority window. Dropping the handle leaves renewal running.
    let _renewal = spawn_renewal(db.clone(), Arc::clone(activation), tenant.cloned());
    #[cfg(test)]
    {
        let delay = *activation.startup_delay.lock().unwrap();
        tokio::time::sleep(delay).await;
    }
    if db.leased_generation() != previous
        && let Some(tenant) = tenant
        && let Err(error) = tenant.raise_held_writer_epoch().await
    {
        activation.mark_lock_held(false);
        if writer_lease_lost(&error) {
            let _ = db::release_active_bot(db, activation.instance()).await;
        }
        activation.set_recovery_in_progress(false);
        return Err(error);
    }
    anyhow::ensure!(
        activation.may_record(),
        "active-bot lease expired during startup"
    );
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
        assert!(
            claim_writer_lease("", &db, &activation, None)
                .await
                .unwrap()
        );
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
            None,
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
    async fn transient_renewal_failure_retries_before_the_next_tick() {
        let (path, db) = temp_db("renew-transient");
        let activation = Activation::new();
        activation.set_lease_window(Duration::from_secs(3), Duration::from_millis(100));
        assert!(wait_for_writer_lock(&db, &activation).await.unwrap());
        let before = activation.authority_deadline().unwrap();
        let (task, started) = start_polled_discord(&activation);
        wait_until_polled(&started).await;
        activation.fail_next_renewal();
        let began = Instant::now();
        let renewal = spawn_renewal_every(
            db.clone(),
            Arc::clone(&activation),
            None,
            Duration::from_millis(500),
        );
        tokio::time::timeout(Duration::from_millis(950), async {
            while activation.authority_deadline() == Some(before)
                && !activation.cut_off.load(Ordering::Acquire)
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("retry must finish before the next 500 ms tick");
        assert!(activation.authority_deadline().unwrap() > before);
        assert!(began.elapsed() < Duration::from_millis(950));
        assert!(
            !activation.renewal_fault.load(Ordering::Acquire),
            "injected first attempt ran"
        );
        assert!(activation.may_record());
        assert!(
            !task.is_finished(),
            "one transport failure must preserve Discord authority"
        );
        task.abort();
        renewal.abort();
        let _ = task.await;
        let _ = renewal.await;
        drop(db);
        cleanup(&path);
    }

    #[tokio::test]
    async fn shutdown_interrupts_renewal_before_and_during_backoff() {
        use crate::logging::persistence_log_tests::CapturedLog;

        for during_backoff in [true, false] {
            let (path, db) = temp_db("renew-backoff-shutdown");
            let activation = Activation::new();
            assert!(wait_for_writer_lock(&db, &activation).await.unwrap());
            let before = activation.authority_deadline();
            *activation.renewal_fault_delay.lock().unwrap() = Duration::from_millis(10);
            activation.fail_next_renewal();
            let log = CapturedLog::default();
            tokio::time::pause();
            let mut renewal = tracing::dispatcher::with_default(&log.dispatch(), || {
                spawn_renewal_every(
                    db.clone(),
                    Arc::clone(&activation),
                    None,
                    Duration::from_millis(20),
                )
            });
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(25)).await;
            assert_eq!(activation.renewal_attempts.load(Ordering::Acquire), 1);
            if during_backoff {
                // Finish the injected error so the loop enters its 250ms sleep.
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            activation.begin_shutdown();
            let finished = tokio::time::timeout(Duration::from_millis(100), &mut renewal).await;
            renewal.abort();
            tokio::time::resume();
            finished
                .expect("shutdown must finish well before the 250ms backoff")
                .unwrap();
            assert_eq!(activation.renewal_attempts.load(Ordering::Acquire), 1);
            assert_eq!(activation.authority_deadline(), before);
            assert!(
                log.text()
                    .contains("active-bot lease renewal interrupted by shutdown")
            );
            assert!(activation.may_record(), "shutdown notices retain authority");

            // Follow the existing shutdown owner: drain notices, then release.
            drain_for_shutdown_notices(&activation, async {}).await;
            db::release_active_bot(&db, activation.instance())
                .await
                .unwrap();
            let replacement = Activation::new();
            assert_eq!(
                poll_writer_lock(&db, &replacement).await.unwrap(),
                WriterLockPoll::Ready
            );
            db::release_active_bot(&db, replacement.instance())
                .await
                .unwrap();
            drop(db);
            cleanup(&path);
        }
    }

    #[tokio::test]
    async fn black_holed_first_renewal_retries_and_extends_the_stored_lease() {
        for window in [Duration::from_secs(15), Duration::from_secs(3)] {
            let (path, db) = temp_db("renew-black-hole");
            let activation = Activation::new();
            activation.set_lease_window(window + Duration::from_secs(1), Duration::from_secs(1));
            assert!(wait_for_writer_lock(&db, &activation).await.unwrap());
            let before = activation.authority_deadline().unwrap();
            let read_lease = || {
                db.with_conn_sync(|conn| db::meta_string_conn(conn, "active_bot_lease"))
                    .unwrap()
                    .unwrap()
            };
            let before_row = read_lease();
            // The existing fault hook leaves the first CAS pending indefinitely.
            // Only the real renewal loop's timeout can reach the SQLite retry.
            activation.stall_next_renewal();
            let renewal = spawn_renewal_every(
                db.clone(),
                Arc::clone(&activation),
                None,
                Duration::from_secs(1),
            );
            let observed = tokio::time::timeout_at(tokio::time::Instant::from_std(before), async {
                while activation.authority_deadline() == Some(before)
                    && !activation.cut_off.load(Ordering::Acquire)
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
            renewal.abort();
            let _ = renewal.await;
            assert!(observed.is_ok(), "hung CAS must leave time for a retry");
            // A missed interval tick may run again before this observer wakes.
            assert!(activation.renewal_attempts.load(Ordering::Acquire) >= 2);
            assert!(activation.authority_deadline().unwrap() > before);
            assert!(activation.may_record());
            assert!(!activation.cut_off.load(Ordering::Acquire));
            let after_row = read_lease();
            let expires = |row: &str| row.rsplit_once(' ').unwrap().1.parse::<i64>().unwrap();
            assert!(expires(&after_row) > expires(&before_row));
            drop(db);
            cleanup(&path);
        }
    }

    #[tokio::test]
    async fn slow_renewal_error_retries_through_the_activation_loop() {
        // Scale 1:1: 20s TTL, 5s margin and 5s tick leave a 10s window.
        let (path, db) = temp_db("renew-slow-error");
        let activation = Activation::new();
        assert!(wait_for_writer_lock(&db, &activation).await.unwrap());
        let before = activation.authority_deadline().unwrap();
        let read_lease = || {
            db.with_conn_sync(|conn| {
                conn.query_row(
                    "SELECT value FROM meta WHERE key='active_bot_lease'",
                    (),
                    |row| Ok(row.get::<String>(0)?),
                )
            })
            .unwrap()
        };
        let before_row = read_lease();
        *activation.renewal_fault_delay.lock().unwrap() = Duration::from_secs(3);
        activation.fail_next_renewal();
        let began = Instant::now();
        let renewal = spawn_renewal(db.clone(), Arc::clone(&activation), None);
        let observed = tokio::time::timeout(Duration::from_secs(12), async {
            while activation.authority_deadline() == Some(before)
                && !activation.cut_off.load(Ordering::Acquire)
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        renewal.abort();
        let _ = renewal.await;
        observed.unwrap();
        assert!(
            activation.authority_deadline().unwrap() > before,
            "a 3s non-timeout error must retry and extend authority"
        );
        assert!(activation.may_record());
        assert_eq!(activation.renewal_attempts.load(Ordering::Acquire), 2);
        let after_row = read_lease();
        let expires = |row: &str| row.rsplit_once(' ').unwrap().1.parse::<i64>().unwrap();
        assert!(
            expires(&after_row) > expires(&before_row),
            "lease extended durably"
        );
        assert!(began.elapsed() >= RENEW_EVERY + Duration::from_secs(3));
        eprintln!(
            "3s injected transport error; activation loop retried twice; authority and SQLite lease extended"
        );
        drop(db);
        cleanup(&path);
    }

    #[tokio::test]
    async fn instant_renewal_errors_have_bounded_backoff_inside_authority() {
        let (path, db) = temp_db("renew-instant-errors");
        let activation = Activation::new();
        assert!(wait_for_writer_lock(&db, &activation).await.unwrap());
        let window = Duration::from_secs(10);
        let deadline = Instant::now() + window;
        *activation.authority_deadline.lock().unwrap() = Some(deadline);
        activation.fail_next_renewal();
        let retrying = Arc::clone(&activation);
        let faults = tokio::spawn(async move {
            loop {
                retrying.fail_next_renewal();
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        let began = Instant::now();
        let renewal = spawn_renewal_every(
            db.clone(),
            Arc::clone(&activation),
            None,
            Duration::from_millis(20),
        );
        let finished = tokio::time::timeout(window + Duration::from_secs(1), renewal).await;
        faults.abort();
        let _ = faults.await;
        finished.unwrap().unwrap();
        let attempts = activation.renewal_attempts.load(Ordering::Acquire);
        assert!(
            (9..=12).contains(&attempts),
            "250ms to 1s backoff: {attempts} attempts"
        );
        assert!(
            activation.renewal_stopped_for_authority(),
            "must stop retrying once remaining authority cannot fit the next backoff"
        );
        let stopped_at = activation
            .renewal_loop_stopped_at()
            .expect("renewal loop records when it exits");
        assert!(
            stopped_at.duration_since(began) >= window - Duration::from_secs(2),
            "renewal should run until authority nears expiry"
        );
        assert!(!activation.may_record());
        eprintln!("instant bot errors: {attempts} attempts; authority backoff exhausted");
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
            None,
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
            None,
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
        static EXIT_CODE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
        let (path, db) = temp_db("renew-shutdown");
        let mut activation = Activation::new();
        Arc::get_mut(&mut activation).unwrap().exit_hook = |code| {
            EXIT_CODE.store(code, Ordering::Release);
        };
        assert!(wait_for_writer_lock(&db, &activation).await.unwrap());
        let (task, started) = start_polled_discord(&activation);
        wait_until_polled(&started).await;
        activation.begin_shutdown();
        let renewal = spawn_renewal_every(
            db.clone(),
            Arc::clone(&activation),
            None,
            Duration::from_millis(20),
        );
        tokio::time::timeout(Duration::from_secs(2), renewal)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(EXIT_CODE.load(Ordering::Acquire), 0);
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

    #[tokio::test]
    async fn slow_startup_epoch_raise_keeps_the_lease_held_and_renewing() {
        let _recovery = crate::recovery::recovery_test_lock().lock().await;
        let (path, db) = temp_db("slow-startup");
        let cfg = crate::config::DbConfig {
            path: path.clone(),
            ..Default::default()
        };
        let tenant =
            crate::tenant::TenantRuntime::open(&db, &cfg, &Default::default(), &Default::default())
                .unwrap();
        let activation = Activation::new();
        // Startup work outlasts the production 15s authority window. Keep the
        // real cadence/window so concurrent DB tests cannot exhaust a tiny TTL.
        *activation.startup_delay.lock().unwrap() = Duration::from_secs(16);
        let started = Instant::now();
        assert!(
            claim_writer_lease("", &db, &activation, Some(&tenant))
                .await
                .unwrap()
        );
        assert!(started.elapsed() > Duration::from_secs(15));
        assert!(
            activation.may_record(),
            "startup work outlasting the original authority window must finish with a renewed lease"
        );
        let deadline = activation.authority_deadline().unwrap();
        tokio::time::sleep(RENEW_EVERY + Duration::from_millis(100)).await;
        assert!(activation.authority_deadline().unwrap() > deadline);
        assert!(!activation.cut_off.load(Ordering::Acquire));
        activation.begin_shutdown();
        drop(tenant);
        drop(db);
        crate::tenant::remove_sidecars(&path);
        cleanup(&path);
    }

    #[tokio::test]
    async fn slow_core_claim_renews_before_remaining_authority_expires() {
        let (path, db) = temp_db("slow-claim");
        let activation = Activation::new();
        assert!(wait_for_writer_lock(&db, &activation).await.unwrap());
        // Model a confirmed claim that spent 12s of its 15s window.
        activation.note_authority(Instant::now() - Duration::from_secs(12));
        let deadline = activation.authority_deadline().unwrap();
        let renewal = spawn_renewal_every(db.clone(), Arc::clone(&activation), None, RENEW_EVERY);
        tokio::time::sleep(Duration::from_millis(3500)).await;
        assert!(
            activation.may_record(),
            "a slow claim must renew before its remaining authority expires"
        );
        assert!(activation.authority_deadline().unwrap() > deadline);
        assert!(!activation.cut_off.load(Ordering::Acquire));
        activation.begin_shutdown();
        renewal.abort();
        drop(db);
        cleanup(&path);
    }

    #[tokio::test]
    async fn zero_row_renewal_logs_once_and_exits_without_retry() {
        use crate::logging::persistence_log_tests::CapturedLog;
        use tracing::instrument::WithSubscriber;
        static EXIT_CODE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
        for expired in [false, true] {
            EXIT_CODE.store(0, Ordering::Release);
            let (path, db) = temp_db("lease-zero-row");
            let mut activation = Activation::new();
            Arc::get_mut(&mut activation).unwrap().exit_hook = |code| {
                EXIT_CODE.store(code, Ordering::Release);
            };
            assert!(wait_for_writer_lock(&db, &activation).await.unwrap());
            let owner = if expired {
                activation.instance().to_owned()
            } else {
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned()
            };
            let writer = Db::open(&path, 2).unwrap();
            writer
                .with_conn(move |conn| {
                    let tx = conn.transaction_immediate()?;
                    let expires = if expired {
                        0
                    } else {
                        chrono::Utc::now().timestamp_millis() + 20_000
                    };
                    db::write_meta_string_conn(
                        &tx,
                        "active_bot_lease",
                        &format!("{owner} {expires}"),
                    )?;
                    tx.commit()
                })
                .await
                .unwrap();
            let log = CapturedLog::default();
            let dispatch = log.dispatch();
            let renewal = tracing::dispatcher::with_default(&dispatch, || {
                spawn_renewal_every(
                    db.clone(),
                    Arc::clone(&activation),
                    None,
                    Duration::from_millis(20),
                )
            });
            tokio::time::timeout(Duration::from_secs(2), renewal.with_subscriber(dispatch))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                activation.renewal_attempts.load(Ordering::Acquire),
                1,
                "zero rows must not retry"
            );
            assert!(activation.cut_off.load(Ordering::Acquire));
            assert!(!activation.may_record());
            assert_eq!(EXIT_CODE.load(Ordering::Acquire), 1);
            let text = log.text();
            assert_eq!(
                text.lines().filter(|line| line.contains("ERROR")).count(),
                1,
                "{text}"
            );
            assert_eq!(
                text.lines().count(),
                1,
                "lease loss must not double-log: {text}"
            );
            assert!(text.contains("renewal matched no row: another instance holds it, it expired, or a restore is in progress"));
            if expired {
                let stored = db
                    .with_conn(|conn| db::meta_string_conn(conn, "active_bot_lease"))
                    .await
                    .unwrap();
                assert!(
                    stored.is_none(),
                    "lost-lease exit must release the row before terminating"
                );
            }
            drop(writer);
            drop(db);
            cleanup(&path);
        }
    }

    #[tokio::test]
    async fn lost_lease_exit_releases_row_so_replacement_can_claim_immediately() {
        static EXIT_CODE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
        static LEASE_CHECK_DB: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);
        fn lost_lease_exit_hook(code: i32) {
            EXIT_CODE.store(code, Ordering::Release);
            let path = LEASE_CHECK_DB
                .lock()
                .unwrap()
                .as_ref()
                .expect("lease check path")
                .clone();
            let check_db = Db::open(&path, 2).expect("lease check db");
            let stored = check_db
                .with_conn_sync(|conn| db::meta_string_conn(conn, "active_bot_lease"))
                .expect("lease read in exit hook");
            assert!(
                stored.is_none(),
                "lease row must be released before the fatal exit hook runs"
            );
        }
        let (path, db) = temp_db("lease-release-exit");
        *LEASE_CHECK_DB.lock().unwrap() = Some(path.clone());
        let mut activation = Activation::new();
        activation.set_lease_window(Duration::from_secs(4), Duration::from_secs(1));
        Arc::get_mut(&mut activation).unwrap().exit_hook = lost_lease_exit_hook;
        assert!(wait_for_writer_lock(&db, &activation).await.unwrap());
        let claim_at = chrono::Utc::now().timestamp_millis();
        let deadline = activation.authority_deadline().unwrap();
        let live = db
            .with_conn(|conn| db::meta_string_conn(conn, "active_bot_lease"))
            .await
            .unwrap()
            .expect("holder lease row");
        let expires = live
            .rsplit_once(' ')
            .map(|(_, expiry)| expiry.parse::<i64>().unwrap())
            .unwrap();
        assert!(expires > claim_at, "fixture must hold a live lease row");
        let challenger = Activation::new();
        assert!(
            !db::claim_active_bot(
                &db,
                challenger.instance(),
                claim_at,
                challenger.refuse_flag()
            )
            .await
            .unwrap(),
            "a live lease must block the replacement before the holder releases it"
        );
        activation.stall_next_renewal();
        let renewal = spawn_renewal_every(
            db.clone(),
            Arc::clone(&activation),
            None,
            Duration::from_millis(20),
        );
        // Stall both attempts. After the first cap and backoff, the second
        // has less than a full attempt budget left before authority expires.
        tokio::time::timeout(Duration::from_secs(1), async {
            while activation.renewal_attempts.load(Ordering::Acquire) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        activation.stall_next_renewal();
        tokio::time::timeout(Duration::from_secs(5), renewal)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(activation.renewal_attempts.load(Ordering::Acquire), 2);
        assert_eq!(activation.authority_deadline(), Some(deadline));
        let stopped_at = activation.renewal_loop_stopped_at().unwrap();
        assert!(stopped_at >= deadline);
        assert!(stopped_at < deadline + Duration::from_millis(150));
        assert_eq!(EXIT_CODE.load(Ordering::Acquire), 1);
        assert!(
            db::claim_active_bot(
                &db,
                challenger.instance(),
                claim_at,
                challenger.refuse_flag()
            )
            .await
            .unwrap(),
            "replacement must claim as soon as the lost holder releases its row"
        );
        *LEASE_CHECK_DB.lock().unwrap() = None;
        drop(db);
        cleanup(&path);
    }

    async fn hold_core_writer(db: Db, hold: Duration) -> tokio::task::JoinHandle<()> {
        let began = Arc::new(tokio::sync::Semaphore::new(0));
        let signal = Arc::clone(&began);
        let writer = tokio::spawn(async move {
            db.with_conn(move |conn| {
                let tx = conn.transaction_immediate()?;
                signal.add_permits(1);
                std::thread::sleep(hold);
                tx.commit()
            })
            .await
            .unwrap();
        });
        began.acquire().await.unwrap().forget();
        writer
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cas_wait_within_authority_extends_bot_deadline() {
        // Uniform 1/2 clock scale: TTL 10s, margin 2.5s, tick 2.5s, 5s remaining.
        // The ~3s CAS lock wait exceeds the actual native 2s busy timeout.
        // The provider cap regressions separately retain unscaled timings.
        let (path, db) = temp_db("cas-wait");
        let writer_db = Db::open(&path, 2).unwrap();
        let activation = Activation::new();
        activation.set_lease_window(Duration::from_secs(10), Duration::from_millis(2500));
        assert!(wait_for_writer_lock(&db, &activation).await.unwrap());
        let before = activation.authority_deadline().unwrap();
        let writer = hold_core_writer(writer_db, Duration::from_millis(5500)).await;
        use crate::logging::persistence_log_tests::CapturedLog;
        let log = CapturedLog::default();
        let renewal = tracing::dispatcher::with_default(&log.dispatch(), || {
            spawn_renewal_every(
                db.clone(),
                Arc::clone(&activation),
                None,
                Duration::from_millis(2500),
            )
        });
        writer.await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while activation.authority_deadline() == Some(before)
                && !activation.cut_off.load(Ordering::Acquire)
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            activation.authority_deadline().unwrap() > before,
            "authority lost: {}",
            log.text()
        );
        assert!(activation.may_record());
        activation.begin_shutdown();
        renewal.abort();
        let _ = renewal.await;
        drop(db);
        cleanup(&path);
    }

    #[tokio::test]
    async fn cas_reply_after_authority_deadline_is_lost_without_extension() {
        use crate::logging::persistence_log_tests::CapturedLog;
        static EXIT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
        // Uniform 1/5 clock scale: TTL 4s, margin 1s, tick 1s.
        // Release a real SQLite writer once CAS starts, then block only the
        // caller's dispatcher until its horizon passes. CAS commits in time,
        // but its ready response must not count after the dispatcher resumes.
        let (path, db) = temp_db("cas-late");
        let writer_db = Db::open(&path, 2).unwrap();
        let mut activation = Activation::new();
        Arc::get_mut(&mut activation).unwrap().exit_hook = |code| {
            EXIT.store(code, Ordering::Release);
        };
        activation.set_lease_window(Duration::from_secs(4), Duration::from_secs(1));
        assert!(wait_for_writer_lock(&db, &activation).await.unwrap());
        let before = activation.authority_deadline().unwrap();
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let gate = Arc::clone(&release);
        let began = Arc::new(tokio::sync::Semaphore::new(0));
        let signal = Arc::clone(&began);
        let writer = tokio::spawn(async move {
            writer_db
                .with_conn(move |conn| {
                    let tx = conn.transaction_immediate()?;
                    signal.add_permits(1);
                    let (lock, ready) = &*gate;
                    let _released = ready
                        .wait_while(lock.lock().unwrap(), |released| !*released)
                        .unwrap();
                    tx.commit()
                })
                .await
                .unwrap();
        });
        began.acquire().await.unwrap().forget();
        let log = CapturedLog::default();
        let renewal = tracing::dispatcher::with_default(&log.dispatch(), || {
            spawn_renewal_every(
                db.clone(),
                Arc::clone(&activation),
                None,
                Duration::from_secs(1),
            )
        });
        let started = tokio::time::timeout(Duration::from_secs(5), async {
            while activation.renewal_attempts.load(Ordering::Acquire) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        let (lock, ready) = &*release;
        *lock.lock().unwrap() = true;
        ready.notify_all();
        started.unwrap();
        std::thread::sleep(
            before.saturating_duration_since(Instant::now()) + Duration::from_millis(100),
        );
        tokio::time::timeout(Duration::from_secs(5), renewal)
            .await
            .unwrap()
            .unwrap();
        writer.await.unwrap();
        assert_eq!(activation.authority_deadline(), Some(before));
        assert!(!activation.may_record());
        assert!(activation.cut_off.load(Ordering::Acquire));
        assert_eq!(EXIT.load(Ordering::Acquire), 1);
        assert_eq!(
            activation.renewal_attempts.load(Ordering::Acquire),
            1,
            "late CAS is never retried"
        );
        let text = log.text();
        assert!(text.contains("completed too late"), "{text}");
        assert_eq!(
            text.lines().filter(|line| line.contains("ERROR")).count(),
            1
        );
        assert!(
            db.with_conn_sync(|conn| db::meta_string_conn(conn, "active_bot_lease"))
                .unwrap()
                .is_none(),
            "lost-lease exit must release the activation row"
        );
        drop(db);
        cleanup(&path);
    }

    #[tokio::test]
    async fn successful_retry_clears_last_error_before_later_zero_row_loss() {
        use crate::logging::persistence_log_tests::CapturedLog;
        let (path, db) = temp_db("renew-error-clear");
        let activation = Activation::new();
        assert!(wait_for_writer_lock(&db, &activation).await.unwrap());
        let before = activation.authority_deadline().unwrap();
        activation.fail_next_renewal();
        let log = CapturedLog::default();
        let renewal = tracing::dispatcher::with_default(&log.dispatch(), || {
            spawn_renewal_every(
                db.clone(),
                Arc::clone(&activation),
                None,
                Duration::from_millis(500),
            )
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while activation.authority_deadline() == Some(before) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let foreign = Db::open(&path, 2).unwrap();
        foreign
            .with_conn(|conn| {
                let tx = conn.transaction_immediate()?;
                db::write_meta_string_conn(
                    &tx,
                    "active_bot_lease",
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb 9999999999999",
                )?;
                tx.commit()
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), renewal)
            .await
            .unwrap()
            .unwrap();
        let text = log.text();
        assert!(text.contains("last_error=\"none\""), "{text}");
        assert!(
            !text.contains("connection reset"),
            "stale error leaked: {text}"
        );
        drop(foreign);
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

    #[tokio::test]
    async fn stalled_renewal_cuts_off_at_the_authority_deadline_and_a_late_success_stays_off() {
        let (path, db) = temp_db("renew-stall");
        let activation = Activation::new();
        activation.set_lease_window(Duration::from_millis(500), Duration::from_millis(40));
        assert!(wait_for_writer_lock(&db, &activation).await.unwrap());
        assert!(activation.may_record());
        let horizon = activation
            .authority_deadline()
            .expect("claim sets a deadline");
        let (task, started) = start_polled_discord(&activation);
        wait_until_polled(&started).await;
        activation.stall_next_renewal();
        let renewal = spawn_renewal_every(
            db.clone(),
            Arc::clone(&activation),
            None,
            Duration::from_millis(20),
        );
        assert_cancelled(task).await;
        assert!(!activation.may_record());
        assert!(!activation.lock_held.load(Ordering::Acquire));
        assert!(activation.cut_off.load(Ordering::Acquire));
        activation.note_authority(Instant::now());
        assert_eq!(activation.authority_deadline(), Some(horizon));
        assert!(!super::settle_renewal(&activation, &Ok(true)));
        assert!(!activation.may_record());
        assert!(!activation.lock_held.load(Ordering::Acquire));
        assert_eq!(
            activation.authority_deadline(),
            Some(horizon),
            "a late renewal must not extend the deadline"
        );
        renewal.abort();
        drop(db);
        cleanup(&path);
    }
}
