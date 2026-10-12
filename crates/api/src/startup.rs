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
    let remote = !opts.url.trim().is_empty();
    let budget = db::startup_open_retry(&opts);
    let mut attempt = 0_u32;
    loop {
        attempt += 1;
        let opts = opts.clone();
        let probe = db::current_startup_probe();
        let opened = tokio::task::spawn_blocking(move || {
            let _probe = db::enter_startup_probe(probe);
            // Check before opening: the bot may create the file at any time.
            // A post-error check could mistake creation for a fatal open error.
            if !remote && opts.token.trim().is_empty() && local_database_is_missing(&opts.path) {
                return Err(db::SchemaNotReady(
                    "local database file is not yet created by the bot".into(),
                )
                .into());
            }
            Db::open_with(opts)
        })
        .await
        .context("database open worker stopped before returning")?;
        match opened {
            Ok(db) => return Ok(db),
            Err(error) if db::is_startup_retryable(&error, remote) => {
                tracing::warn!(
                    attempt,
                    "API database is not ready; retrying without migrating: {error:#}"
                );
                tokio::time::sleep(db::startup_backoff(
                    attempt,
                    budget.initial_backoff,
                    budget.max_backoff,
                ))
                .await;
            }
            Err(error) => return Err(error),
        }
    }
}

// SQLite's open error also covers permissions and invalid paths. Retry only
// confirmed absence of the final entry, not a dangling symlink or missing parent.
fn local_database_is_missing(path: &Path) -> bool {
    // Inspect the raw final component: Path::parent normalizes a trailing `/.`,
    // but the filesystem still treats it as a directory, never a database file.
    if path
        .as_os_str()
        .as_encoded_bytes()
        .rsplit(|byte| std::path::is_separator(char::from(*byte)))
        .next()
        .is_some_and(|component| component.is_empty() || component == b".")
    {
        return false;
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    matches!(std::fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
        && parent.is_dir()
}

/// Prime API state. An older schema or a retryable read waits with a capped
/// delay. The API does not migrate. An incompatible schema still returns.
pub async fn build_state(cfg: ApiProcessConfig, db: Db) -> Result<Arc<ApiState>> {
    let budget = db::startup_open_retry(&cfg.db_open_options());
    let remote = !cfg.db.url.trim().is_empty();
    let started = Instant::now();
    let mut attempt = 0_u32;
    loop {
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
                tracing::warn!(
                    attempt,
                    "API starting-state read is not ready; retrying: {error:#}"
                );
            }
            Ok(Err(error)) => return Err(error),
            Err(error) => return Err(error),
        }
        tokio::time::sleep(db::startup_backoff(
            attempt,
            budget.initial_backoff,
            budget.max_backoff,
        ))
        .await;
    }
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
    cfg.bot.control_target = format!("127.0.0.1:{}", port.trim());
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
        db::ControlReply::Failed { message, .. } => anyhow::bail!(message.clone()),
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

    #[tokio::test(flavor = "multi_thread")]
    async fn older_schema_stays_live_then_becomes_ready_without_exiting() {
        jmodguard::set_process_ready(false);
        let path = temp_db("older");
        let prepared = prepare_bot(&path);
        drop(prepared);
        db::set_user_version_unfenced(&path, db::CORE_SCHEMA_VERSION - 1).unwrap();
        let cfg = api_config(&path);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let probe = Arc::new(db::StartupProbe::default());
        let _guard = db::enter_startup_probe(Some(Arc::clone(&probe)));
        let tripped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        db::arm_writer_open_tripwire(Arc::clone(&tripped));
        let cfg_wait = cfg.clone();
        let opened = tokio::spawn(async move {
            crate::health::serve_while(&listener, async move {
                let db = open_database(&cfg_wait).await?;
                jmodguard::set_process_ready(true);
                tokio::time::sleep(Duration::from_millis(400)).await;
                Ok::<_, anyhow::Error>(db)
            })
            .await
        });
        assert_eq!(
            wait_status(addr, "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n", 200).await,
            200
        );
        assert_eq!(
            http_status(addr, "GET /ready HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await,
            503
        );
        assert_eq!(
            http_status(addr, "GET /api/guilds HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await,
            503
        );
        assert!(
            !opened.is_finished(),
            "startup must not exit on an older schema"
        );
        let path_upgrade = path.clone();
        std::thread::spawn(move || {
            db::set_user_version_unfenced(&path_upgrade, db::CORE_SCHEMA_VERSION).unwrap();
        })
        .join()
        .unwrap();
        let mut ready = 0;
        for _ in 0..40 {
            ready = http_status(addr, "GET /ready HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await;
            if ready == 200 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(
            ready, 200,
            "the API becomes ready after the schema is upgraded"
        );
        let db = opened.await.unwrap().unwrap();
        assert!(db.is_read_only());
        assert_eq!(probe.immediate(), 0);
        assert!(!tripped.load(std::sync::atomic::Ordering::SeqCst));
        jmodguard::set_process_ready(false);
        drop(db);
        cleanup(&path);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn newer_schema_exits_instead_of_waiting() {
        let path = temp_db("newer");
        let prepared = prepare_bot(&path);
        drop(prepared);
        db::set_user_version_unfenced(&path, db::CORE_SCHEMA_VERSION + 1).unwrap();
        let cfg = api_config(&path);
        let opened = tokio::time::timeout(Duration::from_secs(3), open_database(&cfg)).await;
        assert!(opened.unwrap().is_err(), "a newer schema must exit");
        cleanup(&path);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn invalid_local_database_paths_and_contents_do_not_retry() {
        let root = temp_db("invalid_paths");
        std::fs::create_dir(&root).unwrap();
        let corrupt = root.join("corrupt.db");
        std::fs::write(&corrupt, b"not a SQLite database").unwrap();
        let foreign = root.join("foreign.db");
        let foreign_db = libsql::Builder::new_local(&foreign).build().await.unwrap();
        foreign_db
            .connect()
            .unwrap()
            .execute("CREATE TABLE foreign_data (value TEXT)", ())
            .await
            .unwrap();
        drop(foreign_db);
        for path in [
            root.join("absent_parent/app.db"),
            corrupt.join("app.db"),
            root.clone(),
            root.join("absent.db/"),
            root.join("absent.db/."),
            corrupt,
            foreign,
        ] {
            let cfg = api_config(&path);
            let result = tokio::time::timeout(Duration::from_secs(3), open_database(&cfg))
                .await
                .expect("invalid local databases must fail, not retry");
            assert!(result.is_err(), "unexpectedly opened {}", path.display());
        }
        let mut cfg = api_config(&root.join("absent.db"));
        cfg.db.token = "token-without-url".into();
        let result = tokio::time::timeout(Duration::from_secs(3), open_database(&cfg))
            .await
            .expect("invalid database configuration must fail, not retry");
        assert!(result.err().unwrap().to_string().contains("db.url"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn inaccessible_parent_and_dangling_database_symlink_do_not_retry() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = temp_db("denied_paths");
        std::fs::create_dir(&root).unwrap();
        let link = root.join("dangling.db");
        symlink(root.join("absent.db"), &link).unwrap();
        let cfg = api_config(&link);
        let result = tokio::time::timeout(Duration::from_secs(3), open_database(&cfg)).await;
        assert!(result.expect("dangling link must fail promptly").is_err());
        let denied = root.join("denied");
        std::fs::create_dir(&denied).unwrap();
        std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o0)).unwrap();
        let path = denied.join("app.db");
        // Root may bypass mode bits; only claim a permission failure when real.
        if std::fs::metadata(&path).unwrap_err().kind() == std::io::ErrorKind::PermissionDenied {
            let cfg = api_config(&path);
            let result = tokio::time::timeout(Duration::from_secs(3), open_database(&cfg)).await;
            std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert!(
                result
                    .expect("permission error must fail promptly")
                    .is_err()
            );
        } else {
            std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    fn guild_path(core: &Path) -> PathBuf {
        let mut store = core.as_os_str().to_owned();
        store.push(".guild.db");
        PathBuf::from(store)
    }

    fn guild_options(path: PathBuf, read_only: bool) -> db::guild_data::GuildStoreOptions {
        db::guild_data::GuildStoreOptions {
            path,
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            read_only,
        }
    }

    fn shared_registration() -> db::guild_stores::StoreRegistration {
        db::guild_stores::StoreRegistration::for_test(
            db::guild_stores::StoreId::new(),
            db::guild_stores::StorePolicy::Shared,
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn older_guild_store_stays_live_then_becomes_ready_after_bot_migration() {
        jmodguard::set_process_ready(false);
        let path = temp_db("guild14");
        let core = Db::open(&path, 2).unwrap();
        assert_eq!(
            db::core_schema_version(&core).await.unwrap(),
            db::CORE_SCHEMA_VERSION
        );
        drop(core);
        let guild = guild_path(&path);
        let registration = shared_registration();
        db::guild_data::GuildDataStore::create_historical_store(
            guild_options(guild.clone(), false),
            registration.clone(),
            14,
        )
        .unwrap();
        let cfg = api_config(&path);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let probe = Arc::new(db::StartupProbe::default());
        let _guard = db::enter_startup_probe(Some(Arc::clone(&probe)));
        let cfg_wait = cfg.clone();
        let serving = tokio::spawn(async move {
            let router = crate::health::serve_while(&listener, async move {
                let db = open_database(&cfg_wait).await?;
                let state = build_state(cfg_wait, db).await?;
                jmodguard::set_process_ready(true);
                Ok::<_, anyhow::Error>(crate::web::router(state))
            })
            .await?;
            axum::serve(listener, router)
                .await
                .map_err(|error| anyhow::anyhow!(error))
        });
        assert_eq!(
            wait_status(addr, "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n", 200).await,
            200
        );
        assert_eq!(
            http_status(addr, "GET /ready HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await,
            503
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            http_status(addr, "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await,
            200
        );
        assert_eq!(
            http_status(addr, "GET /ready HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await,
            503
        );
        assert!(
            !serving.is_finished(),
            "startup must not exit while the guild store is older than 15"
        );
        std::thread::spawn(move || {
            db::guild_data::GuildDataStore::initialize(guild_options(guild, false), registration)
                .unwrap();
        })
        .join()
        .unwrap();
        let mut ready = 0;
        let mut app_route = 0;
        for _ in 0..80 {
            ready = http_status(addr, "GET /ready HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await;
            app_route =
                http_status(addr, "GET /api/guilds HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await;
            if ready == 200 && app_route != 503 && app_route != 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert_eq!(
            ready, 200,
            "the API becomes ready after the bot migrates the guild store"
        );
        assert_ne!(
            app_route, 503,
            "an application route must leave the startup page"
        );
        assert_ne!(app_route, 0);
        assert_eq!(
            http_status(addr, "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await,
            200
        );
        assert!(
            !serving.is_finished(),
            "the API keeps serving after it is ready"
        );
        assert_eq!(probe.immediate(), 0);
        serving.abort();
        jmodguard::set_process_ready(false);
        cleanup(&path);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn newer_guild_store_exits_instead_of_waiting() {
        let path = temp_db("guild16");
        let core = Db::open(&path, 2).unwrap();
        drop(core);
        let guild = guild_path(&path);
        let registration = shared_registration();
        let store = db::guild_data::GuildDataStore::initialize(
            guild_options(guild.clone(), false),
            registration,
        )
        .unwrap();
        drop(store);
        db::set_user_version_unfenced(&guild, db::guild_data::SCHEMA_VERSION + 1).unwrap();
        let cfg = api_config(&path);
        let db = open_database(&cfg).await.unwrap();
        let opened = tokio::time::timeout(Duration::from_secs(3), build_state(cfg, db)).await;
        let built = opened.expect("a newer guild schema must not wait");
        let Err(error) = built else {
            panic!("a newer guild schema must exit");
        };
        assert!(!db::is_startup_retryable(&error, false), "{error:#}");
        cleanup(&path);
    }

    async fn wait_status(addr: std::net::SocketAddr, request: &str, want: u16) -> u16 {
        let mut seen = 0;
        for _ in 0..40 {
            seen = http_status(addr, request).await;
            if seen == want {
                return seen;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        seen
    }

    async fn http_status(addr: std::net::SocketAddr, request: &str) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let Ok(mut stream) = tokio::net::TcpStream::connect(addr).await else {
            return 0;
        };
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 1024];
        let n = stream.read(&mut buf).await.unwrap_or(0);
        let text = String::from_utf8_lossy(&buf[..n]);
        text.split_whitespace()
            .nth(1)
            .unwrap_or("0")
            .parse()
            .unwrap_or(0)
    }
}
