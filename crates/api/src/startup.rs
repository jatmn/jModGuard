//! Database startup shared by the API executable and the offline process probe.
//!
//! The probe does not bind HTTP or call Discord. It opens with the API's
//! non-migrating options and commits a settings row the bot process can read.

use crate::config::ApiProcessConfig;
use crate::db::{self, Db};
use crate::web::state::ApiState;
use anyhow::{Context as _, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

fn write_probe_marker(dir: &Path, name: &str, contents: &str) -> Result<()> {
    let path = dir.join(name);
    let tmp = dir.join(format!("{name}.part"));
    std::fs::write(&tmp, contents).with_context(|| format!("write probe marker {name}"))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("publish probe marker {name}"))?;
    Ok(())
}

pub async fn open_database(cfg: &ApiProcessConfig) -> Result<Db> {
    let opts = cfg.db_open_options();
    let budget = db::startup_open_retry(&opts);
    Db::open_with_budget(opts, budget).await
}

/// Prime API state with the same retry budget as opening the database.
/// A retry does not write terminal defaults.
pub async fn build_state(cfg: ApiProcessConfig, db: Db) -> Result<Arc<ApiState>> {
    let budget = db::startup_open_retry(&cfg.db_open_options());
    let remote = !cfg.db.url.trim().is_empty();
    let started = Instant::now();
    let deadline = started + budget.budget;
    let mut attempt = 0_u32;
    let mut last = None;
    loop {
        if attempt > 0 && Instant::now() >= deadline {
            break;
        }
        attempt += 1;
        let cfg = cfg.clone();
        let db = db.clone();
        let probe = db::current_startup_probe();
        let built = tokio::task::spawn_blocking(move || {
            let _probe = db::enter_startup_probe(probe);
            ApiState::new(cfg, db)
        })
        .await
        .context("app state worker stopped before returning");
        match built {
            Ok(Ok(state)) => {
                let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                tracing::info!(
                    attempt,
                    retried = attempt > 1,
                    elapsed_ms,
                    "starting-state read finished"
                );
                return Ok(state);
            }
            Ok(Err(error)) if db::is_startup_retryable(&error, remote) => {
                last = Some(error);
            }
            Ok(Err(error)) => return Err(error),
            Err(error) => return Err(error),
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let sleep_for =
            db::startup_backoff(attempt, budget.initial_backoff, budget.max_backoff).min(remaining);
        if sleep_for.is_zero() {
            break;
        }
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        tracing::info!(
            attempt,
            elapsed_ms,
            "starting-state read not ready; retrying: {:#}",
            last.as_ref().expect("retry stores the error")
        );
        tokio::time::sleep(sleep_for).await;
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("starting-state read failed")))
        .context("timed out retrying the starting-state read")
}

/// Open the shared database without migrating, then commit one prefix change.
pub async fn write_shared_config(cfg: ApiProcessConfig) -> Result<()> {
    let guild_id: i64 = std::env::var("JMODGUARD_PROBE_GUILD")
        .context("JMODGUARD_PROBE_GUILD")?
        .parse()
        .context("JMODGUARD_PROBE_GUILD")?;
    let prefix = std::env::var("JMODGUARD_PROBE_PREFIX").context("JMODGUARD_PROBE_PREFIX")?;
    let dir = PathBuf::from(std::env::var("JMODGUARD_PROBE_DIR").context("JMODGUARD_PROBE_DIR")?);
    let migrate = cfg.db_open_options().migrate;
    write_probe_marker(&dir, "api-migrate", if migrate { "true" } else { "false" })?;
    let port = std::fs::read_to_string(dir.join("writer-port")).context("writer-port")?;
    let scope_id = std::fs::read_to_string(dir.join("writer-scope")).context("writer-scope")?;
    let mut cfg = cfg;
    cfg.bot.control_bind = format!("127.0.0.1:{}", port.trim());
    let db = open_database(&cfg).await?;
    let app = ApiState::new(cfg, db)?;
    let reply = app
        .commit_actor(
            1,
            db::ControlOp::PatchSettings {
                scope: db::ScopeWire {
                    global: false,
                    guild_id,
                    enrollment_id: scope_id.trim().to_owned(),
                    joined_at_ns: 1,
                },
                prefix: db::FieldPatch::Set(prefix),
                bot_name: db::FieldPatch::Absent,
                reconcile: false,
            },
        )
        .await?;
    // The marker is the bot's write result. A following read on this process
    // can miss that commit on a replica.
    let seen = prefix_shown_after_write(&reply)?;
    write_probe_marker(&dir, "api-wrote", &seen)?;
    Ok(())
}

/// Value to show after a settings write. This is the bot's response, not a
/// later read of this process's database connection.
pub fn prefix_shown_after_write(reply: &db::ControlReply) -> Result<String> {
    match reply {
        db::ControlReply::Saved { body } | db::ControlReply::Unchanged { body } => body
            .get("prefix")
            .and_then(|value| value.as_str())
            .map(str::to_owned)
            .context("write failed"),
        db::ControlReply::Failed { message } => anyhow::bail!(message.clone()),
        db::ControlReply::Waiting | db::ControlReply::StillWaiting => anyhow::bail!("waiting"),
        db::ControlReply::Busy => anyhow::bail!("busy"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ApiProcessConfig;
    use jmodguard::config::BotProcessConfig;
    use jmodguard::db::{self, Db};
    use jmodguard::tenant::TenantRuntime;
    use std::sync::Arc;
    use std::time::Duration;

    fn temp_db(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "api_startup_{label}_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn cleanup(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
            let _ = std::fs::remove_file(format!("{}.guild.db{suffix}", path.display()));
        }
        jmodguard::tenant::remove_sidecars(path);
        if let Some(name) = path.file_name() {
            let mut key = name.to_os_string();
            key.push(".writer-link");
            let _ = std::fs::remove_file(path.with_file_name(key));
        }
    }

    fn prepare_bot(path: &Path) -> Db {
        let db = Db::open(path, 2).unwrap();
        let mut cfg = BotProcessConfig::default();
        cfg.db.path = path.to_path_buf();
        let _tenant = TenantRuntime::open(&db, &cfg.db, &cfg.guild_stores, &cfg.evidence).unwrap();
        db
    }

    fn api_config(path: &Path) -> ApiProcessConfig {
        let mut cfg = ApiProcessConfig::default();
        cfg.db.path = path.to_path_buf();
        cfg.db.pool_size = 2;
        cfg
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn api_startup_opens_no_write_transaction() {
        jmodguard::set_process_ready(false);
        let path = temp_db("no_write");
        let prepared = prepare_bot(&path);
        let before_defaults = prepared.read_terminal_defaults().unwrap();
        let before_version = db::get_config_version(&prepared).await.unwrap();
        let before_prefix = db::get_settings(&prepared, 0).await.unwrap().prefix;
        drop(prepared);

        let probe = Arc::new(db::StartupProbe::default());
        let _guard = db::enter_startup_probe(Some(Arc::clone(&probe)));
        let cfg = api_config(&path);
        let db = open_database(&cfg).await.unwrap();
        let app = build_state(cfg, db).await.unwrap();
        crate::web::updates::poll_releases(&app, async |_component, _channel| {
            Ok::<_, anyhow::Error>("digest".to_owned())
        })
        .await;
        assert_eq!(
            probe.immediate(),
            0,
            "API startup and the release check must not open a write transaction"
        );
        assert_eq!(app.db.read_terminal_defaults().unwrap(), before_defaults);
        assert_eq!(
            db::get_config_version(&app.db).await.unwrap(),
            before_version
        );
        assert_eq!(
            db::get_settings(&app.db, 0).await.unwrap().prefix,
            before_prefix
        );

        let queries = probe.queries();
        jmodguard::set_process_ready(false);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let probe_check = async {
            for _ in 0..4 {
                let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                stream
                    .write_all(b"GET /ready HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                    .await
                    .unwrap();
                let mut buf = vec![0u8; 512];
                let n = stream.read(&mut buf).await.unwrap();
                let text = String::from_utf8_lossy(&buf[..n]);
                assert!(
                    text.starts_with("HTTP/1.1 503"),
                    "readiness stays down until startup sets the flag: {text}"
                );
            }
            jmodguard::set_process_ready(true);
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            stream
                .write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                .await
                .unwrap();
            let mut buf = vec![0u8; 256];
            let n = stream.read(&mut buf).await.unwrap();
            let text = String::from_utf8_lossy(&buf[..n]);
            assert!(text.starts_with("HTTP/1.1 200"), "liveness: {text}");
            assert!(text.contains("\r\n\r\nok"), "liveness: {text}");
        };
        jmodguard::health::serve_while(&listener, probe_check).await;
        assert_eq!(
            probe.queries(),
            queries,
            "repeated readiness checks must not query the database"
        );
        jmodguard::set_process_ready(false);
        drop(app);
        cleanup(&path);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn starting_state_read_retries_then_succeeds_without_writing_defaults() {
        let path = temp_db("retry");
        let prepared = prepare_bot(&path);
        let before_defaults = prepared.read_terminal_defaults().unwrap();
        let before_version = db::get_config_version(&prepared).await.unwrap();
        let before_prefix = db::get_settings(&prepared, 0).await.unwrap().prefix;
        drop(prepared);

        db::test_fail_starting_state_read(&path, 1);
        let cfg = api_config(&path);
        let started = std::time::Instant::now();
        let db = open_database(&cfg).await.unwrap();
        let app = build_state(cfg, db)
            .await
            .expect("a failed-then-successful starting-state read must come up");
        assert!(
            started.elapsed() >= Duration::from_millis(900),
            "the retry must use the startup backoff"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(app.db.read_terminal_defaults().unwrap(), before_defaults);
        assert_eq!(
            db::get_config_version(&app.db).await.unwrap(),
            before_version
        );
        assert_eq!(
            db::get_settings(&app.db, 0).await.unwrap().prefix,
            before_prefix
        );
        drop(app);
        cleanup(&path);
    }
}
