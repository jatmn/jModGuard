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

pub struct Activation {
    instance: String,
    lock_held: AtomicBool,
    discord_connected: AtomicBool,
    shutting_down: Arc<AtomicBool>,
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
        })
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

    pub fn note_discord_connected(&self) {
        self.discord_connected.store(true, Ordering::Release);
    }

    pub fn mark_lock_held(&self, held: bool) {
        self.lock_held.store(held, Ordering::Release);
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }

    /// Stop acting immediately. The lease itself stays until the shutdown
    /// notice has been posted.
    pub fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
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
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(RENEW_EVERY);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            if activation.is_shutting_down() {
                break;
            }
            let now_ms = chrono::Utc::now().timestamp_millis();
            match db::claim_active_bot(&db, activation.instance(), now_ms, activation.refuse_flag())
                .await
            {
                Ok(true) => activation.mark_lock_held(true),
                Ok(false) => {
                    activation.mark_lock_held(false);
                    set_process_ready(false);
                    if !activation.is_shutting_down() {
                        tracing::warn!("active-bot lease lost");
                    }
                    break;
                }
                Err(error) => {
                    activation.mark_lock_held(false);
                    set_process_ready(false);
                    tracing::warn!("active-bot lease renewal failed: {error:#}");
                }
            }
        }
    })
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

        challenger.begin_shutdown();
        assert!(!challenger.allows_discord_connect());
        assert!(!challenger.allows_started_notice());
        assert!(!challenger.allows_rule_trigger());
        drop(db);
        cleanup(&path);
    }
}
