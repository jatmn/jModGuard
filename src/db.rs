//! libSQL data layer: local SQLite or remote libSQL persistence.
//!
//! Every async function runs its work on a libSQL connection via
//! `spawn_blocking`; parameters are moved (owned) into the closures so no
//! borrowed data ever crosses the async boundary.

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(debug_assertions)]
pub use crate::db_compat::test_drop_next_commit_response;
use crate::db_compat::{
    Connection, DbRuntime, OptionalExtension, Row, SqlExecutor, Value,
    database_operation_timed_out, params, params_from_iter,
};
use crate::types::{
    CommandDef, GLOBAL_SCOPE, GuildSettings, GuildSettingsOverride, RuleDef, resolve_settings,
};

mod bootstrap;
pub use bootstrap::*;
mod configuration;
pub use configuration::*;
mod nickname_jobs;
pub use nickname_jobs::*;
mod moderation;
pub use moderation::*;
mod settings;
pub use settings::*;

const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_config_version.sql"),
    include_str!("../migrations/0003_archive_global_log_channels.sql"),
    include_str!("../migrations/0004_nickname_reconcile.sql"),
    include_str!("../migrations/0005_mutation_receipts_nickname_jobs.sql"),
];

/// A remote request that has not completed within this deadline is cancelled.
/// Callers receive `DatabaseOperationTimedOut`; writes are not retried here
/// because the server may have committed before the response was lost.
const REMOTE_OPERATION_TIMEOUT: Duration = Duration::from_secs(75);

/// Largest supported retention period. It stays below chrono's representable
/// duration range and is also enforced at the WebUI/database write boundary.
pub const MAX_RETENTION_DAYS: u32 = 100_000;

/// API-open failure that is expected until the bot finishes migrations.
#[derive(Debug)]
pub struct SchemaNotReady(pub String);

impl std::fmt::Display for SchemaNotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SchemaNotReady {}

pub fn is_schema_not_ready(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|e| e.downcast_ref::<SchemaNotReady>().is_some())
}

/// True when a remote operation exceeded the fixed request deadline.
///
/// Callers may retry reads. Mutating operations must not be retried blindly:
/// the server could have committed before its response was lost.
pub fn is_database_operation_timeout(err: &anyhow::Error) -> bool {
    crate::db_compat::is_database_operation_timeout(err)
}

pub fn is_database_commit_indeterminate(err: &anyhow::Error) -> bool {
    crate::db_compat::is_database_commit_indeterminate(err)
}

/// Local SQLite can be temporarily locked while the bot is applying migrations.
/// That is an expected API-startup race, unlike other SQLite failures.
fn is_sqlite_startup_lock(err: &anyhow::Error) -> bool {
    err.chain().any(|error| {
        let message = error.to_string().to_ascii_lowercase();
        message.contains("database is locked") || message.contains("database is busy")
    })
}

/// True when `message` reports this HTTP status. libSQL Hrana formats
/// `status=503 Service Unavailable`; reqwest-style errors use `status code: 503`.
fn http_status_in(message: &str, code: u16) -> bool {
    let code = code.to_string();
    message.contains(&format!("status={code}")) || message.contains(&format!("status code: {code}"))
}

fn is_rejected_remote_auth(err: &anyhow::Error) -> bool {
    err.chain().any(|error| {
        let message = error.to_string().to_ascii_lowercase();
        message.contains("unauthorized")
            || message.contains("forbidden")
            || message.contains("invalid token")
            || message.contains("authentication failed")
            || message.contains("authentication error")
            || http_status_in(&message, 401)
            || http_status_in(&message, 403)
    })
}

fn is_transient_remote_open(err: &anyhow::Error) -> bool {
    if is_database_operation_timeout(err) {
        return true;
    }
    err.chain().any(|error| {
        let message = error.to_string().to_ascii_lowercase();
        const NEEDLES: &[&str] = &[
            "connection refused",
            "connection reset",
            "connection aborted",
            "broken pipe",
            "timed out",
            "timeout",
            "temporarily unavailable",
            "service unavailable",
            "try again",
            "network is unreachable",
            "no route to host",
            "failed to lookup",
            "dns error",
            "name or service not known",
            "tls handshake",
            "handshake timed",
            "server is unavailable",
            "bad gateway",
            "goaway",
            "stream closed",
        ];
        NEEDLES.iter().any(|needle| message.contains(needle))
            || http_status_in(&message, 502)
            || http_status_in(&message, 503)
    })
}

/// Startup retries incomplete schema, SQLite locks, and transient remote
/// transport errors. Auth failures still fail immediately. Shared by the
/// API (`migrate=false`) and the bot (`migrate=true`).
pub fn is_startup_retryable(err: &anyhow::Error, remote: bool) -> bool {
    if is_schema_not_ready(err) || is_sqlite_startup_lock(err) {
        return true;
    }
    remote && !is_rejected_remote_auth(err) && is_transient_remote_open(err)
}

/// How long the API (and a remote bot) should keep retrying `open_with`.
/// The attempt count and delay define when new attempts may start. An
/// in-flight `open_with` is allowed to finish so migrate/bootstrap work is
/// not abandoned and the last error stays classified.
pub fn startup_open_retry(opts: &DbOpenOptions) -> (u32, Duration) {
    if opts.url.trim().is_empty() {
        (30, Duration::from_secs(1))
    } else {
        (180, Duration::from_secs(2))
    }
}

/// Options for opening either local SQLite or a remote libSQL database.
#[derive(Debug, Clone)]
pub struct DbOpenOptions {
    pub path: PathBuf,
    pub url: String,
    pub token: String,
    pub pool_size: u32,
    /// When true, apply migrations + seed globals. Bot uses true; API uses false.
    pub migrate: bool,
    /// Seeded into the global settings row on first boot (`bot.default_prefix`).
    pub default_prefix: String,
    /// Seeded into the global settings row on first boot (`bot.retention_days`).
    pub retention_days: u32,
}

#[derive(Clone)]
pub struct Db {
    _database: Arc<libsql::Database>,
    runtime: DbRuntime,
    connections: Arc<ConnectionPool>,
    permits: Arc<tokio::sync::Semaphore>,
    permit_timeout: Option<Duration>,
    required_config_version: Arc<std::sync::atomic::AtomicU64>,
    config_write_generation: Arc<std::sync::atomic::AtomicU64>,
    config_floor_lock: Arc<parking_lot::Mutex<()>>,
    settings_write_lock: Arc<tokio::sync::Mutex<()>>,
    #[cfg(debug_assertions)]
    test_config_snapshot: Arc<Mutex<Option<(u64, String, ConfigParts)>>>,
}

struct ConnectionPool {
    idle: Mutex<Vec<Connection>>,
    database: Arc<libsql::Database>,
    runtime: DbRuntime,
    remote: bool,
}

struct ConnectionLease {
    connection: Option<Connection>,
    pool: Arc<ConnectionPool>,
    reusable: bool,
}

impl ConnectionLease {
    fn checkout(pool: Arc<ConnectionPool>) -> Result<Self> {
        let connection = pool
            .idle
            .lock()
            .map_err(|_| anyhow::anyhow!("database connection pool lock poisoned"))?
            .pop();
        let connection = match connection {
            Some(connection) => connection,
            None => {
                let connection =
                    Connection::from_database(Arc::clone(&pool.database), pool.runtime.clone())
                        .context("failed to replace discarded database connection")?;
                configure_connection(&connection, pool.remote)?;
                connection
            }
        };
        Ok(Self {
            connection: Some(connection),
            pool,
            reusable: true,
        })
    }

    fn discard(&mut self) {
        self.reusable = false;
    }
}

impl Deref for ConnectionLease {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.connection
            .as_ref()
            .expect("connection lease already returned")
    }
}

impl DerefMut for ConnectionLease {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.connection
            .as_mut()
            .expect("connection lease already returned")
    }
}

impl Drop for ConnectionLease {
    fn drop(&mut self) {
        let Some(connection) = self.connection.take() else {
            return;
        };
        if !self.reusable {
            drop_connection_without_runtime(connection);
            return;
        }
        match self.pool.idle.lock() {
            Ok(mut idle) => idle.push(connection),
            Err(_) => {
                tracing::error!("database connection pool lock poisoned while returning lease")
            }
        }
    }
}

fn drop_connection_without_runtime(connection: Connection) {
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|scope| {
            scope
                .spawn(move || drop(connection))
                .join()
                .expect("database connection drop thread panicked")
        });
    } else {
        drop(connection);
    }
}

impl Db {
    /// Local helper used by tests and callers that always migrate.
    pub fn open(path: &Path, pool_size: u32) -> Result<Self> {
        Self::open_with(DbOpenOptions {
            path: path.to_path_buf(),
            url: String::new(),
            token: String::new(),
            pool_size,
            migrate: true,
            default_prefix: GuildSettings::default().prefix,
            retention_days: GuildSettings::default().retention_days,
        })
    }

    /// Current durable terminal defaults from `meta` (fresh read).
    pub fn read_terminal_defaults(&self) -> Result<GuildSettings> {
        self.with_conn_sync(|conn| terminal_defaults_conn(conn))
    }

    pub fn open_with(opts: DbOpenOptions) -> Result<Self> {
        let remote = !opts.url.trim().is_empty();
        if remote && opts.token.trim().is_empty() {
            anyhow::bail!("db.token / JMODGUARD_DB_TOKEN is required when db.url is set");
        }
        if !remote && !opts.token.trim().is_empty() {
            anyhow::bail!("db.url / JMODGUARD_DB_URL is required when db.token is set");
        }
        let runtime =
            DbRuntime::with_operation_timeout(remote.then_some(REMOTE_OPERATION_TIMEOUT))?;
        let database = if remote {
            let connector = crate::remote_https::https_connector()
                .context("failed to configure rustls for the remote database")?;
            runtime.block_on_database(
                "connect",
                libsql::Builder::new_remote(opts.url.clone(), opts.token.clone())
                    .connector(connector)
                    .build(),
            )?
        } else {
            runtime.block_on(libsql::Builder::new_local(opts.path.clone()).build())?
        };
        let database = Arc::new(database);

        let opts_defaults = GuildSettings {
            prefix: opts.default_prefix.clone(),
            bot_name: GuildSettings::default().bot_name,
            retention_days: opts.retention_days,
        };

        // Migrate / schema-check on a dedicated connection first, so no other
        // connections are racing the DDL at startup.
        {
            let mut bootstrap = Connection::from_database(Arc::clone(&database), runtime.clone())
                .context("failed to connect to database")?;
            if !remote {
                bootstrap.execute_batch(
                    "PRAGMA busy_timeout=5000;
                     PRAGMA journal_mode=WAL;
                     PRAGMA synchronous=NORMAL;
                     PRAGMA foreign_keys=ON;",
                )?;
            } else {
                bootstrap.execute_batch("PRAGMA foreign_keys=ON;")?;
            }
            if opts.migrate {
                // Hide readiness only when this open will actually rebuild
                // schema/defaults. Clearing on every reconnect made a
                // retryable pool failure look like an incomplete first boot
                // and blocked the API for the whole remote retry window.
                if migrate_open_needs_api_fence(&bootstrap)? {
                    clear_bootstrap_complete(&bootstrap)?;
                }
                migrate_conn(&mut bootstrap)?;
                // Bot is authority for terminal defaults: sync from config every
                // migrate-open so both processes resolve empty global fields from
                // the same durable meta row (not each process's config.toml).
                write_terminal_defaults_meta(&bootstrap, &opts_defaults)?;
                seed_globals_conn(
                    &mut bootstrap,
                    &opts.default_prefix,
                    opts.retention_days,
                    &opts_defaults,
                )?;
                // Idempotent when guild_settings changed; skipped via checksum when not.
                heal_default_guild_settings_conn(&mut bootstrap, &opts_defaults)?;
            } else {
                // One primary-routed snapshot is the API readiness barrier.
                // Replica reads can otherwise see an old completed marker
                // while a new bot bootstrap is in progress on the primary.
                let _ = read_bootstrap_defaults_primary(&bootstrap)?;
            }

            let pool_size = opts.pool_size.max(2) as usize;
            let mut connections = Vec::with_capacity(pool_size);
            for _ in 0..pool_size {
                let connection = Connection::from_database(Arc::clone(&database), runtime.clone())
                    .context("failed to create pooled database connection")?;
                configure_connection(&connection, remote)?;
                connections.push(connection);
            }

            // Publish readiness only after the pool exists. A retry of
            // `open_with_until_ready` (remote first-boot) would otherwise
            // clear this marker after a later pool-connect failure, while an
            // API that already passed the gate is serving.
            if opts.migrate {
                write_meta_string_conn(
                    &bootstrap,
                    META_BOOTSTRAP_COMPLETE,
                    &MIGRATIONS.len().to_string(),
                )?;
            }

            Ok(Self {
                _database: Arc::clone(&database),
                runtime: runtime.clone(),
                connections: Arc::new(ConnectionPool {
                    idle: Mutex::new(connections),
                    database,
                    runtime: runtime.clone(),
                    remote,
                }),
                permits: Arc::new(tokio::sync::Semaphore::new(pool_size)),
                permit_timeout: remote.then_some(REMOTE_OPERATION_TIMEOUT),
                required_config_version: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                config_write_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                config_floor_lock: Arc::new(parking_lot::Mutex::new(())),
                settings_write_lock: Arc::new(tokio::sync::Mutex::new(())),
                #[cfg(debug_assertions)]
                test_config_snapshot: Arc::new(Mutex::new(None)),
            })
        }
    }

    /// Fault injection for remote replica-lag regression coverage. The next
    /// ordinary config read returns this previously captured snapshot; a
    /// primary-routed fallback must still satisfy the process's durable floor.
    #[cfg(debug_assertions)]
    pub fn test_inject_next_config_snapshot(&self, snapshot: (u64, String, ConfigParts)) {
        *self
            .test_config_snapshot
            .lock()
            .expect("test config snapshot lock poisoned") = Some(snapshot);
    }

    #[cfg(debug_assertions)]
    pub(super) fn take_test_config_snapshot(&self) -> Option<(u64, String, ConfigParts)> {
        self.test_config_snapshot
            .lock()
            .expect("test config snapshot lock poisoned")
            .take()
    }

    /// Retries `open_with` for both processes: incomplete schema, a local
    /// SQLite lock, or a remote database that is still waking.
    ///
    /// systemd `After=jmodguard-bot.service` only waits for the bot *process*
    /// to exist, not for migrate to finish. Fail-fast plus `Restart=on-failure`
    /// turns that into a crash loop. Auth failures and other non-retryable
    /// errors still fail immediately. Exhaustion wraps the last classified
    /// error; callers add process-specific context (API vs bot).
    pub async fn open_with_until_ready(
        opts: DbOpenOptions,
        attempts: u32,
        delay: Duration,
    ) -> Result<Self> {
        let attempts = attempts.max(1);
        // `open_with` owns a synchronous libSQL compatibility boundary. Run
        // each attempt on its own OS thread so callers can continue polling
        // health and shutdown futures. Do not abort an in-flight open on the
        // retry deadline: that would drop the real error (including
        // `SchemaNotReady`) and leave a migrate worker running after the
        // caller has given up. Shutdown still drops the oneshot receiver;
        // process exit then abandons the OS thread.
        let deadline = (!delay.is_zero()).then(|| Instant::now() + delay.saturating_mul(attempts));
        let mut last = None;
        for i in 0..attempts {
            if deadline.is_some_and(|deadline| {
                deadline.saturating_duration_since(Instant::now()).is_zero()
            }) {
                break;
            }
            let opened = Self::open_with_on_worker(opts.clone()).await;
            match opened {
                Ok(db) => return Ok(db),
                Err(err) if is_startup_retryable(&err, !opts.url.trim().is_empty()) => {
                    tracing::info!(
                        "database not ready (attempt {}/{attempts}); retrying: {err:#}",
                        i + 1
                    );
                    last = Some(err);
                    if i + 1 < attempts {
                        let sleep_for = deadline
                            .map(|deadline| {
                                deadline
                                    .saturating_duration_since(Instant::now())
                                    .min(delay)
                            })
                            .unwrap_or(delay);
                        if sleep_for.is_zero() {
                            break;
                        }
                        tokio::time::sleep(sleep_for).await;
                    }
                }
                Err(err) => return Err(err),
            }
        }
        Err(last.unwrap_or_else(|| {
            anyhow::Error::new(SchemaNotReady("database schema not ready".into()))
        }))
        .context("timed out retrying database open")
    }

    async fn open_with_on_worker(opts: DbOpenOptions) -> Result<Self> {
        let (send, receive) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("jmodguard-db-open".into())
            .spawn(move || {
                // The receiver can be dropped on shutdown. The worker still
                // finishes the synchronous open; process exit reaps it.
                let _ = send.send(Self::open_with(opts));
            })
            .context("failed to start database-open worker")?;
        receive
            .await
            .context("database-open worker stopped before returning a result")?
    }

    /// Runs a blocking compatibility closure on a libSQL connection off the
    /// application async runtime.
    pub async fn with_conn<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let acquire = Arc::clone(&self.permits).acquire_owned();
        let permit = match self.permit_timeout {
            Some(timeout) => tokio::time::timeout(timeout, acquire)
                .await
                .map_err(|_| database_operation_timed_out("waiting for connection", timeout))?,
            None => acquire.await,
        }
        .context("database connection limiter closed")?;
        let connections = Arc::clone(&self.connections);
        let required_config_version = Arc::clone(&self.required_config_version);
        let config_write_generation = Arc::clone(&self.config_write_generation);
        let config_floor_lock = Arc::clone(&self.config_floor_lock);
        tokio::task::spawn_blocking(move || {
            // The permit belongs to the blocking task, not its awaiter. Tokio
            // cannot cancel blocking work, so an aborted request must not let
            // another operation exceed the configured pool bound.
            let _permit = permit;
            let mut connection = ConnectionLease::checkout(connections)?;
            let result = f(&mut connection);
            if result.is_ok()
                || result
                    .as_ref()
                    .err()
                    .is_some_and(is_database_commit_indeterminate)
            {
                let committed_version = connection.take_config_version();
                if committed_version > 0 {
                    let _floor = config_floor_lock.lock();
                    // Odd while publishing, even when stable. Rebaseline code
                    // uses this as a seqlock so reused version numbers after a
                    // provider rollback cannot hide a concurrent local write.
                    config_write_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    required_config_version
                        .fetch_max(committed_version, std::sync::atomic::Ordering::SeqCst);
                    config_write_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }
            if connection.pool.remote && result.is_err()
                || connection.is_tainted()
                || result
                    .as_ref()
                    .err()
                    .is_some_and(is_database_operation_timeout)
            {
                connection.discard();
            }
            result
        })
        .await
        .context("db task panicked")?
    }

    /// Runs a closure synchronously (tests only).
    #[allow(dead_code)]
    pub fn with_conn_sync<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T>,
    {
        let acquire = Arc::clone(&self.permits).acquire_owned();
        let permit = match self.permit_timeout {
            Some(timeout) => self
                .runtime
                .block_on(async move { tokio::time::timeout(timeout, acquire).await })
                .map_err(|_| database_operation_timed_out("waiting for connection", timeout))?,
            None => self.runtime.block_on(acquire),
        }
        .context("database connection limiter closed")?;
        let _permit = permit;
        let mut connection = ConnectionLease::checkout(Arc::clone(&self.connections))?;
        let result = f(&mut connection);
        if result.is_ok()
            || result
                .as_ref()
                .err()
                .is_some_and(is_database_commit_indeterminate)
        {
            let committed_version = connection.take_config_version();
            if committed_version > 0 {
                let _floor = self.config_floor_lock.lock();
                self.config_write_generation
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.required_config_version
                    .fetch_max(committed_version, std::sync::atomic::Ordering::SeqCst);
                self.config_write_generation
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        if connection.pool.remote && result.is_err()
            || connection.is_tainted()
            || result
                .as_ref()
                .err()
                .is_some_and(is_database_operation_timeout)
        {
            connection.discard();
        }
        result
    }

    pub fn required_config_version(&self) -> u64 {
        self.required_config_version
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn config_write_generation(&self) -> u64 {
        self.config_write_generation
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub(crate) fn require_config_version(&self, version: u64) {
        let _floor = self.config_floor_lock.lock();
        self.required_config_version
            .fetch_max(version, std::sync::atomic::Ordering::SeqCst);
    }

    /// Lowers a process-local read-your-write floor only when it still equals
    /// the value whose loss was confirmed by a primary-routed transaction.
    pub fn rebaseline_required_config_version(
        &self,
        expected: u64,
        version: u64,
        write_generation: u64,
    ) -> bool {
        let _floor = self.config_floor_lock.lock();
        if write_generation & 1 != 0 || self.config_write_generation() != write_generation {
            return false;
        }
        let mut current = self.required_config_version();
        while current > version && current <= expected {
            match self.required_config_version.compare_exchange(
                current,
                version,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
        true
    }
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests;
