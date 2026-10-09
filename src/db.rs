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
    Connection, DbRuntime, OptionalExtension, Row, Value, database_operation_timed_out,
    params_from_iter,
};
#[doc(hidden)]
pub use crate::db_compat::{
    StartupProbe, StartupProbeGuard, current_startup_probe, enter_startup_probe,
    install_startup_probe,
};
// SQL operations/parameters for connections supplied by Db::with_conn.
pub use crate::db_compat::{SqlExecutor, params};
use crate::types::{
    CommandDef, GLOBAL_SCOPE, GuildSettings, GuildSettingsOverride, RuleDef, resolve_settings,
};

mod bot_actions;
pub use bot_actions::*;
mod member_sync_pace;
pub use member_sync_pace::*;
mod bootstrap;
pub use bootstrap::*;
pub use bootstrap::{meta_string_conn, write_meta_string_conn};
mod enrollments;
pub use enrollments::*;
mod configuration;
pub use configuration::*;
mod nickname_jobs;
pub use nickname_jobs::*;
mod moderation;
pub use moderation::*;
pub mod guild_data;
pub mod guild_stores;
mod tenant_lifecycle;
pub use tenant_lifecycle::*;
mod settings;
pub use settings::*;
mod control_apply;
pub use control_apply::*;
mod updates;
pub use updates::*;
mod update_control;
pub use update_control::*;
pub mod usage;
mod write_budget;
use write_budget::BoundedWrites;

const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_config_version.sql"),
    include_str!("../migrations/0003_archive_global_log_channels.sql"),
    include_str!("../migrations/0004_nickname_reconcile.sql"),
    include_str!("../migrations/0005_mutation_receipts_nickname_jobs.sql"),
    include_str!("../migrations/0006_bot_actions.sql"),
    include_str!("../migrations/0007_guild_enrollments.sql"),
    include_str!("../migrations/0008_action_enrollments.sql"),
    include_str!("../migrations/0009_nickname_enrollments.sql"),
    include_str!("../migrations/0010_guild_store_catalog.sql"),
    include_str!("../migrations/0011_tenant_runtime.sql"),
    include_str!("../migrations/0012_guild_moves.sql"),
    include_str!("../migrations/0013_log_filters.sql"),
    include_str!("../migrations/0014_ban_display_identity.sql"),
    include_str!("../migrations/0015_component_updates.sql"),
    include_str!("../migrations/0016_member_sync_pacing.sql"),
    include_str!("../migrations/0017_hot_query_indexes.sql"),
];

/// Completed core schema required by service startup and release observations.
pub const CORE_SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// Set `PRAGMA user_version` on a local file without an application pool.
/// Startup tests use this so the writer-open tripwire stays quiet.
/// The busy timeout waits out a concurrent read-only open.
pub fn set_user_version_unfenced(path: &Path, version: i64) -> Result<()> {
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA busy_timeout=5000")?;
    conn.pragma_update(None, "user_version", version)
}

/// A remote request that has not completed within this deadline is cancelled.
/// Callers receive `DatabaseOperationTimedOut`; writes are not retried here
/// because the server may have committed before the response was lost.
/// Startup and the bot keep this bound. Interactive API work uses
/// [`API_REMOTE_OPERATION_TIMEOUT`].
const REMOTE_OPERATION_TIMEOUT: Duration = Duration::from_secs(75);

/// Interactive API and web database work must fail inside the browser abort.
/// Two of these plus one Discord call stay under that 30 second budget.
pub const API_REMOTE_OPERATION_TIMEOUT: Duration = Duration::from_secs(5);

/// Local API writes wait this long for the bot's reserved lock, then fail
/// retriably. They do not sit in the bot's write queue.
const API_LOCAL_BUSY_TIMEOUT_MS: u32 = 200;

const BOT_LOCAL_BUSY_TIMEOUT_MS: u32 = 5_000;

/// Transitional internal settings validation; no chat history is collected.
/// Remove with the old settings shape in the fresh tenant/config model.
pub const MAX_RETENTION_DAYS: u32 = 100_000;

/// Identifies this fresh schema lineage; versions only have meaning within it.
const SCHEMA_FAMILY: &str = "jmodguard-core-1";

#[derive(Debug)]
pub struct UnsupportedSchema(pub String);
impl std::fmt::Display for UnsupportedSchema {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for UnsupportedSchema {}

/// API-open failure: the remote token can read schema but cannot write.
/// Distinct from `SchemaNotReady` so startup does not retry a permanent
/// permission failure for the whole remote window.
#[derive(Debug)]
pub struct RemoteTokenCannotWrite(String);

impl std::fmt::Display for RemoteTokenCannotWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "database connection cannot write. Original error: {}",
            self.0
        )
    }
}

impl std::error::Error for RemoteTokenCannotWrite {}

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

/// True when a remote operation exceeded the fixed request deadline, or the
/// caller stopped waiting while that operation was still in flight.
///
/// Callers may retry reads. Mutating operations must not be retried blindly:
/// the server could have committed before its response was lost.
pub fn is_database_operation_timeout(err: &anyhow::Error) -> bool {
    crate::db_compat::is_database_operation_timeout(err)
}

/// True when Bunny Database / Turso closed an interactive transaction.
///
/// Distinct from [`is_database_operation_timeout`]: that is this process's
/// request deadline. This is the provider killing the Hrana stream, which
/// reads may retry.
pub fn is_remote_interactive_timeout(err: &anyhow::Error) -> bool {
    crate::db_compat::is_remote_interactive_timeout(err)
}

/// Interactive remote reads that lost their Hrana stream may be retried.
/// Never use this around a mutation: COMMIT could have succeeded.
const REMOTE_INTERACTIVE_READ_ATTEMPTS: u32 = 3;

pub(super) async fn retry_remote_interactive_read<T, F, Fut>(mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut last = None;
    for attempt in 1..=REMOTE_INTERACTIVE_READ_ATTEMPTS {
        match op().await {
            Ok(value) => return Ok(value),
            Err(error)
                if is_remote_interactive_timeout(&error)
                    && attempt < REMOTE_INTERACTIVE_READ_ATTEMPTS =>
            {
                tracing::warn!(
                    attempt,
                    error = %error,
                    "retrying read after remote interactive transaction timeout"
                );
                last = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(last.expect("remote interactive read retry exhausted without an error"))
}

pub fn is_database_commit_indeterminate(err: &anyhow::Error) -> bool {
    crate::db_compat::is_database_commit_indeterminate(err)
}

/// Hidden helper for unit tests in dependent crates.
#[doc(hidden)]
pub fn test_database_commit_indeterminate_error() -> anyhow::Error {
    crate::db_compat::test_database_commit_indeterminate_error()
}

/// Hidden helper for unit tests in dependent crates.
#[doc(hidden)]
pub fn test_database_operation_timeout_error() -> anyhow::Error {
    crate::db_compat::database_operation_timed_out("marker save", std::time::Duration::from_secs(1))
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

pub(super) fn is_rejected_remote_auth(err: &anyhow::Error) -> bool {
    if err.downcast_ref::<RemoteTokenCannotWrite>().is_some() {
        return true;
    }
    err.chain().any(|error| {
        if error.downcast_ref::<RemoteTokenCannotWrite>().is_some() {
            return true;
        }
        let message = error.to_string().to_ascii_lowercase();
        message.contains("unauthorized")
            || message.contains("not authorized")
            || message.contains("forbidden")
            || message.contains("invalid token")
            || message.contains("authentication failed")
            || message.contains("authentication error")
            || message.contains("permission denied")
            || message.contains("readonly")
            || message.contains("read-only")
            || message.contains("read only")
            || http_status_in(&message, 401)
            || http_status_in(&message, 403)
    })
}

fn is_transient_remote_open(err: &anyhow::Error) -> bool {
    // A provider TRANSACTION_TIMEOUT is an interactive transaction that lived
    // too long. It contains the word "timeout", but it is not a transport
    // failure and must not be classified as one.
    if is_remote_interactive_timeout(err) {
        return false;
    }
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

/// Why a database open may be tried again. Interactive transaction timeouts
/// are not transport failures: the timed-out stream is discarded and the next
/// attempt opens a new connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupRetryKind {
    Schema,
    SqliteLock,
    InteractiveTransaction,
    Transport,
}

/// Startup retries incomplete schema, SQLite locks, remote transport errors,
/// and a remote interactive transaction timeout. Auth failures still fail
/// immediately. Shared by the API (`migrate=false`) and the bot (`migrate=true`).
pub fn startup_retry_kind(err: &anyhow::Error, remote: bool) -> Option<StartupRetryKind> {
    if err
        .chain()
        .any(|cause| cause.downcast_ref::<UnsupportedSchema>().is_some())
    {
        return None;
    }
    if is_schema_not_ready(err)
        || err.chain().any(|cause| {
            cause
                .downcast_ref::<crate::control::WriterLinkNotReady>()
                .is_some()
        })
    {
        return Some(StartupRetryKind::Schema);
    }
    if is_sqlite_startup_lock(err) {
        return Some(StartupRetryKind::SqliteLock);
    }
    if !remote || is_rejected_remote_auth(err) {
        return None;
    }
    if is_remote_interactive_timeout(err) {
        return Some(StartupRetryKind::InteractiveTransaction);
    }
    if is_transient_remote_open(err) {
        return Some(StartupRetryKind::Transport);
    }
    None
}

pub fn is_startup_retryable(err: &anyhow::Error, remote: bool) -> bool {
    startup_retry_kind(err, remote).is_some()
}

/// Wall-clock budget for `open_with_budget`. Attempt duration counts toward
/// `budget`; it is not multiplied into a fake attempt total.
#[derive(Clone, Copy, Debug)]
pub struct StartupOpenBudget {
    pub budget: Duration,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

/// How long startup should keep retrying `open_with`.
///
/// Local SQLite gets a short budget for the migrate/lock race. A remote open
/// gets a real wall-clock budget. The old `(180, 2s)` pair looked like 180
/// tries and was actually `180 * 2s` of clock, so a multi-minute attempt exited
/// after a handful of tries.
pub fn startup_retry_budget(remote: bool) -> StartupOpenBudget {
    if remote {
        StartupOpenBudget {
            budget: Duration::from_secs(30 * 60),
            initial_backoff: Duration::from_secs(2),
            max_backoff: Duration::from_secs(30),
        }
    } else {
        StartupOpenBudget {
            budget: Duration::from_secs(30),
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(1),
        }
    }
}

pub fn startup_open_retry(opts: &DbOpenOptions) -> StartupOpenBudget {
    startup_retry_budget(!opts.url.trim().is_empty())
}

/// Backoff after `failed_attempts` failures. The first failure waits
/// `initial`; later failures double until `max`.
pub fn startup_backoff(failed_attempts: u32, initial: Duration, max: Duration) -> Duration {
    let shift = failed_attempts.saturating_sub(1).min(16);
    initial.saturating_mul(1_u32 << shift).min(max)
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
    /// Readers never take the writer lock. The API and pure bot reads set this.
    pub read_only: bool,
    /// Seeded into the global settings row on first boot (`bot.default_prefix`).
    pub default_prefix: String,
    /// Seeded into the global settings row on first boot (`bot.retention_days`).
    pub retention_days: u32,
}

#[cfg(test)]
type BeforeFinalLeaseHook = Box<dyn FnOnce() + Send>;
#[cfg(test)]
type BeforeFinalLeaseReread = Arc<Mutex<Option<BeforeFinalLeaseHook>>>;
#[cfg(test)]
type DuringLeasedGuildCommit = Arc<Mutex<Option<BeforeFinalLeaseHook>>>;

#[derive(Clone)]
pub struct Db {
    path: PathBuf,
    remote: bool,
    read_only: bool,
    _database: Arc<libsql::Database>,
    runtime: DbRuntime,
    startup_deferred: bool,
    connections: Arc<ConnectionPool>,
    permits: Arc<tokio::sync::Semaphore>,
    permit_timeout: Option<Duration>,
    required_config_version: Arc<std::sync::atomic::AtomicU64>,
    config_write_generation: Arc<std::sync::atomic::AtomicU64>,
    config_floor_lock: Arc<parking_lot::Mutex<()>>,
    settings_write_lock: Arc<tokio::sync::Mutex<()>>,
    /// Expected active-bot instance copied onto connections at checkout.
    /// Empty means commits are not fenced. Claiming the writer lease arms this
    /// and leaves it armed. Control apply restores the previous value.
    lease_owner: Arc<Mutex<Option<String>>>,
    /// Bot launcher instance. Empty means update-control commits are not fenced
    /// by the launcher lease. This is not the application writer lease.
    launcher_fence: Arc<Mutex<Option<String>>>,
    /// In-flight guild commits for the current lease owner. Renewal of that
    /// owner does not wait. Release and a different claimant do.
    guild_commits: Arc<(Mutex<GuildCommitFence>, std::sync::Condvar)>,
    /// Moves the core lease after the early guild-commit probe and before the
    /// final re-read. The re-read does not hold the writer lock across COMMIT.
    #[cfg(test)]
    before_final_lease_reread: BeforeFinalLeaseReread,
    #[cfg(test)]
    during_leased_guild_commit: DuringLeasedGuildCommit,
    #[cfg(test)]
    fail_lease_cleanup: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(debug_assertions)]
    test_config_snapshot: Arc<Mutex<Option<(u64, String, ConfigParts)>>>,
}

struct CallerCancel {
    watch: Arc<crate::db_compat::AbandonWatch>,
}

impl CallerCancel {
    fn arm(watch: Arc<crate::db_compat::AbandonWatch>) -> Self {
        Self { watch }
    }
}

impl Drop for CallerCancel {
    fn drop(&mut self) {
        self.watch.signal();
    }
}

impl Db {
    /// This pool's known capacity state. Cross-process admission remains unknown.
    pub fn capacity_runtime_status(&self) -> CapacityRuntimeStatus {
        let growth_paused = self.runtime.growth_paused();
        CapacityRuntimeStatus {
            growth_paused,
            startup_deferred: self.startup_deferred,
            reason: if growth_paused {
                Some("write_bound_exceeded")
            } else if self.startup_deferred {
                Some("startup_deferred")
            } else {
                None
            },
        }
    }
    pub fn sqlite_path(&self) -> Option<&Path> {
        (!self.remote).then_some(self.path.as_path())
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_remote(&self) -> bool {
        self.remote
    }
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Treat later commits from this pool as belonging to `owner`. A commit is
    /// rejected when that instance does not hold the active-bot lease.
    pub fn arm_lease_owner(&self, owner: &str) {
        self.restore_lease_owner(Some(owner.to_owned()));
    }

    /// Short fence for bot-launcher update-control writes. It does not use the
    /// application writer lease, so it still works while the bot process is stopped.
    pub fn arm_launcher_fence(&self, instance: &str) {
        *self
            .launcher_fence
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(instance.to_owned());
    }

    pub fn launcher_instance(&self) -> Option<String> {
        self.launcher_fence
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    #[cfg(test)]
    pub(crate) fn clear_lease_owner(&self) {
        self.restore_lease_owner(None);
    }

    pub(crate) fn leased_owner(&self) -> Option<String> {
        self.lease_owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) fn restore_lease_owner(&self, owner: Option<String>) {
        *self
            .lease_owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = owner;
    }

    pub(crate) fn install_commit_lease_probe(&self, probe: crate::db_compat::CommitLeaseProbe) {
        *self
            .connections
            .commit_lease_probe
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(probe);
    }

    #[cfg(test)]
    pub(crate) fn arm_before_final_lease_reread(&self, hook: impl FnOnce() + Send + 'static) {
        *self
            .before_final_lease_reread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Box::new(hook));
    }

    #[cfg(test)]
    pub(crate) fn take_before_final_lease_reread(&self) -> Option<BeforeFinalLeaseHook> {
        self.before_final_lease_reread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    #[cfg(test)]
    pub(crate) fn arm_during_leased_guild_commit(&self, hook: impl FnOnce() + Send + 'static) {
        *self
            .during_leased_guild_commit
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Box::new(hook));
    }

    #[cfg(test)]
    fn take_during_leased_guild_commit(&self) -> Option<Box<dyn FnOnce() + Send>> {
        self.during_leased_guild_commit
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    #[cfg(test)]
    pub(crate) fn arm_lease_cleanup_failure(&self) {
        self.fail_lease_cleanup
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    fn take_lease_cleanup_failure(&self) -> bool {
        self.fail_lease_cleanup
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    }

    pub(crate) fn begin_guild_commit(&self, owner: &str) {
        let mut fence = self
            .guild_commits
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        fence.inflight = fence.inflight.saturating_add(1);
        if fence.owner.is_none() {
            fence.owner = Some(owner.to_owned());
        }
    }

    pub(crate) fn end_guild_commit(&self) {
        let mut fence = self
            .guild_commits
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        fence.inflight = fence.inflight.saturating_sub(1);
        if fence.inflight == 0 {
            fence.owner = None;
        }
        self.guild_commits.1.notify_all();
    }

    pub(crate) fn guild_commit_blocks_claim(&self, instance: &str) -> bool {
        let fence = self
            .guild_commits
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        fence.inflight > 0 && fence.owner.as_deref() != Some(instance)
    }

    pub(crate) fn guild_commit_blocks_release(&self, instance: &str) -> bool {
        let fence = self
            .guild_commits
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        fence.inflight > 0 && fence.owner.as_deref() == Some(instance)
    }

    pub(crate) fn wait_for_guild_commit_claim(&self, instance: &str) {
        let mut fence = self
            .guild_commits
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while fence.inflight > 0 && fence.owner.as_deref() != Some(instance) {
            fence = self
                .guild_commits
                .1
                .wait(fence)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    pub(crate) fn wait_for_guild_commit_release(&self, instance: &str) {
        let mut fence = self
            .guild_commits
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while fence.inflight > 0 && fence.owner.as_deref() == Some(instance) {
            fence = self
                .guild_commits
                .1
                .wait(fence)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

struct GuildCommitFence {
    inflight: usize,
    owner: Option<String>,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct CapacityRuntimeStatus {
    pub growth_paused: bool,
    pub startup_deferred: bool,
    pub reason: Option<&'static str>,
}

struct ConnectionPool {
    idle: Mutex<Vec<Connection>>,
    database: Arc<libsql::Database>,
    runtime: DbRuntime,
    remote: bool,
    read_only: bool,
    busy_timeout_ms: u32,
    lease_owner: Arc<Mutex<Option<String>>>,
    launcher_fence: Arc<Mutex<Option<String>>>,
    commit_lease_probe: Arc<Mutex<Option<crate::db_compat::CommitLeaseProbe>>>,
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
        let mut connection = match connection {
            Some(connection) => connection,
            None => {
                let mut connection =
                    Connection::from_database(Arc::clone(&pool.database), pool.runtime.clone())
                        .context("failed to replace discarded database connection")?;
                connection.set_read_only(pool.read_only);
                configure_connection(
                    &connection,
                    pool.remote,
                    pool.busy_timeout_ms,
                    pool.read_only,
                )?;
                connection
            }
        };
        if pool.remote {
            // Successful autocommit reads can leave an Hrana cursor baton on
            // this connection. Never carry that potentially expired server
            // stream into a new logical operation. Do not retry the closure:
            // it may contain writes whose outcome cannot safely be replayed.
            connection.reset_remote_stream()?;
        }
        let owner = pool
            .lease_owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        connection.set_expected_lease_owner(owner);
        let launcher = pool
            .launcher_fence
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        connection.set_expected_launcher(launcher);
        let probe = pool
            .commit_lease_probe
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        connection.set_commit_lease_probe(probe);
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
    crate::db_compat::drop_pooled_connection(connection);
}

struct CloseBootstrap {
    connection: Option<Connection>,
    runtime: DbRuntime,
}

impl Drop for CloseBootstrap {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            self.runtime.close_connection(connection);
        }
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
            read_only: false,
            default_prefix: GuildSettings::default().prefix,
            retention_days: GuildSettings::default().retention_days,
        })
    }

    /// Current durable terminal defaults from `meta` (fresh read).
    pub fn read_terminal_defaults(&self) -> Result<GuildSettings> {
        self.with_conn_sync(|conn| terminal_defaults_conn(conn))
    }

    pub fn open_with(opts: DbOpenOptions) -> Result<Self> {
        if !opts.read_only {
            note_writable_open();
        }
        let startup_deferred = std::cell::Cell::new(false);
        let opts_defaults = GuildSettings {
            prefix: opts.default_prefix.clone(),
            bot_name: GuildSettings::default().bot_name,
            retention_days: opts.retention_days,
        };
        let mut db = Self::open_with_bootstrap(
            &opts,
            |bootstrap| {
                if opts.migrate {
                    // A completed, current core can reconnect for inspection and
                    // bounded cleanup above the ordinary growth stop. Its existing
                    // terminal defaults remain authoritative until a later restart
                    // can safely finish ordinary seed/heal work.
                    if bootstrap_at_capacity(bootstrap)? {
                        startup_deferred.set(true);
                        return Ok(());
                    }
                    // Hide readiness only when this open will actually rebuild
                    // schema/defaults. Clearing on every reconnect made a
                    // retryable pool failure look like an incomplete first boot
                    // and blocked the API for the whole remote retry window.
                    if migrate_open_needs_api_fence(bootstrap)? {
                        clear_bootstrap_complete(bootstrap)?;
                    }
                    migrate_conn(bootstrap).context("migrate")?;
                    // Bot is authority for terminal defaults: sync from config every
                    // migrate-open so both processes resolve empty global fields from
                    // the same durable meta row (not each process's config.toml).
                    write_terminal_defaults_meta(bootstrap, &opts_defaults)
                        .context("terminal defaults")?;
                    seed_globals_conn(
                        bootstrap,
                        &opts.default_prefix,
                        opts.retention_days,
                        &opts_defaults,
                    )
                    .context("seed_globals")?;
                    // Idempotent when guild_settings changed; skipped via checksum when not.
                    heal_default_guild_settings_conn(bootstrap, &opts_defaults)
                        .context("heal_guild_settings")?;
                } else {
                    // One primary-routed snapshot is the API readiness barrier.
                    // Replica reads can otherwise see an old completed marker
                    // while a new bot bootstrap is in progress on the primary.
                    // This snapshot does not take the writer lock. A read-only
                    // token fails on the first real WebUI write instead of here.
                    let _ = read_bootstrap_defaults_primary(bootstrap)?;
                }
                Ok(())
            },
            |bootstrap| {
                if opts.migrate && !startup_deferred.get() {
                    write_meta_string_conn(
                        bootstrap,
                        META_BOOTSTRAP_COMPLETE,
                        &MIGRATIONS.len().to_string(),
                    )?;
                }
                Ok(())
            },
        )?;
        db.startup_deferred = startup_deferred.get();
        Ok(db)
    }

    // Share transport/pool creation while keeping core and guild-store schemas separate.
    // No caller can obtain an unchecked pool before its initializer succeeds.
    fn open_with_bootstrap(
        opts: &DbOpenOptions,
        initialize: impl FnOnce(&mut Connection) -> Result<()>,
        ready: impl FnOnce(&Connection) -> Result<()>,
    ) -> Result<Self> {
        let remote = !opts.url.trim().is_empty();
        if remote && opts.token.trim().is_empty() {
            anyhow::bail!("db.token / JMODGUARD_DB_TOKEN is required when db.url is set");
        }
        if !remote && !opts.token.trim().is_empty() {
            anyhow::bail!("db.url / JMODGUARD_DB_URL is required when db.token is set");
        }
        let open_started = Instant::now();
        let runtime =
            DbRuntime::with_operation_timeout(remote.then_some(REMOTE_OPERATION_TIMEOUT))?;
        let database = if remote {
            let connector = crate::remote_https::https_connector();
            runtime.block_on_database(
                "connect",
                libsql::Builder::new_remote(opts.url.clone(), opts.token.clone())
                    .connector(connector)
                    .build(),
            )?
        } else {
            let mut builder = libsql::Builder::new_local(opts.path.clone());
            if opts.read_only {
                builder = builder.flags(libsql::OpenFlags::SQLITE_OPEN_READ_ONLY);
            }
            runtime.block_on(builder.build())?
        };
        let database = Arc::new(database);
        let open_ms = u64::try_from(open_started.elapsed().as_millis()).unwrap_or(u64::MAX);

        // Migrate / schema-check on a dedicated connection first, so no other
        // connections are racing the DDL at startup. The connection is closed
        // on the database runtime, including the error path.
        {
            let mut held = CloseBootstrap {
                connection: Some({
                    let mut connection =
                        Connection::from_database(Arc::clone(&database), runtime.clone())
                            .context("failed to connect to database")?;
                    connection.set_read_only(opts.read_only);
                    connection
                }),
                runtime: runtime.clone(),
            };
            let bootstrap = held
                .connection
                .as_mut()
                .expect("bootstrap connection is installed");
            let step_started = Instant::now();
            if !remote {
                if opts.read_only {
                    bootstrap.execute_batch(
                        "PRAGMA busy_timeout=5000;
                         PRAGMA foreign_keys=ON;
                         PRAGMA query_only=ON;",
                    )?;
                } else {
                    bootstrap.execute_batch(
                        "PRAGMA busy_timeout=5000;
                         PRAGMA journal_mode=WAL;
                         PRAGMA synchronous=NORMAL;
                         PRAGMA foreign_keys=ON;",
                    )?;
                }
            } else {
                bootstrap.execute_batch("PRAGMA foreign_keys=ON;")?;
            }
            initialize(bootstrap)?;
            let step_ms = u64::try_from(step_started.elapsed().as_millis()).unwrap_or(u64::MAX);
            tracing::info!(
                migrate = opts.migrate,
                open_ms,
                step_ms,
                "database startup step finished"
            );

            let pool_size = opts.pool_size.max(2) as usize;
            // The API does not migrate. Its pooled work is interactive and must
            // fail inside the browser abort. Startup above keeps the longer bound.
            let interactive_api = remote && !opts.migrate;
            let pool_runtime = if interactive_api {
                runtime.for_operation_timeout(Some(API_REMOTE_OPERATION_TIMEOUT))
            } else {
                runtime.clone()
            };
            let busy_timeout_ms = if opts.migrate {
                BOT_LOCAL_BUSY_TIMEOUT_MS
            } else {
                API_LOCAL_BUSY_TIMEOUT_MS
            };
            let mut connections = Vec::with_capacity(pool_size);
            for _ in 0..pool_size {
                let mut connection =
                    Connection::from_database(Arc::clone(&database), pool_runtime.clone())
                        .context("failed to create pooled database connection")?;
                connection.set_read_only(opts.read_only);
                configure_connection(&connection, remote, busy_timeout_ms, opts.read_only)?;
                connections.push(connection);
            }

            // Publish readiness only after the pool exists. A retry of
            // `open_with_until_ready` (remote first-boot) would otherwise
            // clear this marker after a later pool-connect failure, while an
            // API that already passed the gate is serving.
            ready(bootstrap)?;

            let lease_owner = Arc::new(Mutex::new(None));
            let launcher_fence = Arc::new(Mutex::new(None));
            let commit_lease_probe = Arc::new(Mutex::new(None));
            Ok(Self {
                path: opts.path.clone(),
                remote,
                read_only: opts.read_only,
                _database: Arc::clone(&database),
                runtime: runtime.clone(),
                startup_deferred: false,
                connections: Arc::new(ConnectionPool {
                    idle: Mutex::new(connections),
                    database,
                    runtime: pool_runtime,
                    remote,
                    read_only: opts.read_only,
                    busy_timeout_ms,
                    lease_owner: Arc::clone(&lease_owner),
                    launcher_fence: Arc::clone(&launcher_fence),
                    commit_lease_probe,
                }),
                permits: Arc::new(tokio::sync::Semaphore::new(pool_size)),
                permit_timeout: remote.then_some(if opts.migrate {
                    REMOTE_OPERATION_TIMEOUT
                } else {
                    API_REMOTE_OPERATION_TIMEOUT
                }),
                required_config_version: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                config_write_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                config_floor_lock: Arc::new(parking_lot::Mutex::new(())),
                settings_write_lock: Arc::new(tokio::sync::Mutex::new(())),
                lease_owner,
                launcher_fence,
                guild_commits: Arc::new((
                    Mutex::new(GuildCommitFence {
                        inflight: 0,
                        owner: None,
                    }),
                    std::sync::Condvar::new(),
                )),
                #[cfg(test)]
                before_final_lease_reread: Arc::new(Mutex::new(None)),
                #[cfg(test)]
                during_leased_guild_commit: Arc::new(Mutex::new(None)),
                #[cfg(test)]
                fail_lease_cleanup: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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

    /// Retries `open_with` until `budget.budget` elapses. An attempt already
    /// running is allowed to finish. The next attempt is a new runtime and
    /// connection; a timed-out stream from the previous attempt is not reused.
    pub async fn open_with_budget(opts: DbOpenOptions, budget: StartupOpenBudget) -> Result<Self> {
        let started = Instant::now();
        let deadline = started + budget.budget;
        let remote = !opts.url.trim().is_empty();
        let mut last = None;
        let mut attempt = 0_u32;
        loop {
            if attempt > 0 && Instant::now() >= deadline {
                break;
            }
            attempt += 1;
            match Self::open_with_on_worker(opts.clone()).await {
                Ok(db) => {
                    let elapsed_ms =
                        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                    tracing::info!(
                        attempt,
                        retried = attempt > 1,
                        elapsed_ms,
                        "database open finished"
                    );
                    return Ok(db);
                }
                Err(err) => {
                    let Some(kind) = startup_retry_kind(&err, remote) else {
                        return Err(err);
                    };
                    last = Some(err);
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    let sleep_for =
                        startup_backoff(attempt, budget.initial_backoff, budget.max_backoff)
                            .min(remaining);
                    if sleep_for.is_zero() {
                        break;
                    }
                    let elapsed = started.elapsed();
                    tracing::info!(
                        attempt,
                        kind = ?kind,
                        elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                        budget_ms = u64::try_from(budget.budget.as_millis()).unwrap_or(u64::MAX),
                        backoff_ms = u64::try_from(sleep_for.as_millis()).unwrap_or(u64::MAX),
                        "database not ready; retrying: {:#}",
                        last.as_ref().expect("retry stores the error")
                    );
                    tokio::time::sleep(sleep_for).await;
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            anyhow::Error::new(SchemaNotReady("database schema not ready".into()))
        }))
        .context("timed out retrying database open")
    }

    async fn open_with_on_worker(opts: DbOpenOptions) -> Result<Self> {
        let probe = current_startup_probe();
        let tripwire = writer_open_tripwire();
        let (send, receive) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("jmodguard-db-open".into())
            .spawn(move || {
                // The receiver can be dropped on shutdown. The worker still
                // finishes the synchronous open; process exit reaps it.
                let _probe = enter_startup_probe(probe);
                if let Some(flag) = tripwire {
                    arm_writer_open_tripwire(flag);
                }
                let _ = send.send(Self::open_with(opts));
            })
            .context("failed to start database-open worker")?;
        receive
            .await
            .context("database-open worker stopped before returning a result")?
    }

    /// Runs a blocking compatibility closure on a libSQL connection off the
    /// application async runtime. Dropping this future tells the in-flight
    /// database call to stop so the blocking task can release the connection.
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
        let watch = crate::db_compat::AbandonWatch::new();
        let _caller = CallerCancel::arm(Arc::clone(&watch));
        tokio::task::spawn_blocking(move || {
            // The permit belongs to the blocking task, not its awaiter. Tokio
            // cannot cancel blocking work, so an aborted request must not let
            // another operation exceed the configured pool bound. Dropping the
            // caller signals the in-flight database call so this task can roll
            // back and return the permit instead of keeping both until the
            // operation deadline.
            let _caller = crate::db_compat::enter_caller_abandon(watch);
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

    pub fn require_config_version(&self, version: u64) {
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

std::thread_local! {
    static WRITER_OPEN_FLAG: std::cell::Cell<Option<Arc<std::sync::atomic::AtomicBool>>> =
        const { std::cell::Cell::new(None) };
}

/// Arms a thread-local flag that records writable `Db::open_with` and
/// `UpdateControl::open` calls. Other tests leave it unset.
pub fn arm_writer_open_tripwire(flag: Arc<std::sync::atomic::AtomicBool>) {
    WRITER_OPEN_FLAG.with(|slot| slot.set(Some(flag)));
}

pub(crate) fn writer_open_tripwire() -> Option<Arc<std::sync::atomic::AtomicBool>> {
    WRITER_OPEN_FLAG
        .with(|slot| slot.take())
        .inspect(|flag| WRITER_OPEN_FLAG.with(|slot| slot.set(Some(Arc::clone(flag)))))
}

fn note_writable_open() {
    if let Some(flag) = writer_open_tripwire() {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests;
