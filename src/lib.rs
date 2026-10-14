//! Shared jModGuard configuration, data model, database and tracing setup.
//! Service implementations live in independent bot and API packages.

pub mod ban_reason;
pub mod bot_actions;
#[cfg(unix)]
pub mod bunny_evidence;
pub mod capacity;
pub mod config;
pub mod control;
pub mod custom_rules;
pub mod db;
mod db_compat;
pub mod defaults;
#[cfg(unix)]
pub mod evidence_transfer;
pub mod launcher_child;
pub mod read_link;
pub use jmodguard_runtime::cgroup;
pub use jmodguard_runtime::health;
pub use jmodguard_runtime::log_shutdown_finished;
pub use jmodguard_runtime::mark_child_ready;
pub use jmodguard_runtime::process_ready;
pub use jmodguard_runtime::set_process_ready;
pub use jmodguard_runtime::set_shutdown_context;
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
    let dir = cfg.directory.clone();
    // The loader already applies JMODGUARD_LOG_LEVEL over log.level. Do not
    // override that configuration with RUST_LOG or crate-specific directives.
    let filter = logging_filter(&cfg.level);

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

// Accept an application level, not arbitrary target directives that could
// override the privacy limits with a more specific dependency target.
fn logging_filter(level: &str) -> tracing_subscriber::EnvFilter {
    use tracing_subscriber::filter::LevelFilter;
    let level = level.parse::<LevelFilter>().unwrap_or(LevelFilter::INFO);
    let mut filter = tracing_subscriber::EnvFilter::default().add_directive(level.into());
    for (target, ceiling) in [
        ("serenity", LevelFilter::WARN),
        ("serenity::gateway", LevelFilter::ERROR),
        ("tungstenite", LevelFilter::WARN),
        ("libsql", LevelFilter::WARN),
        ("hyper", LevelFilter::WARN),
        ("reqwest", LevelFilter::WARN),
    ] {
        let directive = format!("{target}={}", level.min(ceiling));
        filter = filter.add_directive(directive.parse().expect("valid logging directive"));
    }
    filter
}

#[cfg(test)]
mod logging_tests {
    use super::logging_filter;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn sensitive_library_events_are_filtered_at_every_app_level() {
        for level in [
            "debug",
            "trace",
            "info",
            "warn",
            "error",
            "off",
            "debug,serenity::gateway::ws=trace",
        ] {
            let bytes = Capture(Arc::new(Mutex::new(Vec::new())));
            let writer = bytes.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_env_filter(logging_filter(level))
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish();
            tracing::subscriber::with_default(subscriber, || {
                tracing::debug!(target: "jmodguard_bot", "app debug");
                tracing::info!(target: "jmodguard_api", "app info");
                tracing::warn!(target: "serenity::gateway::ws", "PRIVATE gateway payload");
                tracing::warn!(target: "serenity::gateway::shard", "PRIVATE shard payload");
                tracing::debug!(target: "serenity::http::ratelimiting", "PRIVATE request body");
                tracing::trace!(target: "tungstenite::protocol", "PRIVATE identify token");
                tracing::debug!(target: "tungstenite::protocol", "PRIVATE frame");
                tracing::trace!(target: "libsql::remote", "PRIVATE query values");
                tracing::debug!(target: "hyper::client", "PRIVATE http");
                tracing::debug!(target: "reqwest::connect", "PRIVATE URL");
                tracing::warn!(target: "reqwest", "transport warning");
                tracing::error!(target: "serenity::gateway", "gateway error");
            });
            let logs = String::from_utf8(bytes.0.lock().unwrap().clone()).unwrap();
            assert!(!logs.contains("PRIVATE"), "level {level}: {logs}");
            assert_eq!(
                logs.contains("app debug"),
                matches!(level, "debug" | "trace")
            );
            assert_eq!(
                logs.contains("app info"),
                !matches!(level, "warn" | "error" | "off")
            );
            assert_eq!(
                logs.contains("transport warning"),
                !matches!(level, "error" | "off")
            );
            assert_eq!(logs.contains("gateway error"), level != "off");
        }
    }
}
