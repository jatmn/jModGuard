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
use std::time::Duration;

use crate::db_compat::{
    Connection, DbRuntime, OptionalExtension, Row, SqlExecutor, Value,
    database_operation_timed_out, params, params_from_iter,
};
use crate::types::{
    CommandDef, GLOBAL_SCOPE, GuildSettings, GuildSettingsOverride, RuleDef, resolve_settings,
};

const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_config_version.sql"),
    include_str!("../migrations/0003_archive_global_log_channels.sql"),
    include_str!("../migrations/0004_nickname_reconcile.sql"),
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

fn is_api_startup_retryable(err: &anyhow::Error) -> bool {
    is_schema_not_ready(err) || is_sqlite_startup_lock(err)
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
    settings_write_lock: Arc<tokio::sync::Mutex<()>>,
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
                let connection = Connection::from_database(&pool.database, pool.runtime.clone())
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
        let runtime =
            DbRuntime::with_operation_timeout(remote.then_some(REMOTE_OPERATION_TIMEOUT))?;
        let database = if remote {
            runtime.block_on(
                libsql::Builder::new_remote(opts.url.clone(), opts.token.clone()).build(),
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
            let mut bootstrap = Connection::from_database(&database, runtime.clone())
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
                // A previous successful boot may have left this marker behind.
                // Clear it before any new migration/default work so an API
                // restarting concurrently cannot mistake stale readiness for
                // completion of this bootstrap.
                clear_bootstrap_complete(&bootstrap)?;
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
            } else {
                ensure_schema_current(&bootstrap)?;
                ensure_bootstrap_complete(&bootstrap)?;
            }
            // API must never seed terminal defaults — that poisoned meta when API
            // opened before the bot with a different config.toml.
            let defaults =
                read_terminal_defaults_meta_with_retry(&bootstrap, remote)?.ok_or_else(|| {
                    SchemaNotReady(
                        "database is missing meta.settings_terminal_defaults. \
                     Start jmodguard-bot once first, then start jmodguard-api."
                            .into(),
                    )
                })?;
            // Idempotent when guild_settings changed; skipped via checksum when not.
            heal_default_guild_settings_conn(&mut bootstrap, &defaults)?;
            // Only publish readiness after all migrations, durable defaults,
            // default rows, and the settings heal have completed. Otherwise an
            // API process can serve an incomplete registry during first boot.
            if opts.migrate {
                write_meta_string_conn(
                    &bootstrap,
                    META_BOOTSTRAP_COMPLETE,
                    &MIGRATIONS.len().to_string(),
                )?;
            }
        }

        let pool_size = opts.pool_size.max(2) as usize;
        let mut connections = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            let connection = Connection::from_database(&database, runtime.clone())
                .context("failed to create pooled database connection")?;
            configure_connection(&connection, remote)?;
            connections.push(connection);
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
            settings_write_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    /// Retries `open_with` while the bot is still applying schema.
    ///
    /// systemd `After=jmodguard-bot.service` only waits for the bot *process*
    /// to exist, not for migrate to finish. Fail-fast plus `Restart=on-failure`
    /// turns that into a crash loop. Other errors (remote URL, I/O) still fail
    /// immediately.
    pub async fn open_with_until_ready(
        opts: DbOpenOptions,
        attempts: u32,
        delay: Duration,
    ) -> Result<Self> {
        let attempts = attempts.max(1);
        let mut last = None;
        for i in 0..attempts {
            match Self::open_with(opts.clone()) {
                Ok(db) => return Ok(db),
                Err(err) if is_api_startup_retryable(&err) => {
                    tracing::info!(
                        "database not ready for API (attempt {}/{attempts}); waiting for jmodguard-bot: {err:#}",
                        i + 1
                    );
                    last = Some(err);
                    if i + 1 < attempts {
                        tokio::time::sleep(delay).await;
                    }
                }
                Err(err) => return Err(err),
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("database schema not ready")))
            .context("timed out waiting for jmodguard-bot to migrate the database")
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
        tokio::task::spawn_blocking(move || {
            // The permit belongs to the blocking task, not its awaiter. Tokio
            // cannot cancel blocking work, so an aborted request must not let
            // another operation exceed the configured pool bound.
            let _permit = permit;
            let mut connection = ConnectionLease::checkout(connections)?;
            let result = f(&mut connection);
            if result.is_ok() {
                required_config_version.fetch_max(
                    connection.take_config_version(),
                    std::sync::atomic::Ordering::SeqCst,
                );
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
        if result.is_ok() {
            self.required_config_version.fetch_max(
                connection.take_config_version(),
                std::sync::atomic::Ordering::SeqCst,
            );
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

    /// Lowers a process-local read-your-write floor only when it still equals
    /// the value whose loss was confirmed by a primary-routed transaction.
    pub fn rebaseline_required_config_version(&self, expected: u64, version: u64) {
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
    }
}

fn configure_connection(connection: &Connection, remote: bool) -> Result<()> {
    if remote {
        // Hrana opens a new stream for each transaction, so a connection-level
        // foreign_keys pragma would not carry into transactional writes. The
        // current schema has no FOREIGN KEY clauses; add per-transaction stream
        // setup if a future migration introduces them.
        Ok(())
    } else {
        connection.execute_batch(
            "PRAGMA busy_timeout=5000;
             PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;",
        )
    }
}

fn migrate_conn(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", (), |r| Ok(r.get(0)?))?;
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let target = (i + 1) as i64;
        if version < target {
            let tx = conn.transaction_immediate()?;
            tx.execute_batch(sql)
                .with_context(|| format!("migration {target}"))?;
            tx.pragma_update(None, "user_version", target)?;
            tx.commit()?;
            tracing::info!("applied migration {target}");
        }
    }
    Ok(())
}

fn ensure_schema_current(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", (), |r| Ok(r.get(0)?))?;
    let required = MIGRATIONS.len() as i64;
    if version < required {
        return Err(SchemaNotReady(format!(
            "database schema is behind (user_version={version}, required={required}). \
             Start jmodguard-bot once first so migrations run, then start jmodguard-api."
        ))
        .into());
    }
    // Require durable config_version marker so cross-process invalidation works.
    let has_meta: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='meta'",
        (),
        |r| Ok(r.get(0)?),
    )?;
    if has_meta == 0 {
        return Err(SchemaNotReady(
            "database is missing meta table (config_version). \
             Start jmodguard-bot once so migrations run, then start jmodguard-api."
                .into(),
        )
        .into());
    }
    Ok(())
}

/// Bumps durable `config_version` on an open connection (config writers only).
pub(crate) fn bump_config_version_conn(conn: &impl SqlExecutor) -> Result<()> {
    let version: u64 = conn.query_row(
        "INSERT INTO meta (key, value) VALUES ('config_version', '1')
         ON CONFLICT(key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)
         RETURNING CAST(value AS INTEGER)",
        (),
        |row| Ok(row.get::<i64>(0)? as u64),
    )?;
    conn.record_config_version(version);
    Ok(())
}

/// Bumps durable `slash_registry_version` when the Discord slash command
/// registry may have changed (commands table writes / command copies).
pub(crate) fn bump_slash_registry_version_conn(conn: &impl SqlExecutor) -> Result<()> {
    bump_meta_version_conn(conn, "slash_registry_version")
}

fn bump_meta_version_conn(conn: &impl SqlExecutor, key: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, '1')
         ON CONFLICT(key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)",
        params![key],
    )?;
    Ok(())
}

pub async fn bump_config_version(db: &Db) -> Result<()> {
    db.with_conn(|conn| bump_config_version_conn(conn)).await
}

fn get_meta_version_conn(conn: &impl SqlExecutor, key: &str) -> Result<u64> {
    let value: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
            Ok(r.get(0)?)
        })
        .optional()?;
    Ok(value.as_deref().unwrap_or("0").parse::<u64>().unwrap_or(0))
}

fn get_config_version_conn(conn: &impl SqlExecutor) -> Result<u64> {
    get_meta_version_conn(conn, "config_version")
}

fn get_slash_registry_version_conn(conn: &impl SqlExecutor) -> Result<u64> {
    get_meta_version_conn(conn, "slash_registry_version")
}

pub async fn get_config_version(db: &Db) -> Result<u64> {
    db.with_conn(|conn| get_config_version_conn(conn)).await
}

/// Sync read for process startup (prime poll baseline before traffic).
pub fn get_config_version_sync(db: &Db) -> Result<u64> {
    db.with_conn_sync(|conn| get_config_version_conn(conn))
}

pub async fn get_slash_registry_version(db: &Db) -> Result<u64> {
    db.with_conn(|conn| get_slash_registry_version_conn(conn))
        .await
}

/// Reads both invalidation counters through an IMMEDIATE transaction. Bunny
/// routes write transactions to the primary, so callers can distinguish an
/// authoritative failover rollback from one stale replica observation.
pub async fn get_primary_versions(db: &Db) -> Result<(u64, u64)> {
    db.with_conn(|conn| {
        let tx = conn.transaction_immediate()?;
        let config = get_config_version_conn(&tx)?;
        let slash = get_slash_registry_version_conn(&tx)?;
        tx.commit()?;
        Ok((config, slash))
    })
    .await
}

/// Sync read for process startup (prime slash-registry poll baseline).
pub fn get_slash_registry_version_sync(db: &Db) -> Result<u64> {
    db.with_conn_sync(|conn| get_slash_registry_version_conn(conn))
}

/// Primary-routed startup baseline. A replica can expose schema metadata before
/// the atomically seeded command registry has converged.
pub fn get_primary_versions_sync(db: &Db) -> Result<(u64, u64)> {
    db.with_conn_sync(|conn| {
        let tx = conn.transaction_immediate()?;
        let config = get_config_version_conn(&tx)?;
        let slash = get_slash_registry_version_conn(&tx)?;
        tx.commit()?;
        Ok((config, slash))
    })
}

/// Durable terminal settings fallback (`bot.default_prefix` / `bot.retention_days`).
///
/// Stored in `meta` so bot and API share one source of truth when the global
/// settings override is empty. Process-local config alone caused split-brain
/// whenever the two processes loaded different config values.
const META_TERMINAL_DEFAULTS: &str = "settings_terminal_defaults";
/// Written only after a bot has completed the entire bootstrap sequence.
/// The API must not treat schema version alone as a readiness signal.
const META_BOOTSTRAP_COMPLETE: &str = "bootstrap_complete";
/// Bump when heal logic changes so every DB re-runs migration once.
const SETTINGS_HEAL_GENERATION: &str = "3";
const META_SETTINGS_HEAL_GENERATION: &str = "settings_heal_generation";
const META_SETTINGS_HEAL_CHECKSUM: &str = "settings_heal_checksum";

fn meta_string_conn(conn: &impl SqlExecutor, key: &str) -> Result<Option<String>> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
        Ok(r.get(0)?)
    })
    .optional()
    .context("meta lookup failed")
}

fn write_meta_string_conn(conn: &impl SqlExecutor, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn write_terminal_defaults_meta(conn: &impl SqlExecutor, defaults: &GuildSettings) -> Result<()> {
    let json = serde_json::json!({
        "prefix": defaults.prefix,
        "retention_days": defaults.retention_days,
    });
    let json = json.to_string();
    let prev = meta_string_conn(conn, META_TERMINAL_DEFAULTS)?;
    write_meta_string_conn(conn, META_TERMINAL_DEFAULTS, &json)?;
    // Bump only when an existing meta value changes (not first seed).
    if let Some(p) = prev
        && p != json
    {
        bump_config_version_conn(conn)?;
    }
    Ok(())
}

fn read_terminal_defaults_meta(conn: &impl SqlExecutor) -> Result<Option<GuildSettings>> {
    let value = meta_string_conn(conn, META_TERMINAL_DEFAULTS)?;
    let Some(raw) = value else {
        return Ok(None);
    };
    let v: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("corrupt meta.{META_TERMINAL_DEFAULTS}"))?;
    Ok(Some(GuildSettings {
        prefix: v
            .get("prefix")
            .and_then(|p| p.as_str())
            .unwrap_or(GuildSettings::default().prefix.as_str())
            .to_string(),
        bot_name: GuildSettings::default().bot_name,
        retention_days: v
            .get("retention_days")
            .and_then(|r| r.as_u64())
            .map(|r| r as u32)
            .unwrap_or(GuildSettings::default().retention_days),
    }))
}

fn read_terminal_defaults_meta_with_retry(
    conn: &impl SqlExecutor,
    remote: bool,
) -> Result<Option<GuildSettings>> {
    let attempts = if remote { 10 } else { 1 };
    for attempt in 0..attempts {
        if let Some(defaults) = read_terminal_defaults_meta(conn)? {
            return Ok(Some(defaults));
        }
        if attempt + 1 < attempts {
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    Ok(None)
}

fn ensure_bootstrap_complete(conn: &impl SqlExecutor) -> Result<()> {
    let expected = MIGRATIONS.len().to_string();
    if meta_string_conn(conn, META_BOOTSTRAP_COMPLETE)?.as_deref() != Some(expected.as_str()) {
        return Err(SchemaNotReady(
            "database bootstrap is incomplete. Start jmodguard-bot once first, then start jmodguard-api."
                .into(),
        )
        .into());
    }
    Ok(())
}

fn clear_bootstrap_complete(conn: &impl SqlExecutor) -> Result<()> {
    let has_meta: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='meta'",
        (),
        |r| Ok(r.get(0)?),
    )?;
    if has_meta != 0 {
        conn.execute(
            "DELETE FROM meta WHERE key = ?1",
            params![META_BOOTSTRAP_COMPLETE],
        )?;
    }
    Ok(())
}

/// Terminal defaults for settings resolution — always read from durable meta.
fn terminal_defaults_conn(conn: &impl SqlExecutor) -> Result<GuildSettings> {
    read_terminal_defaults_meta(conn)?.ok_or_else(|| {
        anyhow::anyhow!("meta.settings_terminal_defaults is missing; start jmodguard-bot first")
    })
}

/// Deterministic FNV-1a 64-bit mix (stable across processes/restarts).
///
/// `DefaultHasher` is randomly seeded per process, so a checksum written by the
/// bot never matched after an API/bot restart — the heal fast-path was dead.
fn fnv1a64_mix(hash: u64, bytes: &[u8]) -> u64 {
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut hash = hash;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn guild_settings_checksum_conn(conn: &impl SqlExecutor) -> Result<u64> {
    let mut stmt =
        conn.prepare("SELECT guild_id, settings FROM guild_settings ORDER BY guild_id")?;
    let rows = stmt.query_map((), |r| Ok((r.get::<i64>(0)?, r.get::<String>(1)?)))?;
    // FNV-1a offset basis — fixed seed so bot and API agree on the same DB.
    let mut hash = 0xcbf29ce484222325u64;
    for row in rows {
        let (guild_id, settings) = row?;
        hash = fnv1a64_mix(hash, &guild_id.to_le_bytes());
        hash = fnv1a64_mix(hash, settings.as_bytes());
        // Row separator so adjacent (id, json) pairs cannot collide by concatenation.
        hash = fnv1a64_mix(hash, &[0xff]);
    }
    Ok(hash)
}

/// Keep heal fast-path accurate after any committed `guild_settings` mutation.
fn refresh_settings_heal_checksum_conn(conn: &impl SqlExecutor) -> Result<()> {
    let checksum = guild_settings_checksum_conn(conn)?;
    write_meta_string_conn(
        conn,
        META_SETTINGS_HEAL_GENERATION,
        SETTINGS_HEAL_GENERATION,
    )?;
    write_meta_string_conn(conn, META_SETTINGS_HEAL_CHECKSUM, &checksum.to_string())?;
    Ok(())
}

fn seed_globals_conn(
    conn: &mut Connection,
    default_prefix: &str,
    retention_days: u32,
    terminal: &GuildSettings,
) -> Result<()> {
    let tx = conn.transaction_immediate()?;
    let conn = &tx;
    // Seed only config-driven fields, then strip anything equal to terminal
    // defaults (not hardcoded `GuildSettings::default()`). Never pin empty
    // bot_name as a global override — that blocked inheritance.
    let seeded = GuildSettingsOverride {
        prefix: Some(default_prefix.to_string()),
        bot_name: None,
        retention_days: Some(retention_days),
    };
    let global_settings = normalize_settings_override_against_parent(&seeded, terminal);
    let json = if global_settings.is_empty() {
        "{}".to_string()
    } else {
        serde_json::to_string(&global_settings)?
    };
    let mut changed = conn.execute(
        "INSERT OR IGNORE INTO guild_settings (guild_id, settings) VALUES (?1, ?2)",
        params![GLOBAL_SCOPE, json],
    )?;
    let rules = crate::defaults::default_rules();
    for (i, rule) in rules.iter().enumerate() {
        changed += conn.execute(
            "INSERT OR IGNORE INTO rules (guild_id, rule_id, data) VALUES (?1, ?2, ?3)",
            params![
                GLOBAL_SCOPE,
                crate::types::RULE_IDS[i],
                serde_json::to_string(rule)?
            ],
        )?;
    }
    let mut commands_changed = 0usize;
    for cmd in crate::defaults::default_commands() {
        commands_changed += conn.execute(
            "INSERT OR IGNORE INTO commands (guild_id, name, data) VALUES (?1, ?2, ?3)",
            params![
                GLOBAL_SCOPE,
                cmd.name.to_lowercase(),
                serde_json::to_string(&cmd)?
            ],
        )?;
    }
    for (event, template) in crate::defaults::default_templates() {
        changed += conn.execute(
            "INSERT OR IGNORE INTO templates (guild_id, event_type, template) VALUES (?1, ?2, ?3)",
            params![GLOBAL_SCOPE, event, template],
        )?;
    }
    changed += commands_changed;
    if changed > 0 {
        bump_config_version_conn(conn)?;
    }
    if commands_changed > 0 {
        bump_slash_registry_version_conn(conn)?;
    }
    tx.commit()?;
    Ok(())
}

/// Strip override fields that equal the parent layer (global → hardcoded defaults).
///
/// This is the durable storage contract for settings: a guild row must only
/// retain intentional differences. Identical fields re-enable inheritance.
fn normalize_settings_override_against_parent(
    ov: &GuildSettingsOverride,
    parent: &GuildSettings,
) -> GuildSettingsOverride {
    GuildSettingsOverride {
        prefix: ov.prefix.as_ref().filter(|p| *p != &parent.prefix).cloned(),
        bot_name: ov
            .bot_name
            .as_ref()
            .filter(|n| *n != &parent.bot_name)
            .cloned(),
        retention_days: ov.retention_days.filter(|&r| r != parent.retention_days),
    }
}

/// `bot_name` is a Discord guild nickname, never a global default. Keep this
/// invariant in the data layer as well as the HTTP layer: settings can also be
/// copied or written by command/runtime paths that do not pass through the API.
fn normalize_settings_override_for_scope(
    guild_id: i64,
    mut override_: GuildSettingsOverride,
) -> GuildSettingsOverride {
    if guild_id == GLOBAL_SCOPE {
        override_.bot_name = None;
    }
    override_
}

/// Normalize a legacy settings row against its parent layer.
///
/// Legacy rows contain no provenance: a complete hardcoded-default blob can
/// be either an old automatic `ensure_guild` seed or an intentional operator
/// override. Preserve that ambiguity rather than deleting a user setting.
fn heal_settings_override(
    ov: &GuildSettingsOverride,
    parent: &GuildSettings,
) -> GuildSettingsOverride {
    normalize_settings_override_against_parent(ov, parent)
}

/// Parse a stored settings JSON into an override, or `None` when corrupt.
///
/// Corrupt rows are not valid overrides: callers clear them so inheritance
/// resumes instead of leaving `get_settings` permanently broken.
fn parse_settings_override_json(guild_id: i64, json: &str) -> Option<GuildSettingsOverride> {
    match serde_json::from_str::<GuildSettingsOverride>(json) {
        Ok(ov) => Some(ov),
        Err(e) => {
            tracing::warn!(
                "guild_settings row guild_id={guild_id} has corrupt JSON ({e}); \
                 clearing so inheritance can resume"
            );
            None
        }
    }
}

/// One-time (idempotent) settings migration run on bot migrate-open / API open:
/// convert legacy full blobs into parent-relative partial overrides.
///
/// Root causes addressed:
/// - `ensure_guild` seeded full blobs, but those are indistinguishable from
///   intentional legacy operator settings. Ambiguous differences are retained.
/// - Old writers stored *effective* settings as guild rows. Fields equal to
///   the parent layer are stripped so only intentional differences remain.
/// - Global rows were left unmigrated, so redundant full blobs pinned
///   hardcoded defaults at the global layer forever.
/// - Unparseable JSON was skipped, leaving `get_settings` broken for that
///   guild; corrupt rows are cleared instead.
///
/// Order: migrate **global first** (parent = config terminal defaults), then
/// guild rows against the healed global. When any row changes, bumps
/// `config_version` so an already-running API process invalidates its cache.
fn heal_default_guild_settings_conn(conn: &mut Connection, defaults: &GuildSettings) -> Result<()> {
    let checksum = guild_settings_checksum_conn(conn)?;
    let stored_gen = meta_string_conn(conn, META_SETTINGS_HEAL_GENERATION)?;
    let stored_checksum =
        meta_string_conn(conn, META_SETTINGS_HEAL_CHECKSUM)?.and_then(|s| s.parse::<u64>().ok());
    if stored_gen.as_deref() == Some(SETTINGS_HEAL_GENERATION) && stored_checksum == Some(checksum)
    {
        return Ok(());
    }

    let tx = conn.transaction_immediate()?;
    let mut changed = 0usize;

    // 1) Global layer first — parent is config terminal defaults.
    let global_raw: Option<String> = tx
        .query_row(
            "SELECT settings FROM guild_settings WHERE guild_id = ?1",
            params![GLOBAL_SCOPE],
            |r| Ok(r.get(0)?),
        )
        .optional()?;
    if let Some(json) = global_raw {
        match parse_settings_override_json(GLOBAL_SCOPE, &json) {
            Some(ov) => {
                let normalized = normalize_settings_override_for_scope(
                    GLOBAL_SCOPE,
                    heal_settings_override(&ov, defaults),
                );
                if normalized != ov {
                    write_settings_override_conn(&tx, GLOBAL_SCOPE, &normalized)?;
                    changed += 1;
                }
            }
            None => {
                write_settings_override_conn(&tx, GLOBAL_SCOPE, &GuildSettingsOverride::default())?;
                changed += 1;
            }
        }
    }

    // 2) Guild rows — parent from (possibly healed) global + terminal defaults.
    let parent = {
        let global = read_settings_override_conn(&tx, GLOBAL_SCOPE)?;
        resolve_settings(&None, &global, defaults)
    };
    let mut stmt =
        tx.prepare("SELECT guild_id, settings FROM guild_settings WHERE guild_id != ?1")?;
    let rows: Vec<(i64, String)> = stmt
        .query_map(params![GLOBAL_SCOPE], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    drop(stmt);

    for (guild_id, json) in rows {
        match parse_settings_override_json(guild_id, &json) {
            Some(ov) => {
                let normalized = heal_settings_override(&ov, &parent);
                if normalized == ov {
                    continue;
                }
                write_settings_override_conn(&tx, guild_id, &normalized)?;
                changed += 1;
            }
            None => {
                // Corrupt guild override → delete (inherit global).
                write_settings_override_conn(&tx, guild_id, &GuildSettingsOverride::default())?;
                changed += 1;
            }
        }
    }

    if changed > 0 {
        bump_config_version_conn(&tx)?;
        tracing::info!(
            "normalized {changed} guild_settings row(s) to partial overrides \
             (inherited fields stripped; ambiguous legacy overrides retained; \
             bumped config_version)"
        );
    }
    tx.commit()?;

    refresh_settings_heal_checksum_conn(conn)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn now_str() -> String {
    Utc::now().to_rfc3339()
}

fn read_json<T: DeserializeOwned>(data: &str, what: &str) -> Result<T> {
    serde_json::from_str(data).with_context(|| format!("corrupt JSON in {what}"))
}

/// Resolves a per-guild row against the global row, falling back to `default`.
pub(crate) fn resolve<T: Clone>(guild_row: &Option<T>, global_row: &Option<T>, default: &T) -> T {
    guild_row
        .clone()
        .or_else(|| global_row.clone())
        .unwrap_or_else(|| default.clone())
}

// ---------------------------------------------------------------------------
// generic scope row helpers
// ---------------------------------------------------------------------------

pub(crate) async fn load_rows<T: DeserializeOwned + Send + 'static>(
    db: &Db,
    guild_id: i64,
    table: &'static str,
    key_col: &'static str,
    key_value: Option<&str>,
) -> Result<Vec<(String, T)>> {
    let key_value = key_value.map(|s| s.to_string());
    db.with_conn(move |conn| {
        let sql = if key_value.is_some() {
            format!("SELECT {key_col}, data FROM {table} WHERE guild_id = ?1 AND {key_col} = ?2")
        } else {
            format!("SELECT {key_col}, data FROM {table} WHERE guild_id = ?1")
        };
        let mut stmt = conn.prepare(&sql)?;
        let mut args: Vec<Value> = vec![guild_id.into()];
        if let Some(kv) = &key_value {
            args.push(kv.to_string().into());
        }
        let rows = stmt.query_map(params_from_iter(args), |r| {
            Ok((r.get::<String>(0)?, r.get::<String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (k, v) = row?;
            out.push((k, read_json(&v, table)?));
        }
        Ok(out)
    })
    .await
}

async fn upsert_row<T: Serialize + Send + 'static>(
    db: &Db,
    table: &'static str,
    key_cols: &'static str,
    key_params: Vec<Value>,
    data_col: &'static str,
    data: T,
    bump_slash_registry: bool,
) -> Result<()> {
    let json = serde_json::to_string(&data)?;
    db.with_conn(move |conn| {
        let keys = key_params
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        let n = key_params.len() + 1;
        let tx = conn.transaction_immediate()?;
        tx.execute(
            &format!(
                "INSERT INTO {table} ({key_cols}, {data_col}) VALUES ({keys}, ?{n})
                 ON CONFLICT({key_cols}) DO UPDATE SET {data_col} = excluded.{data_col}"
            ),
            params_from_iter(key_params.into_iter().chain(std::iter::once(json.into()))),
        )?;
        bump_config_version_conn(&tx)?;
        if bump_slash_registry {
            bump_slash_registry_version_conn(&tx)?;
        }
        tx.commit()?;
        Ok(())
    })
    .await
}

async fn delete_row(
    db: &Db,
    table: &'static str,
    key_cols: &'static str,
    key_params: Vec<Value>,
    bump_slash_registry: bool,
) -> Result<()> {
    db.with_conn(move |conn| {
        let keys = key_params
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        let tx = conn.transaction_immediate()?;
        let n = tx.execute(
            &format!("DELETE FROM {table} WHERE ({key_cols}) = ({keys})"),
            params_from_iter(key_params),
        )?;
        // No-op deletes must not bump versions — that forced cross-process
        // cache invalidation and slash re-sync sweeps for already-gone rows.
        if n > 0 {
            bump_config_version_conn(&tx)?;
            if bump_slash_registry {
                bump_slash_registry_version_conn(&tx)?;
            }
        }
        tx.commit()?;
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------
// settings
// ---------------------------------------------------------------------------

fn read_settings_override_conn(
    conn: &impl SqlExecutor,
    guild_id: i64,
) -> Result<Option<GuildSettingsOverride>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT settings FROM guild_settings WHERE guild_id = ?1",
            params![guild_id],
            |r| Ok(r.get(0)?),
        )
        .optional()?;
    let Some(json) = raw else {
        return Ok(None);
    };
    // Corrupt JSON is not a valid override. Clear it immediately so inheritance
    // resumes without waiting for the next process restart / heal pass.
    match parse_settings_override_json(guild_id, &json) {
        Some(ov) => Ok(Some(ov)),
        None => {
            write_settings_override_conn(conn, guild_id, &GuildSettingsOverride::default())?;
            bump_config_version_conn(conn)?;
            // Autocommit read path (e.g. get_settings): refresh heal checksum so
            // the next process open does not re-scan when the row is already clean.
            if conn.is_autocommit() {
                refresh_settings_heal_checksum_conn(conn)?;
            }
            Ok(None)
        }
    }
}

fn effective_settings_conn(
    conn: &impl SqlExecutor,
    guild_id: i64,
    defaults: &GuildSettings,
) -> Result<GuildSettings> {
    let guild = if guild_id == GLOBAL_SCOPE {
        None
    } else {
        read_settings_override_conn(conn, guild_id)?
    };
    let global = read_settings_override_conn(conn, GLOBAL_SCOPE)?;
    Ok(resolve_settings(&guild, &global, defaults))
}

fn parent_settings_conn(
    conn: &impl SqlExecutor,
    guild_id: i64,
    defaults: &GuildSettings,
) -> Result<GuildSettings> {
    if guild_id == GLOBAL_SCOPE {
        return Ok(defaults.clone());
    }
    let global = read_settings_override_conn(conn, GLOBAL_SCOPE)?;
    Ok(resolve_settings(&None, &global, defaults))
}

fn write_settings_override_conn(
    conn: &impl SqlExecutor,
    guild_id: i64,
    override_: &GuildSettingsOverride,
) -> Result<()> {
    let override_ = normalize_settings_override_for_scope(guild_id, override_.clone());
    if override_.is_empty() {
        if guild_id != GLOBAL_SCOPE {
            conn.execute(
                "DELETE FROM guild_settings WHERE guild_id = ?1",
                params![guild_id],
            )?;
        } else {
            // Global row should exist as an explicit layer; store `{}` rather than delete.
            conn.execute(
                "INSERT INTO guild_settings (guild_id, settings) VALUES (?1, ?2)
                 ON CONFLICT(guild_id) DO UPDATE SET settings = excluded.settings",
                params![guild_id, "{}"],
            )?;
        }
        return Ok(());
    }
    let json = serde_json::to_string(&override_)?;
    conn.execute(
        "INSERT INTO guild_settings (guild_id, settings) VALUES (?1, ?2)
         ON CONFLICT(guild_id) DO UPDATE SET settings = excluded.settings",
        params![guild_id, json],
    )?;
    Ok(())
}

/// Effective + override settings for the WebUI config bundle (one conn read).
pub struct SettingsWebBundle {
    pub settings: GuildSettings,
    pub settings_override: Option<GuildSettingsOverride>,
    pub global_settings: GuildSettings,
    pub global_settings_override: Option<GuildSettingsOverride>,
}

fn settings_web_bundle_conn(conn: &impl SqlExecutor, guild_id: i64) -> Result<SettingsWebBundle> {
    let defaults = terminal_defaults_conn(conn)?;
    let global_override = read_settings_override_conn(conn, GLOBAL_SCOPE)?;
    let global_settings = resolve_settings(&None, &global_override, &defaults);
    let (settings_override, settings) = if guild_id == GLOBAL_SCOPE {
        (global_override.clone(), global_settings.clone())
    } else {
        let guild_override = read_settings_override_conn(conn, guild_id)?;
        let settings = resolve_settings(&guild_override, &global_override, &defaults);
        (guild_override, settings)
    };
    Ok(SettingsWebBundle {
        settings,
        settings_override,
        global_settings,
        global_settings_override: global_override,
    })
}

pub async fn get_settings_web_bundle(db: &Db, guild_id: i64) -> Result<SettingsWebBundle> {
    db.with_conn(move |conn| settings_web_bundle_conn(conn, guild_id))
        .await
}

pub async fn get_settings(db: &Db, guild_id: i64) -> Result<GuildSettings> {
    db.with_conn(move |conn| {
        let defaults = terminal_defaults_conn(conn)?;
        effective_settings_conn(conn, guild_id, &defaults)
    })
    .await
}

/// Reads effective settings from a primary-routed snapshot for side-effect
/// reconciliation after independently running bot/API writers.
pub async fn get_settings_primary(db: &Db, guild_id: i64) -> Result<GuildSettings> {
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        let defaults = terminal_defaults_conn(&tx)?;
        let settings = effective_settings_conn(&tx, guild_id, &defaults)?;
        tx.commit()?;
        Ok(settings)
    })
    .await
}

fn enqueue_nickname_reconcile_conn(conn: &impl SqlExecutor, guild_id: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO nickname_reconcile (guild_id, generation) VALUES (?1, 1)
         ON CONFLICT(guild_id) DO UPDATE SET generation = generation + 1",
        params![guild_id],
    )?;
    Ok(())
}

/// Primary-routed durable nickname work. The generation prevents an older
/// Discord request from acknowledging a newer cross-process settings commit.
pub async fn pending_nickname_reconciles(db: &Db) -> Result<Vec<(i64, i64, String)>> {
    db.with_conn(|conn| {
        let tx = conn.transaction_immediate()?;
        let defaults = terminal_defaults_conn(&tx)?;
        let mut stmt =
            tx.prepare("SELECT guild_id, generation FROM nickname_reconcile ORDER BY guild_id")?;
        let pending = stmt.query_map((), |row| Ok((row.get::<i64>(0)?, row.get::<i64>(1)?)))?;
        let mut out = Vec::new();
        for row in pending {
            let (guild_id, generation) = row?;
            let name = effective_settings_conn(&tx, guild_id, &defaults)?.bot_name;
            out.push((guild_id, generation, name));
        }
        tx.commit()?;
        Ok(out)
    })
    .await
}

pub async fn acknowledge_nickname_reconcile(db: &Db, guild_id: i64, generation: i64) -> Result<()> {
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        tx.execute(
            "DELETE FROM nickname_reconcile WHERE guild_id = ?1 AND generation = ?2",
            params![guild_id, generation],
        )?;
        tx.commit()?;
        Ok(())
    })
    .await
}

pub async fn get_settings_override(
    db: &Db,
    guild_id: i64,
) -> Result<Option<GuildSettingsOverride>> {
    db.with_conn(move |conn| read_settings_override_conn(conn, guild_id))
        .await
}

/// Parent-layer effective settings for `guild_id` (global → config defaults for
/// guilds; config terminal defaults for global scope).
pub async fn get_parent_settings(db: &Db, guild_id: i64) -> Result<GuildSettings> {
    db.with_conn(move |conn| {
        let defaults = terminal_defaults_conn(conn)?;
        parent_settings_conn(conn, guild_id, &defaults)
    })
    .await
}

/// Effective settings the destination would have after a settings-section copy
/// from `src` (without writing). Used so Discord nickname can be applied
/// *before* the SQLite copy — same ordering contract as `update_settings`.
pub async fn preview_settings_after_copy(db: &Db, src: i64, dst: i64) -> Result<GuildSettings> {
    db.with_conn(move |conn| {
        let defaults = terminal_defaults_conn(conn)?;
        let dst_parent = parent_settings_conn(conn, dst, &defaults)?;
        let src_ov = read_settings_override_conn(conn, src)?;
        let to_write = match src_ov {
            Some(ov) if !ov.is_empty() => {
                // Normalize against the destination parent while preserving
                // intentional hardcoded-default pins.
                normalize_settings_override_against_parent(&ov, &dst_parent)
            }
            _ => GuildSettingsOverride::default(),
        };
        Ok(to_write.merge_into(&dst_parent))
    })
    .await
}

/// Writes a complete settings blob, then normalizes to a parent-relative
/// partial override (same durable contract as `patch_settings`). Test helper
/// only — production writers must use `patch_settings`.
#[cfg(test)]
pub(crate) async fn upsert_settings(
    db: &Db,
    guild_id: i64,
    settings: &GuildSettings,
) -> Result<()> {
    let override_ = GuildSettingsOverride::from_complete(settings);
    db.with_conn(move |conn| {
        let defaults = terminal_defaults_conn(conn)?;
        let tx = conn.transaction_immediate()?;
        let parent = parent_settings_conn(&tx, guild_id, &defaults)?;
        let ov = normalize_settings_override_against_parent(&override_, &parent);
        write_settings_override_conn(&tx, guild_id, &ov)?;
        bump_config_version_conn(&tx)?;
        refresh_settings_heal_checksum_conn(&tx)?;
        tx.commit()?;
        Ok(())
    })
    .await
}

/// Patch settings fields into the guild/global override row.
///
/// Each field uses [`crate::types::SettingsPatchField`]:
/// - absent (`None`) — leave unchanged;
/// - `null` (`Some(None)`) — clear override so the parent layer inherits;
/// - value (`Some(Some(v))`) — set override (normalized against parent).
pub async fn patch_settings(
    db: &Db,
    guild_id: i64,
    prefix: crate::types::SettingsPatchField<String>,
    bot_name: crate::types::SettingsPatchField<String>,
    retention_days: crate::types::SettingsPatchField<u32>,
) -> Result<GuildSettings> {
    // Trim prefix at the write gate so WebUI / API / !prefix cannot persist
    // leading/trailing whitespace that would break prefix command matching.
    let prefix = match prefix {
        Some(Some(p)) => {
            let trimmed = p.trim().to_string();
            if trimmed.is_empty() {
                anyhow::bail!("prefix cannot be empty");
            }
            Some(Some(trimmed))
        }
        other => other,
    };
    if let Some(Some(days)) = retention_days
        && days > MAX_RETENTION_DAYS
    {
        anyhow::bail!("retention_days must be between 0 and {MAX_RETENTION_DAYS}");
    }
    // No-op patch: return current effective settings without bumping versions.
    if prefix.is_none() && bot_name.is_none() && retention_days.is_none() {
        return get_settings(db, guild_id).await;
    }
    let nickname_changed = bot_name.is_some() && guild_id != GLOBAL_SCOPE;
    let write_guard = Arc::clone(&db.settings_write_lock).lock_owned().await;
    db.with_conn(move |conn| {
        let _write_guard = write_guard;
        let defaults = terminal_defaults_conn(conn)?;
        let tx = conn.transaction_immediate()?;
        let parent = parent_settings_conn(&tx, guild_id, &defaults)?;
        let mut ov = read_settings_override_conn(&tx, guild_id)?.unwrap_or_default();

        // Apply provided fields, then normalize the *whole* override against
        // parent. Normalizing only patched fields would leave legacy
        // materialized inherited values pinned forever.
        // Heal's ensure_guild-blob clear deliberately does NOT run here — that
        // heuristic belongs only to legacy migration (see heal_settings_override).
        if let Some(p) = prefix {
            ov.prefix = p;
        }
        if let Some(n) = bot_name {
            ov.bot_name = n;
        }
        if let Some(r) = retention_days {
            ov.retention_days = r;
        }
        let ov = normalize_settings_override_against_parent(&ov, &parent);

        write_settings_override_conn(&tx, guild_id, &ov)?;
        if nickname_changed {
            enqueue_nickname_reconcile_conn(&tx, guild_id)?;
        }
        bump_config_version_conn(&tx)?;
        let effective = effective_settings_conn(&tx, guild_id, &defaults)?;
        refresh_settings_heal_checksum_conn(&tx)?;
        tx.commit()?;
        Ok(effective)
    })
    .await
}

// ---------------------------------------------------------------------------
// rules
// ---------------------------------------------------------------------------

pub async fn list_rules(db: &Db, guild_id: i64) -> Result<Vec<(String, RuleDef)>> {
    load_rows(db, guild_id, "rules", "rule_id", None).await
}

pub async fn get_rule(db: &Db, guild_id: i64, rule_id: &str) -> Result<Option<RuleDef>> {
    let rows: Vec<(String, RuleDef)> =
        load_rows(db, guild_id, "rules", "rule_id", Some(rule_id)).await?;
    Ok(rows.into_iter().next().map(|(_, r)| r))
}

pub async fn upsert_rule(db: &Db, guild_id: i64, rule_id: &str, rule: &RuleDef) -> Result<()> {
    upsert_row(
        db,
        "rules",
        "guild_id, rule_id",
        vec![guild_id.into(), rule_id.to_string().into()],
        "data",
        rule.clone(),
        false,
    )
    .await
}

pub async fn delete_rule(db: &Db, guild_id: i64, rule_id: &str) -> Result<()> {
    delete_row(
        db,
        "rules",
        "guild_id, rule_id",
        vec![guild_id.into(), rule_id.to_string().into()],
        false,
    )
    .await
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

pub async fn list_commands(db: &Db, guild_id: i64) -> Result<Vec<(String, CommandDef)>> {
    load_rows(db, guild_id, "commands", "name", None).await
}

pub async fn get_command_row(db: &Db, guild_id: i64, name: &str) -> Result<Option<CommandDef>> {
    let rows: Vec<(String, CommandDef)> =
        load_rows(db, guild_id, "commands", "name", Some(name)).await?;
    Ok(rows.into_iter().next().map(|(_, c)| c))
}

pub async fn upsert_command(db: &Db, guild_id: i64, name: &str, cmd: &CommandDef) -> Result<()> {
    upsert_row(
        db,
        "commands",
        "guild_id, name",
        vec![guild_id.into(), name.to_string().into()],
        "data",
        cmd.clone(),
        true,
    )
    .await
}

pub async fn delete_command(db: &Db, guild_id: i64, name: &str) -> Result<()> {
    delete_row(
        db,
        "commands",
        "guild_id, name",
        vec![guild_id.into(), name.to_string().into()],
        true,
    )
    .await
}

// ---------------------------------------------------------------------------
// templates
// ---------------------------------------------------------------------------

pub async fn list_templates(db: &Db, guild_id: i64) -> Result<Vec<(String, String)>> {
    db.with_conn(move |conn| {
        let mut stmt =
            conn.prepare("SELECT event_type, template FROM templates WHERE guild_id = ?1")?;
        let rows = stmt.query_map(params![guild_id], |r| {
            Ok((r.get::<String>(0)?, r.get::<String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
    .await
}

pub async fn upsert_template(db: &Db, guild_id: i64, event: &str, template: &str) -> Result<()> {
    let (event, template) = (event.to_string(), template.to_string());
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        tx.execute(
            "INSERT INTO templates (guild_id, event_type, template) VALUES (?1, ?2, ?3)
             ON CONFLICT(guild_id, event_type) DO UPDATE SET template = excluded.template",
            params![guild_id, event, template],
        )?;
        bump_config_version_conn(&tx)?;
        tx.commit()?;
        Ok(())
    })
    .await
}

pub async fn delete_template(db: &Db, guild_id: i64, event: &str) -> Result<()> {
    delete_row(
        db,
        "templates",
        "guild_id, event_type",
        vec![guild_id.into(), event.to_string().into()],
        false,
    )
    .await
}

pub async fn get_template_row(db: &Db, guild_id: i64, event: &str) -> Result<Option<String>> {
    let (guild_id, event) = (guild_id, event.to_string());
    db.with_conn(move |conn| {
        conn.query_row(
            "SELECT template FROM templates WHERE guild_id = ?1 AND event_type = ?2",
            params![guild_id, event],
            |r| Ok(r.get(0)?),
        )
        .optional()
    })
    .await
}

// ---------------------------------------------------------------------------
// log channels
// ---------------------------------------------------------------------------

pub async fn list_log_channels(db: &Db, guild_id: i64) -> Result<Vec<(String, i64)>> {
    db.with_conn(move |conn| {
        let mut stmt =
            conn.prepare("SELECT event_type, channel_id FROM log_channels WHERE guild_id = ?1")?;
        let rows = stmt.query_map(params![guild_id], |r| {
            Ok((r.get::<String>(0)?, r.get::<i64>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
    .await
}

pub async fn upsert_log_channel(
    db: &Db,
    guild_id: i64,
    event: &str,
    channel_id: i64,
) -> Result<()> {
    if channel_id <= 0 {
        anyhow::bail!("channel_id must be a positive Discord snowflake");
    }
    let event = event.to_string();
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        tx.execute(
            "INSERT INTO log_channels (guild_id, event_type, channel_id) VALUES (?1, ?2, ?3)
             ON CONFLICT(guild_id, event_type) DO UPDATE SET channel_id = excluded.channel_id",
            params![guild_id, event, channel_id],
        )?;
        bump_config_version_conn(&tx)?;
        tx.commit()?;
        Ok(())
    })
    .await
}

pub async fn delete_log_channel(db: &Db, guild_id: i64, event: &str) -> Result<()> {
    delete_row(
        db,
        "log_channels",
        "guild_id, event_type",
        vec![guild_id.into(), event.to_string().into()],
        false,
    )
    .await
}

// ---------------------------------------------------------------------------
// badwords
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Badword {
    pub id: i64,
    pub guild_id: i64,
    pub pattern: String,
    pub note: Option<String>,
}

/// API/UI view of a badword with compile status (legacy rows may be invalid).
#[derive(Debug, Clone, serde::Serialize)]
pub struct BadwordView {
    pub id: i64,
    #[serde(serialize_with = "crate::ids::ser_i64_str")]
    pub guild_id: i64,
    pub pattern: String,
    pub note: Option<String>,
    /// False when the pattern fails `compile_user_regex` — not enforced by the bot.
    pub valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Badword {
    /// Annotates compile status so the WebUI can flag legacy/invalid rows.
    pub fn with_status(&self) -> BadwordView {
        match crate::normalize::compile_user_regex(&self.pattern) {
            Ok(_) => BadwordView {
                id: self.id,
                guild_id: self.guild_id,
                pattern: self.pattern.clone(),
                note: self.note.clone(),
                valid: true,
                error: None,
            },
            Err(e) => BadwordView {
                id: self.id,
                guild_id: self.guild_id,
                pattern: self.pattern.clone(),
                note: self.note.clone(),
                valid: false,
                error: Some(e),
            },
        }
    }
}

pub async fn list_badwords(db: &Db, guild_id: i64) -> Result<Vec<Badword>> {
    db.with_conn(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, guild_id, pattern, note FROM badwords
             WHERE guild_id = ?1 OR guild_id = ?2 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![guild_id, GLOBAL_SCOPE], |r| {
            Ok(Badword {
                id: r.get(0)?,
                guild_id: r.get(1)?,
                pattern: r.get(2)?,
                note: r.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
    .await
}

pub async fn add_badword(db: &Db, guild_id: i64, pattern: &str, note: Option<&str>) -> Result<i64> {
    // Single gate for every writer (API, future commands) — reject before INSERT.
    crate::normalize::compile_user_regex(pattern).map_err(|e| anyhow::anyhow!(e))?;
    let (pattern, note) = (pattern.to_string(), note.map(|s| s.to_string()));
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        tx.execute(
            "INSERT INTO badwords (guild_id, pattern, note) VALUES (?1, ?2, ?3)",
            params![guild_id, pattern, note],
        )?;
        let id = tx.last_insert_rowid();
        bump_config_version_conn(&tx)?;
        tx.commit()?;
        Ok(id)
    })
    .await
}

/// Deletes a badword only when `id` belongs to the exact `guild_id` scope.
/// Callers that want to delete a global row must pass `GLOBAL_SCOPE` and
/// enforce owner auth at the API layer.
pub async fn get_badword(db: &Db, id: i64) -> Result<Option<Badword>> {
    db.with_conn(move |conn| {
        conn.query_row(
            "SELECT id, guild_id, pattern, note FROM badwords WHERE id = ?1",
            params![id],
            |r| {
                Ok(Badword {
                    id: r.get(0)?,
                    guild_id: r.get(1)?,
                    pattern: r.get(2)?,
                    note: r.get(3)?,
                })
            },
        )
        .optional()
    })
    .await
}

pub async fn delete_badword(db: &Db, id: i64, guild_id: i64) -> Result<()> {
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        let n = tx.execute(
            "DELETE FROM badwords WHERE id = ?1 AND guild_id = ?2",
            params![id, guild_id],
        )?;
        if n == 0 {
            // Same public wording as API denials — never leak guild/scope detail.
            anyhow::bail!("badword not found");
        }
        bump_config_version_conn(&tx)?;
        tx.commit()?;
        Ok(())
    })
    .await
}

/// Result of `copy_guild_config` — sections applied plus patterns skipped.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CopyResult {
    pub copied: Vec<&'static str>,
    /// Invalid/legacy badword patterns omitted from the destination replacement set.
    /// Only set when the badwords section succeeded (validated replace committed).
    pub skipped_badwords: usize,
    /// True when logchannels were requested but skipped because Discord channel
    /// IDs are not portable across guilds (or to/from global scope).
    pub skipped_logchannels: bool,
}

/// One validated badword row ready to insert (pattern + optional note).
type BadwordReplacement = (String, Option<String>);

/// Builds the enforceable replacement set for a badwords copy.
///
/// Contract (validate-then-replace — never DELETE-then-filter):
/// - Empty source → empty replacement (intentional clear of destination).
/// - Source has rows but none compile → `Err` (caller must leave destination untouched).
/// - Otherwise → only patterns that pass `compile_user_regex`.
fn prepare_badword_replacement(
    rows: Vec<BadwordReplacement>,
) -> Result<(Vec<BadwordReplacement>, usize)> {
    let source_count = rows.len();
    let mut valid = Vec::with_capacity(source_count);
    let mut skipped = 0usize;
    for (pattern, note) in rows {
        if let Err(e) = crate::normalize::compile_user_regex(&pattern) {
            tracing::warn!("copy_guild_config: rejecting badword `{pattern}`: {e}");
            skipped += 1;
            continue;
        }
        valid.push((pattern, note));
    }
    if source_count > 0 && valid.is_empty() {
        anyhow::bail!(
            "badwords copy refused: {skipped} source pattern(s) failed validation and none \
             were enforceable; destination left unchanged. Fix or delete invalid patterns \
             on the source server first."
        );
    }
    Ok((valid, skipped))
}

fn section_wanted(sections: &[String], name: &str) -> bool {
    sections.iter().any(|s| s == name || s == "all")
}

/// Read-only checks for copy refuse conditions (no writes).
///
/// Call this *before* Discord nickname side effects. The dual-write contract is
/// Discord-then-SQLite; without a preflight, a late refuse (badwords /
/// logchannels-only) leaves Discord updated while SQLite rolls back.
fn preflight_copy_guild_config_conn(
    conn: &impl SqlExecutor,
    src: i64,
    dst: i64,
    sections: &[String],
) -> Result<()> {
    if section_wanted(sections, "badwords") {
        let mut stmt = conn.prepare("SELECT pattern, note FROM badwords WHERE guild_id = ?1")?;
        let rows: Vec<(String, Option<String>)> = stmt
            .query_map(params![src], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        let _ = prepare_badword_replacement(rows)?;
    }

    if section_wanted(sections, "logchannels") && src != dst {
        let only_logchannels = sections.iter().all(|s| s == "logchannels");
        if only_logchannels {
            anyhow::bail!(
                "log channel IDs are server-specific and cannot be copied \
                 between servers; set them on the destination after copying \
                 other sections"
            );
        }
    }
    Ok(())
}

/// Public preflight for WebUI/API: refuse known hard-fail copy cases before
/// applying Discord nickname (see [`preflight_copy_guild_config_conn`]).
pub async fn preflight_copy_guild_config(
    db: &Db,
    src: i64,
    dst: i64,
    sections: Vec<String>,
) -> Result<()> {
    db.with_conn(move |conn| preflight_copy_guild_config_conn(conn, src, dst, &sections))
        .await
}

/// Copies selected config sections from `src` to `dst` in one transaction
/// with a single `config_version` bump (avoids mid-copy cache snapshots).
///
/// Always re-runs [`preflight_copy_guild_config`] inside this call so a
/// concurrent source change between an earlier WebUI preflight and this write
/// cannot skip validation. Callers may still preflight earlier to refuse
/// before Discord nickname side effects.
///
/// Badwords use validate-then-atomic-replace: the destination is only cleared
/// after a usable replacement set is prepared. If every source pattern fails
/// validation, the whole transaction fails and destination badwords are preserved.
pub async fn copy_guild_config(
    db: &Db,
    src: i64,
    dst: i64,
    sections: Vec<String>,
) -> Result<CopyResult> {
    let write_guard = Arc::clone(&db.settings_write_lock).lock_owned().await;
    db.with_conn(move |conn| {
        let _write_guard = write_guard;
        let defaults = terminal_defaults_conn(conn)?;
        // Always re-validate inside the write path (closes preflight TOCTOU).
        preflight_copy_guild_config_conn(conn, src, dst, &sections)?;

        let tx = conn.transaction_immediate()?;
        let mut result = CopyResult::default();
        let wants = |name: &str| section_wanted(&sections, name);
        let copied_settings = wants("settings");

        if copied_settings {
            // Copy the source's explicit override, then normalize against the
            // *destination* parent so inherited fields are not re-pinned.
            // Do not use heal_settings_override: that ensure_guild-blob clear
            // is migrate-only. Writers (patch / copy) preserve intentional
            // hardcoded-default pins when they differ from the parent.
            let src_ov = read_settings_override_conn(&tx, src)?;
            let dst_parent = parent_settings_conn(&tx, dst, &defaults)?;
            let to_write = match src_ov {
                Some(ov) if !ov.is_empty() => {
                    normalize_settings_override_against_parent(&ov, &dst_parent)
                }
                _ => GuildSettingsOverride::default(),
            };
            let to_write = normalize_settings_override_for_scope(dst, to_write);
            write_settings_override_conn(&tx, dst, &to_write)?;
            if dst != GLOBAL_SCOPE {
                enqueue_nickname_reconcile_conn(&tx, dst)?;
            }
            result.copied.push("settings");
        }

        if wants("rules") {
            let mut stmt = tx.prepare("SELECT rule_id, data FROM rules WHERE guild_id = ?1")?;
            let rows: Vec<(String, String)> = stmt
                .query_map(params![src], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);
            tx.execute("DELETE FROM rules WHERE guild_id = ?1", params![dst])?;
            for (rid, data) in rows {
                tx.execute(
                    "INSERT INTO rules (guild_id, rule_id, data) VALUES (?1, ?2, ?3)
                     ON CONFLICT(guild_id, rule_id) DO UPDATE SET data = excluded.data",
                    params![dst, rid, data],
                )?;
            }
            result.copied.push("rules");
        }

        if wants("badwords") {
            let mut stmt = tx.prepare("SELECT pattern, note FROM badwords WHERE guild_id = ?1")?;
            let rows: Vec<(String, Option<String>)> = stmt
                .query_map(params![src], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);

            // Validate BEFORE touching destination — DELETE-then-filter wiped
            // dest when every source pattern was invalid (recurring review bug).
            let (replacement, skipped) = prepare_badword_replacement(rows)?;
            result.skipped_badwords = skipped;

            tx.execute("DELETE FROM badwords WHERE guild_id = ?1", params![dst])?;
            for (pattern, note) in &replacement {
                tx.execute(
                    "INSERT INTO badwords (guild_id, pattern, note) VALUES (?1, ?2, ?3)",
                    params![dst, pattern.clone(), note.clone()],
                )?;
            }
            tracing::debug!(
                "copy_guild_config badwords src={src} dst={dst}: inserted={} skipped={skipped}",
                replacement.len()
            );
            result.copied.push("badwords");
        }

        if wants("commands") {
            let mut stmt = tx.prepare("SELECT name, data FROM commands WHERE guild_id = ?1")?;
            let rows: Vec<(String, String)> = stmt
                .query_map(params![src], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);
            tx.execute("DELETE FROM commands WHERE guild_id = ?1", params![dst])?;
            for (name, data) in rows {
                tx.execute(
                    "INSERT INTO commands (guild_id, name, data) VALUES (?1, ?2, ?3)
                     ON CONFLICT(guild_id, name) DO UPDATE SET data = excluded.data",
                    params![dst, name, data],
                )?;
            }
            result.copied.push("commands");
        }

        if wants("templates") {
            let mut stmt =
                tx.prepare("SELECT event_type, template FROM templates WHERE guild_id = ?1")?;
            let rows: Vec<(String, String)> = stmt
                .query_map(params![src], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);
            tx.execute("DELETE FROM templates WHERE guild_id = ?1", params![dst])?;
            for (event, template) in rows {
                tx.execute(
                    "INSERT INTO templates (guild_id, event_type, template) VALUES (?1, ?2, ?3)
                     ON CONFLICT(guild_id, event_type) DO UPDATE SET template = excluded.template",
                    params![dst, event, template],
                )?;
            }
            result.copied.push("templates");
        }

        if wants("logchannels") {
            // Discord channel snowflakes are guild-scoped. Copying them to
            // another guild (or into/out of global) routes events to invalid
            // or foreign channels. Skip rather than silently misconfigure.
            if src == dst {
                tracing::debug!(
                    "copy_guild_config: no-op logchannels src=dst={src} \
                     (channels already belong to this server)"
                );
                // Do not report as copied — nothing changed.
            } else {
                tracing::warn!(
                    "copy_guild_config: skipping logchannels src={src} dst={dst} \
                     (channel IDs are not portable across Discord servers)"
                );
                result.skipped_logchannels = true;
                // Hard refuse already handled in preflight for logchannels-only.
            }
        }

        if !result.copied.is_empty() {
            bump_config_version_conn(&tx)?;
            if result.copied.contains(&"commands") {
                bump_slash_registry_version_conn(&tx)?;
            }
        }
        if copied_settings {
            refresh_settings_heal_checksum_conn(&tx)?;
        }
        tx.commit()?;
        Ok(result)
    })
    .await
}

// ---------------------------------------------------------------------------
// members
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemberRow {
    #[serde(
        serialize_with = "crate::ids::ser_i64_str",
        deserialize_with = "crate::ids::de_i64"
    )]
    pub guild_id: i64,
    #[serde(
        serialize_with = "crate::ids::ser_i64_str",
        deserialize_with = "crate::ids::de_i64"
    )]
    pub user_id: i64,
    pub username: Option<String>,
    pub first_joined_at: Option<String>,
    pub last_joined_at: Option<String>,
    pub last_left_at: Option<String>,
    pub join_count: i64,
    pub message_count: i64,
    pub last_seen_at: Option<String>,
    pub currently_in: bool,
}

pub async fn member_joined(db: &Db, guild_id: i64, user_id: i64, username: &str) -> Result<()> {
    let (now, username) = (now_str(), username.to_string());
    db.with_conn(move |conn| {
        conn.execute(
            "INSERT INTO members (guild_id, user_id, username, first_joined_at, last_joined_at, last_left_at, join_count, currently_in)
             VALUES (?1, ?2, ?3, ?4, ?4, NULL, 1, 1)
             ON CONFLICT(guild_id, user_id) DO UPDATE SET
               username = excluded.username,
               last_joined_at = excluded.last_joined_at,
               first_joined_at = COALESCE(members.first_joined_at, excluded.first_joined_at),
               join_count = members.join_count + 1,
               currently_in = 1",
            params![guild_id, user_id, username, now],
        )?;
        Ok(())
    })
    .await
}

pub async fn member_left(db: &Db, guild_id: i64, user_id: i64) -> Result<()> {
    let now = now_str();
    db.with_conn(move |conn| {
        conn.execute(
            "UPDATE members SET last_left_at = ?3, currently_in = 0 WHERE guild_id = ?1 AND user_id = ?2",
            params![guild_id, user_id, now],
        )?;
        Ok(())
    })
    .await
}

pub async fn bump_message(db: &Db, guild_id: i64, user_id: i64, username: &str) -> Result<()> {
    let (now, username) = (now_str(), username.to_string());
    db.with_conn(move |conn| {
        conn.execute(
            "INSERT INTO members (guild_id, user_id, username, join_count, message_count, last_seen_at, currently_in)
             VALUES (?1, ?2, ?3, 0, 1, ?4, 1)
             ON CONFLICT(guild_id, user_id) DO UPDATE SET
               username = excluded.username,
               message_count = members.message_count + 1,
               last_seen_at = excluded.last_seen_at",
            params![guild_id, user_id, username, now],
        )?;
        Ok(())
    })
    .await
}

fn row_to_member(r: &Row) -> Result<MemberRow> {
    Ok(MemberRow {
        guild_id: r.get(0)?,
        user_id: r.get(1)?,
        username: r.get(2)?,
        first_joined_at: r.get(3)?,
        last_joined_at: r.get(4)?,
        last_left_at: r.get(5)?,
        join_count: r.get(6)?,
        message_count: r.get(7)?,
        last_seen_at: r.get(8)?,
        currently_in: r.get::<i64>(9)? != 0,
    })
}

const MEMBER_COLS: &str = "guild_id, user_id, username, first_joined_at, last_joined_at, last_left_at, join_count, message_count, last_seen_at, currently_in";

pub async fn member_row(db: &Db, guild_id: i64, user_id: i64) -> Result<Option<MemberRow>> {
    db.with_conn(move |conn| {
        conn.query_row(
            &format!("SELECT {MEMBER_COLS} FROM members WHERE guild_id = ?1 AND user_id = ?2"),
            params![guild_id, user_id],
            row_to_member,
        )
        .optional()
    })
    .await
}

pub async fn search_members(db: &Db, guild_id: i64, q: &str, limit: i64) -> Result<Vec<MemberRow>> {
    let limit = limit.clamp(1, 200);
    let (like, q, limit) = (format!("%{}%", q.to_lowercase()), q.to_string(), limit);
    db.with_conn(move |conn| {
        let mut stmt = conn.prepare(&format!(
            "SELECT {MEMBER_COLS} FROM members WHERE guild_id = ?1 AND (LOWER(COALESCE(username,'')) LIKE ?2 OR CAST(user_id AS TEXT) LIKE ?2)
             ORDER BY last_seen_at DESC LIMIT ?3"
        ))?;
        let rows = stmt.query_map(params![guild_id, like, limit], row_to_member)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        let _ = q;
        Ok(out)
    })
    .await
}

pub async fn backfill_member(db: &Db, guild_id: i64, user_id: i64, username: &str) -> Result<()> {
    let (now, username) = (now_str(), username.to_string());
    db.with_conn(move |conn| {
        conn.execute(
            "INSERT INTO members (guild_id, user_id, username, first_joined_at, last_joined_at, join_count, currently_in)
             VALUES (?1, ?2, ?3, ?4, ?4, 1, 1)
             ON CONFLICT(guild_id, user_id) DO UPDATE SET
               username = excluded.username, currently_in = 1,
               first_joined_at = COALESCE(members.first_joined_at, excluded.first_joined_at)",
            params![guild_id, user_id, username, now],
        )?;
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------
// messages
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub async fn insert_message(
    db: &Db,
    message_id: i64,
    guild_id: i64,
    channel_id: i64,
    author_id: i64,
    content: &str,
    attachments: &str,
    created_at: &str,
) -> Result<()> {
    let (content, attachments, created_at) = (
        content.to_string(),
        attachments.to_string(),
        created_at.to_string(),
    );
    db.with_conn(move |conn| {
        conn.execute(
            "INSERT OR REPLACE INTO messages (message_id, guild_id, channel_id, author_id, content, attachments, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![message_id, guild_id, channel_id, author_id, content, attachments, created_at],
        )?;
        Ok(())
    })
    .await
}

pub async fn get_message(db: &Db, message_id: i64) -> Result<Option<(String, String)>> {
    db.with_conn(move |conn| {
        conn.query_row(
            "SELECT content, attachments FROM messages WHERE message_id = ?1",
            params![message_id],
            |r| Ok((r.get::<String>(0)?, r.get::<String>(1)?)),
        )
        .optional()
    })
    .await
}

pub async fn mark_deleted(db: &Db, message_id: i64) -> Result<()> {
    let now = now_str();
    db.with_conn(move |conn| {
        conn.execute(
            "UPDATE messages SET deleted_at = ?1 WHERE message_id = ?2 AND deleted_at IS NULL",
            params![now, message_id],
        )?;
        Ok(())
    })
    .await
}

pub async fn mark_edited(db: &Db, message_id: i64, content: &str) -> Result<()> {
    let (now, content) = (now_str(), content.to_string());
    db.with_conn(move |conn| {
        conn.execute(
            "UPDATE messages SET content = ?1, edited_at = ?2 WHERE message_id = ?3",
            params![content, now, message_id],
        )?;
        Ok(())
    })
    .await
}

pub async fn purge_old_messages(db: &Db) -> Result<usize> {
    db.with_conn(move |conn| {
        let defaults = terminal_defaults_conn(conn)?;
        let guild_ids: Vec<i64> = {
            let mut stmt = conn.prepare("SELECT DISTINCT guild_id FROM messages")?;
            let rows = stmt.query_map((), |r| Ok(r.get::<i64>(0)?))?;
            let mut v = Vec::new();
            for row in rows {
                v.push(row?);
            }
            v
        };

        let mut total = 0usize;
        for guild_id in guild_ids {
            let days = retention_days_for_guild(conn, guild_id, &defaults)?;
            // 0 = keep forever (UI allows min 0).
            if days == 0 {
                continue;
            }
            let Some(duration) = chrono::Duration::try_days(days as i64) else {
                tracing::warn!(
                    "skipping invalid retention_days={days} for guild {guild_id}; \
                     set it to at most {MAX_RETENTION_DAYS}"
                );
                continue;
            };
            let cutoff = (Utc::now() - duration).to_rfc3339();
            let n = conn.execute(
                "DELETE FROM messages WHERE guild_id = ?1 AND created_at < ?2",
                params![guild_id, cutoff],
            )?;
            total += n;
        }
        Ok(total)
    })
    .await
}

/// Effective retention for a guild: field-level guild → global → config defaults.
fn retention_days_for_guild(
    conn: &impl SqlExecutor,
    guild_id: i64,
    defaults: &GuildSettings,
) -> Result<u32> {
    let guild = read_settings_override_conn(conn, guild_id)?;
    let global = read_settings_override_conn(conn, GLOBAL_SCOPE)?;
    Ok(resolve_settings(&guild, &global, defaults).retention_days)
}

// ---------------------------------------------------------------------------
// events (audit log)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EventRow {
    pub id: i64,
    #[serde(
        serialize_with = "crate::ids::ser_i64_str",
        deserialize_with = "crate::ids::de_i64"
    )]
    pub guild_id: i64,
    pub event_type: String,
    #[serde(
        default,
        serialize_with = "crate::ids::ser_opt_i64_str",
        deserialize_with = "crate::ids::de_opt_i64"
    )]
    pub user_id: Option<i64>,
    #[serde(
        default,
        serialize_with = "crate::ids::ser_opt_i64_str",
        deserialize_with = "crate::ids::de_opt_i64"
    )]
    pub actor_id: Option<i64>,
    pub data: Option<String>,
    pub created_at: String,
}

pub async fn insert_event(
    db: &Db,
    guild_id: i64,
    event_type: &str,
    user_id: Option<i64>,
    actor_id: Option<i64>,
    data: &serde_json::Value,
) -> Result<i64> {
    let (now, event_type) = (now_str(), event_type.to_string());
    // Persist WebUI-safe JSON: snowflakes above JS MAX_SAFE_INTEGER as strings.
    let data = if data.is_null() {
        None
    } else {
        Some(crate::ids::stringify_js_unsafe_integers(data).to_string())
    };
    db.with_conn(move |conn| {
        conn.execute(
            "INSERT INTO events (guild_id, event_type, user_id, actor_id, data, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![guild_id, event_type, user_id, actor_id, data, now],
        )?;
        Ok(conn.last_insert_rowid())
    })
    .await
}

pub async fn query_events(
    db: &Db,
    guild_id: i64,
    event_type: Option<&str>,
    user_id: Option<i64>,
    limit: i64,
) -> Result<Vec<EventRow>> {
    let limit = limit.clamp(1, 200);
    let event_type = event_type.map(|s| s.to_string());
    let mut sql = String::from(
        "SELECT id, guild_id, event_type, user_id, actor_id, data, created_at
         FROM events WHERE guild_id = ?1",
    );
    let mut args: Vec<Value> = vec![guild_id.into()];
    if let Some(et) = &event_type {
        sql.push_str(" AND event_type = ?");
        args.push(et.to_string().into());
    }
    if let Some(uid) = user_id {
        sql.push_str(" AND user_id = ?");
        args.push(uid.into());
    }
    sql.push_str(" ORDER BY id DESC LIMIT ?");
    args.push(limit.into());

    db.with_conn(move |conn| {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(args), |r| {
            Ok(EventRow {
                id: r.get(0)?,
                guild_id: r.get(1)?,
                event_type: r.get(2)?,
                user_id: r.get(3)?,
                actor_id: r.get(4)?,
                data: r.get(5)?,
                created_at: r.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            let mut row = row?;
            // Heal legacy rows that stored snowflakes as JSON numbers.
            if let Some(raw) = row.data.take() {
                row.data = Some(crate::ids::wire_safe_event_data_json(&raw));
            }
            out.push(row);
        }
        Ok(out)
    })
    .await
}

// ---------------------------------------------------------------------------
// warns / bans
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WarnRow {
    pub id: i64,
    #[serde(
        serialize_with = "crate::ids::ser_i64_str",
        deserialize_with = "crate::ids::de_i64"
    )]
    pub guild_id: i64,
    #[serde(
        serialize_with = "crate::ids::ser_i64_str",
        deserialize_with = "crate::ids::de_i64"
    )]
    pub user_id: i64,
    #[serde(
        serialize_with = "crate::ids::ser_i64_str",
        deserialize_with = "crate::ids::de_i64"
    )]
    pub moderator_id: i64,
    pub reason: Option<String>,
    pub created_at: String,
}

pub async fn add_warn(
    db: &Db,
    guild_id: i64,
    user_id: i64,
    moderator_id: i64,
    reason: &str,
) -> Result<i64> {
    let (now, reason) = (now_str(), reason.to_string());
    db.with_conn(move |conn| {
        conn.execute(
            "INSERT INTO warns (guild_id, user_id, moderator_id, reason, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![guild_id, user_id, moderator_id, reason, now],
        )?;
        Ok(conn.last_insert_rowid())
    })
    .await
}

pub async fn list_warns(db: &Db, guild_id: i64, user_id: i64) -> Result<Vec<WarnRow>> {
    db.with_conn(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, guild_id, user_id, moderator_id, reason, created_at
             FROM warns WHERE guild_id = ?1 AND user_id = ?2 ORDER BY id DESC",
        )?;
        let rows = stmt.query_map(params![guild_id, user_id], |r| {
            Ok(WarnRow {
                id: r.get(0)?,
                guild_id: r.get(1)?,
                user_id: r.get(2)?,
                moderator_id: r.get(3)?,
                reason: r.get(4)?,
                created_at: r.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
    .await
}

pub async fn clear_warns(db: &Db, guild_id: i64, user_id: i64) -> Result<usize> {
    db.with_conn(move |conn| {
        conn.execute(
            "DELETE FROM warns WHERE guild_id = ?1 AND user_id = ?2",
            params![guild_id, user_id],
        )
    })
    .await
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BanRow {
    pub id: i64,
    #[serde(
        serialize_with = "crate::ids::ser_i64_str",
        deserialize_with = "crate::ids::de_i64"
    )]
    pub guild_id: i64,
    #[serde(
        serialize_with = "crate::ids::ser_i64_str",
        deserialize_with = "crate::ids::de_i64"
    )]
    pub user_id: i64,
    #[serde(
        default,
        serialize_with = "crate::ids::ser_opt_i64_str",
        deserialize_with = "crate::ids::de_opt_i64"
    )]
    pub moderator_id: Option<i64>,
    pub reason: Option<String>,
    pub created_at: String,
    pub active: bool,
}

pub async fn record_ban(
    db: &Db,
    guild_id: i64,
    user_id: i64,
    moderator_id: Option<i64>,
    reason: &str,
) -> Result<()> {
    let (now, reason) = (now_str(), reason.to_string());
    db.with_conn(move |conn| {
        conn.execute(
            "INSERT INTO bans (guild_id, user_id, moderator_id, reason, created_at, active)
             VALUES (?1, ?2, ?3, ?4, ?5, 1)",
            params![guild_id, user_id, moderator_id, reason, now],
        )?;
        Ok(())
    })
    .await
}

pub async fn mark_unbanned(db: &Db, guild_id: i64, user_id: i64) -> Result<()> {
    // Mirror of Discord ban state. Callers that already succeeded at Discord
    // `remove_ban` should treat errors here as non-fatal (log + continue);
    // `GUILD_BAN_REMOVE` on the bot also calls this to heal drift.
    db.with_conn(move |conn| {
        conn.execute(
            "UPDATE bans SET active = 0 WHERE guild_id = ?1 AND user_id = ?2 AND active = 1",
            params![guild_id, user_id],
        )?;
        Ok(())
    })
    .await
}

pub async fn list_bans(db: &Db, guild_id: i64, limit: i64) -> Result<Vec<BanRow>> {
    let limit = limit.clamp(1, 200);
    db.with_conn(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, guild_id, user_id, moderator_id, reason, created_at, active
             FROM bans WHERE guild_id = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![guild_id, limit], |r| {
            Ok(BanRow {
                id: r.get(0)?,
                guild_id: r.get(1)?,
                user_id: r.get(2)?,
                moderator_id: r.get(3)?,
                reason: r.get(4)?,
                created_at: r.get(5)?,
                active: r.get::<i64>(6)? != 0,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
    .await
}

// ---------------------------------------------------------------------------
// users
// ---------------------------------------------------------------------------

pub async fn upsert_user(
    db: &Db,
    user_id: i64,
    username: &str,
    global_name: Option<&str>,
    avatar: Option<&str>,
) -> Result<()> {
    let (now, username) = (now_str(), username.to_string());
    let (global_name, avatar) = (
        global_name.map(|s| s.to_string()),
        avatar.map(|s| s.to_string()),
    );
    db.with_conn(move |conn| {
        conn.execute(
            "INSERT INTO users (user_id, username, global_name, avatar, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(user_id) DO UPDATE SET
               username = excluded.username,
               global_name = excluded.global_name,
               avatar = excluded.avatar,
               updated_at = excluded.updated_at
             WHERE users.username IS NOT excluded.username
                OR users.global_name IS NOT excluded.global_name
                OR users.avatar IS NOT excluded.avatar",
            params![user_id, username, global_name, avatar, now],
        )?;
        Ok(())
    })
    .await
}

#[allow(dead_code)]
pub async fn get_user(db: &Db, user_id: i64) -> Result<Option<(String, Option<String>)>> {
    db.with_conn(move |conn| {
        conn.query_row(
            "SELECT username, avatar FROM users WHERE user_id = ?1",
            params![user_id],
            |r| Ok((r.get::<String>(0)?, r.get::<Option<String>>(1)?)),
        )
        .optional()
    })
    .await
}

// ---------------------------------------------------------------------------
// stats
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Stats {
    pub messages: i64,
    pub events: i64,
    pub members: i64,
    pub warns: i64,
    pub bans: i64,
    pub guilds: i64,
}

/// Process-wide aggregates for bot housekeeping logs only.
/// Do **not** expose this to Discord commands or the WebUI — use [`guild_stats`].
pub async fn stats(db: &Db) -> Result<Stats> {
    db.with_conn(|conn| {
        let one = |sql: &str| conn.query_row(sql, (), |r| Ok(r.get::<i64>(0)?));
        Ok(Stats {
            messages: one("SELECT COUNT(*) FROM messages")?,
            events: one("SELECT COUNT(*) FROM events")?,
            members: one("SELECT COUNT(*) FROM members")?,
            warns: one("SELECT COUNT(*) FROM warns")?,
            bans: one("SELECT COUNT(*) FROM bans")?,
            guilds: one("SELECT COUNT(DISTINCT guild_id) FROM members")?,
        })
    })
    .await
}

/// Per-guild aggregates for the WebUI (avoids leaking other servers' data).
pub async fn guild_stats(db: &Db, guild_id: i64) -> Result<Stats> {
    db.with_conn(move |conn| {
        let one = |sql: &str| conn.query_row(sql, params![guild_id], |r| Ok(r.get::<i64>(0)?));
        Ok(Stats {
            messages: one("SELECT COUNT(*) FROM messages WHERE guild_id = ?1")?,
            events: one("SELECT COUNT(*) FROM events WHERE guild_id = ?1")?,
            members: one("SELECT COUNT(*) FROM members WHERE guild_id = ?1")?,
            warns: one("SELECT COUNT(*) FROM warns WHERE guild_id = ?1")?,
            bans: one("SELECT COUNT(*) FROM bans WHERE guild_id = ?1")?,
            guilds: 1,
        })
    })
    .await
}

/// Parses an RFC3339 timestamp into a friendly relative duration string.
pub fn time_ago(ts: &Option<String>) -> String {
    let Some(ts) = ts else { return "never".into() };
    let Ok(dt) = DateTime::parse_from_rfc3339(ts) else {
        return ts.clone();
    };
    let diff = Utc::now() - dt.with_timezone(&Utc);
    let secs = diff.num_seconds();
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

// ---------------------------------------------------------------------------
// full guild config (all scopes resolved) — used by the in-memory config cache
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ConfigParts {
    pub settings: GuildSettings,
    pub rules: std::collections::HashMap<String, RuleDef>,
    pub commands: std::collections::HashMap<String, CommandDef>,
    pub templates: std::collections::HashMap<String, String>,
    pub log_channels: std::collections::HashMap<String, i64>,
    pub badwords: Vec<String>,
}

/// Loads `(key, value)` rows where the value column holds JSON-serialized
/// config (rules, commands). The value column name varies per table.
fn rows_map_json<T: DeserializeOwned + Send + 'static>(
    conn: &impl SqlExecutor,
    table: &str,
    key_col: &str,
    value_col: &str,
    guild_id: i64,
) -> Result<std::collections::HashMap<String, T>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {key_col}, {value_col} FROM {table} WHERE guild_id = ?1"
    ))?;
    let rows = stmt.query_map(params![guild_id], |r| {
        Ok((r.get::<String>(0)?, r.get::<String>(1)?))
    })?;
    let mut out = std::collections::HashMap::new();
    for row in rows {
        let (k, v) = row?;
        out.insert(k, read_json(&v, table)?);
    }
    Ok(out)
}

/// Loads `(key, value)` rows where the value column is plain text
/// (templates) or an integer (log_channels).
trait PlainValue: Sized {
    fn get(row: &Row, index: i32) -> Result<Self>;
}

impl PlainValue for String {
    fn get(row: &Row, index: i32) -> Result<Self> {
        Ok(row.get(index)?)
    }
}

impl PlainValue for i64 {
    fn get(row: &Row, index: i32) -> Result<Self> {
        Ok(row.get(index)?)
    }
}

fn rows_map_plain<T: PlainValue + Send + 'static>(
    conn: &impl SqlExecutor,
    table: &str,
    key_col: &str,
    value_col: &str,
    guild_id: i64,
) -> Result<std::collections::HashMap<String, T>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {key_col}, {value_col} FROM {table} WHERE guild_id = ?1"
    ))?;
    let rows = stmt.query_map(params![guild_id], |r| {
        Ok((r.get::<String>(0)?, T::get(r, 1)?))
    })?;
    let mut out = std::collections::HashMap::new();
    for row in rows {
        let (k, v) = row?;
        out.insert(k, v);
    }
    Ok(out)
}

pub async fn load_config_parts(db: &Db, guild_id: i64) -> Result<ConfigParts> {
    Ok(load_config_parts_with_version(db, guild_id).await?.1)
}

pub async fn load_config_parts_with_version(db: &Db, guild_id: i64) -> Result<(u64, ConfigParts)> {
    db.with_conn(move |conn| {
        let tx = conn.transaction()?;
        let version = get_config_version_conn(&tx)?;
        let conn = &tx;
        let defaults = terminal_defaults_conn(conn)?;
        // settings (effective: field-level guild → global → config terminal defaults)
        let g_settings = if guild_id == GLOBAL_SCOPE {
            None
        } else {
            read_settings_override_conn(conn, guild_id)?
        };
        let gl_settings = read_settings_override_conn(conn, GLOBAL_SCOPE)?;
        let settings = resolve_settings(&g_settings, &gl_settings, &defaults);

        // rules: map rule_id -> effective def
        let g_rules: std::collections::HashMap<String, RuleDef> =
            rows_map_json(conn, "rules", "rule_id", "data", guild_id)?;
        let gl_rules: std::collections::HashMap<String, RuleDef> =
            rows_map_json(conn, "rules", "rule_id", "data", GLOBAL_SCOPE)?;
        let default_rules = crate::defaults::default_rules();
        let mut rules = std::collections::HashMap::new();
        for (i, id) in crate::types::RULE_IDS.iter().enumerate() {
            let def = default_rules[i].clone();
            rules.insert(
                id.to_string(),
                resolve(
                    &g_rules.get(*id).cloned(),
                    &gl_rules.get(*id).cloned(),
                    &def,
                ),
            );
        }

        // commands: name(lowercase) -> effective def (guild ?? global)
        let g_cmds: std::collections::HashMap<String, CommandDef> =
            rows_map_json(conn, "commands", "name", "data", guild_id)?;
        let gl_cmds: std::collections::HashMap<String, CommandDef> =
            rows_map_json(conn, "commands", "name", "data", GLOBAL_SCOPE)?;
        let mut commands: std::collections::HashMap<String, CommandDef> = gl_cmds.clone();
        for (name, def) in g_cmds {
            commands.insert(name, def);
        }

        // templates: effective per event (plain text column `template`)
        let g_tpl: std::collections::HashMap<String, String> =
            rows_map_plain(conn, "templates", "event_type", "template", guild_id)?;
        let gl_tpl: std::collections::HashMap<String, String> =
            rows_map_plain(conn, "templates", "event_type", "template", GLOBAL_SCOPE)?;
        let mut templates = std::collections::HashMap::new();
        for (event, def) in crate::defaults::default_templates() {
            let resolved = resolve(
                &g_tpl.get(event).cloned(),
                &gl_tpl.get(event).cloned(),
                &def.to_string(),
            );
            templates.insert(event.to_string(), resolved);
        }
        for (event, t) in &g_tpl {
            templates.insert(event.clone(), t.clone());
        }
        for (event, t) in &gl_tpl {
            templates.entry(event.clone()).or_insert_with(|| t.clone());
        }

        // log channels: effective per event (integer column `channel_id`)
        let g_lc: std::collections::HashMap<String, i64> =
            rows_map_plain(conn, "log_channels", "event_type", "channel_id", guild_id)?;
        let mut log_channels = std::collections::HashMap::new();
        // Discord channel IDs belong to exactly one guild. Deliberately ignore
        // legacy global rows: falling back to one would post another guild's
        // moderation events into a foreign channel.
        for (event, c) in &g_lc {
            log_channels.insert(event.clone(), *c);
        }

        // badwords: effective = global + guild (union)
        let mut badwords = Vec::new();
        for scope in [GLOBAL_SCOPE, guild_id] {
            let mut stmt = conn.prepare("SELECT pattern FROM badwords WHERE guild_id = ?1")?;
            let rows = stmt.query_map(params![scope], |r| Ok(r.get::<String>(0)?))?;
            for row in rows {
                badwords.push(row?);
            }
        }

        let parts = ConfigParts {
            settings,
            rules,
            commands,
            templates,
            log_channels,
            badwords,
        };
        tx.commit()?;
        Ok((version, parts))
    })
    .await
}

/// Loads the effective command registry and its durable version from one
/// read transaction. This prevents a remote replica from supplying version N
/// on one pooled connection and stale N-1 command rows on another.
pub async fn load_slash_commands_with_version(
    db: &Db,
    guild_id: i64,
) -> Result<(u64, std::collections::HashMap<String, CommandDef>)> {
    db.with_conn(move |conn| {
        let tx = conn.transaction()?;
        let version = get_slash_registry_version_conn(&tx)?;
        let guild: std::collections::HashMap<String, CommandDef> =
            rows_map_json(&tx, "commands", "name", "data", guild_id)?;
        let mut commands: std::collections::HashMap<String, CommandDef> =
            rows_map_json(&tx, "commands", "name", "data", GLOBAL_SCOPE)?;
        commands.extend(guild);
        tx.commit()?;
        Ok((version, commands))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_pool_admission_has_a_deadline() {
        let tmp = tmp_db_path("pool_admission_timeout");
        let mut db = Db::open(&tmp, 2).unwrap();
        db.permit_timeout = Some(Duration::from_millis(50));

        tokio_test_block_on(async {
            let first = Arc::clone(&db.permits).acquire_owned().await.unwrap();
            let second = Arc::clone(&db.permits).acquire_owned().await.unwrap();
            let error = db
                .with_conn(|_| Ok(()))
                .await
                .expect_err("pool admission must not wait forever");
            assert!(
                is_database_operation_timeout(&error),
                "unexpected: {error:#}"
            );
            assert!(error.to_string().contains("waiting for connection"));
            drop((first, second));
        });

        let _ = std::fs::remove_file(tmp);
    }

    #[test]
    fn timed_out_connection_is_discarded_before_pool_reuse() {
        let tmp = tmp_db_path("discard_timed_out_connection");
        let db = Db::open(&tmp, 2).unwrap();

        let error = tokio_test_block_on(db.with_conn(|_| {
            Err::<(), _>(database_operation_timed_out(
                "test operation",
                Duration::from_millis(1),
            ))
        }))
        .expect_err("simulated operation must time out");
        assert!(is_database_operation_timeout(&error));
        assert_eq!(
            db.connections.idle.lock().unwrap().len(),
            1,
            "timed-out connection must not be returned to the idle pool"
        );

        let first = ConnectionLease::checkout(Arc::clone(&db.connections)).unwrap();
        let replacement = ConnectionLease::checkout(Arc::clone(&db.connections)).unwrap();
        drop((first, replacement));
        assert_eq!(db.connections.idle.lock().unwrap().len(), 2);

        let _ = std::fs::remove_file(tmp);
    }

    #[test]
    fn failed_remote_operation_discards_connection_before_reuse() {
        let tmp = tmp_db_path("discard_failed_remote_connection");
        let mut db = Db::open(&tmp, 2).unwrap();
        Arc::get_mut(&mut db.connections).unwrap().remote = true;

        let error = tokio_test_block_on(
            db.with_conn(|_| Err::<(), _>(anyhow::anyhow!("simulated transport failure"))),
        )
        .expect_err("simulated remote operation must fail");
        assert!(error.to_string().contains("simulated transport failure"));
        assert_eq!(
            db.connections.idle.lock().unwrap().len(),
            1,
            "failed remote connection must not be returned to the idle pool"
        );

        let _ = std::fs::remove_file(tmp);
    }

    #[test]
    fn cancelled_caller_does_not_release_blocking_task_permit() {
        let tmp = tmp_db_path("cancelled_permit");
        let db = Db::open(&tmp, 2).unwrap();

        tokio_test_block_on(async {
            let worker_db = db.clone();
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let task = tokio::spawn(async move {
                worker_db
                    .with_conn(move |_conn| {
                        started_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        Ok(())
                    })
                    .await
            });

            started_rx.await.unwrap();
            assert_eq!(db.permits.available_permits(), 1);
            assert_eq!(db.connections.idle.lock().unwrap().len(), 1);

            task.abort();
            tokio::task::yield_now().await;
            assert_eq!(
                db.permits.available_permits(),
                1,
                "aborting the async caller must not release blocking work's permit"
            );
            assert_eq!(
                db.connections.idle.lock().unwrap().len(),
                1,
                "aborting the async caller must not return an in-use connection"
            );

            release_tx.send(()).unwrap();
            for _ in 0..100 {
                if db.permits.available_permits() == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert_eq!(db.permits.available_permits(), 2);
            assert_eq!(db.connections.idle.lock().unwrap().len(), 2);
        });

        cleanup_db(&tmp);
    }

    #[test]
    fn seeds_globals_in_memory() {
        let tmp = std::env::temp_dir().join(format!("jmodguard_test_{}.db", std::process::id()));
        let db = Db::open(&tmp, 2).unwrap();
        let parts = db
            .with_conn_sync(|conn| {
                // directly inspect rows
                let rules: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM rules WHERE guild_id = ?1",
                        params![GLOBAL_SCOPE],
                        |r| Ok(r.get(0)?),
                    )
                    .unwrap();
                let commands: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM commands WHERE guild_id = ?1",
                        params![GLOBAL_SCOPE],
                        |r| Ok(r.get(0)?),
                    )
                    .unwrap();
                let templates: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM templates WHERE guild_id = ?1",
                        params![GLOBAL_SCOPE],
                        |r| Ok(r.get(0)?),
                    )
                    .unwrap();
                let badwords: i64 = conn
                    .query_row("SELECT COUNT(*) FROM badwords", (), |r| Ok(r.get(0)?))
                    .unwrap();
                Ok((rules, commands, templates, badwords))
            })
            .unwrap();
        assert_eq!(
            parts.0,
            crate::types::RULE_IDS.len() as i64,
            "all 7 rules seeded"
        );
        assert!(parts.1 >= 20, "built-in commands seeded");
        assert_eq!(
            parts.2,
            crate::types::EVENT_TYPES.len() as i64,
            "all event templates seeded"
        );
        assert_eq!(parts.3, 0, "no badwords by default");
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(format!("{}-wal", tmp.display()));
        let _ = std::fs::remove_file(format!("{}-shm", tmp.display()));
    }

    #[test]
    fn config_parts_loads_every_section() {
        // Regression: load_config_parts must succeed for a real guild and
        // return rules, commands, templates AND log channels (these tables
        // use non-`data` value columns).
        let tmp = std::env::temp_dir().join(format!("jmodguard_test3_{}.db", std::process::id()));
        let db = Db::open(&tmp, 2).unwrap();
        let guild = 271083998816632834i64;
        let parts = tokio_test_block_on(crate::db::load_config_parts(&db, guild));
        let parts = parts.unwrap();
        assert_eq!(parts.rules.len(), crate::types::RULE_IDS.len(), "all rules");
        assert!(
            parts.commands.len() >= 20,
            "commands present: {}",
            parts.commands.len()
        );
        assert_eq!(
            parts.templates.len(),
            crate::types::EVENT_TYPES.len(),
            "all templates"
        );
        assert!(parts.templates.contains_key("automod"));
        assert!(parts.log_channels.is_empty(), "no log channels by default");
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(format!("{}-wal", tmp.display()));
        let _ = std::fs::remove_file(format!("{}-shm", tmp.display()));
    }

    #[test]
    fn migration_archives_legacy_global_log_channels() {
        let tmp = tmp_db_path("archive_global_logchannels");
        let db = Db::open(&tmp, 2).unwrap();
        db.with_conn_sync(|conn| {
            conn.execute(
                "INSERT INTO log_channels (guild_id, event_type, channel_id) VALUES (0, 'automod', 12345)",
                (),
            )?;
            // Simulate an installation that had reached migration 0002 before
            // the per-guild-only log-channel migration was released.
            conn.pragma_update(None, "user_version", 2)?;
            migrate_conn(conn)?;

            let active: i64 = conn.query_row(
                "SELECT COUNT(*) FROM log_channels WHERE guild_id = 0",
                (),
                |r| Ok(r.get(0)?),
            )?;
            let archived: (String, i64) = conn.query_row(
                "SELECT event_type, channel_id FROM legacy_global_log_channels",
                (),
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            assert_eq!(active, 0);
            assert_eq!(archived, ("automod".into(), 12345));
            Ok(())
        })
        .unwrap();
        cleanup_db(&tmp);
    }

    fn tokio_test_block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    fn tmp_db_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "jmodguard_{label}_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn cleanup_db(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }

    #[test]
    fn config_version_bumps_on_settings_write() {
        let tmp = tmp_db_path("cfgver");
        let db = Db::open(&tmp, 2).unwrap();
        let v0 = tokio_test_block_on(crate::db::get_config_version(&db)).unwrap();
        assert!(v0 > 0, "initial seed publishes its config version");
        tokio_test_block_on(crate::db::upsert_settings(
            &db,
            1,
            &GuildSettings::default(),
        ))
        .unwrap();
        let v1 = tokio_test_block_on(crate::db::get_config_version(&db)).unwrap();
        assert_eq!(v1, v0 + 1);
        // A remote database must never be opened without credentials.
        let err = Db::open_with(DbOpenOptions {
            path: tmp.clone(),
            url: "libsql://example.invalid".into(),
            token: String::new(),
            pool_size: 2,
            migrate: true,
            default_prefix: "!".into(),
            retention_days: 7,
        })
        .err()
        .expect("remote URL without a token should fail");
        assert!(
            err.to_string().contains("JMODGUARD_DB_TOKEN is required"),
            "unexpected: {err:#}"
        );
        cleanup_db(&tmp);
    }

    #[test]
    fn api_open_refuses_unmigrated_db() {
        let tmp = tmp_db_path("unmigrated");
        Connection::open(&tmp).unwrap();
        let err = Db::open_with(DbOpenOptions {
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: false,
            default_prefix: "!".into(),
            retention_days: 7,
        })
        .err()
        .expect("unmigrated open should fail");
        let msg = err.to_string();
        assert!(msg.contains("schema is behind"), "unexpected error: {msg}");
        assert!(
            crate::db::is_schema_not_ready(&err),
            "unmigrated API open must be retryable SchemaNotReady"
        );
        cleanup_db(&tmp);
    }

    #[test]
    fn api_open_retries_until_bot_migrates() {
        let tmp = tmp_db_path("api_retry_migrate");
        Connection::open(&tmp).unwrap();
        let api_opts = DbOpenOptions {
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: false,
            default_prefix: "!".into(),
            retention_days: 7,
        };
        let bot_opts = DbOpenOptions {
            migrate: true,
            ..api_opts.clone()
        };
        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(80));
            Db::open_with(bot_opts).expect("bot migrate must succeed");
        });
        tokio_test_block_on(Db::open_with_until_ready(
            api_opts,
            40,
            std::time::Duration::from_millis(25),
        ))
        .expect("API must succeed once bot migrations finish");
        handle.join().expect("bot migrate thread panicked");
        cleanup_db(&tmp);
    }

    #[test]
    fn api_waits_for_completed_bot_bootstrap() {
        let tmp = tmp_db_path("api_bootstrap_marker");
        let mut conn = Connection::open(&tmp).unwrap();
        migrate_conn(&mut conn).unwrap();
        write_terminal_defaults_meta(&conn, &GuildSettings::default()).unwrap();

        let api_opts = DbOpenOptions {
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: false,
            default_prefix: "!".into(),
            retention_days: 7,
        };
        let err = Db::open_with(api_opts.clone())
            .err()
            .expect("API must not open before default rows are seeded");
        assert!(is_schema_not_ready(&err), "unexpected error: {err:#}");
        assert!(
            err.to_string().contains("bootstrap is incomplete"),
            "unexpected error: {err:#}"
        );

        Db::open_with(DbOpenOptions {
            migrate: true,
            ..api_opts.clone()
        })
        .expect("bot bootstrap must complete");
        Db::open_with(api_opts).expect("API must open after bot bootstrap completes");
        cleanup_db(&tmp);
    }

    #[test]
    fn api_open_retries_while_bot_holds_sqlite_lock() {
        let tmp = tmp_db_path("api_retry_locked");
        let db = Db::open(&tmp, 2).unwrap();
        drop(db);

        let lock_path = tmp.clone();
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let conn = Connection::open(lock_path).unwrap();
            conn.execute_batch("BEGIN EXCLUSIVE").unwrap();
            locked_tx.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(100));
            conn.execute_batch("COMMIT").unwrap();
        });
        locked_rx.recv().unwrap();

        tokio_test_block_on(Db::open_with_until_ready(
            DbOpenOptions {
                path: tmp.clone(),
                url: String::new(),
                token: String::new(),
                pool_size: 2,
                migrate: false,
                default_prefix: "!".into(),
                retention_days: 7,
            },
            10,
            std::time::Duration::from_millis(25),
        ))
        .expect("API must wait for a transient bot bootstrap lock");
        holder.join().unwrap();
        cleanup_db(&tmp);
    }

    #[test]
    fn api_open_until_ready_times_out_without_bot() {
        let tmp = tmp_db_path("api_retry_timeout");
        Connection::open(&tmp).unwrap();
        let err = tokio_test_block_on(Db::open_with_until_ready(
            DbOpenOptions {
                path: tmp.clone(),
                url: String::new(),
                token: String::new(),
                pool_size: 2,
                migrate: false,
                default_prefix: "!".into(),
                retention_days: 7,
            },
            2,
            std::time::Duration::from_millis(1),
        ))
        .err()
        .expect("must not succeed without bot migrate");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("timed out waiting for jmodguard-bot"),
            "unexpected error: {msg}"
        );
        assert!(
            crate::db::is_schema_not_ready(&err),
            "timeout must still chain SchemaNotReady"
        );
        cleanup_db(&tmp);
    }

    #[test]
    fn delete_badword_rejects_other_guild_idor() {
        let tmp = tmp_db_path("badword_idor");
        let db = Db::open(&tmp, 2).unwrap();
        let id_a = tokio_test_block_on(crate::db::add_badword(&db, 11, "spam", None)).unwrap();
        let id_b = tokio_test_block_on(crate::db::add_badword(&db, 22, "scam", None)).unwrap();
        let err = tokio_test_block_on(crate::db::delete_badword(&db, id_a, 22))
            .err()
            .expect("cross-guild delete should fail");
        assert!(err.to_string().contains("not found"), "unexpected: {err:#}");
        assert!(
            !err.to_string().contains("for this guild"),
            "must not leak guild-scope detail: {err:#}"
        );
        tokio_test_block_on(crate::db::delete_badword(&db, id_b, 22)).unwrap();
        // Global rows only delete when the exact GLOBAL_SCOPE is passed.
        let id_g =
            tokio_test_block_on(crate::db::add_badword(&db, GLOBAL_SCOPE, "global", None)).unwrap();
        let err = tokio_test_block_on(crate::db::delete_badword(&db, id_g, 11))
            .err()
            .expect("guild context must not delete global via exact match");
        assert!(err.to_string().contains("not found"), "unexpected: {err:#}");
        assert!(
            !err.to_string().contains("for this guild"),
            "must not leak guild-scope detail: {err:#}"
        );
        tokio_test_block_on(crate::db::delete_badword(&db, id_g, GLOBAL_SCOPE)).unwrap();
        cleanup_db(&tmp);
    }

    #[test]
    fn add_badword_rejects_pathological_pattern() {
        let tmp = tmp_db_path("badword_patho");
        let db = Db::open(&tmp, 2).unwrap();
        let err = tokio_test_block_on(crate::db::add_badword(&db, 11, r"\w{100000}", None))
            .err()
            .expect("pathological pattern must be rejected at persist");
        assert!(
            err.to_string().contains("invalid regex") || err.to_string().contains("size"),
            "unexpected: {err:#}"
        );
        let huge = "a".repeat(crate::normalize::USER_REGEX_MAX_PATTERN_BYTES + 1);
        let err = tokio_test_block_on(crate::db::add_badword(&db, 11, &huge, None))
            .err()
            .expect("oversized pattern must be rejected");
        assert!(err.to_string().contains("too long"), "unexpected: {err:#}");
        cleanup_db(&tmp);
    }

    #[test]
    fn copy_guild_config_skips_invalid_badwords() {
        let tmp = tmp_db_path("copy_badword");
        let db = Db::open(&tmp, 2).unwrap();
        tokio_test_block_on(crate::db::add_badword(&db, 11, "scam", None)).unwrap();
        // Legacy row inserted past the gate (raw SQL) — copy must not propagate it.
        tokio_test_block_on(db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO badwords (guild_id, pattern, note) VALUES (11, ?1, NULL)",
                params![r"\w{100000}"],
            )?;
            Ok(())
        }))
        .unwrap();
        let result = tokio_test_block_on(crate::db::copy_guild_config(
            &db,
            11,
            22,
            vec!["badwords".into()],
        ))
        .unwrap();
        assert_eq!(result.copied, vec!["badwords"]);
        assert_eq!(
            result.skipped_badwords, 1,
            "pathological pattern must be counted"
        );
        let words = tokio_test_block_on(crate::db::list_badwords(&db, 22)).unwrap();
        assert_eq!(words.len(), 1, "only valid pattern should copy: {words:?}");
        assert_eq!(words[0].pattern, "scam");
        let view = words[0].with_status();
        assert!(view.valid);
        cleanup_db(&tmp);
    }

    #[test]
    fn copy_badwords_refuses_all_invalid_and_preserves_destination() {
        // Root-cause regression: DELETE-then-filter wiped dest when source had
        // only unenforceable patterns.
        let tmp = tmp_db_path("copy_wipe");
        let db = Db::open(&tmp, 2).unwrap();
        tokio_test_block_on(crate::db::add_badword(&db, 22, "keep-me", None)).unwrap();
        tokio_test_block_on(db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO badwords (guild_id, pattern, note) VALUES (11, ?1, NULL)",
                params![r"\w{100000}"],
            )?;
            conn.execute(
                "INSERT INTO badwords (guild_id, pattern, note) VALUES (11, ?1, NULL)",
                params!["a".repeat(crate::normalize::USER_REGEX_MAX_PATTERN_BYTES + 1)],
            )?;
            Ok(())
        }))
        .unwrap();

        let err = tokio_test_block_on(crate::db::copy_guild_config(
            &db,
            11,
            22,
            vec!["badwords".into()],
        ))
        .err()
        .expect("all-invalid source must refuse copy");
        assert!(
            err.to_string().contains("refused")
                || err.to_string().contains("destination left unchanged"),
            "unexpected: {err:#}"
        );

        let dest = tokio_test_block_on(crate::db::list_badwords(&db, 22)).unwrap();
        assert_eq!(dest.len(), 1, "destination must be preserved: {dest:?}");
        assert_eq!(dest[0].pattern, "keep-me");
        cleanup_db(&tmp);
    }

    #[test]
    fn copy_badwords_empty_source_clears_destination() {
        // Intentional empty replace — source has zero rows, dest should clear.
        let tmp = tmp_db_path("copy_empty");
        let db = Db::open(&tmp, 2).unwrap();
        tokio_test_block_on(crate::db::add_badword(&db, 22, "old", None)).unwrap();
        let result = tokio_test_block_on(crate::db::copy_guild_config(
            &db,
            11,
            22,
            vec!["badwords".into()],
        ))
        .unwrap();
        assert_eq!(result.copied, vec!["badwords"]);
        assert_eq!(result.skipped_badwords, 0);
        let dest = tokio_test_block_on(crate::db::list_badwords(&db, 22)).unwrap();
        assert!(dest.is_empty(), "empty source must clear dest: {dest:?}");
        cleanup_db(&tmp);
    }

    #[test]
    fn copy_all_rolls_back_when_badwords_refuse() {
        // Atomic transaction: refused badwords must not leave partial section writes.
        let tmp = tmp_db_path("copy_atomic");
        let db = Db::open(&tmp, 2).unwrap();
        let mut settings = GuildSettings::default();
        settings.prefix = "!src".into();
        tokio_test_block_on(crate::db::upsert_settings(&db, 11, &settings)).unwrap();
        tokio_test_block_on(crate::db::add_badword(&db, 22, "keep", None)).unwrap();
        tokio_test_block_on(db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO badwords (guild_id, pattern, note) VALUES (11, ?1, NULL)",
                params![r"\w{100000}"],
            )?;
            Ok(())
        }))
        .unwrap();

        let err = tokio_test_block_on(crate::db::copy_guild_config(
            &db,
            11,
            22,
            vec!["all".into()],
        ))
        .err()
        .expect("all-invalid badwords must refuse entire copy");
        assert!(err.to_string().contains("refused"), "unexpected: {err:#}");

        let dest_settings = tokio_test_block_on(crate::db::get_settings(&db, 22)).unwrap();
        assert_ne!(
            dest_settings.prefix, "!src",
            "settings must roll back when badwords refuse"
        );
        let dest = tokio_test_block_on(crate::db::list_badwords(&db, 22)).unwrap();
        assert_eq!(dest[0].pattern, "keep");
        cleanup_db(&tmp);
    }

    #[test]
    fn guild_stats_are_scoped_not_global() {
        let tmp = tmp_db_path("guild_stats");
        let db = Db::open(&tmp, 2).unwrap();
        let g1 = 111i64;
        let g2 = 222i64;
        tokio_test_block_on(crate::db::bump_message(&db, g1, 1, "a")).unwrap();
        tokio_test_block_on(crate::db::bump_message(&db, g1, 2, "b")).unwrap();
        tokio_test_block_on(crate::db::bump_message(&db, g2, 3, "c")).unwrap();
        tokio_test_block_on(crate::db::add_warn(&db, g1, 1, 9, "spam")).unwrap();
        tokio_test_block_on(crate::db::add_warn(&db, g2, 3, 9, "spam")).unwrap();
        let s1 = tokio_test_block_on(crate::db::guild_stats(&db, g1)).unwrap();
        let s2 = tokio_test_block_on(crate::db::guild_stats(&db, g2)).unwrap();
        assert_eq!(s1.members, 2);
        assert_eq!(s2.members, 1);
        assert_eq!(s1.warns, 1);
        assert_eq!(s2.warns, 1);
        assert_eq!(s1.guilds, 1);
        let global = tokio_test_block_on(crate::db::stats(&db)).unwrap();
        assert_eq!(global.members, 3);
        assert_eq!(global.warns, 2);
        // Discord !stats / WebUI must use guild_stats — prove g1 never sees g2 totals.
        assert_ne!(s1.members, global.members);
        assert_ne!(s1.warns, global.warns);
        cleanup_db(&tmp);
    }

    #[test]
    fn dual_process_local_share_config_version() {
        // Bot migrates; API opens migrate=false; writer bump visible to both.
        let tmp = tmp_db_path("dual");
        let bot = Db::open_with(DbOpenOptions {
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: true,
            default_prefix: "!".into(),
            retention_days: 7,
        })
        .unwrap();
        let api = Db::open_with(DbOpenOptions {
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: false,
            default_prefix: "!".into(),
            retention_days: 7,
        })
        .unwrap();
        let baseline = tokio_test_block_on(crate::db::get_config_version(&bot)).unwrap();
        tokio_test_block_on(crate::db::upsert_settings(
            &api,
            42,
            &GuildSettings::default(),
        ))
        .unwrap();
        assert_eq!(
            tokio_test_block_on(crate::db::get_config_version(&bot)).unwrap(),
            baseline + 1
        );
        assert_eq!(
            tokio_test_block_on(crate::db::get_config_version(&api)).unwrap(),
            baseline + 1
        );
        cleanup_db(&tmp);
    }

    #[test]
    fn config_parts_loads_effective_rules() {
        let tmp = std::env::temp_dir().join(format!("jmodguard_test2_{}.db", std::process::id()));
        let db = Db::open(&tmp, 2).unwrap();
        let parts = db.with_conn_sync(|_| Ok(())).unwrap();
        let _ = parts;
        // rule id keys must exist and match RULE_IDS
        let ids: Vec<String> = db
            .with_conn_sync(|conn| {
                let mut stmt =
                    conn.prepare("SELECT rule_id FROM rules WHERE guild_id = 0 ORDER BY rule_id")?;
                let rows = stmt.query_map((), |r| Ok(r.get::<String>(0)?))?;
                let mut v = Vec::new();
                for r in rows {
                    v.push(r?);
                }
                Ok(v)
            })
            .unwrap();
        let mut expected: Vec<String> = crate::types::RULE_IDS
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut got = ids;
        got.sort();
        expected.sort();
        assert_eq!(got, expected);
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(format!("{}-wal", tmp.display()));
        let _ = std::fs::remove_file(format!("{}-shm", tmp.display()));
    }

    #[test]
    fn purge_old_messages_respects_per_guild_retention() {
        let tmp = tmp_db_path("purge_guild");
        let db = Db::open(&tmp, 2).unwrap();
        let g1 = 11i64;
        let g2 = 22i64;
        let old = (Utc::now() - chrono::Duration::days(10)).to_rfc3339();
        let recent = Utc::now().to_rfc3339();
        tokio_test_block_on(db.with_conn({
            let old = old.clone();
            let recent = recent.clone();
            move |conn| {
                for (mid, gid, ts) in [
                    (1i64, g1, old.as_str()),
                    (2, g1, recent.as_str()),
                    (3, g2, old.as_str()),
                    (4, g2, recent.as_str()),
                ] {
                    conn.execute(
                        "INSERT INTO messages (message_id, guild_id, channel_id, author_id, content, attachments, created_at)
                         VALUES (?1, ?2, 1, 1, 'x', '[]', ?3)",
                        params![mid, gid, ts],
                    )?;
                }
                Ok(())
            }
        }))
        .unwrap();

        let mut s1 = GuildSettings::default();
        s1.retention_days = 3;
        tokio_test_block_on(crate::db::upsert_settings(&db, g1, &s1)).unwrap();
        let mut s2 = GuildSettings::default();
        s2.retention_days = 30;
        tokio_test_block_on(crate::db::upsert_settings(&db, g2, &s2)).unwrap();

        let n = tokio_test_block_on(crate::db::purge_old_messages(&db)).unwrap();
        assert_eq!(n, 1, "only g1's 10-day-old message should purge");

        let count = |gid: i64| {
            tokio_test_block_on(db.with_conn(move |conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM messages WHERE guild_id = ?1",
                    params![gid],
                    |r| Ok(r.get::<i64>(0)?),
                )?)
            }))
            .unwrap()
        };
        assert_eq!(count(g1), 1);
        assert_eq!(count(g2), 2);
        cleanup_db(&tmp);
    }

    #[test]
    fn purge_retention_zero_keeps_messages() {
        let tmp = tmp_db_path("purge_zero");
        let db = Db::open(&tmp, 2).unwrap();
        let old = (Utc::now() - chrono::Duration::days(100)).to_rfc3339();
        tokio_test_block_on(db.with_conn({
            let old = old.clone();
            move |conn| {
                conn.execute(
                    "INSERT INTO messages (message_id, guild_id, channel_id, author_id, content, attachments, created_at)
                     VALUES (1, 9, 1, 1, 'x', '[]', ?1)",
                    params![old],
                )?;
                Ok(())
            }
        }))
        .unwrap();
        let mut s = GuildSettings::default();
        s.retention_days = 0;
        tokio_test_block_on(crate::db::upsert_settings(&db, 9, &s)).unwrap();
        let n = tokio_test_block_on(crate::db::purge_old_messages(&db)).unwrap();
        assert_eq!(n, 0);
        cleanup_db(&tmp);
    }

    #[test]
    fn slash_registry_version_bumps_only_on_command_writes() {
        let tmp = tmp_db_path("slashver");
        let db = Db::open(&tmp, 2).unwrap();
        let s0 = tokio_test_block_on(crate::db::get_slash_registry_version(&db)).unwrap();
        assert!(
            s0 > 0,
            "initial command seed publishes its registry version"
        );

        tokio_test_block_on(crate::db::upsert_settings(
            &db,
            1,
            &GuildSettings::default(),
        ))
        .unwrap();
        assert_eq!(
            tokio_test_block_on(crate::db::get_slash_registry_version(&db)).unwrap(),
            s0,
            "settings must not bump slash_registry_version"
        );

        let cmd = crate::defaults::default_commands()
            .into_iter()
            .next()
            .expect("defaults include commands");
        tokio_test_block_on(crate::db::upsert_command(&db, 1, &cmd.name, &cmd)).unwrap();
        assert_eq!(
            tokio_test_block_on(crate::db::get_slash_registry_version(&db)).unwrap(),
            s0 + 1,
            "command upsert must bump slash_registry_version"
        );
        assert!(
            tokio_test_block_on(crate::db::get_config_version(&db)).unwrap() >= 2,
            "command upsert still bumps config_version"
        );
        let (loaded_version, loaded_commands) =
            tokio_test_block_on(crate::db::load_slash_commands_with_version(&db, 1)).unwrap();
        assert_eq!(loaded_version, s0 + 1);
        assert_eq!(
            loaded_commands.get(&cmd.name).map(|loaded| &loaded.name),
            Some(&cmd.name)
        );

        tokio_test_block_on(crate::db::delete_command(&db, 1, &cmd.name)).unwrap();
        assert_eq!(
            tokio_test_block_on(crate::db::get_slash_registry_version(&db)).unwrap(),
            s0 + 2
        );
        cleanup_db(&tmp);
    }

    #[test]
    fn event_row_snowflakes_serialize_as_strings() {
        let row = EventRow {
            id: 1,
            guild_id: 1329331947876479006,
            event_type: "kick".into(),
            user_id: Some(1329331947876479007),
            actor_id: None,
            data: None,
            created_at: "t".into(),
        };
        let v = serde_json::to_value(&row).unwrap();
        assert_eq!(v["guild_id"], "1329331947876479006");
        assert_eq!(v["user_id"], "1329331947876479007");
        assert!(v["actor_id"].is_null());
    }

    #[test]
    fn insert_event_query_events_roundtrips_data_snowflakes_as_strings() {
        let tmp = tmp_db_path("event_data_sf");
        let db = Db::open(&tmp, 2).unwrap();
        let guild = 11i64;
        let snow = 1329331947876479006u64;
        tokio_test_block_on(crate::db::insert_event(
            &db,
            guild,
            "message_delete",
            Some(snow as i64),
            None,
            &serde_json::json!({
                "channel_id": snow,
                "message_id": 99,
                "nested": { "id": snow + 1 },
            }),
        ))
        .unwrap();

        let rows =
            tokio_test_block_on(crate::db::query_events(&db, guild, None, None, 10)).unwrap();
        assert_eq!(rows.len(), 1);
        let data: serde_json::Value =
            serde_json::from_str(rows[0].data.as_deref().unwrap()).unwrap();
        assert_eq!(data["channel_id"], "1329331947876479006");
        assert_eq!(data["message_id"], 99);
        assert_eq!(data["nested"]["id"], "1329331947876479007");
        // Top-level EventRow IDs are also strings on the wire.
        let wire = serde_json::to_value(&rows[0]).unwrap();
        assert_eq!(wire["user_id"], "1329331947876479006");
        cleanup_db(&tmp);
    }

    #[test]
    fn query_events_heals_legacy_numeric_data_blobs() {
        let tmp = tmp_db_path("event_legacy");
        let db = Db::open(&tmp, 2).unwrap();
        let snow = 1329331947876479006i64;
        // Simulate a pre-fix row that stored snowflakes as JSON numbers.
        tokio_test_block_on(db.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO events (guild_id, event_type, user_id, actor_id, data, created_at)
                 VALUES (1, 'message_edit', NULL, NULL, ?1, 't')",
                params![format!(r#"{{"channel_id":{snow},"message_id":1}}"#)],
            )?;
            Ok(())
        }))
        .unwrap();
        let rows = tokio_test_block_on(crate::db::query_events(&db, 1, None, None, 10)).unwrap();
        let data: serde_json::Value =
            serde_json::from_str(rows[0].data.as_deref().unwrap()).unwrap();
        assert_eq!(data["channel_id"], "1329331947876479006");
        assert_eq!(data["message_id"], 1);
        cleanup_db(&tmp);
    }

    #[test]
    fn guild_without_settings_row_inherits_global() {
        let tmp = tmp_db_path("settings_inherit");
        let db = Db::open(&tmp, 2).unwrap();
        let guild = 4242i64;
        // No guild_settings row — global prefix must apply.
        tokio_test_block_on(crate::db::patch_settings(
            &db,
            GLOBAL_SCOPE,
            Some(Some("?".into())),
            None,
            None,
        ))
        .unwrap();
        let effective = tokio_test_block_on(crate::db::get_settings(&db, guild)).unwrap();
        assert_eq!(effective.prefix, "?");
        assert!(
            tokio_test_block_on(crate::db::get_settings_override(&db, guild))
                .unwrap()
                .is_none()
        );
        cleanup_db(&tmp);
    }

    #[test]
    fn patch_settings_only_overrides_changed_fields() {
        let tmp = tmp_db_path("settings_patch");
        let db = Db::open(&tmp, 2).unwrap();
        let guild = 77i64;
        tokio_test_block_on(crate::db::patch_settings(
            &db,
            GLOBAL_SCOPE,
            Some(Some("g".into())),
            Some(Some("GlobalBot".into())),
            Some(Some(14)),
        ))
        .unwrap();
        // Guild sets only prefix; nickname/retention stay inherited.
        tokio_test_block_on(crate::db::patch_settings(
            &db,
            guild,
            Some(Some("p".into())),
            None,
            None,
        ))
        .unwrap();
        let ov = tokio_test_block_on(crate::db::get_settings_override(&db, guild))
            .unwrap()
            .expect("guild override row");
        assert_eq!(ov.prefix.as_deref(), Some("p"));
        assert!(ov.bot_name.is_none());
        assert!(ov.retention_days.is_none());
        let effective = tokio_test_block_on(crate::db::get_settings(&db, guild)).unwrap();
        assert_eq!(effective.prefix, "p");
        assert_eq!(effective.bot_name, "");
        assert_eq!(effective.retention_days, 14);

        // Saving a value equal to the parent clears that override field.
        tokio_test_block_on(crate::db::patch_settings(
            &db,
            guild,
            Some(Some("g".into())),
            None,
            None,
        ))
        .unwrap();
        assert!(
            tokio_test_block_on(crate::db::get_settings_override(&db, guild))
                .unwrap()
                .is_none(),
            "override row must be deleted when empty"
        );
        cleanup_db(&tmp);
    }

    #[test]
    fn patch_settings_null_clears_field_override() {
        let tmp = tmp_db_path("settings_clear_field");
        let db = Db::open(&tmp, 2).unwrap();
        let guild = 88i64;
        tokio_test_block_on(crate::db::patch_settings(
            &db,
            GLOBAL_SCOPE,
            None,
            None,
            Some(Some(30)),
        ))
        .unwrap();
        tokio_test_block_on(crate::db::patch_settings(
            &db,
            guild,
            None,
            None,
            Some(Some(14)),
        ))
        .unwrap();
        assert_eq!(
            tokio_test_block_on(crate::db::get_settings(&db, guild))
                .unwrap()
                .retention_days,
            14
        );
        tokio_test_block_on(crate::db::patch_settings(
            &db,
            guild,
            None,
            None,
            Some(None),
        ))
        .unwrap();
        assert_eq!(
            tokio_test_block_on(crate::db::get_settings_override(&db, guild))
                .unwrap()
                .and_then(|o| o.retention_days),
            None,
            "null patch must clear retention override"
        );
        assert_eq!(
            tokio_test_block_on(crate::db::get_settings(&db, guild))
                .unwrap()
                .retention_days,
            30,
            "must inherit global retention after clear"
        );
        cleanup_db(&tmp);
    }

    #[test]
    fn heal_normalizes_legacy_blobs_and_preserves_intentional_overrides() {
        let tmp = tmp_db_path("settings_heal");
        // Pre-fix DB shapes:
        // - guild 99: ambiguous full hardcoded-default blob (could be an
        //   ensure_guild seed or an intentional operator override)
        // - guild 88: intentional *partial* prefix override "!" (new durable shape)
        // - guild 77: old get_settings→upsert full effective blob with only
        //   retention intentionally different
        {
            let db = Db::open(&tmp, 2).unwrap();
            tokio_test_block_on(crate::db::patch_settings(
                &db,
                GLOBAL_SCOPE,
                Some(Some("?".into())),
                Some(Some(String::new())),
                Some(Some(7)),
            ))
            .unwrap();
            tokio_test_block_on(db.with_conn(|conn| {
                conn.execute(
                    "INSERT INTO guild_settings (guild_id, settings) VALUES (99, ?1)",
                    params![serde_json::to_string(&GuildSettings::default())?],
                )?;
                // Partial override — the durable intentional shape.
                conn.execute(
                    "INSERT INTO guild_settings (guild_id, settings) VALUES (88, ?1)",
                    params![serde_json::to_string(&GuildSettingsOverride {
                        prefix: Some("!".into()),
                        bot_name: None,
                        retention_days: None,
                    })?],
                )?;
                conn.execute(
                    "INSERT INTO guild_settings (guild_id, settings) VALUES (77, ?1)",
                    params![serde_json::to_string(&GuildSettingsOverride {
                        prefix: Some("?".into()),
                        bot_name: Some(String::new()),
                        retention_days: Some(30),
                    })?],
                )?;
                Ok(())
            }))
            .unwrap();
        }

        let db = Db::open(&tmp, 2).unwrap();
        // 99: legacy format has no provenance. Preserve the explicit values
        // instead of deleting a possible intentional operator override.
        let ov99 = tokio_test_block_on(crate::db::get_settings_override(&db, 99))
            .unwrap()
            .expect("ambiguous full default blob must be preserved");
        assert_eq!(ov99.prefix.as_deref(), Some("!"));
        assert!(ov99.bot_name.is_none());
        assert!(ov99.retention_days.is_none());
        assert_eq!(
            tokio_test_block_on(crate::db::get_settings(&db, 99))
                .unwrap()
                .prefix,
            "!"
        );

        // 88: intentional partial ! override preserved
        let ov88 = tokio_test_block_on(crate::db::get_settings_override(&db, 88))
            .unwrap()
            .expect("intentional ! override must survive");
        assert_eq!(ov88.prefix.as_deref(), Some("!"));
        assert!(ov88.bot_name.is_none());
        assert!(ov88.retention_days.is_none());

        // 77: only retention differs → partial override
        let ov77 = tokio_test_block_on(crate::db::get_settings_override(&db, 77))
            .unwrap()
            .expect("retention-only override");
        assert!(ov77.prefix.is_none());
        assert!(ov77.bot_name.is_none());
        assert_eq!(ov77.retention_days, Some(30));

        assert!(
            tokio_test_block_on(crate::db::get_config_version(&db)).unwrap() >= 1,
            "heal must bump config_version when rows change"
        );

        // Pure default blob when global also matches Default → delete.
        tokio_test_block_on(crate::db::patch_settings(
            &db,
            GLOBAL_SCOPE,
            Some(Some("!".into())),
            Some(Some(String::new())),
            Some(Some(7)),
        ))
        .unwrap();
        tokio_test_block_on(db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO guild_settings (guild_id, settings) VALUES (66, ?1)",
                params![serde_json::to_string(&GuildSettings::default())?],
            )?;
            Ok(())
        }))
        .unwrap();
        drop(db);
        let db = Db::open(&tmp, 2).unwrap();
        assert!(
            tokio_test_block_on(crate::db::get_settings_override(&db, 66))
                .unwrap()
                .is_none(),
            "default blob equal to global must be removed"
        );
        cleanup_db(&tmp);
    }

    #[test]
    fn heal_normalizes_global_and_clears_corrupt_rows() {
        let tmp = tmp_db_path("settings_heal_global");
        {
            let db = Db::open(&tmp, 2).unwrap();
            // Replace seed with a legacy full hardcoded-default global blob
            // (old writers pinned every field).
            tokio_test_block_on(db.with_conn(|conn| {
                conn.execute(
                    "UPDATE guild_settings SET settings = ?1 WHERE guild_id = 0",
                    params![serde_json::to_string(&GuildSettings::default())?],
                )?;
                conn.execute(
                    "INSERT INTO guild_settings (guild_id, settings) VALUES (55, ?1)",
                    params!["{not-json"],
                )?;
                Ok(())
            }))
            .unwrap();
        }

        let db = Db::open(&tmp, 2).unwrap();
        let global = tokio_test_block_on(crate::db::get_settings_override(&db, GLOBAL_SCOPE))
            .unwrap()
            .unwrap_or_default();
        assert!(
            global.is_empty(),
            "legacy full-default global blob must normalize to empty override"
        );
        assert!(
            tokio_test_block_on(crate::db::get_settings_override(&db, 55))
                .unwrap()
                .is_none(),
            "corrupt guild_settings JSON must be cleared"
        );
        // get_settings must succeed after corrupt-row clear.
        let effective = tokio_test_block_on(crate::db::get_settings(&db, 55)).unwrap();
        assert_eq!(effective.prefix, GuildSettings::default().prefix);
        cleanup_db(&tmp);
    }

    #[test]
    fn seed_globals_writes_parent_relative_partial_override() {
        let tmp = tmp_db_path("settings_seed_partial");
        let db = Db::open_with(DbOpenOptions {
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: true,
            default_prefix: "?".into(),
            retention_days: 7,
        })
        .unwrap();
        // Seed writes config-vs-hardcoded diffs, then heal normalizes the global
        // row against config terminal defaults — so a prefix that only mirrors
        // config ends up empty in storage and is provided by terminal defaults.
        let ov = tokio_test_block_on(crate::db::get_settings_override(&db, GLOBAL_SCOPE))
            .unwrap()
            .unwrap_or_default();
        assert!(
            ov.prefix.is_none(),
            "prefix equal to config terminal default must not stay pinned in global"
        );
        assert!(
            ov.bot_name.is_none(),
            "seed must not pin empty bot_name as an override"
        );
        assert!(
            ov.retention_days.is_none(),
            "seed retention equal to hardcoded default must be stripped"
        );
        assert_eq!(db.read_terminal_defaults().unwrap().prefix, "?");
        assert_eq!(
            tokio_test_block_on(crate::db::get_settings(&db, GLOBAL_SCOPE))
                .unwrap()
                .prefix,
            "?"
        );
        cleanup_db(&tmp);
    }

    #[test]
    fn preflight_copy_refuses_all_invalid_badwords_before_writes() {
        let tmp = tmp_db_path("preflight_badwords");
        let db = Db::open(&tmp, 2).unwrap();
        // Legacy row past the add gate — same shape that refuses a live copy.
        tokio_test_block_on(db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO badwords (guild_id, pattern, note) VALUES (11, ?1, NULL)",
                params![r"("],
            )?;
            Ok(())
        }))
        .unwrap();
        let err = tokio_test_block_on(crate::db::preflight_copy_guild_config(
            &db,
            11,
            22,
            vec!["settings".into(), "badwords".into()],
        ))
        .err()
        .expect("preflight must refuse");
        assert!(err.to_string().contains("refused"), "unexpected: {err:#}");
        cleanup_db(&tmp);
    }

    #[test]
    fn patch_settings_strips_legacy_materialized_fields() {
        let tmp = tmp_db_path("settings_patch_legacy");
        let db = Db::open(&tmp, 2).unwrap();
        tokio_test_block_on(crate::db::patch_settings(
            &db,
            GLOBAL_SCOPE,
            Some(Some("g".into())),
            Some(Some("GlobalBot".into())),
            Some(Some(14)),
        ))
        .unwrap();
        // Simulate old writer: full effective blob stored as guild row.
        tokio_test_block_on(db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO guild_settings (guild_id, settings) VALUES (77, ?1)",
                params![serde_json::to_string(&GuildSettingsOverride {
                    prefix: Some("g".into()),
                    bot_name: None,
                    retention_days: Some(14),
                })?],
            )?;
            Ok(())
        }))
        .unwrap();
        // Patch only retention — must clear inherited prefix/bot_name pins.
        tokio_test_block_on(crate::db::patch_settings(
            &db,
            77,
            None,
            None,
            Some(Some(30)),
        ))
        .unwrap();
        let ov = tokio_test_block_on(crate::db::get_settings_override(&db, 77))
            .unwrap()
            .unwrap();
        assert!(ov.prefix.is_none());
        assert!(ov.bot_name.is_none());
        assert_eq!(ov.retention_days, Some(30));
        cleanup_db(&tmp);
    }

    #[test]
    fn copy_settings_copies_override_not_defaults() {
        let tmp = tmp_db_path("copy_settings_inherit");
        let db = Db::open(&tmp, 2).unwrap();
        tokio_test_block_on(crate::db::patch_settings(
            &db,
            GLOBAL_SCOPE,
            Some(Some("@@".into())),
            None,
            None,
        ))
        .unwrap();
        // Source has no override — dest override must be cleared / absent.
        tokio_test_block_on(crate::db::patch_settings(
            &db,
            22,
            Some(Some("old".into())),
            None,
            None,
        ))
        .unwrap();
        let result = tokio_test_block_on(crate::db::copy_guild_config(
            &db,
            11,
            22,
            vec!["settings".into()],
        ))
        .unwrap();
        assert_eq!(result.copied, vec!["settings"]);
        assert!(
            tokio_test_block_on(crate::db::get_settings_override(&db, 22))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            tokio_test_block_on(crate::db::get_settings(&db, 22))
                .unwrap()
                .prefix,
            "@@"
        );

        // Source with an explicit override copies that override only.
        tokio_test_block_on(crate::db::patch_settings(
            &db,
            11,
            Some(Some("src".into())),
            None,
            None,
        ))
        .unwrap();
        let result = tokio_test_block_on(crate::db::copy_guild_config(
            &db,
            11,
            22,
            vec!["settings".into()],
        ))
        .unwrap();
        assert_eq!(result.copied, vec!["settings"]);
        let ov = tokio_test_block_on(crate::db::get_settings_override(&db, 22))
            .unwrap()
            .unwrap();
        assert_eq!(ov.prefix.as_deref(), Some("src"));
        assert!(ov.bot_name.is_none());

        // Write-path copy must preserve intentional hardcoded-default pins
        // (same contract as patch_settings), including hardcoded-default pins.
        let d = GuildSettings::default();
        tokio_test_block_on(crate::db::patch_settings(
            &db,
            GLOBAL_SCOPE,
            Some(Some("?".into())),
            Some(Some("GlobalBot".into())),
            Some(Some(30)),
        ))
        .unwrap();
        tokio_test_block_on(crate::db::patch_settings(
            &db,
            11,
            Some(Some(d.prefix.clone())),
            Some(Some(d.bot_name.clone())),
            Some(Some(d.retention_days)),
        ))
        .unwrap();
        let preview =
            tokio_test_block_on(crate::db::preview_settings_after_copy(&db, 11, 22)).unwrap();
        assert_eq!(preview.prefix, "!");
        assert_eq!(preview.bot_name, "");
        assert_eq!(preview.retention_days, 7);
        let result = tokio_test_block_on(crate::db::copy_guild_config(
            &db,
            11,
            22,
            vec!["settings".into()],
        ))
        .unwrap();
        assert_eq!(result.copied, vec!["settings"]);
        let ov = tokio_test_block_on(crate::db::get_settings_override(&db, 22))
            .unwrap()
            .expect("intentional hardcoded-default pin must copy");
        assert_eq!(ov.prefix.as_deref(), Some("!"));
        assert!(ov.bot_name.is_none());
        assert_eq!(ov.retention_days, Some(7));
        cleanup_db(&tmp);
    }

    #[test]
    fn copy_sections_remove_destination_only_overrides() {
        let tmp = tmp_db_path("copy_sections_replace");
        let db = Db::open(&tmp, 2).unwrap();

        // The source inherits every section from global defaults, while the
        // destination has local rows. A copy must clear those rows rather than
        // leave them silently winning effective resolution.
        let mut rule = crate::defaults::default_rules().into_iter().next().unwrap();
        let rule_id = crate::types::RULE_IDS[0];
        rule.enabled = false;
        tokio_test_block_on(crate::db::upsert_rule(&db, 22, rule_id, &rule)).unwrap();

        let command = crate::defaults::sample_plan_command();
        tokio_test_block_on(crate::db::upsert_command(&db, 22, &command.name, &command)).unwrap();
        tokio_test_block_on(crate::db::upsert_template(
            &db,
            22,
            "member_join",
            "destination-only template",
        ))
        .unwrap();

        tokio_test_block_on(crate::db::copy_guild_config(
            &db,
            11,
            22,
            vec!["rules".into(), "commands".into(), "templates".into()],
        ))
        .unwrap();

        assert!(
            tokio_test_block_on(crate::db::get_rule(&db, 22, rule_id))
                .unwrap()
                .is_none()
        );
        assert!(
            tokio_test_block_on(crate::db::get_command_row(&db, 22, &command.name))
                .unwrap()
                .is_none()
        );
        assert!(
            tokio_test_block_on(crate::db::get_template_row(&db, 22, "member_join"))
                .unwrap()
                .is_none()
        );
        cleanup_db(&tmp);
    }

    #[test]
    fn global_settings_never_persist_a_guild_nickname() {
        let tmp = tmp_db_path("global_settings_no_nickname");
        let db = Db::open(&tmp, 2).unwrap();

        // The shared write path enforces the invariant even when a caller does
        // not go through the Web API.
        tokio_test_block_on(crate::db::patch_settings(
            &db,
            GLOBAL_SCOPE,
            Some(Some("?".into())),
            Some(Some("not-global".into())),
            None,
        ))
        .unwrap();
        let global = tokio_test_block_on(crate::db::get_settings_override(&db, GLOBAL_SCOPE))
            .unwrap()
            .unwrap();
        assert_eq!(global.prefix.as_deref(), Some("?"));
        assert!(global.bot_name.is_none());

        // Copy uses the same invariant, so a per-guild nickname cannot regain
        // global scope through this alternate writer.
        tokio_test_block_on(crate::db::patch_settings(
            &db,
            11,
            None,
            Some(Some("guild-only".into())),
            None,
        ))
        .unwrap();
        tokio_test_block_on(crate::db::copy_guild_config(
            &db,
            11,
            GLOBAL_SCOPE,
            vec!["settings".into()],
        ))
        .unwrap();
        let global = tokio_test_block_on(crate::db::get_settings_override(&db, GLOBAL_SCOPE))
            .unwrap()
            .unwrap();
        assert!(global.bot_name.is_none());
        cleanup_db(&tmp);
    }

    #[test]
    fn copy_logchannels_skips_cross_guild() {
        let tmp = tmp_db_path("copy_logchannels_skip");
        let db = Db::open(&tmp, 2).unwrap();
        tokio_test_block_on(crate::db::upsert_log_channel(&db, 11, "member_join", 12345)).unwrap();
        let err = tokio_test_block_on(crate::db::copy_guild_config(
            &db,
            11,
            22,
            vec!["logchannels".into()],
        ))
        .err()
        .expect("cross-guild logchannels must refuse");
        assert!(
            err.to_string().contains("server-specific"),
            "unexpected: {err:#}"
        );
        let dest = tokio_test_block_on(crate::db::list_log_channels(&db, 22)).unwrap();
        assert!(dest.is_empty());

        // "all" skips logchannels without failing other sections.
        let result = tokio_test_block_on(crate::db::copy_guild_config(
            &db,
            11,
            22,
            vec!["all".into()],
        ))
        .unwrap();
        assert!(result.skipped_logchannels);
        assert!(!result.copied.contains(&"logchannels"));
        let dest = tokio_test_block_on(crate::db::list_log_channels(&db, 22)).unwrap();
        assert!(dest.is_empty(), "must not copy foreign channel ids");

        // Same guild — logchannels is a no-op, not a false "copied".
        let result = tokio_test_block_on(crate::db::copy_guild_config(
            &db,
            11,
            11,
            vec!["logchannels".into()],
        ))
        .unwrap();
        assert!(
            !result.copied.contains(&"logchannels"),
            "same-guild logchannels must not report copied"
        );
        assert!(!result.skipped_logchannels);
        cleanup_db(&tmp);
    }

    #[test]
    fn log_channels_require_positive_snowflakes() {
        let tmp = tmp_db_path("positive_log_channel");
        let db = Db::open(&tmp, 2).unwrap();
        let err = tokio_test_block_on(crate::db::upsert_log_channel(&db, 11, "member_join", -1))
            .unwrap_err();
        assert!(err.to_string().contains("positive Discord snowflake"));
        assert!(
            tokio_test_block_on(crate::db::list_log_channels(&db, 11))
                .unwrap()
                .is_empty()
        );
        cleanup_db(&tmp);
    }

    #[test]
    fn api_open_heals_legacy_guild_settings() {
        let tmp = tmp_db_path("heal_api_open");
        {
            let db = Db::open(&tmp, 2).unwrap();
            tokio_test_block_on(db.with_conn(|conn| {
                conn.execute(
                    "INSERT INTO guild_settings (guild_id, settings) VALUES (55, ?1)",
                    params!["{not-json"],
                )?;
                Ok(())
            }))
            .unwrap();
        }

        let db = Db::open_with(DbOpenOptions {
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: false,
            default_prefix: "!".into(),
            retention_days: 7,
        })
        .unwrap();
        assert!(
            tokio_test_block_on(crate::db::get_settings_override(&db, 55))
                .unwrap()
                .is_none(),
            "API migrate=false open must heal corrupt guild_settings rows"
        );
        tokio_test_block_on(crate::db::get_settings(&db, 55)).unwrap();
        cleanup_db(&tmp);
    }

    #[test]
    fn patch_settings_pins_hardcoded_defaults_when_global_differs() {
        // Root cause lock: write-path must NOT clear ensure_guild-shaped values.
        // Intentional "match hardcoded defaults while global differs" must pin.
        let tmp = tmp_db_path("patch_pin_defaults");
        let db = Db::open(&tmp, 2).unwrap();
        tokio_test_block_on(crate::db::patch_settings(
            &db,
            GLOBAL_SCOPE,
            Some(Some("?".into())),
            Some(Some("GlobalBot".into())),
            Some(Some(30)),
        ))
        .unwrap();
        let d = GuildSettings::default();
        tokio_test_block_on(crate::db::patch_settings(
            &db,
            42,
            Some(Some(d.prefix.clone())),
            Some(Some(d.bot_name.clone())),
            Some(Some(d.retention_days)),
        ))
        .unwrap();
        let ov = tokio_test_block_on(crate::db::get_settings_override(&db, 42))
            .unwrap()
            .expect("intentional hardcoded-default override must survive write path");
        assert_eq!(ov.prefix.as_deref(), Some("!"));
        assert!(ov.bot_name.is_none());
        assert_eq!(ov.retention_days, Some(7));
        let effective = tokio_test_block_on(crate::db::get_settings(&db, 42)).unwrap();
        assert_eq!(effective.prefix, "!");
        assert_eq!(effective.bot_name, "");
        assert_eq!(effective.retention_days, 7);
        cleanup_db(&tmp);
    }

    #[test]
    fn patch_settings_trims_prefix_whitespace() {
        let tmp = tmp_db_path("patch_prefix_trim");
        let db = Db::open(&tmp, 2).unwrap();
        let effective = tokio_test_block_on(crate::db::patch_settings(
            &db,
            7,
            Some(Some("  ?  ".into())),
            None,
            None,
        ))
        .unwrap();
        assert_eq!(effective.prefix, "?");
        let ov = tokio_test_block_on(crate::db::get_settings_override(&db, 7))
            .unwrap()
            .unwrap();
        assert_eq!(ov.prefix.as_deref(), Some("?"));
        let err = tokio_test_block_on(crate::db::patch_settings(
            &db,
            7,
            Some(Some("   ".into())),
            None,
            None,
        ))
        .unwrap_err();
        assert!(
            err.to_string().contains("prefix cannot be empty"),
            "whitespace-only prefix must be rejected: {err:#}"
        );
        cleanup_db(&tmp);
    }

    #[test]
    fn terminal_defaults_come_from_durable_meta_not_process_config() {
        let tmp = tmp_db_path("terminal_defaults_meta");
        // Bot migrate-open writes meta terminal defaults from its config.
        {
            let db = Db::open_with(DbOpenOptions {
                path: tmp.clone(),
                url: String::new(),
                token: String::new(),
                pool_size: 2,
                migrate: true,
                default_prefix: "bot!".into(),
                retention_days: 21,
            })
            .unwrap();
            tokio_test_block_on(db.with_conn(|conn| {
                // Empty global override → effective falls through to terminal defaults.
                conn.execute(
                    "UPDATE guild_settings SET settings = '{}' WHERE guild_id = 0",
                    (),
                )?;
                Ok(())
            }))
            .unwrap();
            assert_eq!(db.read_terminal_defaults().unwrap().prefix, "bot!");
            assert_eq!(db.read_terminal_defaults().unwrap().retention_days, 21);
        }
        // API opens with *different* process config — must still use bot's meta.
        let api = Db::open_with(DbOpenOptions {
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: false,
            default_prefix: "api!".into(),
            retention_days: 3,
        })
        .unwrap();
        assert_eq!(
            api.read_terminal_defaults().unwrap().prefix,
            "bot!",
            "API must not override durable terminal defaults from its own config"
        );
        assert_eq!(api.read_terminal_defaults().unwrap().retention_days, 21);
        let effective = tokio_test_block_on(crate::db::get_settings(&api, 99)).unwrap();
        assert_eq!(effective.prefix, "bot!");
        assert_eq!(effective.retention_days, 21);
        let parts = tokio_test_block_on(crate::db::load_config_parts(&api, 99)).unwrap();
        assert_eq!(parts.settings.prefix, "bot!");
        assert_eq!(parts.settings.retention_days, 21);
        cleanup_db(&tmp);
    }

    #[test]
    fn runtime_read_clears_corrupt_guild_settings_json() {
        let tmp = tmp_db_path("runtime_corrupt_clear");
        let db = Db::open(&tmp, 2).unwrap();
        tokio_test_block_on(db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO guild_settings (guild_id, settings) VALUES (44, ?1)",
                params!["{not-json"],
            )?;
            Ok(())
        }))
        .unwrap();
        // get_settings must succeed and persist the clear (no restart / heal needed).
        let effective = tokio_test_block_on(crate::db::get_settings(&db, 44)).unwrap();
        assert_eq!(
            effective.prefix,
            db.read_terminal_defaults().unwrap().prefix
        );
        assert!(
            tokio_test_block_on(crate::db::get_settings_override(&db, 44))
                .unwrap()
                .is_none(),
            "corrupt row must be deleted on read"
        );
        let v_after_clear = tokio_test_block_on(crate::db::get_config_version(&db)).unwrap();
        drop(db);
        let db2 = Db::open_with(DbOpenOptions {
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: false,
            default_prefix: "!".into(),
            retention_days: 7,
        })
        .unwrap();
        let v_after_reopen = tokio_test_block_on(crate::db::get_config_version(&db2)).unwrap();
        assert_eq!(
            v_after_clear, v_after_reopen,
            "corrupt clear must refresh heal checksum so reopen skips heal"
        );
        cleanup_db(&tmp);
    }

    #[test]
    fn api_open_refuses_missing_terminal_defaults_meta() {
        let tmp = tmp_db_path("api_no_terminal_meta");
        let db = Db::open(&tmp, 2).unwrap();
        tokio_test_block_on(db.with_conn(|conn| {
            conn.execute(
                "DELETE FROM meta WHERE key = 'settings_terminal_defaults'",
                (),
            )?;
            Ok(())
        }))
        .unwrap();
        drop(db);
        let err = Db::open_with(DbOpenOptions {
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: false,
            default_prefix: "!".into(),
            retention_days: 7,
        })
        .err()
        .expect("API must refuse when terminal defaults meta is missing");
        assert!(
            err.to_string().contains("settings_terminal_defaults"),
            "unexpected: {err:#}"
        );
        cleanup_db(&tmp);
    }

    #[test]
    fn heal_skips_when_guild_settings_unchanged() {
        let tmp = tmp_db_path("heal_checksum_skip");
        let db = Db::open(&tmp, 2).unwrap();
        let v_after_first = tokio_test_block_on(crate::db::get_config_version(&db)).unwrap();
        let checksum_after_first = db
            .with_conn_sync(|conn| {
                Ok(meta_string_conn(conn, META_SETTINGS_HEAL_CHECKSUM)?
                    .and_then(|s| s.parse::<u64>().ok()))
            })
            .unwrap();
        drop(db);
        let db2 = Db::open_with(DbOpenOptions {
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: false,
            default_prefix: "!".into(),
            retention_days: 7,
        })
        .unwrap();
        let v_after_second = tokio_test_block_on(crate::db::get_config_version(&db2)).unwrap();
        assert_eq!(
            v_after_first, v_after_second,
            "second open must skip heal when guild_settings checksum unchanged"
        );
        let checksum_after_second = db2
            .with_conn_sync(|conn| {
                Ok(meta_string_conn(conn, META_SETTINGS_HEAL_CHECKSUM)?
                    .and_then(|s| s.parse::<u64>().ok()))
            })
            .unwrap();
        assert_eq!(
            checksum_after_first, checksum_after_second,
            "heal checksum must be stable across process opens (not DefaultHasher)"
        );
        // Recompute must match stored meta (FNV is deterministic).
        let recomputed = db2
            .with_conn_sync(|conn| guild_settings_checksum_conn(conn))
            .unwrap();
        assert_eq!(checksum_after_second, Some(recomputed));
        cleanup_db(&tmp);
    }

    #[test]
    fn delete_noop_does_not_bump_config_or_slash_versions() {
        let tmp = tmp_db_path("delete_noop");
        let db = Db::open(&tmp, 2).unwrap();
        let v0 = tokio_test_block_on(crate::db::get_config_version(&db)).unwrap();
        let s0 = tokio_test_block_on(crate::db::get_slash_registry_version(&db)).unwrap();
        // Delete a command / rule that was never inserted.
        tokio_test_block_on(crate::db::delete_command(&db, 99, "nope")).unwrap();
        tokio_test_block_on(crate::db::delete_rule(&db, 99, "mention_spam")).unwrap();
        assert_eq!(
            tokio_test_block_on(crate::db::get_config_version(&db)).unwrap(),
            v0,
            "no-op delete must not bump config_version"
        );
        assert_eq!(
            tokio_test_block_on(crate::db::get_slash_registry_version(&db)).unwrap(),
            s0,
            "no-op command delete must not bump slash_registry_version"
        );
        cleanup_db(&tmp);
    }

    #[test]
    fn seeded_commands_publish_a_primary_registry_version() {
        let tmp = tmp_db_path("seed_registry_version");
        let db = Db::open(&tmp, 2).unwrap();
        let (config, slash) = get_primary_versions_sync(&db).unwrap();
        assert!(config > 0, "atomic seed must publish config visibility");
        assert!(
            slash > 0,
            "atomic command seed must publish slash visibility"
        );
        cleanup_db(&tmp);
    }

    #[test]
    fn nickname_outbox_generation_cannot_ack_a_newer_write() {
        let tmp = tmp_db_path("nickname_outbox_generation");
        let db = Db::open(&tmp, 2).unwrap();
        tokio_test_block_on(patch_settings(
            &db,
            42,
            None,
            Some(Some("first".into())),
            None,
        ))
        .unwrap();
        let first = tokio_test_block_on(pending_nickname_reconciles(&db)).unwrap();
        assert_eq!(first, vec![(42, 1, "first".into())]);

        tokio_test_block_on(patch_settings(
            &db,
            42,
            None,
            Some(Some("second".into())),
            None,
        ))
        .unwrap();
        tokio_test_block_on(acknowledge_nickname_reconcile(&db, 42, 1)).unwrap();
        let second = tokio_test_block_on(pending_nickname_reconciles(&db)).unwrap();
        assert_eq!(second, vec![(42, 2, "second".into())]);
        tokio_test_block_on(acknowledge_nickname_reconcile(&db, 42, 2)).unwrap();
        assert!(
            tokio_test_block_on(pending_nickname_reconciles(&db))
                .unwrap()
                .is_empty()
        );
        cleanup_db(&tmp);
    }

    #[test]
    fn badword_view_stringifies_guild_snowflake() {
        let snow = 1329331947876479006i64;
        let view = Badword {
            id: 1,
            guild_id: snow,
            pattern: "scam".into(),
            note: None,
        }
        .with_status();
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["guild_id"], "1329331947876479006");
        assert!(json["guild_id"].is_string());
        let global = Badword {
            id: 2,
            guild_id: 0,
            pattern: "x".into(),
            note: None,
        }
        .with_status();
        let g = serde_json::to_value(&global).unwrap();
        assert_eq!(g["guild_id"], "0");
    }

    #[test]
    fn bot_terminal_defaults_update_visible_without_api_restart() {
        let tmp = tmp_db_path("terminal_defaults_live");
        let db = Db::open_with(DbOpenOptions {
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: true,
            default_prefix: "a!".into(),
            retention_days: 7,
        })
        .unwrap();
        tokio_test_block_on(db.with_conn(|conn| {
            conn.execute(
                "UPDATE guild_settings SET settings = '{}' WHERE guild_id = 0",
                (),
            )?;
            Ok(())
        }))
        .unwrap();
        let api = Db::open_with(DbOpenOptions {
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: false,
            default_prefix: "wrong".into(),
            retention_days: 99,
        })
        .unwrap();
        assert_eq!(
            tokio_test_block_on(crate::db::get_settings(&api, 1))
                .unwrap()
                .prefix,
            "a!"
        );
        drop(db);
        // Bot migrate-open rewrites terminal defaults; keep handle only for side effect.
        let _bot = Db::open_with(DbOpenOptions {
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: true,
            default_prefix: "b!".into(),
            retention_days: 7,
        })
        .unwrap();
        // API process still open — must see bot's new meta without restart.
        assert_eq!(
            tokio_test_block_on(crate::db::get_settings(&api, 1))
                .unwrap()
                .prefix,
            "b!"
        );
        cleanup_db(&tmp);
    }

    #[test]
    fn patch_settings_rejects_empty_prefix() {
        let tmp = tmp_db_path("empty_prefix");
        let db = Db::open(&tmp, 2).unwrap();
        let err = tokio_test_block_on(crate::db::patch_settings(
            &db,
            GLOBAL_SCOPE,
            Some(Some(String::new())),
            None,
            None,
        ))
        .err()
        .expect("empty prefix must fail");
        assert!(
            err.to_string().contains("prefix cannot be empty"),
            "unexpected: {err:#}"
        );
        cleanup_db(&tmp);
    }

    #[test]
    fn patch_settings_noop_does_not_bump_config_version() {
        let tmp = tmp_db_path("patch_noop");
        let db = Db::open(&tmp, 2).unwrap();
        let v0 = tokio_test_block_on(crate::db::get_config_version(&db)).unwrap();
        tokio_test_block_on(crate::db::patch_settings(&db, 1, None, None, None)).unwrap();
        let v1 = tokio_test_block_on(crate::db::get_config_version(&db)).unwrap();
        assert_eq!(v0, v1, "empty patch must not bump config_version");
        cleanup_db(&tmp);
    }
}
