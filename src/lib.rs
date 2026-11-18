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
pub mod launcher_link;
pub mod read_link;
pub use jmodguard_runtime::cgroup;
pub use jmodguard_runtime::health;
pub use jmodguard_runtime::log_shutdown_finished;
pub use jmodguard_runtime::mark_child_ready;
pub use jmodguard_runtime::process_ready;
pub use jmodguard_runtime::resources;
pub use jmodguard_runtime::set_process_ready;
pub use jmodguard_runtime::set_shutdown_context;
pub mod emoji;
pub mod ids;
pub mod image_spam;
pub mod known_spam;
pub mod log_embeds;
pub mod normalize;
mod remote_https;
pub mod templates;
pub mod text_spam;
pub mod types;

pub mod tenant;
pub use crate::config::LogConfig;
pub use jmodguard_runtime::shutdown_signal;

/// Installs ring as the process-wide Rustls provider if none has been installed.
/// Call before building clients that enable reqwest's `rustls-no-provider` feature.
pub fn init_tls_provider() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Initializes tracing from config (console or rolling files under `log.directory`).
///
/// `process` selects a per-binary log file stem (`jmodguard-bot` / `jmodguard-api`)
/// so dual-process deploys do not contend on one shared appender.
pub fn init_tracing(cfg: &LogConfig, process: &str) {
    let dir = cfg.directory.clone();
    // The loader already applies JMODGUARD_LOG_LEVEL over log.level. Do not
    // override that configuration with RUST_LOG or crate-specific directives.
    use tracing_subscriber::util::SubscriberInitExt as _;

    if dir.as_os_str().is_empty() {
        log_subscriber(&cfg.level, std::io::stdout).try_init().ok();
        return;
    }

    if let Err(e) = std::fs::create_dir_all(&dir) {
        log_subscriber(&cfg.level, std::io::stdout).try_init().ok();
        tracing::warn!(
            "failed to create log dir {}: {e}; falling back to console",
            dir.display()
        );
        return;
    }
    let file_stem = format!("jmodguard-{process}.log");
    let file_appender = tracing_appender::rolling::daily(&dir, &file_stem);
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    // The guard must live for the process lifetime: dropping it shuts down
    // the writer thread and silently loses all log output.
    std::mem::forget(guard);
    log_subscriber(&cfg.level, non_blocking).try_init().ok();
}

/// The process log output plus the resource counters derived from tracing:
/// written log bytes and Discord REST calls. The counter layer has its own
/// filter, so it never widens what the log output records and never formats
/// span fields (request routes or bodies).
fn log_subscriber<W>(level: &str, writer: W) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    use tracing_subscriber::Layer as _;
    use tracing_subscriber::layer::SubscriberExt as _;
    let output = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_ansi(false)
        .with_writer(CountedWriter(writer))
        .with_filter(logging_filter(level));
    let activity = DiscordActivity.with_filter(
        tracing_subscriber::filter::filter_fn(DiscordActivity::wanted)
            .with_max_level_hint(tracing::Level::DEBUG),
    );
    tracing_subscriber::registry().with(output).with(activity)
}

/// Counts bytes the log output writes as `log_bytes`.
struct CountedWriter<M>(M);

impl<'a, M: tracing_subscriber::fmt::MakeWriter<'a>> tracing_subscriber::fmt::MakeWriter<'a>
    for CountedWriter<M>
{
    type Writer = Counted<M::Writer>;
    fn make_writer(&'a self) -> Self::Writer {
        Counted(self.0.make_writer())
    }
}

struct Counted<W>(W);

impl<W: std::io::Write> std::io::Write for Counted<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = self.0.write(bytes)?;
        resources::count("log_bytes", written as u64);
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// Serenity's instrumented REST entry point opens one `request` span per
/// Discord call; its rate limiter logs real 429s and pre-emptive bucket waits
/// at debug level. Only those callsites reach this layer.
struct DiscordActivity;

const DISCORD_REST_TARGET: &str = "serenity::http::client";
const DISCORD_LIMIT_TARGET: &str = "serenity::http::ratelimiting";

impl DiscordActivity {
    fn wanted(meta: &tracing::Metadata<'_>) -> bool {
        (meta.is_span() && meta.target() == DISCORD_REST_TARGET && meta.name() == "request")
            || (meta.is_event() && meta.target() == DISCORD_LIMIT_TARGET)
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for DiscordActivity {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if Self::wanted(attrs.metadata()) {
            resources::count("discord_rest", 1);
        }
    }

    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        struct Message(Option<String>);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = Some(format!("{value:?}"));
                }
            }
        }
        if event.metadata().target() != DISCORD_LIMIT_TARGET {
            return;
        }
        let mut message = Message(None);
        event.record(&mut message);
        match message.0.as_deref() {
            Some(text) if text.starts_with("Ratelimited on route") => {
                resources::count("discord_429", 1)
            }
            Some(text) if text.starts_with("Pre-emptive ratelimit") => {
                resources::count("discord_rate_limit_waits", 1)
            }
            _ => {}
        }
    }
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
    use super::resources;
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
            let subscriber = super::log_subscriber(level, move || writer.clone());
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

    #[test]
    fn log_output_and_discord_calls_are_counted_without_recording_their_fields() {
        let bytes = Capture(Arc::new(Mutex::new(Vec::new())));
        let writer = bytes.clone();
        let subscriber = super::log_subscriber("info", move || writer.clone());
        let before = (
            resources::peek_counter("discord_rest"),
            resources::peek_counter("discord_429"),
            resources::peek_counter("discord_rate_limit_waits"),
            resources::peek_counter("log_bytes"),
        );
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::span!(
                target: "serenity::http::client",
                tracing::Level::INFO,
                "request",
                req = "PRIVATE route"
            );
            drop(span);
            tracing::debug!(target: "serenity::http::ratelimiting", "Ratelimited on route PRIVATE for 1s");
            tracing::debug!(target: "serenity::http::ratelimiting", "Pre-emptive ratelimit on route PRIVATE for 5ms");
            tracing::info!(target: "jmodguard_bot", "counted line");
        });
        let logs = String::from_utf8(bytes.0.lock().unwrap().clone()).unwrap();
        assert!(!logs.contains("PRIVATE"), "{logs}");
        assert!(logs.contains("counted line"));
        // Other tests may count concurrently; these only ever grow.
        assert!(resources::peek_counter("discord_rest") > before.0);
        assert!(resources::peek_counter("discord_429") > before.1);
        assert!(resources::peek_counter("discord_rate_limit_waits") > before.2);
        assert!(resources::peek_counter("log_bytes") >= before.3 + logs.len() as u64);
    }
}
