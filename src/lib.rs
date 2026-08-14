//! jModGuard shared library: Discord bot logic, libSQL data layer, and WebUI/API.
//!
//! Two binaries consume this crate:
//! - `jmodguard-bot` — Discord gateway only
//! - `jmodguard-api` — axum HTTP API / WebUI only

pub mod automod;
pub mod commands;
pub mod config;
pub mod db;
mod db_compat;
pub mod defaults;
pub mod discord;
pub mod handlers;
pub mod health;
pub mod ids;
pub mod logging;
pub mod normalize;
pub mod state;
pub mod templates;
pub mod types;
#[cfg(feature = "web")]
pub mod web;

use crate::config::Config;

/// Completes when the process should stop (Ctrl+C or SIGTERM).
///
/// Magic Containers sends SIGTERM on restart/redeploy. Without this, the
/// Discord gateway is hard-killed and in-flight work is abandoned.
pub async fn shutdown_signal() {
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
    tracing::info!("shutdown signal received");
}

/// Initializes tracing from config (console or rolling files under `log.directory`).
///
/// `process` selects a per-binary log file stem (`jmodguard-bot` / `jmodguard-api`)
/// so dual-process deploys do not contend on one shared appender.
pub fn init_tracing(cfg: &Config, process: &str) {
    let level = cfg.log.level.clone();
    let dir = cfg.log.directory.clone();
    // Our crate logs per-message diagnostics at debug; everything else at the
    // configured level. This keeps the log file useful without drowning in
    // serenity internals.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(format!("{level},jmodguard=debug")));

    let fmt = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(false);

    if dir.as_os_str().is_empty() {
        fmt.try_init().ok();
        return;
    }

    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            "failed to create log dir {}: {e}; falling back to console",
            dir.display()
        );
        fmt.try_init().ok();
        return;
    }
    let file_stem = format!("jmodguard-{process}.log");
    let file_appender = tracing_appender::rolling::daily(&dir, &file_stem);
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    // The guard must live for the process lifetime: dropping it shuts down
    // the writer thread and silently loses all log output.
    std::mem::forget(guard);
    fmt.with_writer(non_blocking).try_init().ok();
}
