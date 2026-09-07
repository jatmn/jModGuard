//! Shared process utilities. No application state, database, Discord or provider client.

pub mod health;

use std::sync::OnceLock;
use tokio::sync::watch;

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
            tracing::info!("shutdown signal received");
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
