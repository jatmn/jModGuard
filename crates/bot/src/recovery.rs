//! Maintenance recovery runs only after this process holds the writer lease.
//! It is restartable from the pending list, and it does not block moderation.

use crate::activation::Activation;
use crate::db::{self, Db};
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;

#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const PENDING_KEY: &str = "maintenance_recovery_pending";

#[cfg(test)]
static FAIL_STEPS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static FAIL_PENDING_READS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static HOLD_PENDING: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static PENDING_ENTERED: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static RELEASE_PENDING: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static BEFORE_COMMIT: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn recovery_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[cfg(test)]
pub(crate) fn arm_pending_pause() {
    RELEASE_PENDING.store(false, Ordering::Release);
    PENDING_ENTERED.store(false, Ordering::Release);
    HOLD_PENDING.store(true, Ordering::Release);
}

#[cfg(test)]
pub(crate) fn pending_pause_entered() -> bool {
    PENDING_ENTERED.load(Ordering::Acquire)
}

#[cfg(test)]
pub(crate) fn release_pending_pause() {
    RELEASE_PENDING.store(true, Ordering::Release);
}

#[cfg(test)]
async fn pause_pending_read() {
    if HOLD_PENDING.swap(false, Ordering::AcqRel) {
        PENDING_ENTERED.store(true, Ordering::Release);
        while !RELEASE_PENDING.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

pub async fn queue(db: &Db, steps: &[&str]) -> Result<()> {
    let value = steps.join("\n");
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        db::write_meta_string_conn(&tx, PENDING_KEY, &value)?;
        tx.commit()?;
        Ok(())
    })
    .await
}

pub async fn pending(db: &Db) -> Result<Vec<String>> {
    #[cfg(test)]
    {
        pause_pending_read().await;
        if FAIL_PENDING_READS.load(Ordering::Acquire) > 0 {
            FAIL_PENDING_READS.fetch_sub(1, Ordering::AcqRel);
            anyhow::bail!("injected pending read failure");
        }
    }
    let raw = db
        .with_conn(|conn| {
            let tx = conn.transaction_primary_snapshot()?;
            let value = db::meta_string_conn(&tx, PENDING_KEY)?;
            tx.finish_read()?;
            Ok(value)
        })
        .await?;
    Ok(split_pending(raw.as_deref()))
}

struct RecoveryFlagGuard<'a>(&'a Activation);

impl Drop for RecoveryFlagGuard<'_> {
    fn drop(&mut self) {
        self.0.set_recovery_in_progress(false);
    }
}

/// Apply queued recovery after the lease is held. A process that does not hold
/// the lease returns without writing. Failures are logged and retried with a
/// capped delay. Losing the lease fences the open write and stops this pass.
pub async fn run(db: &Db, activation: &Arc<Activation>) -> Result<()> {
    let _clear = RecoveryFlagGuard(activation);
    if !activation.may_record() {
        return Ok(());
    }
    activation.set_recovery_in_progress(true);
    let mut attempt = 0_u32;
    loop {
        if !activation.may_record() {
            return Ok(());
        }
        let steps = match pending(db).await {
            Ok(steps) => steps,
            Err(error) => {
                attempt = attempt.saturating_add(1);
                tracing::error!("maintenance recovery failed; retrying: {error:#}");
                tokio::time::sleep(backoff(attempt)).await;
                continue;
            }
        };
        let Some(step) = steps.first().cloned() else {
            return Ok(());
        };
        match apply_step(db, &step).await {
            Ok(()) => attempt = 0,
            Err(error) if lease_lost(&error) => {
                tracing::error!("maintenance recovery lost the writer lease: {error:#}");
                return Ok(());
            }
            Err(error) => {
                attempt = attempt.saturating_add(1);
                tracing::error!("maintenance recovery failed; retrying: {error:#}");
                tokio::time::sleep(backoff(attempt)).await;
            }
        }
    }
}

pub async fn apply_step(db: &Db, step: &str) -> Result<()> {
    #[cfg(test)]
    if FAIL_STEPS.load(Ordering::Acquire) > 0 {
        FAIL_STEPS.fetch_sub(1, Ordering::AcqRel);
        anyhow::bail!("injected recovery failure");
    }
    let step = step.to_owned();
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        let current = db::meta_string_conn(&tx, PENDING_KEY)?;
        let mut steps = split_pending(current.as_deref());
        if steps.first().is_some_and(|next| next == &step) {
            steps.remove(0);
            let done_key = format!("maintenance_recovery_done_{step}");
            db::write_meta_string_conn(&tx, &done_key, "1")?;
            db::write_meta_string_conn(&tx, PENDING_KEY, &steps.join("\n"))?;
        }
        #[cfg(test)]
        if let Some(hook) = BEFORE_COMMIT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            hook();
        }
        tx.commit()?;
        Ok(())
    })
    .await
}

pub async fn step_done(db: &Db, step: &str) -> Result<bool> {
    let key = format!("maintenance_recovery_done_{step}");
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let value = db::meta_string_conn(&tx, &key)?;
        tx.finish_read()?;
        Ok(value.as_deref() == Some("1"))
    })
    .await
}

fn split_pending(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or("")
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

fn lease_lost(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string().contains("active bot lease"))
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(200)
        .saturating_mul(1_u32.checked_shl(attempt.min(7)).unwrap_or(128))
        .min(Duration::from_secs(30))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn test_lock() -> &'static tokio::sync::Mutex<()> {
        super::recovery_test_lock()
    }

    fn temp_db() -> (std::path::PathBuf, Db) {
        let path = std::env::temp_dir().join(format!(
            "recovery_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        (path.clone(), Db::open(&path, 2).unwrap())
    }

    fn cleanup(path: &std::path::Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    async fn hold(db: &Db, activation: &Activation) {
        activation.mark_lock_held(true);
        assert!(
            db::claim_active_bot(
                db,
                activation.instance(),
                jmodguard::control::now_ms(),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_recovery_write_before_the_lease_including_while_another_bot_holds_it() {
        let _guard = test_lock().lock().await;
        let (path, db) = temp_db();
        queue(&db, &["mark-api", "mark-bot"]).await.unwrap();
        let challenger = Activation::new();
        run(&db, &challenger).await.unwrap();
        assert_eq!(pending(&db).await.unwrap(), ["mark-api", "mark-bot"]);
        assert!(!step_done(&db, "mark-api").await.unwrap());
        let holder = Activation::new();
        hold(&db, &holder).await;
        let challenger_db = Db::open(&path, 2).unwrap();
        challenger_db.arm_lease_owner(challenger.instance());
        let error = apply_step(&challenger_db, "mark-api").await.unwrap_err();
        assert!(lease_lost(&error), "{error:#}");
        assert!(!step_done(&db, "mark-api").await.unwrap());
        drop(challenger_db);
        drop(db);
        cleanup(&path);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn losing_the_lease_mid_recovery_fences_the_write() {
        let _guard = test_lock().lock().await;
        let (path, db) = temp_db();
        queue(&db, &["mark-api"]).await.unwrap();
        let activation = Activation::new();
        hold(&db, &activation).await;
        let instance = activation.instance().to_owned();
        let expires = jmodguard::control::now_ms() + 30;
        db.with_conn(move |conn| {
            let tx = conn.transaction_immediate()?;
            db::write_meta_string_conn(&tx, "active_bot_lease", &format!("{instance} {expires}"))?;
            tx.commit()?;
            Ok(())
        })
        .await
        .unwrap();
        *BEFORE_COMMIT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Box::new(|| {
            std::thread::sleep(Duration::from_millis(80));
        }));
        let error = apply_step(&db, "mark-api").await.unwrap_err();
        assert!(lease_lost(&error), "{error:#}");
        assert!(!step_done(&db, "mark-api").await.unwrap());
        drop(db);
        cleanup(&path);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_bot_records_a_ban_while_recovery_retries() {
        let _guard = test_lock().lock().await;
        let (path, db) = temp_db();
        queue(&db, &["mark-api"]).await.unwrap();
        let activation = Activation::new();
        hold(&db, &activation).await;
        FAIL_STEPS.store(1, Ordering::Release);
        let worker = {
            let db = db.clone();
            let activation = Arc::clone(&activation);
            tokio::spawn(async move { run(&db, &activation).await.unwrap() })
        };
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(activation.recovery_in_progress());
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
        assert!(id.is_some(), "moderation proceeds while recovery retries");
        worker.await.unwrap();
        assert!(step_done(&db, "mark-api").await.unwrap());
        assert!(!activation.recovery_in_progress());
        drop(db);
        cleanup(&path);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_interrupted_recovery_is_finished_by_the_next_holder() {
        let _guard = test_lock().lock().await;
        let (path, db) = temp_db();
        queue(&db, &["mark-api", "mark-bot"]).await.unwrap();
        let first = Activation::new();
        hold(&db, &first).await;
        apply_step(&db, "mark-api").await.unwrap();
        first.note_lease_lost();
        db::release_active_bot(&db, first.instance()).await.unwrap();
        let next = Activation::new();
        hold(&db, &next).await;
        run(&db, &next).await.unwrap();
        assert!(step_done(&db, "mark-api").await.unwrap());
        assert!(step_done(&db, "mark-bot").await.unwrap());
        assert!(pending(&db).await.unwrap().is_empty());
        run(&db, &next).await.unwrap();
        assert!(step_done(&db, "mark-bot").await.unwrap());
        drop(db);
        cleanup(&path);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_transient_pending_read_is_retried_and_recovery_finishes() {
        let _guard = test_lock().lock().await;
        let (path, db) = temp_db();
        queue(&db, &["mark-api"]).await.unwrap();
        let activation = Activation::new();
        hold(&db, &activation).await;
        FAIL_PENDING_READS.store(1, Ordering::Release);
        run(&db, &activation).await.unwrap();
        assert!(step_done(&db, "mark-api").await.unwrap());
        assert!(pending(&db).await.unwrap().is_empty());
        assert!(!activation.recovery_in_progress());
        drop(db);
        cleanup(&path);
    }
}
