//! Shared process utilities. No application state, database, Discord or provider client.

pub mod cgroup;
pub mod health;

/// The launcher uses this separate signal after database/bootstrap work has
/// completed. `/health` is intentionally live earlier and cannot prove that a
/// replacement child is ready to take traffic.
pub fn mark_child_ready() -> std::io::Result<()> {
    if let Some(path) = std::env::var_os("JMODGUARD_READY_FILE") {
        std::fs::write(path, b"ready\n")?;
    }
    Ok(())
}

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use tokio::sync::watch;

static PROCESS_READY: AtomicBool = AtomicBool::new(false);

/// In-memory readiness. Liveness does not read this, and readiness does not
/// touch a database.
pub fn set_process_ready(ready: bool) {
    PROCESS_READY.store(ready, Ordering::Release);
}

pub fn process_ready() -> bool {
    PROCESS_READY.load(Ordering::Acquire)
}

static SHUTDOWN_CONTEXT: Mutex<&'static str> = Mutex::new("process");
static SIGNAL_AT: Mutex<Option<Instant>> = Mutex::new(None);

/// Names the process in the stop-signal line. Set this before waiting.
pub fn set_shutdown_context(context: &'static str) {
    if let Ok(mut slot) = SHUTDOWN_CONTEXT.lock() {
        *slot = context;
    }
}

fn shutdown_context() -> &'static str {
    SHUTDOWN_CONTEXT
        .lock()
        .map(|slot| *slot)
        .unwrap_or("process")
}

/// Logs elapsed time since the stop signal. Does nothing when no signal arrived.
pub fn log_shutdown_finished(work: &str) {
    let started = SIGNAL_AT.lock().ok().and_then(|slot| *slot);
    let Some(started) = started else {
        return;
    };
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    tracing::info!(elapsed_ms, "{work} shutdown finished");
}

/// Process-wide shutdown latch. One OS signal unblocks every waiter
/// (database retry, health, axum, Discord shards) instead of being
/// consumed by the first `shutdown_signal()` call.
fn shutdown_tx() -> &'static watch::Sender<bool> {
    static TX: OnceLock<watch::Sender<bool>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, _rx) = watch::channel(false);
        tx
    })
}

fn arm_shutdown_listener() {
    static ARMED: std::sync::Once = std::sync::Once::new();
    ARMED.call_once(|| {
        let tx = shutdown_tx().clone();
        tokio::spawn(async move {
            wait_for_os_shutdown().await;
            tracing::info!("stop signal arrived ({})", shutdown_context());
            if let Ok(mut slot) = SIGNAL_AT.lock() {
                *slot = Some(Instant::now());
            }
            let _ = tx.send(true);
        });
    });
}

async fn wait_for_os_shutdown() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut sigterm =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(signal) => signal,
                Err(error) => {
                    tracing::error!(
                        "failed to install SIGTERM handler: {error}; waiting for Ctrl+C"
                    );
                    if let Err(error) = ctrl_c.await {
                        tracing::error!("ctrl_c handler failed: {error}");
                    }
                    return;
                }
            };
        tokio::select! {
            result = ctrl_c => {
                if let Err(error) = result {
                    tracing::error!("ctrl_c handler failed: {error}");
                }
            }
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    if let Err(error) = ctrl_c.await {
        tracing::error!("ctrl_c handler failed: {error}");
    }
}

/// Completes when the process should stop (Ctrl+C or SIGTERM).
///
/// Magic Containers sends SIGTERM on restart/redeploy. The first waiter
/// arms the OS handler; later waiters share the same latch so a signal
/// during remote database retry still stops the Discord gateway and API
/// listener after that work unwinds.
pub async fn shutdown_signal() {
    arm_shutdown_listener();
    let mut rx = shutdown_tx().subscribe();
    loop {
        if *rx.borrow() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}
