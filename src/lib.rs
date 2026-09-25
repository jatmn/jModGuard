//! Shared jModGuard configuration, data model, database and tracing setup.
//! Service implementations live in independent bot and API packages.

pub mod bot_actions;
#[cfg(unix)]
pub mod bunny_evidence;
pub mod capacity;
pub mod config;
pub mod db;
mod db_compat;
pub mod defaults;
#[cfg(unix)]
pub mod evidence_transfer;
pub use jmodguard_runtime::health;
pub use jmodguard_runtime::mark_child_ready;
pub mod ids;
pub mod image_spam;
pub mod normalize;
mod remote_https;
pub mod templates;
pub mod text_spam;
pub mod types;

pub mod tenant;
pub use crate::config::LogConfig;
pub use jmodguard_runtime::shutdown_signal;

/// Initializes tracing from config (console or rolling files under `log.directory`).
///
/// `process` selects a per-binary log file stem (`jmodguard-bot` / `jmodguard-api`)
/// so dual-process deploys do not contend on one shared appender.
pub fn init_tracing(cfg: &LogConfig, process: &str) {
    let level = cfg.level.clone();
    let dir = cfg.directory.clone();
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
