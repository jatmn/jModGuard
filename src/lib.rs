//! Shared jModGuard application library.
//!
//! The current binary remains the monolithic bot and WebUI process. Keeping
//! the application logic in a library is a behavior-preserving prerequisite
//! for running those entrypoints as separate processes in the next change.

pub mod automod;
pub mod commands;
pub mod config;
pub mod db;
pub mod defaults;
pub mod handlers;
pub mod logging;
pub mod normalize;
pub mod state;
pub mod templates;
pub mod types;
pub mod web;

use crate::config::Config;

/// Initializes tracing from config (console or rolling files under
/// `log.directory`).
pub fn init_tracing(cfg: &Config) {
    let level = cfg.log.level.clone();
    let dir = cfg.log.directory.clone();
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
    let file_appender = tracing_appender::rolling::daily(&dir, "jmodguard.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    std::mem::forget(guard);
    fmt.with_writer(non_blocking).try_init().ok();
}
