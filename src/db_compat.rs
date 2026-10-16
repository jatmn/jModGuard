//! Small synchronous facade over libSQL's async connection API.
//!
//! The data layer predates remote database support and intentionally performs
//! each logical operation inside a blocking closure. Keeping that boundary
//! lets local SQLite and remote libSQL share the exact same SQL paths while
//! the libSQL runtime handles either transport underneath.

use anyhow::{Context as _, Result};
use fallible_iterator::FallibleIterator;
use libsql::params::IntoParams;
use libsql_sqlite3_parser::ast::{Cmd, PragmaBody, Stmt};
use libsql_sqlite3_parser::lexer::sql::Parser;
use std::cell::RefCell;
use std::fmt::Display;
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

pub use libsql::{Row, Value, params, params_from_iter};

static RECEIPT_PRUNE_COUNTER: AtomicU64 = AtomicU64::new(0);
// 64 possible 64-KiB pages cover row, both receipt B-trees and split paths.
// This is independently admitted before creating/refreshing our own receipt.
const RECEIPT_GROWTH_ALLOWANCE: u64 = 4 * 1024 * 1024;
const INSERT_MUTATION_RECEIPT_SQL: &str = "
    INSERT INTO mutation_receipts (receipt_id, created_at)
    SELECT ?1, unixepoch()
    WHERE COALESCE((SELECT value FROM meta WHERE key='restore_fence'), '0') != '1'
    RETURNING receipt_id";
const INSERT_UNFENCED_MUTATION_RECEIPT_SQL: &str = "
    INSERT INTO mutation_receipts (receipt_id, created_at)
    VALUES (?1, unixepoch())
    RETURNING receipt_id";

#[derive(Debug, thiserror::Error)]
#[error("query returned no rows")]
struct QueryReturnedNoRows;

#[derive(Debug, thiserror::Error)]
#[error("remote database {operation} timed out after {timeout:?}")]
pub struct DatabaseOperationTimedOut {
    operation: &'static str,
    timeout: Duration,
}

#[derive(Debug, thiserror::Error)]
#[error("remote database {operation} ended because the caller stopped waiting")]
pub struct DatabaseCallerAbandoned {
    operation: &'static str,
}

#[derive(Debug, thiserror::Error)]
#[error("remote database commit outcome is indeterminate: {0}")]
pub struct DatabaseCommitIndeterminate(#[source] anyhow::Error);

pub(crate) fn is_database_operation_timeout(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<DatabaseOperationTimedOut>().is_some()
            || cause.downcast_ref::<DatabaseCallerAbandoned>().is_some()
    })
}

pub(crate) fn database_caller_abandoned(operation: &'static str) -> anyhow::Error {
    DatabaseCallerAbandoned { operation }.into()
}

pub(crate) fn is_database_commit_indeterminate(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<DatabaseCommitIndeterminate>()
            .is_some()
    })
}

/// After our own deadline, one attempt to drop a remote writer lock.
///
/// Short enough that a hung ROLLBACK cannot hold the API call, and shorter
/// than the provider's transaction steal so the next writer is not stuck
/// behind an abandoned `BEGIN IMMEDIATE`.
pub(crate) const REMOTE_LOCK_RELEASE_TIMEOUT: Duration = Duration::from_millis(300);

/// Remote reads and snapshots must end before the provider closes an
/// interactive transaction (about five seconds). One hung statement otherwise
/// waits out the pool deadline and can hold the writer across the lease TTL.
pub(crate) const INTERACTIVE_READ_BUDGET: Duration = Duration::from_secs(4);

/// True when Bunny Database / Turso closed an interactive Hrana transaction.
///
/// This is distinct from the client-side [`DatabaseOperationTimedOut`] deadline.
/// The provider kills idle or long interactive transactions (often around five
/// seconds) and the next statement fails at cursor step 0 with
/// `TRANSACTION_TIMEOUT`. ROLLBACK after that then reports that no transaction
/// is active. Our own deadline's text also contains "transaction timed out"
/// when the operation name ends in "transaction"; that is not a dead stream.
pub(crate) fn is_remote_interactive_timeout(error: &anyhow::Error) -> bool {
    if is_database_operation_timeout(error) {
        return false;
    }
    error.chain().any(|cause| {
        let message = cause.to_string();
        let lower = message.to_ascii_lowercase();
        message.contains("TRANSACTION_TIMEOUT")
            || lower.contains("transaction timed out")
            || lower.contains("cannot rollback - no transaction is active")
    })
}

/// A read already produced its snapshot. ROLLBACK is only how we avoid COMMIT
/// semantics; if the provider already dropped the transaction, the read is
/// still determinate.
fn remote_read_rollback_is_complete(error: &anyhow::Error) -> bool {
    is_remote_interactive_timeout(error)
}

/// Hidden helper for unit tests in dependent crates.
#[doc(hidden)]
pub fn test_database_commit_indeterminate_error() -> anyhow::Error {
    DatabaseCommitIndeterminate(anyhow::anyhow!("lost commit")).into()
}

pub(crate) fn database_operation_timed_out(
    operation: &'static str,
    timeout: Duration,
) -> anyhow::Error {
    DatabaseOperationTimedOut { operation, timeout }.into()
}

pub trait OptionalExtension<T> {
    fn optional(self) -> Result<Option<T>>;
}

impl<T> OptionalExtension<T> for Result<T> {
    fn optional(self) -> Result<Option<T>> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.downcast_ref::<QueryReturnedNoRows>().is_some() => Ok(None),
            Err(error) => Err(error),
        }
    }
}

struct RuntimeThread {
    shutdown: Mutex<Option<mpsc::Sender<()>>>,
    join: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Drop for RuntimeThread {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.lock().expect("runtime shutdown lock").take() {
            let _ = shutdown.send(());
        }
        if let Some(join) = self.join.lock().expect("runtime join lock").take() {
            let _ = join.join();
        }
    }
}

/// Guild-store identity last confirmed on one pooled connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaVerification {
    pub schema_version: i64,
    pub user_version: i64,
    pub store_id: String,
    pub dedicated_guild_id: Option<i64>,
}

/// Counts write transactions and database calls for the startup that installed it.
///
/// Only connections created while this probe is current increment it, so
/// parallel tests do not share a counter.
#[derive(Debug, Default)]
pub struct StartupProbe {
    immediate: AtomicU64,
    transactions: AtomicU64,
    queries: AtomicU64,
    catalog: AtomicU64,
}

impl StartupProbe {
    pub fn transactions(&self) -> u64 {
        self.transactions.load(Ordering::Relaxed)
    }

    pub fn immediate(&self) -> u64 {
        self.immediate.load(Ordering::Relaxed)
    }

    pub fn queries(&self) -> u64 {
        self.queries.load(Ordering::Relaxed)
    }

    pub fn catalog(&self) -> u64 {
        self.catalog.load(Ordering::Relaxed)
    }

    pub(crate) fn record_immediate(&self) {
        self.immediate.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_query(&self) {
        self.queries.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_catalog(&self) {
        self.catalog.fetch_add(1, Ordering::Relaxed);
    }
}

thread_local! {
    static STARTUP_PROBE: RefCell<Option<Arc<StartupProbe>>> = const { RefCell::new(None) };
}

#[doc(hidden)]
pub fn install_startup_probe(probe: Option<Arc<StartupProbe>>) -> Option<Arc<StartupProbe>> {
    STARTUP_PROBE.with(|slot| slot.replace(probe))
}

#[doc(hidden)]
pub fn current_startup_probe() -> Option<Arc<StartupProbe>> {
    STARTUP_PROBE.with(|slot| slot.borrow().clone())
}

/// Restores the previous probe when dropped. Forward this into a worker
/// thread before that thread opens a database runtime.
pub struct StartupProbeGuard {
    previous: Option<Arc<StartupProbe>>,
}

impl Drop for StartupProbeGuard {
    fn drop(&mut self) {
        install_startup_probe(self.previous.take());
    }
}

#[doc(hidden)]
pub fn enter_startup_probe(probe: Option<Arc<StartupProbe>>) -> StartupProbeGuard {
    StartupProbeGuard {
        previous: install_startup_probe(probe),
    }
}

#[derive(Clone)]
pub struct DbRuntime {
    handle: tokio::runtime::Handle,
    _thread: Arc<RuntimeThread>,
    operation_timeout: Option<Duration>,
    // Shared by all connections for one physical Db pool. A discovered bound
    // violation pauses growth until that runtime is restarted and repaired.
    growth_paused: Arc<AtomicBool>,
    startup_probe: Option<Arc<StartupProbe>>,
    /// Test-only stand-in for `tokio::time::timeout`. The database runtime is
    /// multi-thread, and `tokio::time::pause` refuses that flavor.
    #[cfg(test)]
    manual_clock: Arc<Mutex<Option<Arc<ManualClock>>>>,
    /// How many timed-out opens have entered the short release. Tests wait for
    /// this before unblocking a hung BEGIN, so the response cannot win the
    /// original deadline race.
    #[cfg(test)]
    open_releases: Arc<AtomicU64>,
}

impl DbRuntime {
    pub(crate) fn growth_paused(&self) -> bool {
        self.growth_paused.load(Ordering::Acquire)
    }
    pub fn new() -> Result<Self> {
        Self::with_operation_timeout(None)
    }

    pub fn for_operation_timeout(&self, operation_timeout: Option<Duration>) -> Self {
        Self {
            handle: self.handle.clone(),
            _thread: Arc::clone(&self._thread),
            operation_timeout,
            growth_paused: Arc::clone(&self.growth_paused),
            startup_probe: self.startup_probe.clone(),
            #[cfg(test)]
            manual_clock: Arc::clone(&self.manual_clock),
            #[cfg(test)]
            open_releases: Arc::clone(&self.open_releases),
        }
    }

    pub fn with_operation_timeout(operation_timeout: Option<Duration>) -> Result<Self> {
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let join = std::thread::Builder::new()
            .name("jmodguard-db-owner".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .thread_name("jmodguard-db")
                    .build();
                match runtime {
                    Ok(runtime) => {
                        if ready_tx.send(Ok(runtime.handle().clone())).is_ok() {
                            let _ = shutdown_rx.recv();
                        }
                        // The runtime is deliberately dropped on this plain OS
                        // thread, never from an async request or test context.
                        drop(runtime);
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error.to_string()));
                    }
                }
            })
            .context("failed to spawn libSQL runtime owner")?;
        let handle = ready_rx
            .recv()
            .context("libSQL runtime owner stopped during startup")?
            .map_err(anyhow::Error::msg)?;
        Ok(Self {
            handle,
            _thread: Arc::new(RuntimeThread {
                shutdown: Mutex::new(Some(shutdown_tx)),
                join: Mutex::new(Some(join)),
            }),
            operation_timeout,
            growth_paused: Arc::new(AtomicBool::new(false)),
            startup_probe: current_startup_probe(),
            #[cfg(test)]
            manual_clock: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            open_releases: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn block_on<F>(&self, future: F) -> F::Output
    where
        F: Future + Send,
        F::Output: Send,
    {
        // Startup and tests may call the synchronous facade from an application
        // Tokio runtime. Multi-thread runtimes can hand the worker slot back
        // while this call blocks. Current-thread runtimes cannot, so use one
        // scoped OS thread in that less common case.
        match tokio::runtime::Handle::try_current() {
            Ok(current)
                if current.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread =>
            {
                tokio::task::block_in_place(|| self.handle.block_on(future))
            }
            Ok(_) => std::thread::scope(|scope| {
                scope
                    .spawn(|| self.handle.block_on(future))
                    .join()
                    .expect("libSQL worker thread panicked")
            }),
            Err(_) => self.handle.block_on(future),
        }
    }

    pub fn block_on_database<F, T, E>(&self, operation: &'static str, future: F) -> Result<T>
    where
        F: Future<Output = std::result::Result<T, E>> + Send,
        T: Send,
        E: Into<anyhow::Error> + Send,
    {
        self.block_on_database_within(operation, self.operation_timeout, future, true)
    }

    /// Rollback and lock release must finish even after the caller has left.
    /// They keep their own deadline and ignore the caller-abandon flag.
    fn block_on_database_cleanup<F, T, E>(&self, operation: &'static str, future: F) -> Result<T>
    where
        F: Future<Output = std::result::Result<T, E>> + Send,
        T: Send,
        E: Into<anyhow::Error> + Send,
    {
        self.block_on_database_within(operation, self.operation_timeout, future, false)
    }

    fn block_on_database_within<F, T, E>(
        &self,
        operation: &'static str,
        timeout: Option<Duration>,
        future: F,
        honor_caller: bool,
    ) -> Result<T>
    where
        F: Future<Output = std::result::Result<T, E>> + Send,
        T: Send,
        E: Into<anyhow::Error> + Send,
    {
        if let Some(probe) = &self.startup_probe {
            probe.record_query();
        }
        let watch = if honor_caller {
            active_caller_abandon()
        } else {
            None
        };
        #[cfg(test)]
        let clock = self.manual_clock.lock().expect("manual clock lock").clone();
        self.block_on(async move {
            let abandoned = async {
                match watch {
                    Some(watch) => watch.wait().await,
                    None => std::future::pending::<()>().await,
                }
            };
            #[cfg(test)]
            let manual_deadline =
                timeout.and_then(|timeout| clock.as_ref().map(|clock| clock.wait(timeout)));
            #[cfg(test)]
            let deadline = async {
                match timeout {
                    Some(timeout) => {
                        if let Some(deadline) = manual_deadline {
                            deadline.await;
                        } else {
                            tokio::time::sleep(timeout).await;
                        }
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            #[cfg(not(test))]
            let deadline = async {
                match timeout {
                    Some(timeout) => tokio::time::sleep(timeout).await,
                    None => std::future::pending::<()>().await,
                }
            };
            let remote = timeout.is_some();
            tokio::select! {
                biased;
                result = future => {
                    let result = result.map_err(Into::into);
                    if remote {
                        result.with_context(|| format!("remote database {operation} failed"))
                    } else {
                        result
                    }
                }
                _ = abandoned => Err(database_caller_abandoned(operation)),
                _ = deadline, if remote => Err(database_operation_timed_out(
                    operation,
                    timeout.unwrap_or(Duration::ZERO),
                )),
            }
        })
    }

    /// Drive a remote `BEGIN` on this runtime.
    ///
    /// libsql stores the Hrana baton only after the open response is read, and
    /// it sends stream close only when a tokio runtime is current. Cancelling
    /// the open at the caller deadline, or dropping it off this runtime, skips
    /// that close. The server can keep `BEGIN IMMEDIATE` and the writer lock
    /// until its own timeout. On a deadline or caller-abandon, keep the open
    /// here for the short release budget, roll it back if it completes, and
    /// drop whatever remains while this runtime is still current.
    fn complete_or_release_open<F>(
        &self,
        operation: &'static str,
        future: F,
    ) -> Result<libsql::Transaction>
    where
        F: Future<Output = std::result::Result<libsql::Transaction, libsql::Error>> + Send,
    {
        self.complete_or_release_open_within(operation, self.operation_timeout, future)
    }

    fn complete_or_release_open_within<F>(
        &self,
        operation: &'static str,
        operation_timeout: Option<Duration>,
        future: F,
    ) -> Result<libsql::Transaction>
    where
        F: Future<Output = std::result::Result<libsql::Transaction, libsql::Error>> + Send,
    {
        let Some(operation_timeout) = operation_timeout else {
            return self.block_on(future).map_err(Into::into);
        };
        let watch = active_caller_abandon();
        #[cfg(test)]
        let clock = self.manual_clock.lock().expect("manual clock lock").clone();
        #[cfg(test)]
        let open_releases = Arc::clone(&self.open_releases);
        enum Stop {
            Abandoned,
            Deadline,
        }
        self.block_on(async move {
            tokio::pin!(future);
            let abandoned = async {
                match &watch {
                    Some(watch) => watch.wait().await,
                    None => std::future::pending::<()>().await,
                }
            };
            #[cfg(test)]
            let manual_deadline = clock.as_ref().map(|clock| clock.wait(operation_timeout));
            #[cfg(test)]
            let deadline = async {
                if let Some(deadline) = manual_deadline {
                    deadline.await;
                } else {
                    tokio::time::sleep(operation_timeout).await;
                }
            };
            #[cfg(not(test))]
            let deadline = async { tokio::time::sleep(operation_timeout).await };
            let stop = tokio::select! {
                biased;
                result = future.as_mut() => {
                    return result
                        .map_err(anyhow::Error::from)
                        .with_context(|| format!("remote database {operation} failed"));
                }
                _ = abandoned, if watch.is_some() => Stop::Abandoned,
                _ = deadline => Stop::Deadline,
            };
            #[cfg(test)]
            open_releases.fetch_add(1, Ordering::Release);
            #[cfg(test)]
            let opened = {
                let manual_deadline = clock
                    .as_ref()
                    .map(|clock| clock.wait(REMOTE_LOCK_RELEASE_TIMEOUT));
                let wait = async {
                    if let Some(deadline) = manual_deadline {
                        deadline.await;
                    } else {
                        tokio::time::sleep(REMOTE_LOCK_RELEASE_TIMEOUT).await;
                    }
                };
                tokio::select! {
                    biased;
                    result = future.as_mut() => Some(result),
                    _ = wait => None,
                }
            };
            #[cfg(not(test))]
            let opened = tokio::select! {
                biased;
                result = future.as_mut() => Some(result),
                _ = tokio::time::sleep(REMOTE_LOCK_RELEASE_TIMEOUT) => None,
            };
            if let Some(Ok(tx)) = opened {
                #[cfg(test)]
                let rolled_back = {
                    let manual_deadline = clock
                        .as_ref()
                        .map(|clock| clock.wait(REMOTE_LOCK_RELEASE_TIMEOUT));
                    let wait = async {
                        if let Some(deadline) = manual_deadline {
                            deadline.await;
                        } else {
                            tokio::time::sleep(REMOTE_LOCK_RELEASE_TIMEOUT).await;
                        }
                    };
                    tokio::select! {
                        biased;
                        result = tx.execute_batch("ROLLBACK") => result.is_ok(),
                        _ = wait => false,
                    }
                };
                #[cfg(not(test))]
                let rolled_back = tokio::select! {
                    biased;
                    result = tx.execute_batch("ROLLBACK") => result.is_ok(),
                    _ = tokio::time::sleep(REMOTE_LOCK_RELEASE_TIMEOUT) => false,
                };
                if rolled_back {
                    // The ending statement already closed the stream. Drop off
                    // this runtime so libsql does not spawn a second rollback.
                    drop_transaction_without_runtime(tx);
                } else {
                    // Still inside this runtime, so libsql can send close.
                    drop(tx);
                }
            }
            match stop {
                Stop::Abandoned => Err(database_caller_abandoned(operation)),
                Stop::Deadline => Err(database_operation_timed_out(operation, operation_timeout)),
            }
        })
    }

    pub(crate) fn is_remote(&self) -> bool {
        self.operation_timeout.is_some()
    }

    pub(crate) fn operation_limit(&self) -> Option<Duration> {
        self.operation_timeout
    }

    #[cfg(test)]
    pub(crate) fn same_owner(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self._thread, &other._thread)
    }

    /// Drop a remote connection on this runtime. libsql sends a stream close
    /// only when a tokio runtime is current; an off-runtime drop logs that no
    /// runtime was available and never sends it.
    pub(crate) fn spawn_drop<T>(&self, value: T)
    where
        T: Send + 'static,
    {
        self.handle.spawn(async move {
            drop(value);
        });
    }

    /// Close a startup connection on this runtime and wait until that drop
    /// returns. libsql sends a stream close only when a tokio runtime is
    /// current; dropping the connection on the plain open thread never sends it.
    pub(crate) fn close_connection(&self, connection: Connection) {
        if self.is_remote() {
            self.block_on(async move {
                drop(connection);
            });
            return;
        }
        if tokio::runtime::Handle::try_current().is_ok() {
            std::thread::scope(|scope| {
                scope
                    .spawn(move || drop(connection))
                    .join()
                    .expect("database connection close panicked")
            });
        } else {
            drop(connection);
        }
    }

    #[cfg(test)]
    pub(crate) fn open_release_count(&self) -> u64 {
        self.open_releases.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn install_manual_clock(&self, clock: Option<Arc<ManualClock>>) {
        *self.manual_clock.lock().expect("manual clock lock") = clock;
    }
}

/// Virtual deadlines for tests. `tokio::time::pause` only works on a
/// current-thread runtime; database work runs on a multi-thread runtime.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct ManualClock {
    now_ms: AtomicU64,
    notify: tokio::sync::Notify,
}

#[cfg(test)]
impl ManualClock {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            now_ms: AtomicU64::new(0),
            notify: tokio::sync::Notify::new(),
        })
    }

    pub(crate) fn advance(&self, by: Duration) {
        let millis = u64::try_from(by.as_millis()).unwrap_or(u64::MAX);
        self.now_ms.fetch_add(millis, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    // Capture the deadline before polling the operation: its HTTP request can
    // wake the test and advance this clock before the timer is first polled.
    fn wait(&self, timeout: Duration) -> impl Future<Output = ()> + '_ {
        let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
        let deadline = self
            .now_ms
            .load(Ordering::SeqCst)
            .saturating_add(timeout_ms);
        async move {
            loop {
                let notified = self.notify.notified();
                if self.now_ms.load(Ordering::SeqCst) >= deadline {
                    return;
                }
                notified.await;
            }
        }
    }
}

/// Set while `Db::with_conn` is still being awaited. Dropping that future
/// signals the watch so the in-flight database call returns and can release
/// its connection and any writer lock. Cleanup rollbacks do not honor it.
pub(crate) struct AbandonWatch {
    flag: AtomicBool,
    notify: tokio::sync::Notify,
}

thread_local! {
    static CALLER_ABANDON: RefCell<Option<Arc<AbandonWatch>>> = const { RefCell::new(None) };
}

impl AbandonWatch {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            flag: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        })
    }

    pub(crate) fn signal(&self) {
        self.flag.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            if self.flag.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

pub(crate) struct CallerAbandonScope {
    previous: Option<Arc<AbandonWatch>>,
}

pub(crate) fn enter_caller_abandon(watch: Arc<AbandonWatch>) -> CallerAbandonScope {
    let previous = CALLER_ABANDON.with(|slot| slot.borrow_mut().replace(watch));
    CallerAbandonScope { previous }
}

impl Drop for CallerAbandonScope {
    fn drop(&mut self) {
        CALLER_ABANDON.with(|slot| *slot.borrow_mut() = self.previous.take());
    }
}

fn active_caller_abandon() -> Option<Arc<AbandonWatch>> {
    CALLER_ABANDON.with(|slot| slot.borrow().clone())
}

// A delayed renewal or backward clock must not undo a later expiry. Keep a
// matching no-op successful, but fence it to the term captured by the caller.
pub(crate) const ACTIVE_BOT_LEASE_CAS: &str = "UPDATE meta SET value=?2 || max(CAST(substr(value,34) AS INTEGER), CAST(substr(?1,34) AS INTEGER)) WHERE key='active_bot_lease' AND substr(value,1,33)=?2 AND CAST(substr(value,34) AS INTEGER) > ?3 AND (SELECT value FROM meta WHERE key='active_bot_generation')=?4 AND COALESCE((SELECT value FROM meta WHERE key='restore_fence'),'0')!='1'";
pub(crate) const LAUNCHER_FENCE_CAS: &str = "UPDATE meta SET value=?1 WHERE key='launcher_lease' AND (substr(value,1,33)=?2 OR CAST(substr(value,34) AS INTEGER) <= ?3) AND COALESCE((SELECT value FROM meta WHERE key='restore_fence'),'0')!='1'";

pub struct Connection {
    inner: libsql::Connection,
    database: Arc<libsql::Database>,
    runtime: DbRuntime,
    tainted: Arc<AtomicBool>,
    config_version: Arc<std::sync::atomic::AtomicU64>,
    capacity_limits: CapacityLimits,
    read_only: bool,
    /// Copied from the pool at checkout. `None` leaves commits unchanged.
    expected_lease_owner: Option<String>,
    /// Guild-store writer generation snapshotted at checkout. `None` skips the epoch check.
    expected_writer_generation: Option<i64>,
    /// Bot-launcher fence. Independent of the application writer lease.
    expected_launcher: Option<String>,
    /// Guild pools probe the core lease at commit. `None` leaves commits unchanged.
    commit_lease_probe: Option<CommitLeaseProbe>,
    schema_verification: Arc<Mutex<Option<SchemaVerification>>>,
}

/// The active-bot lease row is not this instance, or it has expired.
#[derive(Debug)]
pub struct ActiveBotLeaseRejected;

/// Re-read of `meta.active_bot_lease`. Missing, foreign, and expired rows fail.
pub(crate) fn reject_active_bot_lease(value: Option<&str>, expected: &str) -> Result<()> {
    let Some(value) = value else {
        return Err(ActiveBotLeaseRejected.into());
    };
    let mut parts = value.split_whitespace();
    let owner = parts.next().unwrap_or("");
    let expires = parts
        .next()
        .and_then(|text| text.parse::<i64>().ok())
        .unwrap_or(0);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    if owner != expected || expires <= now_ms {
        return Err(ActiveBotLeaseRejected.into());
    }
    Ok(())
}

/// Guild commits pass the remainder of `COMMIT`. The probe checks the core
/// lease under a short writer lock, then runs this continuation after that
/// lock is released.
pub(crate) type CommitLeaseProbe =
    std::sync::Arc<dyn Fn(&mut dyn FnMut() -> Result<()>) -> Result<()> + Send + Sync>;

impl std::fmt::Display for ActiveBotLeaseRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("active bot lease is not held")
    }
}

impl std::error::Error for ActiveBotLeaseRejected {}

#[derive(Clone, Copy)]
struct CapacityLimits {
    growth: u64,
    reclaim: u64,
}

#[cfg(test)]
thread_local! {
    static TEST_DEFAULT_CAPACITY_LIMITS: std::cell::Cell<Option<CapacityLimits>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn with_test_default_capacity_limits<T>(
    growth: u64,
    reclaim: u64,
    f: impl FnOnce() -> T,
) -> T {
    assert!(growth > 0 && growth < reclaim);
    struct Reset(Option<CapacityLimits>);
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_DEFAULT_CAPACITY_LIMITS.with(|slot| slot.set(self.0));
        }
    }
    let previous = TEST_DEFAULT_CAPACITY_LIMITS
        .with(|slot| slot.replace(Some(CapacityLimits { growth, reclaim })));
    let _reset = Reset(previous);
    f()
}

impl Default for CapacityLimits {
    fn default() -> Self {
        #[cfg(test)]
        if let Some(limits) = TEST_DEFAULT_CAPACITY_LIMITS.with(|slot| slot.get()) {
            return limits;
        }
        Self {
            growth: crate::capacity::GROWTH_STOP_BYTES,
            reclaim: crate::capacity::RECLAIM_STOP_BYTES,
        }
    }
}

pub trait SqlExecutor {
    fn execute(&self, sql: &str, params: impl IntoParams + Send) -> Result<usize>;

    /// Execute exactly one caller-audited mutation with a conservative bound
    /// on physical page growth, measured in the same primary transaction.
    fn execute_bounded(
        &self,
        sql: &str,
        params: impl IntoParams + Send,
        max_growth_bytes: u64,
    ) -> Result<usize>;
    fn execute_reclaim_bounded(
        &self,
        sql: &str,
        params: impl IntoParams + Send,
        max_growth_bytes: u64,
    ) -> Result<usize>;

    fn query_row_bounded<T, P, F>(
        &self,
        sql: &str,
        params: P,
        max_growth_bytes: u64,
        map: F,
    ) -> Result<T>
    where
        T: Send,
        P: IntoParams + Send,
        F: FnOnce(&Row) -> Result<T> + Send;

    fn reserve_growth(&self, max_growth_bytes: u64) -> Result<()>;
    fn is_write_transaction(&self) -> bool;
    fn capacity_page_size(&self) -> Result<u64>;

    fn query_row<T, P, F>(&self, sql: &str, params: P, map: F) -> Result<T>
    where
        T: Send,
        P: IntoParams + Send,
        F: FnOnce(&Row) -> Result<T> + Send;

    fn prepare(&self, sql: &str) -> Result<Statement>;

    fn record_config_version(&self, version: u64);

    /// Shared identity memo for this executor's connection.
    ///
    /// The memo type is available through the public database facade:
    ///
    /// ```
    /// use jmodguard::db::{SchemaVerification, SqlExecutor};
    /// use std::sync::{Arc, Mutex};
    ///
    /// fn verification_slot(executor: &impl SqlExecutor) -> Arc<Mutex<Option<SchemaVerification>>> {
    ///     executor.schema_verification_slot()
    /// }
    /// ```
    fn schema_verification_slot(&self) -> Arc<Mutex<Option<SchemaVerification>>>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SqlKind {
    Read,
    Mutation,
    Schema,
    Other,
}

/// The SQLite grammar, rather than a prefix check, rejects comments, writable
/// CTEs, RETURNING and multiple statements at the unbounded read entrypoints.
fn sql_kind(sql: &str) -> Result<SqlKind> {
    let mut parser = Parser::new(sql.as_bytes());
    let Some(command) = parser
        .next()
        .map_err(|_| anyhow::anyhow!("invalid SQL statement"))?
    else {
        anyhow::bail!("empty SQL statement");
    };
    anyhow::ensure!(
        parser
            .next()
            .map_err(|_| anyhow::anyhow!("invalid SQL statement"))?
            .is_none(),
        "multiple SQL statements require an explicit batch budget"
    );
    Ok(match command {
        // SQLite may execute PRAGMA side effects while preparing EXPLAIN, so
        // explain only the ordinary statements whose preparation is inert.
        Cmd::Explain(
            Stmt::Select(_) | Stmt::Insert { .. } | Stmt::Update { .. } | Stmt::Delete { .. },
        )
        | Cmd::ExplainQueryPlan(
            Stmt::Select(_) | Stmt::Insert { .. } | Stmt::Update { .. } | Stmt::Delete { .. },
        ) => SqlKind::Read,
        Cmd::Stmt(Stmt::Select(_)) => SqlKind::Read,
        Cmd::Stmt(Stmt::Pragma(name, body))
            if name.db_name.is_none()
                && matches!(
                    (&name.name.0.to_ascii_lowercase()[..], &body),
                    (
                        "page_count"
                            | "page_size"
                            | "freelist_count"
                            | "user_version"
                            | "query_only"
                            | "foreign_keys",
                        None,
                    ) | (
                        "table_info" | "index_info" | "index_list",
                        Some(PragmaBody::Call(_))
                    )
                ) =>
        {
            SqlKind::Read
        }
        Cmd::Stmt(Stmt::Insert { .. } | Stmt::Update { .. } | Stmt::Delete { .. }) => {
            SqlKind::Mutation
        }
        Cmd::Stmt(
            Stmt::CreateTable { .. }
            | Stmt::CreateIndex { .. }
            | Stmt::CreateTrigger { .. }
            | Stmt::CreateView { .. }
            | Stmt::AlterTable(..)
            | Stmt::DropTable { .. }
            | Stmt::DropIndex { .. }
            | Stmt::DropTrigger { .. }
            | Stmt::DropView { .. },
        ) => SqlKind::Schema,
        _ => SqlKind::Other,
    })
}

fn require_read_sql(sql: &str) -> Result<()> {
    anyhow::ensure!(
        sql_kind(sql)? == SqlKind::Read,
        "unbounded SQL mutation refused"
    );
    Ok(())
}

fn require_mutation_sql(sql: &str) -> Result<()> {
    anyhow::ensure!(
        sql_kind(sql)? == SqlKind::Mutation,
        "bounded SQL must be one data mutation"
    );
    Ok(())
}

fn require_reclaim_sql(sql: &str) -> Result<()> {
    let mut parser = Parser::new(sql.as_bytes());
    let command = parser
        .next()
        .map_err(|_| anyhow::anyhow!("invalid SQL statement"))?;
    anyhow::ensure!(
        parser
            .next()
            .map_err(|_| anyhow::anyhow!("invalid SQL statement"))?
            .is_none(),
        "multiple SQL statements refused"
    );
    let Some(Cmd::Stmt(Stmt::Delete { tbl_name, .. })) = command else {
        anyhow::bail!("reclaim path accepts DELETE only");
    };
    anyhow::ensure!(
        tbl_name.db_name.is_none()
            && matches!(
                tbl_name.name.0.as_str(),
                "guild_message_snapshots"
                    | "guild_cases"
                    | "guild_log_channels"
                    | "guild_settings"
                    | "guild_erasure_jobs"
                    | "guild_store_placements"
                    | "guild_bindings"
                    | "rules"
                    | "commands"
                    | "templates"
                    | "badwords"
                    | "log_channels"
                    | "bot_actions"
                    | "nickname_reconcile"
                    | "warns"
                    | "meta"
                    | "events"
                    | "bans"
                    | "guild_enrollments"
                    | "guild_store_catalog"
                    | "log_filters"
                    | "known_bad_images"
                    | "guild_case_evidence"
                    | "guild_case_history"
                    | "guild_case_sources"
                    | "guild_bunny_evidence"
                    | "guild_tenant_settings"
                    | "guild_memberships"
                    | "guild_member_feature"
                    | "guild_member_seed"
                    | "guild_member_sync"
                    | "guild_member_directory"
                    | "guild_member_pending_bans"
                    | "guild_member_stays"
                    | "guild_member_state_observations"
                    | "guild_member_target_checks"
                    | "guild_member_audit_recovery"
                    | "guild_member_case_index"
                    | "guild_member_unresolved_audits"
                    | "webui_managed_assets"
                    | "component_updates"
                    | "mutation_receipts"
            ),
        "reclaim table is not approved"
    );
    Ok(())
}

fn batch_kind(sql: &str) -> Result<bool> {
    let mut parser = Parser::new(sql.as_bytes());
    let mut saw_write = false;
    let mut saw_statement = false;
    while let Some(command) = parser
        .next()
        .map_err(|_| anyhow::anyhow!("invalid SQL batch"))?
    {
        saw_statement = true;
        match command {
            Cmd::Stmt(Stmt::Select(_)) => {}
            Cmd::Stmt(Stmt::Pragma(name, Some(PragmaBody::Equals(_))))
                if name.db_name.is_none() && name.name.0.eq_ignore_ascii_case("user_version") =>
            {
                saw_write = true
            }
            Cmd::Stmt(
                Stmt::Insert { .. }
                | Stmt::Update { .. }
                | Stmt::Delete { .. }
                | Stmt::CreateTable { .. }
                | Stmt::CreateIndex { .. }
                | Stmt::CreateTrigger { .. }
                | Stmt::CreateView { .. }
                | Stmt::AlterTable(..)
                | Stmt::DropTable { .. }
                | Stmt::DropIndex { .. }
                | Stmt::DropTrigger { .. }
                | Stmt::DropView { .. },
            ) => saw_write = true,
            _ => anyhow::bail!("unsupported SQL batch statement"),
        }
    }
    anyhow::ensure!(saw_statement, "empty SQL batch");
    Ok(saw_write)
}

fn require_connection_setup_batch(sql: &str) -> Result<()> {
    let mut parser = Parser::new(sql.as_bytes());
    let mut seen = false;
    while let Some(command) = parser
        .next()
        .map_err(|_| anyhow::anyhow!("invalid connection setup SQL"))?
    {
        seen = true;
        let allowed = match command {
            Cmd::Stmt(Stmt::Pragma(name, Some(PragmaBody::Equals(_))))
                if name.db_name.is_none() =>
            {
                matches!(
                    name.name.0.to_ascii_lowercase().as_str(),
                    "busy_timeout"
                        | "journal_mode"
                        | "synchronous"
                        | "foreign_keys"
                        | "recursive_triggers"
                        | "query_only"
                )
            }
            Cmd::Stmt(Stmt::Begin(..) | Stmt::Commit(..) | Stmt::Rollback { .. }) => true,
            _ => false,
        };
        anyhow::ensure!(allowed, "unbounded SQL batch mutation refused");
    }
    anyhow::ensure!(seen, "empty SQL batch");
    Ok(())
}

impl Connection {
    pub(crate) fn capacity_growth_limit(&self) -> u64 {
        self.capacity_limits.growth
    }
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let runtime = DbRuntime::new()?;
        let path = path.as_ref().to_path_buf();
        let database = runtime.block_on(libsql::Builder::new_local(path).build())?;
        let database = Arc::new(database);
        let inner = database.connect()?;
        Ok(Self {
            inner,
            database,
            runtime,
            tainted: Arc::new(AtomicBool::new(false)),
            config_version: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            capacity_limits: CapacityLimits::default(),
            read_only: false,
            expected_lease_owner: None,
            expected_writer_generation: None,
            expected_launcher: None,
            commit_lease_probe: None,
            schema_verification: Arc::new(Mutex::new(None)),
        })
    }

    pub fn from_database(database: Arc<libsql::Database>, runtime: DbRuntime) -> Result<Self> {
        Ok(Self {
            inner: database.connect()?,
            database,
            runtime,
            tainted: Arc::new(AtomicBool::new(false)),
            config_version: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            capacity_limits: CapacityLimits::default(),
            read_only: false,
            expected_lease_owner: None,
            expected_writer_generation: None,
            expected_launcher: None,
            commit_lease_probe: None,
            schema_verification: Arc::new(Mutex::new(None)),
        })
    }

    pub(crate) fn set_expected_lease_owner(&mut self, owner: Option<String>) {
        self.expected_lease_owner = owner;
    }

    pub(crate) fn set_expected_writer_generation(&mut self, generation: Option<i64>) {
        self.expected_writer_generation = generation;
    }

    pub(crate) fn expected_writer_generation(&self) -> Option<i64> {
        self.expected_writer_generation
    }

    pub(crate) fn set_expected_launcher(&mut self, instance: Option<String>) {
        self.expected_launcher = instance;
    }

    pub(crate) fn set_commit_lease_probe(&mut self, probe: Option<CommitLeaseProbe>) {
        self.commit_lease_probe = probe;
    }

    pub(crate) fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
    }

    #[cfg(test)]
    pub(crate) fn runtime_same_owner(&self, other: &DbRuntime) -> bool {
        self.runtime.same_owner(other)
    }

    fn refuse_writer_lock(&self) -> Result<()> {
        anyhow::ensure!(
            !self.read_only,
            "read-only connection cannot start a write transaction"
        );
        Ok(())
    }

    /// Start a pooled remote operation without the previous operation's
    /// autocommit cursor baton. The server may expire it while the lease is
    /// idle. libSQL reset clears local stream state without sending SQL or
    /// replaying a failed request; explicit transactions use separate streams.
    ///
    /// Checkout calls this on every remote operation. The guild-store identity
    /// memo stays. A later check still reads schema_version, user_version, and
    /// the store_identity row, and a new connection starts without a memo.
    pub(crate) fn reset_remote_stream(&self) -> Result<()> {
        self.runtime
            .block_on_database("reset pooled stream", async {
                self.inner.reset().await;
                Ok::<_, anyhow::Error>(())
            })
    }

    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        require_connection_setup_batch(sql)?;
        self.runtime
            .block_on_database("execute_batch", self.inner.execute_batch(sql))?;
        Ok(())
    }

    /// Checked schema/bootstrap batch. Startup verifies identity and retries
    /// ambiguous remote results separately because the receipt table may not
    /// exist until this batch has completed.
    pub fn execute_batch_bounded(&self, sql: &str, max_growth_bytes: u64) -> Result<()> {
        anyhow::ensure!(
            self.inner.is_autocommit(),
            "bounded batch needs its own transaction"
        );
        let tx = self.transaction_primary_read()?;
        tx.execute_batch_bounded(sql, max_growth_bytes)?;
        tx.commit()
    }

    pub fn transaction(&self) -> Result<Transaction> {
        self.transaction_immediate()
    }

    /// Remote reads and snapshots. Local files and write transactions pass
    /// `None` and keep the connection deadline.
    fn interactive_read_budget(&self) -> Option<Duration> {
        self.runtime
            .operation_limit()
            .map(|limit| INTERACTIVE_READ_BUDGET.min(limit))
    }

    /// Starts a write transaction at the primary immediately. Remote libSQL
    /// can reject a deferred transaction when it must upgrade from a replica
    /// read to a primary write while another writer is active.
    fn open_remote_transaction(
        &self,
        operation: &'static str,
        behavior: libsql::TransactionBehavior,
        read_budget: Option<Duration>,
    ) -> Result<(libsql::Transaction, Option<tokio::time::Instant>)> {
        let runtime = self.runtime.clone();
        let immediate = matches!(behavior, libsql::TransactionBehavior::Immediate);
        let started = tokio::time::Instant::now();
        let open = self.inner.transaction_with_behavior(behavior);
        let inner = match read_budget {
            Some(budget) => {
                runtime.complete_or_release_open_within(operation, Some(budget), open)?
            }
            None => runtime.complete_or_release_open(operation, open)?,
        };
        if let Some(probe) = &self.runtime.startup_probe {
            probe.transactions.fetch_add(1, Ordering::Relaxed);
            if immediate {
                probe.record_immediate();
            }
        }
        Ok((inner, read_budget.map(|budget| started + budget)))
    }

    pub fn transaction_immediate(&self) -> Result<Transaction> {
        self.refuse_writer_lock()?;
        let (inner, _) = self.open_remote_transaction(
            "begin immediate transaction",
            libsql::TransactionBehavior::Immediate,
            None,
        )?;
        let mut transaction = self.wrap_transaction(inner, None, true, None);
        transaction.ensure_mutation_receipt()?;
        Ok(transaction)
    }

    /// Fresh primary snapshot that does not reserve the writer lock.
    ///
    /// `BEGIN DEFERRED` is not classified read-only, so remote libSQL routes it
    /// to the primary and the snapshot sees committed primary writes. SQLite
    /// takes a shared lock for the reads and does not take the reserved lock
    /// until a statement writes. The bot can therefore start a write while this
    /// snapshot is open, and a held writer does not make the snapshot wait.
    /// `BEGIN READONLY` is not used here: remote libSQL may serve it from a
    /// replica. The snapshot cannot be upgraded into a write.
    pub fn transaction_primary_snapshot(&self) -> Result<Transaction> {
        let (inner, deadline) = self.open_remote_transaction(
            "begin primary snapshot",
            libsql::TransactionBehavior::Deferred,
            self.interactive_read_budget(),
        )?;
        Ok(self.wrap_transaction(inner, None, false, deadline))
    }

    /// Snapshot capture reads before acquiring the writer lock. A concurrent
    /// writer can invalidate promotion; the capture owner must restart the
    /// entire operation on a proven busy error, never reuse the read results.
    pub(crate) fn transaction_capture(&self) -> Result<Transaction> {
        self.refuse_writer_lock()?;
        let (inner, deadline) = self.open_remote_transaction(
            "begin capture transaction",
            libsql::TransactionBehavior::Deferred,
            self.interactive_read_budget(),
        )?;
        Ok(self.wrap_transaction(inner, None, true, deadline))
    }

    /// Replica-scalable read. `BEGIN READONLY` does not take the writer lock.
    /// Remote libSQL may serve it from a replica, so callers that need a fresh
    /// primary snapshot use [`Self::transaction_primary_snapshot`].
    pub fn transaction_consistent_read(&self) -> Result<Transaction> {
        let (inner, deadline) = self.open_remote_transaction(
            "begin consistent read",
            libsql::TransactionBehavior::ReadOnly,
            self.interactive_read_budget(),
        )?;
        Ok(self.wrap_transaction(inner, None, false, deadline))
    }

    /// Maintenance-only DELETE transaction. It preserves a 16 MB hard-limit
    /// reserve and may commit above 900 MB only when allocation did not grow.
    pub fn transaction_reclaim(&self) -> Result<Transaction> {
        let mut transaction = self.transaction_primary_read()?;
        transaction.prepare_reclaim()?;
        Ok(transaction)
    }

    /// Starts an IMMEDIATE transaction routed to the primary without creating
    /// a mutation receipt. Before non-migration writes, enable a mutation receipt
    /// on the returned transaction so lost COMMIT responses remain recoverable.
    pub fn transaction_primary_read(&self) -> Result<Transaction> {
        self.refuse_writer_lock()?;
        let (inner, _) = self.open_remote_transaction(
            "begin primary read transaction",
            libsql::TransactionBehavior::Immediate,
            None,
        )?;
        Ok(self.wrap_transaction(inner, None, true, None))
    }

    fn wrap_transaction(
        &self,
        inner: libsql::Transaction,
        mutation_receipt: Option<String>,
        allows_write: bool,
        read_deadline: Option<tokio::time::Instant>,
    ) -> Transaction {
        Transaction {
            inner: Some(inner),
            database: Arc::clone(&self.database),
            runtime: self.runtime.clone(),
            connection_tainted: Arc::clone(&self.tainted),
            connection_config_version: Arc::clone(&self.config_version),
            pending_config_version: std::sync::atomic::AtomicU64::new(0),
            allows_write,
            read_deadline,
            mutation_receipt,
            capacity_dirty: AtomicBool::new(false),
            capacity_poisoned: AtomicBool::new(false),
            write_prepared: AtomicBool::new(false),
            stream_exhausted: Arc::new(AtomicBool::new(false)),
            client_deadline: Arc::new(AtomicBool::new(false)),
            capacity_reservation: Mutex::new(None),
            capacity_limits: self.capacity_limits,
            expected_lease_owner: self.expected_lease_owner.clone(),
            expected_writer_generation: self.expected_writer_generation,
            expected_launcher: self.expected_launcher.clone(),
            commit_lease_probe: self.commit_lease_probe.clone(),
            schema_verification: Arc::clone(&self.schema_verification),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_test_capacity_limits(&mut self, growth: u64, reclaim: u64) {
        assert!(growth > 0 && growth < reclaim && reclaim <= crate::capacity::DATABASE_LIMIT_BYTES);
        self.capacity_limits = CapacityLimits { growth, reclaim };
    }

    pub fn pragma_update<T: Display>(
        &self,
        _schema: Option<&str>,
        pragma: &str,
        value: T,
    ) -> Result<()> {
        anyhow::ensure!(
            pragma == "user_version",
            "unbounded connection PRAGMA update refused"
        );
        self.execute_batch_bounded(&format!("PRAGMA {pragma} = {value}"), 65_536)
    }

    /// Only fixed-format, existing lease rows may bypass mutation admission.
    pub(crate) fn execute_lease_cas(
        &self,
        sql: &str,
        params: impl IntoParams + Send,
        timeout: Duration,
    ) -> Result<usize> {
        anyhow::ensure!(!timeout.is_zero(), "lease CAS authority window expired");
        anyhow::ensure!(
            matches!(sql, ACTIVE_BOT_LEASE_CAS | LAUNCHER_FENCE_CAS),
            "lease CAS mutation refused"
        );
        self.refuse_writer_lock()?;
        anyhow::ensure!(self.inner.is_autocommit(), "lease CAS requires autocommit");
        // Defensive callers cannot ask for longer than either unchanged TTL.
        // Local SQLite must wait on its writer for this same budget, rather
        // than fail at the connection's default two-second busy timeout.
        // libSQL's remote busy_timeout setter is a no-op (no extra request).
        let timeout = timeout.min(Duration::from_millis(
            crate::db::ACTIVE_BOT_LEASE_TTL_MS.max(crate::db::LAUNCHER_FENCE_TTL_MS) as u64,
        ));
        self.inner.busy_timeout(timeout)?;
        // In-place replacement of an existing fixed-length meta lease cannot
        // allocate pages, so it needs no capacity reservation or receipt. The
        // CAS is idempotent: a lost response is resolved by retry/re-read. Its
        // WHERE clause is the fence; no Transaction::commit re-check is needed.
        Ok(self.runtime.block_on_database_within(
            "lease CAS",
            Some(timeout),
            self.inner.execute(sql, params),
            true,
        )? as usize)
    }

    pub fn is_autocommit(&self) -> bool {
        self.inner.is_autocommit()
    }

    pub fn is_tainted(&self) -> bool {
        self.tainted.load(Ordering::Acquire)
    }

    /// Stable for one pooled connection, including after it is checked out again.
    #[cfg(test)]
    pub(crate) fn pool_identity(&self) -> usize {
        Arc::as_ptr(&self.tainted) as usize
    }

    pub fn take_config_version(&self) -> u64 {
        self.config_version.swap(0, Ordering::AcqRel)
    }
}

impl SqlExecutor for Connection {
    fn execute(&self, sql: &str, params: impl IntoParams + Send) -> Result<usize> {
        require_read_sql(sql)?;
        Ok(self
            .runtime
            .block_on_database("execute", self.inner.execute(sql, params))? as usize)
    }

    fn execute_bounded(
        &self,
        sql: &str,
        params: impl IntoParams + Send,
        max_growth_bytes: u64,
    ) -> Result<usize> {
        require_mutation_sql(sql)?;
        anyhow::ensure!(
            self.inner.is_autocommit(),
            "bounded connection write needs its own transaction"
        );
        let transaction = self.transaction_immediate()?;
        let changed = transaction.execute_bounded(sql, params, max_growth_bytes)?;
        transaction.commit()?;
        Ok(changed)
    }

    fn execute_reclaim_bounded(
        &self,
        sql: &str,
        params: impl IntoParams + Send,
        max_growth_bytes: u64,
    ) -> Result<usize> {
        require_reclaim_sql(sql)?;
        anyhow::ensure!(
            self.inner.is_autocommit(),
            "bounded connection write needs its own transaction"
        );
        let transaction = self.transaction_reclaim()?;
        let changed = transaction.execute_reclaim_bounded(sql, params, max_growth_bytes)?;
        transaction.commit()?;
        Ok(changed)
    }

    fn query_row_bounded<T, P, F>(
        &self,
        sql: &str,
        params: P,
        max_growth_bytes: u64,
        map: F,
    ) -> Result<T>
    where
        T: Send,
        P: IntoParams + Send,
        F: FnOnce(&Row) -> Result<T> + Send,
    {
        require_mutation_sql(sql)?;
        anyhow::ensure!(
            self.inner.is_autocommit(),
            "bounded connection write needs its own transaction"
        );
        let transaction = self.transaction_immediate()?;
        let value = transaction.query_row_bounded(sql, params, max_growth_bytes, map)?;
        transaction.commit()?;
        Ok(value)
    }

    fn reserve_growth(&self, _max_growth_bytes: u64) -> Result<()> {
        anyhow::bail!("reservation requires an immediate transaction")
    }

    fn is_write_transaction(&self) -> bool {
        false
    }

    fn capacity_page_size(&self) -> Result<u64> {
        let page_size: i64 = self.query_row("PRAGMA page_size", (), |row| Ok(row.get(0)?))?;
        anyhow::ensure!(
            (512..=65536).contains(&page_size),
            "unknown database page size"
        );
        Ok(page_size as u64)
    }

    fn query_row<T, P, F>(&self, sql: &str, params: P, map: F) -> Result<T>
    where
        T: Send,
        P: IntoParams + Send,
        F: FnOnce(&Row) -> Result<T> + Send,
    {
        require_read_sql(sql)?;
        if sql.contains("sqlite_master")
            && let Some(probe) = &self.runtime.startup_probe
        {
            probe.record_catalog();
        }
        self.runtime.block_on_database("query", async {
            let mut rows = self.inner.query(sql, params).await?;
            let row = rows.next().await?.ok_or(QueryReturnedNoRows)?;
            map(&row)
        })
    }

    fn prepare(&self, sql: &str) -> Result<Statement> {
        require_read_sql(sql)?;
        let inner = self
            .runtime
            .block_on_database("prepare", self.inner.prepare(sql))?;
        Ok(Statement {
            inner,
            runtime: self.runtime.clone(),
            read_deadline: None,
            client_deadline: Arc::new(AtomicBool::new(false)),
            stream_exhausted: Arc::new(AtomicBool::new(false)),
        })
    }

    fn record_config_version(&self, version: u64) {
        self.config_version.fetch_max(version, Ordering::SeqCst);
    }

    fn schema_verification_slot(&self) -> Arc<Mutex<Option<SchemaVerification>>> {
        Arc::clone(&self.schema_verification)
    }
}

pub struct Statement {
    inner: libsql::Statement,
    runtime: DbRuntime,
    /// Absolute budget copied from a remote read or snapshot. Autocommit
    /// prepares and statements after a capture write leave this empty.
    /// `query_map` is the only execution method. A new one has to honor
    /// [`Self::query_wait`] instead of the pool deadline.
    read_deadline: Option<tokio::time::Instant>,
    client_deadline: Arc<AtomicBool>,
    stream_exhausted: Arc<AtomicBool>,
}

pub struct MappedRows<T>(std::vec::IntoIter<Result<T>>);

impl<T> Iterator for MappedRows<T> {
    type Item = Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

impl Statement {
    pub fn query_map<T, P, F>(&mut self, params: P, mut map: F) -> Result<MappedRows<T>>
    where
        T: Send,
        P: IntoParams + Send,
        F: FnMut(&Row) -> Result<T> + Send,
    {
        let timeout = self.query_wait();
        let runtime = self.runtime.clone();
        let client_deadline = Arc::clone(&self.client_deadline);
        let stream_exhausted = Arc::clone(&self.stream_exhausted);
        let query = async {
            let mut cursor = self.inner.query(params).await?;
            let mut rows = Vec::new();
            while let Some(row) = cursor.next().await? {
                rows.push(map(&row));
            }
            Ok::<_, anyhow::Error>(rows)
        };
        let mapped = if let Some(timeout) = timeout {
            runtime.block_on_database_within("query", Some(timeout), query, true)
        } else {
            runtime.block_on_database("query", query)
        };
        if let Err(error) = &mapped {
            if is_database_operation_timeout(error) {
                client_deadline.store(true, Ordering::Release);
            } else if is_remote_interactive_timeout(error) {
                stream_exhausted.store(true, Ordering::Release);
            }
        }
        Ok(MappedRows(mapped?.into_iter()))
    }

    fn query_wait(&self) -> Option<Duration> {
        let deadline = self.read_deadline?;
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        Some(match self.runtime.operation_limit() {
            Some(limit) => remaining.min(limit),
            None => remaining,
        })
    }
}

pub struct Transaction {
    inner: Option<libsql::Transaction>,
    database: Arc<libsql::Database>,
    runtime: DbRuntime,
    connection_tainted: Arc<AtomicBool>,
    connection_config_version: Arc<std::sync::atomic::AtomicU64>,
    pending_config_version: std::sync::atomic::AtomicU64,
    /// False for snapshots and replica reads. Those transactions must not
    /// upgrade into a write and take the reserved lock.
    allows_write: bool,
    /// Remote reads and snapshots. Cleared in effect once a capture prepares
    /// a write; member write transactions never set it.
    read_deadline: Option<tokio::time::Instant>,
    mutation_receipt: Option<String>,
    capacity_dirty: AtomicBool,
    capacity_poisoned: AtomicBool,
    write_prepared: AtomicBool,
    /// The provider already closed this Hrana stream. Drop must not send
    /// ROLLBACK on it; the connection is tainted and the next open uses a new one.
    /// Shared with prepared statements so a stalled `query_map` can record it.
    stream_exhausted: Arc<AtomicBool>,
    /// Our deadline cancelled an in-flight call. The stream may still accept
    /// ROLLBACK. This is not [`Self::stream_exhausted`].
    client_deadline: Arc<AtomicBool>,
    capacity_reservation: Mutex<Option<CapacityReservation>>,
    capacity_limits: CapacityLimits,
    expected_lease_owner: Option<String>,
    expected_writer_generation: Option<i64>,
    expected_launcher: Option<String>,
    commit_lease_probe: Option<CommitLeaseProbe>,
    schema_verification: Arc<Mutex<Option<SchemaVerification>>>,
}

#[derive(Clone, Copy)]
struct CapacityReservation {
    start_bytes: u64,
    initial_bytes: u64,
    reserved_bytes: u64,
    page_size: u64,
    limit_bytes: u64,
}

impl Transaction {
    /// Startup-only privilege probe. The fixed no-op write is rolled back by
    /// `finish_read`, so it creates no receipt or capacity reservation.
    pub(crate) fn probe_write_permission(&self) -> Result<()> {
        anyhow::ensure!(
            self.allows_write,
            "read snapshot cannot take the write lock"
        );
        let restore_fence: Option<String> = self
            .query_row(
                "SELECT value FROM meta WHERE key='restore_fence'",
                (),
                |row| Ok(row.get(0)?),
            )
            .optional()?;
        anyhow::ensure!(
            restore_fence.as_deref() != Some("1"),
            "restore is in progress"
        );
        self.execute_unchecked(
            "UPDATE meta SET value=value WHERE key='bootstrap_complete'",
            (),
        )?;
        Ok(())
    }
    /// Promote a verified primary snapshot to a maintenance transaction only
    /// after the caller has checked its identity and ownership scope.
    pub(crate) fn prepare_reclaim(&mut self) -> Result<()> {
        self.ensure_mutation_receipt_limit(self.capacity_limits.reclaim)?;
        self.reserve_growth_limit(0, self.capacity_limits.reclaim)
    }

    /// Admit a caller's conservative growth bound while holding the primary
    /// write lock. Measurement failures fail closed; no engine quota is changed.
    pub fn reserve_growth(&self, additional_bytes: u64) -> Result<()> {
        self.reserve_growth_limit(additional_bytes, self.capacity_limits.growth)
    }

    fn reserve_growth_limit(&self, additional_bytes: u64, limit_bytes: u64) -> Result<()> {
        anyhow::ensure!(
            !self.capacity_poisoned.load(Ordering::Acquire),
            "database growth exceeded the reserved bound; transaction is poisoned"
        );
        anyhow::ensure!(
            limit_bytes != self.capacity_limits.growth
                || !self.runtime.growth_paused.load(Ordering::Acquire),
            "database growth paused after an underestimated write bound"
        );
        let mut reservation = self
            .capacity_reservation
            .lock()
            .map_err(|_| anyhow::anyhow!("capacity reservation lock poisoned"))?;
        let current = match *reservation {
            Some(current) => current,
            None => {
                let sample = crate::capacity::sample(self)?;
                CapacityReservation {
                    start_bytes: sample.allocated_bytes,
                    initial_bytes: sample.allocated_bytes,
                    reserved_bytes: 0,
                    page_size: sample.page_size,
                    limit_bytes,
                }
            }
        };
        let mut next = current;
        anyhow::ensure!(
            current.limit_bytes == 0 || current.limit_bytes == limit_bytes,
            "mixed database capacity policies in one transaction"
        );
        next.limit_bytes = limit_bytes;
        let cumulative = current
            .reserved_bytes
            .checked_add(additional_bytes)
            .ok_or_else(|| anyhow::anyhow!("database capacity reservation overflow"))?;
        if crate::capacity::admit_to(current.initial_bytes, cumulative, limit_bytes).is_err() {
            // A long transaction may have reserved far more than it actually
            // allocated. Rebase only after verifying the previous bound.
            let actual = crate::capacity::sample(self)?;
            anyhow::ensure!(
                actual.page_size == current.page_size,
                "database page size changed during transaction"
            );
            if actual.allocated_bytes > current.initial_bytes.saturating_add(current.reserved_bytes)
            {
                self.capacity_poisoned.store(true, Ordering::Release);
                self.runtime.growth_paused.store(true, Ordering::Release);
                anyhow::bail!(
                    "database growth exceeded the reserved bound; transaction is poisoned"
                );
            }
            next.initial_bytes = actual.allocated_bytes;
            next.reserved_bytes = 0;
        }
        next.reserved_bytes = next
            .reserved_bytes
            .checked_add(additional_bytes)
            .ok_or_else(|| anyhow::anyhow!("database capacity reservation overflow"))?;
        crate::capacity::admit_to(next.initial_bytes, next.reserved_bytes, limit_bytes)?;
        *reservation = Some(next);
        self.capacity_dirty.store(true, Ordering::Release);
        Ok(())
    }

    fn capacity_page_size_cached(&self) -> Result<u64> {
        let mut reservation = self
            .capacity_reservation
            .lock()
            .map_err(|_| anyhow::anyhow!("capacity reservation lock poisoned"))?;
        if let Some(current) = *reservation {
            return Ok(current.page_size);
        }
        let sample = crate::capacity::sample(self)?;
        *reservation = Some(CapacityReservation {
            start_bytes: sample.allocated_bytes,
            initial_bytes: sample.allocated_bytes,
            reserved_bytes: 0,
            page_size: sample.page_size,
            limit_bytes: 0,
        });
        Ok(sample.page_size)
    }

    fn note_remote_stream_loss(&self, error: &anyhow::Error) {
        if is_database_operation_timeout(error) {
            self.client_deadline.store(true, Ordering::Release);
            return;
        }
        if is_remote_interactive_timeout(error) {
            self.stream_exhausted.store(true, Ordering::Release);
        }
    }

    /// One short ROLLBACK after our deadline. libSQL sends stream close with
    /// a transaction-ending statement, so a live baton is not left behind.
    /// A provider-closed stream must not call this: that request waits out
    /// the client deadline and cannot change the outcome.
    fn release_remote_write_lock(&self) -> Result<()> {
        self.runtime
            .block_on_database_within(
                "release write lock",
                Some(REMOTE_LOCK_RELEASE_TIMEOUT),
                self.inner().execute_batch("ROLLBACK"),
                false,
            )
            .map(|_| ())
    }

    fn remote_write_needs_prompt_release(&self) -> bool {
        self.runtime.is_remote()
            && self.client_deadline.load(Ordering::Acquire)
            && !self.stream_exhausted.load(Ordering::Acquire)
            && (self.allows_write || self.read_deadline.is_some())
    }

    /// Rollback for a remote read or snapshot, including one whose statements
    /// all succeeded. An already-expired budget still gets one short release
    /// instead of the pool deadline. Writes keep the normal cleanup path.
    fn budgeted_read_rollback(&self, operation: &'static str) -> Option<Result<()>> {
        let wait = self.statement_wait()?;
        let timeout = if wait.is_zero() {
            REMOTE_LOCK_RELEASE_TIMEOUT
        } else {
            wait
        };
        Some(
            self.runtime
                .block_on_database_within(
                    operation,
                    Some(timeout),
                    self.inner().execute_batch("ROLLBACK"),
                    false,
                )
                .map(|_| ()),
        )
    }

    /// Time left in a remote read or snapshot. `None` keeps the runtime deadline
    /// (writes, and a capture after it has prepared a mutation).
    fn statement_wait(&self) -> Option<Duration> {
        let deadline = self.read_deadline?;
        if self.write_prepared.load(Ordering::Acquire) {
            return None;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        Some(match self.runtime.operation_limit() {
            Some(limit) => remaining.min(limit),
            None => remaining,
        })
    }

    fn drive<T, E>(
        &self,
        operation: &'static str,
        future: impl Future<Output = std::result::Result<T, E>> + Send,
    ) -> Result<T>
    where
        T: Send,
        E: Into<anyhow::Error> + Send,
    {
        if let Some(timeout) = self.statement_wait() {
            self.runtime
                .block_on_database_within(operation, Some(timeout), future, true)
        } else {
            self.runtime.block_on_database(operation, future)
        }
    }

    fn map_stream<T>(&self, result: Result<T>) -> Result<T> {
        if let Err(error) = &result {
            self.note_remote_stream_loss(error);
        }
        result
    }

    fn execute_unchecked(&self, sql: &str, params: impl IntoParams + Send) -> Result<usize> {
        self.map_stream(
            self.drive(
                "transaction internal execute",
                self.inner().execute(sql, params),
            )
            .map(|rows| rows as usize),
        )
    }

    fn query_row_unchecked<T, P, F>(&self, sql: &str, params: P, map: F) -> Result<T>
    where
        T: Send,
        P: IntoParams + Send,
        F: FnOnce(&Row) -> Result<T> + Send,
    {
        if sql.contains("sqlite_master")
            && let Some(probe) = &self.runtime.startup_probe
        {
            probe.record_catalog();
        }
        self.map_stream(self.drive("transaction internal query", async {
            let mut rows = self.inner().query(sql, params).await?;
            let row = rows.next().await?.ok_or(QueryReturnedNoRows)?;
            map(&row)
        }))
    }

    fn insert_remote_mutation_receipt(&mut self, receipt: &str) -> Result<String> {
        match self.query_row_unchecked(INSERT_MUTATION_RECEIPT_SQL, params![receipt], |row| {
            Ok(row.get::<String>(0)?)
        }) {
            Ok(inserted) => Ok(inserted),
            Err(error) if self.stream_exhausted.load(Ordering::Acquire) => Err(error),
            Err(error)
                if error
                    .chain()
                    .any(|cause| cause.to_string().contains("no such table: meta")) =>
            {
                self.query_row_unchecked(
                    INSERT_UNFENCED_MUTATION_RECEIPT_SQL,
                    params![receipt],
                    |row| Ok(row.get::<String>(0)?),
                )
            }
            Err(error) => Err(error),
        }
    }

    /// Enable remote commit recovery only once a primary snapshot needs a write.
    /// Call before executing mutations; read-only exits need no receipt or pruning.
    pub(crate) fn ensure_mutation_receipt(&mut self) -> Result<()> {
        self.ensure_mutation_receipt_limit(self.capacity_limits.growth)
    }

    fn ensure_mutation_receipt_limit(&mut self, limit_bytes: u64) -> Result<()> {
        anyhow::ensure!(
            self.allows_write,
            "read snapshot cannot take the write lock"
        );
        if self.mutation_receipt.is_some() {
            return Ok(());
        }
        if !self.runtime.is_remote() {
            match self.query_row(
                "SELECT value FROM meta WHERE key = 'restore_fence'",
                (),
                |r| Ok(r.get::<String>(0)?),
            ) {
                Ok(value) if value == "1" => anyhow::bail!("restore is in progress"),
                Ok(_) => {}
                Err(error)
                    if error.downcast_ref::<QueryReturnedNoRows>().is_some()
                        || error.to_string().contains("no such table: meta") => {}
                Err(error) => return Err(error),
            }
            self.write_prepared.store(true, Ordering::Release);
            return Ok(());
        }
        // Receipt insertion itself can allocate pages. It is independently
        // reserved before the first mutation, even for an otherwise empty tx.
        self.reserve_growth_limit(RECEIPT_GROWTH_ALLOWANCE, limit_bytes)?;
        let receipt = format!(
            "{:032x}{:032x}",
            rand::random::<u128>(),
            rand::random::<u128>()
        );
        // One statement both checks the restore fence and creates the commit
        // receipt. Remote libSQL otherwise pays a network round trip for each.
        match self.insert_remote_mutation_receipt(&receipt) {
            Ok(inserted) if inserted == receipt => {}
            Ok(_) => anyhow::bail!("mutation receipt identity changed"),
            Err(error) if error.downcast_ref::<QueryReturnedNoRows>().is_some() => {
                anyhow::bail!("restore is in progress")
            }
            Err(error) => return Err(error),
        }
        self.mutation_receipt = Some(receipt);
        self.write_prepared.store(true, Ordering::Release);
        // Keep the existing bounded cleanup rate for receipt-bearing writes.
        if RECEIPT_PRUNE_COUNTER.fetch_add(1, Ordering::Relaxed) & 255 == 0 {
            self.execute_unchecked(
                "DELETE FROM mutation_receipts WHERE receipt_id IN (
                     SELECT receipt_id FROM mutation_receipts
                     WHERE created_at < unixepoch() - 600
                     ORDER BY created_at LIMIT 1000
                 )",
                (),
            )?;
        }
        Ok(())
    }

    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        require_connection_setup_batch(sql)?;
        self.map_stream(self.drive("transaction execute_batch", self.inner().execute_batch(sql)))?;
        Ok(())
    }

    pub fn execute_batch_bounded(&self, sql: &str, max_growth_bytes: u64) -> Result<()> {
        anyhow::ensure!(batch_kind(sql)?, "bounded batch must contain a mutation");
        self.reserve_growth(max_growth_bytes)?;
        self.map_stream(self.drive("transaction bounded batch", self.inner().execute_batch(sql)))?;
        Ok(())
    }

    /// The only metadata update allowed in a cleanup transaction. Its fixed
    /// columns record cleanup freshness and invalidate a cached physical sample.
    pub fn execute_reclaim_metadata_bounded(
        &self,
        sql: &str,
        params: impl IntoParams + Send,
        max_growth_bytes: u64,
    ) -> Result<usize> {
        anyhow::ensure!(
            matches!(
                sql.trim(),
                "UPDATE guild_snapshot_health SET last_cleanup_at_ms=?1,updated_at_ms=?1,physical_sample_at_ms=NULL,expired_count=expired_count+?2 WHERE singleton=1"
                    | "UPDATE guild_snapshot_health SET last_cleanup_at_ms=?1,updated_at_ms=?1,physical_sample_at_ms=NULL,evicted_count=evicted_count+?2 WHERE singleton=1"
                    | "UPDATE guild_snapshot_health SET expired_count=expired_count+?1 WHERE singleton=1"
                    | "UPDATE guild_snapshot_health SET physical_sample_at_ms=NULL WHERE singleton=1"
                    | "UPDATE guild_snapshot_health SET last_cleanup_at_ms=?1,updated_at_ms=?1 WHERE singleton=1"
                    | "UPDATE meta SET value=?1 WHERE key='action_request_floor'"
                    | "UPDATE meta SET value=CAST(CAST(value AS INTEGER)+1 AS TEXT) WHERE key='config_version'"
                    | "UPDATE meta SET value=?1 WHERE key='config_revision'"
                    | "UPDATE meta SET value=CAST(CAST(value AS INTEGER)+1 AS TEXT) WHERE key='slash_registry_version'"
                    | "UPDATE meta SET value=?1 WHERE key='slash_registry_revision'"
                    | "UPDATE guild_memberships SET active=0 WHERE guild_id=?1"
                    | "UPDATE guild_memberships SET config_revision=?1 WHERE guild_id=?2"
            ),
            "reclaim metadata mutation refused"
        );
        anyhow::ensure!(
            self.write_prepared.load(Ordering::Acquire),
            "write transaction has no mutation receipt or restore fence check"
        );
        let limit = self
            .capacity_reservation
            .lock()
            .map_err(|_| anyhow::anyhow!("capacity reservation lock poisoned"))?
            .as_ref()
            .map(|r| r.limit_bytes)
            .filter(|n| *n != 0)
            .unwrap_or(self.capacity_limits.growth);
        self.reserve_growth_limit(max_growth_bytes, limit)?;
        self.execute_unchecked(sql, params)
    }

    pub fn pragma_update<T: Display>(
        &self,
        _schema: Option<&str>,
        pragma: &str,
        value: T,
    ) -> Result<()> {
        self.execute_batch_bounded(&format!("PRAGMA {pragma} = {value}"), 65_536)
    }

    pub fn last_insert_rowid(&self) -> i64 {
        self.inner().last_insert_rowid()
    }

    fn renew_launcher_fence(&self, instance: &str) -> Result<()> {
        const KEY: &str = "launcher_lease";

        let now = crate::control::now_ms();
        let current: Option<String> = self
            .query_row("SELECT value FROM meta WHERE key=?1", params![KEY], |row| {
                Ok(row.get(0)?)
            })
            .optional()?;
        if let Some(value) = current.as_deref() {
            let mut parts = value.split_whitespace();
            let owner = parts.next().unwrap_or("");
            let expires = parts
                .next()
                .and_then(|text| text.parse::<i64>().ok())
                .unwrap_or(0);
            if owner != instance && expires > now {
                anyhow::bail!("launcher fence held");
            }
        }
        let next = format!(
            "{instance} {}",
            now.saturating_add(crate::db::LAUNCHER_FENCE_TTL_MS)
        );
        self.execute_bounded(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![KEY, next],
            4096,
        )?;
        Ok(())
    }

    fn reject_unless_active_lease(&self, expected: &str) -> Result<()> {
        let value: Option<String> = self
            .query_row(
                "SELECT value FROM meta WHERE key='active_bot_lease'",
                (),
                |row| Ok(row.get(0)?),
            )
            .optional()?;
        reject_active_bot_lease(value.as_deref(), expected)
    }

    /// The guild transaction already holds its write lock. A mismatch rolls back.
    fn reject_unless_writer_epoch(&self, generation: i64) -> Result<()> {
        let stored: Option<i64> = self
            .query_row(
                "SELECT generation FROM guild_writer_epoch WHERE singleton=1",
                (),
                |row| Ok(row.get(0)?),
            )
            .optional()?;
        if stored != Some(generation) {
            return Err(ActiveBotLeaseRejected.into());
        }
        Ok(())
    }

    pub fn commit(mut self) -> Result<()> {
        anyhow::ensure!(
            !self.capacity_poisoned.load(Ordering::Acquire),
            "database growth exceeded the reserved bound; transaction is poisoned"
        );
        if let Some(receipt) = self.mutation_receipt.as_deref() {
            let limit = self
                .capacity_reservation
                .lock()
                .map_err(|_| anyhow::anyhow!("capacity reservation lock poisoned"))?
                .as_ref()
                .map(|r| r.limit_bytes)
                .filter(|n| *n != 0)
                .unwrap_or(self.capacity_limits.growth);
            self.reserve_growth_limit(RECEIPT_GROWTH_ALLOWANCE, limit)?;
            self.execute_unchecked(
                "UPDATE mutation_receipts SET created_at=unixepoch() WHERE receipt_id=?1",
                params![receipt],
            )?;
            self.capacity_dirty.store(true, Ordering::Release);
        }
        if let Some(expected) = self.expected_lease_owner.as_deref() {
            self.reject_unless_active_lease(expected)?;
        }
        if self.allows_write
            && let Some(instance) = self.expected_launcher.clone()
        {
            self.renew_launcher_fence(&instance)?;
        }
        if let Some(probe) = self.commit_lease_probe.clone() {
            return probe(&mut || self.commit_after_lease_probe());
        }
        self.commit_after_lease_probe()
    }

    /// Capacity check and `COMMIT`. A guild lease probe has already released
    /// its core snapshot. The guild writer lock keeps the checked epoch unchanged
    /// through COMMIT; a later core lease change does not revoke this transaction.
    fn commit_after_lease_probe(&mut self) -> Result<()> {
        if let Some(generation) = self.expected_writer_generation {
            self.reject_unless_writer_epoch(generation)?;
        }
        if self.capacity_dirty.load(Ordering::Acquire) {
            let actual = crate::capacity::sample(&*self)?;
            let expected = self
                .capacity_reservation
                .lock()
                .map_err(|_| anyhow::anyhow!("capacity reservation lock poisoned"))?
                .ok_or_else(|| anyhow::anyhow!("capacity reservation missing"))?;
            anyhow::ensure!(
                actual.page_size == expected.page_size,
                "database page size changed during transaction"
            );
            if actual.allocated_bytes
                > expected
                    .initial_bytes
                    .saturating_add(expected.reserved_bytes)
            {
                self.runtime.growth_paused.store(true, Ordering::Release);
                anyhow::bail!(
                    "database growth exceeded the reserved bound; database growth is paused"
                );
            }
            crate::capacity::admit_to(actual.allocated_bytes, 0, expected.limit_bytes)?;
            if expected.limit_bytes == self.capacity_limits.reclaim {
                anyhow::ensure!(
                    actual.allocated_bytes <= expected.start_bytes.max(self.capacity_limits.growth),
                    "maintenance transaction increased allocation above growth stop"
                );
            }
        }
        let mut result = self
            .runtime
            .block_on_database("commit transaction", self.inner().execute_batch("COMMIT"));
        #[cfg(debug_assertions)]
        if result.is_ok() && TEST_DROP_COMMIT_RESPONSE.swap(false, Ordering::SeqCst) {
            result = Err(database_operation_timed_out(
                "commit transaction",
                self.runtime
                    .operation_timeout
                    .unwrap_or(Duration::from_secs(1)),
            ));
        }
        match result {
            Ok(_) => {
                self.connection_config_version.fetch_max(
                    self.pending_config_version.load(Ordering::Acquire),
                    Ordering::SeqCst,
                );
                drop_transaction_without_runtime(
                    self.inner.take().expect("transaction already completed"),
                );
                Ok(())
            }
            Err(commit_error) if self.runtime.is_remote() => {
                // Cleanup only: a successful ROLLBACK response cannot prove a
                // preceding COMMIT was not accepted when its response was
                // lost. Callers must reconcile every remote commit error as
                // an indeterminate outcome. An interactive timeout already
                // closed the stream; ROLLBACK on it waits out the client
                // deadline and is not cleanup. Our own deadline is different:
                // the baton may still be live, and the writer lock stays until
                // the provider steals it unless we roll back promptly.
                self.note_remote_stream_loss(&commit_error);
                if self.stream_exhausted.load(Ordering::Acquire) {
                    self.connection_tainted.store(true, Ordering::Release);
                } else if self.remote_write_needs_prompt_release() {
                    let _rollback = self.release_remote_write_lock();
                    self.connection_tainted.store(true, Ordering::Release);
                } else {
                    let _rollback = self.runtime.block_on_database_cleanup(
                        "rollback after failed commit",
                        self.inner().execute_batch("ROLLBACK"),
                    );
                }
                let inner = self.inner.take().expect("transaction already completed");
                drop_transaction_without_runtime(inner);
                let Some(receipt) = self.mutation_receipt.as_deref() else {
                    return Err(DatabaseCommitIndeterminate(commit_error).into());
                };
                match self.reconcile_mutation_receipt(receipt) {
                    Ok(true) => {
                        self.connection_config_version.fetch_max(
                            self.pending_config_version.load(Ordering::Acquire),
                            Ordering::SeqCst,
                        );
                        Ok(())
                    }
                    Ok(false) => Err(commit_error),
                    Err(reconcile_error) => {
                        self.connection_config_version.fetch_max(
                            self.pending_config_version.load(Ordering::Acquire),
                            Ordering::SeqCst,
                        );
                        Err(DatabaseCommitIndeterminate(commit_error.context(format!(
                            "authoritative receipt reconciliation failed: {reconcile_error:#}"
                        )))
                        .into())
                    }
                }
            }
            Err(error) => Err(error),
        }
    }

    /// Ends a primary-routed read snapshot without using COMMIT semantics.
    /// A lost response cannot make a read's outcome indeterminate.
    pub fn finish_read(mut self) -> Result<()> {
        if self.stream_exhausted.load(Ordering::Acquire) {
            // The provider already closed this stream. ROLLBACK would wait out
            // the client deadline and cannot change the rows already collected.
            self.connection_tainted.store(true, Ordering::Release);
            let inner = self.inner.take().expect("transaction already completed");
            drop_transaction_without_runtime(inner);
            return Ok(());
        }
        let result = if let Some(result) = self.budgeted_read_rollback("finish read transaction") {
            result
        } else if self.remote_write_needs_prompt_release() {
            self.release_remote_write_lock()
        } else {
            self.runtime
                .block_on_database_cleanup(
                    "finish read transaction",
                    self.inner().execute_batch("ROLLBACK"),
                )
                .map(|_| ())
        };
        if let Err(error) = &result {
            self.note_remote_stream_loss(error);
        }
        let inner = self.inner.take().expect("transaction already completed");
        drop_transaction_without_runtime(inner);
        match result {
            Ok(_) => Ok(()),
            // The rows were already collected. The provider closing the stream
            // (TRANSACTION_TIMEOUT), rejecting ROLLBACK because it already
            // dropped the transaction, or our own rollback deadline does not
            // make that snapshot indeterminate. A cancelled ROLLBACK can leave
            // the server transaction open, so the stream is not reused.
            Err(error)
                if remote_read_rollback_is_complete(&error)
                    || is_database_operation_timeout(&error) =>
            {
                if self.runtime.is_remote() {
                    self.connection_tainted.store(true, Ordering::Release);
                }
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn inner(&self) -> &libsql::Transaction {
        self.inner.as_ref().expect("transaction already completed")
    }

    fn reconcile_mutation_receipt(&self, receipt: &str) -> Result<bool> {
        let connection = self.database.connect()?;
        let mut transaction = Some(self.runtime.complete_or_release_open(
            "begin commit reconciliation",
            connection.transaction_with_behavior(libsql::TransactionBehavior::Immediate),
        )?);
        let query = self
            .runtime
            .block_on_database("query commit receipt", async {
                let mut rows = transaction
                    .as_ref()
                    .expect("transaction active")
                    .query(
                        "SELECT 1 FROM mutation_receipts WHERE receipt_id = ?1",
                        libsql::params![receipt],
                    )
                    .await?;
                Ok::<_, libsql::Error>(rows.next().await?.is_some())
            });
        // The primary snapshot itself establishes the original outcome. Do
        // not make receipt consumption another mutation whose COMMIT could in
        // turn be ambiguous; normal writes prune old receipts after 10 minutes.
        let cleanup = self.runtime.block_on_database(
            "finish commit reconciliation",
            transaction
                .as_ref()
                .expect("transaction active")
                .execute_batch("ROLLBACK"),
        );
        drop_transaction_without_runtime(transaction.take().expect("transaction active"));
        drop(connection);
        match (query, cleanup) {
            // Once the primary receipt query answered, the original outcome
            // is established. Cleanup failure cannot change that snapshot.
            (Ok(found), _) => Ok(found),
            (Err(error), _) => Err(error),
        }
    }

    /// Admit this transaction's mutation receipt without a separate remote round trip.
    ///
    /// Local connections keep the existing restore-fence read. Remote connections
    /// reserve receipt growth and return SQL the caller must run in this same
    /// transaction before any other mutation. The first string is the receipt
    /// insert. The second is receipt upkeep, which must run after the caller
    /// has checked `changes()` so a zero-row insert still means the restore
    /// fence refused the write. `meta_table_present` selects the fenced insert;
    /// a missing `meta` table uses the unfenced insert. A rollback undoes both.
    pub(crate) fn stage_mutation_receipt(
        &mut self,
        meta_table_present: bool,
    ) -> Result<Option<(String, String)>> {
        if !self.runtime.is_remote() {
            self.ensure_mutation_receipt()?;
            return Ok(None);
        }
        if self.mutation_receipt.is_some() {
            return Ok(None);
        }
        anyhow::ensure!(
            self.allows_write,
            "read snapshot cannot take the write lock"
        );
        self.reserve_growth_limit(RECEIPT_GROWTH_ALLOWANCE, self.capacity_limits.growth)?;
        let receipt = format!(
            "{:032x}{:032x}",
            rand::random::<u128>(),
            rand::random::<u128>()
        );
        let quoted = format!("'{}'", receipt.replace('\'', "''"));
        let admit = if meta_table_present {
            format!(
                "INSERT INTO mutation_receipts (receipt_id, created_at) SELECT {quoted}, unixepoch() WHERE COALESCE((SELECT value FROM meta WHERE key='restore_fence'), '0') != '1';"
            )
        } else {
            format!(
                "INSERT INTO mutation_receipts (receipt_id, created_at) VALUES ({quoted}, unixepoch());"
            )
        };
        let prune = if RECEIPT_PRUNE_COUNTER.fetch_add(1, Ordering::Relaxed) & 255 == 0 {
            "DELETE FROM mutation_receipts WHERE receipt_id IN (
                     SELECT receipt_id FROM mutation_receipts
                     WHERE created_at < unixepoch() - 600
                     ORDER BY created_at LIMIT 1000
                 );"
        } else {
            ""
        };
        self.mutation_receipt = Some(receipt);
        self.write_prepared.store(true, Ordering::Release);
        Ok(Some((admit, prune.to_string())))
    }

    /// Drop commit-time reads this transaction has already fenced.
    ///
    /// `lease` removes the core lease probe. `epoch` removes the guild epoch
    /// query. The coverage-gap mark drops the epoch query and installs one
    /// lease read in its place.
    pub(crate) fn suppress_commit_writer_recheck(&mut self, lease: bool, epoch: bool) {
        if lease {
            self.commit_lease_probe = None;
        }
        if epoch {
            self.expected_writer_generation = None;
        }
    }

    pub(crate) fn set_commit_lease_probe(&mut self, probe: Option<CommitLeaseProbe>) {
        self.commit_lease_probe = probe;
    }
}

#[cfg(debug_assertions)]
static TEST_DROP_COMMIT_RESPONSE: AtomicBool = AtomicBool::new(false);

/// Debug-build fault injection used by the remote integration test. The next
/// successful COMMIT is treated as though its response was lost.
#[cfg(debug_assertions)]
pub fn test_drop_next_commit_response() {
    TEST_DROP_COMMIT_RESPONSE.store(true, Ordering::SeqCst);
}

impl Drop for Transaction {
    fn drop(&mut self) {
        let Some(_) = self.inner.as_ref() else {
            return;
        };
        if self.stream_exhausted.load(Ordering::Acquire) {
            // The provider already ended the transaction. Another ROLLBACK on
            // this stream fails and can block on the client deadline. Discard
            // the wrapper; the connection is unusable and the next open builds
            // a new one.
            let inner = self.inner.take().expect("transaction already completed");
            if self.runtime.is_remote() {
                drop_transaction_without_runtime(inner);
            } else {
                std::mem::forget(inner);
            }
            self.connection_tainted.store(true, Ordering::Release);
            return;
        }
        if let Some(released) = self.budgeted_read_rollback("rollback transaction") {
            let inner = self.inner.take().expect("transaction already completed");
            drop_transaction_without_runtime(inner);
            if released.is_err() {
                self.connection_tainted.store(true, Ordering::Release);
            }
            return;
        }
        if self.remote_write_needs_prompt_release() {
            // Our deadline cancelled the in-flight call. The baton can still
            // name the server transaction. Release it on a fresh short
            // deadline, then drop the wrapper where no runtime is current so
            // libSQL does not spawn a second unbounded rollback.
            let released = self.release_remote_write_lock();
            let inner = self.inner.take().expect("transaction already completed");
            drop_transaction_without_runtime(inner);
            if released.is_err() {
                self.connection_tainted.store(true, Ordering::Release);
            }
            return;
        }
        // Use a borrowed SQL rollback instead of libsql::Transaction::rollback,
        // whose consuming future drops the transaction inside Tokio. libsql's
        // Drop callback would then spawn a second, unbounded best-effort
        // rollback. Once this bounded request ends, discard the libsql wrapper
        // where no Tokio runtime is current so that callback cannot enqueue
        // work outside the pool/timeout boundary.
        let result = self.runtime.block_on_database_cleanup(
            "rollback transaction",
            self.inner().execute_batch("ROLLBACK"),
        );
        let inner = self.inner.take().expect("transaction already completed");
        match result {
            Ok(_) => drop_transaction_without_runtime(inner),
            Err(error) => {
                // The local libSQL transaction destructor retries rollback with
                // `unwrap()`, while the remote destructor may spawn unbounded
                // best-effort cleanup when dropped inside Tokio. Drop remote
                // wrappers on a plain thread; for the exceptional local
                // rollback-I/O failure, retaining the wrapper is safer than a
                // process-aborting destructor panic. The owning connection is
                // marked unusable either way.
                if self.runtime.is_remote() {
                    drop_transaction_without_runtime(inner);
                } else {
                    std::mem::forget(inner);
                }
                self.connection_tainted.store(true, Ordering::Release);
                if self.runtime.is_remote() && is_remote_interactive_timeout(&error) {
                    tracing::debug!(%error, "discarded timed-out remote transaction stream");
                } else {
                    tracing::warn!(%error, "failed to roll back uncommitted database transaction");
                }
            }
        }
    }
}

pub(crate) fn drop_pooled_connection(connection: Connection) {
    if connection.runtime.is_remote() {
        let runtime = connection.runtime.clone();
        runtime.spawn_drop(connection);
        return;
    }
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

fn drop_transaction_without_runtime(inner: libsql::Transaction) {
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|scope| {
            scope
                .spawn(move || drop(inner))
                .join()
                .expect("database transaction drop thread panicked")
        });
    } else {
        drop(inner);
    }
}

impl SqlExecutor for Transaction {
    fn execute(&self, sql: &str, params: impl IntoParams + Send) -> Result<usize> {
        require_read_sql(sql)?;
        self.execute_unchecked(sql, params)
    }

    fn execute_bounded(
        &self,
        sql: &str,
        params: impl IntoParams + Send,
        max_growth_bytes: u64,
    ) -> Result<usize> {
        require_mutation_sql(sql)?;
        anyhow::ensure!(
            self.write_prepared.load(Ordering::Acquire),
            "write transaction has no mutation receipt or restore fence check"
        );
        self.reserve_growth(max_growth_bytes)?;
        self.execute_unchecked(sql, params)
    }

    fn execute_reclaim_bounded(
        &self,
        sql: &str,
        params: impl IntoParams + Send,
        max_growth_bytes: u64,
    ) -> Result<usize> {
        require_reclaim_sql(sql)?;
        anyhow::ensure!(
            self.write_prepared.load(Ordering::Acquire),
            "write transaction has no mutation receipt or restore fence check"
        );
        let limit = self
            .capacity_reservation
            .lock()
            .map_err(|_| anyhow::anyhow!("capacity reservation lock poisoned"))?
            .as_ref()
            .map(|r| r.limit_bytes)
            .filter(|n| *n != 0)
            .unwrap_or(self.capacity_limits.growth);
        self.reserve_growth_limit(max_growth_bytes, limit)?;
        self.execute_unchecked(sql, params)
    }

    fn query_row_bounded<T, P, F>(
        &self,
        sql: &str,
        params: P,
        max_growth_bytes: u64,
        map: F,
    ) -> Result<T>
    where
        T: Send,
        P: IntoParams + Send,
        F: FnOnce(&Row) -> Result<T> + Send,
    {
        require_mutation_sql(sql)?;
        anyhow::ensure!(
            self.write_prepared.load(Ordering::Acquire),
            "write transaction has no mutation receipt or restore fence check"
        );
        self.reserve_growth(max_growth_bytes)?;
        self.query_row_unchecked(sql, params, map)
    }

    fn reserve_growth(&self, max_growth_bytes: u64) -> Result<()> {
        Transaction::reserve_growth(self, max_growth_bytes)
    }

    fn is_write_transaction(&self) -> bool {
        self.allows_write
    }

    fn capacity_page_size(&self) -> Result<u64> {
        self.capacity_page_size_cached()
    }

    fn query_row<T, P, F>(&self, sql: &str, params: P, map: F) -> Result<T>
    where
        T: Send,
        P: IntoParams + Send,
        F: FnOnce(&Row) -> Result<T> + Send,
    {
        require_read_sql(sql)?;
        self.query_row_unchecked(sql, params, map)
    }

    fn prepare(&self, sql: &str) -> Result<Statement> {
        require_read_sql(sql)?;
        let inner =
            self.map_stream(self.drive("transaction prepare", self.inner().prepare(sql)))?;
        let read_deadline = if self.write_prepared.load(Ordering::Acquire) {
            None
        } else {
            self.read_deadline
        };
        Ok(Statement {
            inner,
            runtime: self.runtime.clone(),
            read_deadline,
            client_deadline: Arc::clone(&self.client_deadline),
            stream_exhausted: Arc::clone(&self.stream_exhausted),
        })
    }

    fn record_config_version(&self, version: u64) {
        self.pending_config_version
            .fetch_max(version, Ordering::SeqCst);
    }

    fn schema_verification_slot(&self) -> Arc<Mutex<Option<SchemaVerification>>> {
        Arc::clone(&self.schema_verification)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn lease_cas_rejects_nonallowlisted_readonly_and_transactional_calls() {
        let mut connection = Connection::open(":memory:").unwrap();
        assert!(
            connection
                .execute_lease_cas("UPDATE meta SET value='bad'", (), Duration::from_secs(10))
                .unwrap_err()
                .to_string()
                .contains("lease CAS mutation refused")
        );
        assert!(
            connection.execute(ACTIVE_BOT_LEASE_CAS, ()).is_err(),
            "general execute must remain read-only"
        );
        connection.set_read_only(true);
        assert!(
            connection
                .execute_lease_cas(ACTIVE_BOT_LEASE_CAS, (), Duration::from_secs(10))
                .unwrap_err()
                .to_string()
                .contains("read-only")
        );
        connection.set_read_only(false);
        connection.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert!(
            connection
                .execute_lease_cas(ACTIVE_BOT_LEASE_CAS, (), Duration::from_secs(10))
                .unwrap_err()
                .to_string()
                .contains("autocommit")
        );
        connection.execute_batch("ROLLBACK").unwrap();
    }

    use super::*;

    #[test]
    fn manual_deadline_starts_before_the_operation_is_polled() {
        let timeout = Duration::from_millis(300);
        let runtime = DbRuntime::with_operation_timeout(Some(timeout)).unwrap();
        let clock = ManualClock::new();
        runtime.install_manual_clock(Some(Arc::clone(&clock)));
        let mut first_poll = true;
        let operation = std::future::poll_fn(|cx| {
            if first_poll {
                first_poll = false;
                // Reproduce the test thread advancing time after HTTP work
                // starts but before the select polls its deadline branch.
                clock.advance(timeout);
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            } else {
                std::task::Poll::Ready(Ok::<_, anyhow::Error>(()))
            }
        });
        let error = runtime
            .block_on_database("delayed operation", operation)
            .expect_err("the deadline must expire on the first poll");
        assert!(is_database_operation_timeout(&error), "{error:#}");
    }

    #[test]
    fn read_classifier_refuses_explained_side_effects() {
        for sql in [
            "EXPLAIN PRAGMA writable_schema=ON",
            "EXPLAIN QUERY PLAN PRAGMA max_page_count=1",
            "EXPLAIN CREATE TABLE surprise(id INTEGER)",
            "PRAGMA writable_schema=ON",
            "WITH row AS (SELECT 1) DELETE FROM meta WHERE key='x'",
        ] {
            assert!(require_read_sql(sql).is_err(), "accepted {sql}");
        }
        for sql in [
            "EXPLAIN SELECT * FROM meta",
            "EXPLAIN QUERY PLAN DELETE FROM meta WHERE key='x'",
            "PRAGMA page_count",
        ] {
            assert!(require_read_sql(sql).is_ok(), "rejected {sql}");
        }
    }

    #[test]
    fn bounded_batch_rejects_unreviewed_pragmas() {
        for sql in [
            "PRAGMA max_page_count=1",
            "PRAGMA writable_schema=ON",
            "PRAGMA page_size=65536",
            "PRAGMA foreign_keys=OFF",
            "PRAGMA user_version=1; PRAGMA max_page_count=1",
        ] {
            assert!(batch_kind(sql).is_err(), "accepted {sql}");
        }
        assert!(batch_kind("PRAGMA user_version=1").unwrap());
    }

    #[test]
    fn understated_indexed_insert_rolls_back_at_precommit() {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_capacity_under_{:032x}.db",
            rand::random::<u128>()
        ));
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch_bounded("CREATE TABLE items(id INTEGER PRIMARY KEY, body BLOB NOT NULL); CREATE INDEX items_body ON items(body)", 2_000_000).unwrap();
        let oversized = vec![7_u8; 80_000];
        let error = conn
            .execute_bounded("INSERT INTO items(body) VALUES(?1)", params![oversized], 0)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("growth exceeded the reserved bound"),
            "{error:#}"
        );
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM items", (), |r| Ok(r.get(0)?))
            .unwrap();
        assert_eq!(count, 0);
        let denied = conn
            .execute_bounded(
                "INSERT INTO items(body) VALUES(?1)",
                params![vec![1_u8; 4]],
                4096,
            )
            .unwrap_err();
        assert!(
            denied.to_string().contains("database growth paused"),
            "{denied:#}"
        );
        let replacement =
            Connection::from_database(Arc::clone(&conn.database), conn.runtime.clone()).unwrap();
        let denied = replacement
            .execute_bounded(
                "INSERT INTO items(body) VALUES(?1)",
                params![vec![1_u8; 4]],
                4096,
            )
            .unwrap_err();
        assert!(
            denied.to_string().contains("database growth paused"),
            "{denied:#}"
        );
        assert_eq!(
            replacement
                .query_row("SELECT COUNT(*) FROM items", (), |r| Ok(r.get::<i64>(0)?))
                .unwrap(),
            0
        );
    }

    #[test]
    fn detected_rebase_understatement_poisoned_before_future_admission() {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_capacity_rebase_{:032x}.db",
            rand::random::<u128>()
        ));
        let mut conn = Connection::open(&path).unwrap();
        conn.execute_batch_bounded("CREATE TABLE items(id INTEGER PRIMARY KEY, body BLOB NOT NULL); CREATE INDEX items_body ON items(body)", 2_000_000).unwrap();
        let initial = crate::capacity::sample(&conn).unwrap().allocated_bytes;
        conn.set_test_capacity_limits(initial + 4096, initial + 8192);
        let tx = conn.transaction_immediate().unwrap();
        tx.execute_bounded(
            "INSERT INTO items(body) VALUES(?1)",
            params![vec![8_u8; 80_000]],
            0,
        )
        .unwrap();
        let error = tx.reserve_growth(8192).unwrap_err();
        assert!(
            error.to_string().contains("transaction is poisoned"),
            "{error:#}"
        );
        assert!(
            tx.reserve_growth(1)
                .unwrap_err()
                .to_string()
                .contains("transaction is poisoned")
        );
        assert!(
            tx.commit()
                .unwrap_err()
                .to_string()
                .contains("transaction is poisoned")
        );
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM items", (), |r| Ok(r.get(0)?))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn over_growth_stop_rejects_writes_but_allows_bounded_reclaim() {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_capacity_reclaim_{:032x}.db",
            rand::random::<u128>()
        ));
        let mut conn = Connection::open(&path).unwrap();
        conn.execute_batch_bounded("CREATE TABLE meta(key TEXT PRIMARY KEY, value BLOB); INSERT INTO meta VALUES('reclaim',zeroblob(80000))", 2_000_000).unwrap();
        let current = crate::capacity::sample(&conn).unwrap().allocated_bytes;
        conn.set_test_capacity_limits(current - 4096, current + 65_536);
        let denied = conn
            .execute_bounded("INSERT INTO meta VALUES('refused','small')", (), 4096)
            .unwrap_err();
        assert!(
            denied.to_string().contains("capacity budget exhausted"),
            "{denied:#}"
        );
        let still_present: i64 = conn
            .query_row("SELECT COUNT(*) FROM meta WHERE key='reclaim'", (), |r| {
                Ok(r.get(0)?)
            })
            .unwrap();
        assert_eq!(still_present, 1);
        conn.execute_reclaim_bounded("DELETE FROM meta WHERE key='reclaim'", (), 4096)
            .unwrap();
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM meta", (), |r| Ok(r.get(0)?))
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[test]
    fn independent_writers_cannot_both_reserve_the_last_growth_budget() {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_capacity_race_{:032x}.db",
            rand::random::<u128>()
        ));
        let setup = Connection::open(&path).unwrap();
        setup
            .execute_batch_bounded(
                "CREATE TABLE items(id INTEGER PRIMARY KEY, body BLOB NOT NULL)",
                2_000_000,
            )
            .unwrap();
        let initial = crate::capacity::sample(&setup).unwrap().allocated_bytes;
        drop(setup);
        let growth = initial + 131_072;
        let barrier = std::sync::Barrier::new(2);
        let outcomes = std::thread::scope(|scope| {
            let tasks: Vec<_> = (0..2)
                .map(|_| {
                    let path = path.clone();
                    let barrier = &barrier;
                    scope.spawn(move || {
                        let mut conn = Connection::open(&path).unwrap();
                        conn.set_test_capacity_limits(growth, growth + 131_072);
                        conn.execute_batch("PRAGMA busy_timeout=5000").unwrap();
                        barrier.wait();
                        conn.execute_bounded(
                            "INSERT INTO items(body) VALUES(?1)",
                            params![vec![9_u8; 65_536]],
                            131_072,
                        )
                    })
                })
                .collect();
            tasks
                .into_iter()
                .map(|task| task.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            outcomes.iter().filter(|outcome| outcome.is_ok()).count(),
            1,
            "{outcomes:?}"
        );
        assert!(
            outcomes.iter().any(|outcome| outcome
                .as_ref()
                .is_err_and(|error| error.to_string().contains("capacity budget exhausted"))),
            "{outcomes:?}"
        );
        let conn = Connection::open(&path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM items", (), |r| Ok(r.get(0)?))
            .unwrap();
        assert_eq!(count, 1);
        assert!(crate::capacity::sample(&conn).unwrap().allocated_bytes <= growth);
    }

    #[test]
    fn receipt_insert_statement_checks_restore_fence_atomically() {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_receipt_statement_{:032x}.db",
            rand::random::<u128>()
        ));
        // Exercise the remote deadline/error-context path against real SQLite
        // SQL semantics, including guild stores that have no core meta table.
        let runtime = DbRuntime::with_operation_timeout(Some(Duration::from_secs(3))).unwrap();
        let database = runtime
            .block_on(libsql::Builder::new_local(path.clone()).build())
            .unwrap();
        let connection = Connection::from_database(Arc::new(database), runtime.clone()).unwrap();
        connection
            .execute_batch_bounded(
                "CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
                 CREATE TABLE mutation_receipts(
                   receipt_id TEXT PRIMARY KEY,
                   created_at INTEGER NOT NULL
                 );
                 INSERT INTO meta(key,value) VALUES('restore_fence','0');",
                64_000_000,
            )
            .unwrap();

        let tx = connection.transaction_primary_read().unwrap();
        let inserted = tx
            .query_row_unchecked(INSERT_MUTATION_RECEIPT_SQL, params!["receipt"], |row| {
                Ok(row.get::<String>(0)?)
            })
            .unwrap();
        assert_eq!(inserted, "receipt");
        tx.commit().unwrap();

        connection
            .execute_batch_bounded(
                "UPDATE meta SET value='1' WHERE key='restore_fence'",
                4_000_000,
            )
            .unwrap();
        let tx = connection.transaction_primary_read().unwrap();
        let error = tx
            .query_row_unchecked(INSERT_MUTATION_RECEIPT_SQL, params!["blocked"], |row| {
                Ok(row.get::<String>(0)?)
            })
            .unwrap_err();
        assert!(error.downcast_ref::<QueryReturnedNoRows>().is_some());
        tx.finish_read().unwrap();

        connection
            .execute_batch_bounded("DROP TABLE meta", 4_000_000)
            .unwrap();
        let mut tx = connection.transaction_primary_read().unwrap();
        assert_eq!(
            tx.insert_remote_mutation_receipt("guild-receipt").unwrap(),
            "guild-receipt"
        );
        tx.commit().unwrap();

        drop(connection);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[test]
    fn connection_execute_batch_allows_transaction_control_only() {
        let path = std::env::temp_dir().join(format!(
            "db_compat_tx_batch_{:032x}",
            rand::random::<u128>()
        ));
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("BEGIN EXCLUSIVE").unwrap();
        connection.execute_batch("COMMIT").unwrap();
        assert!(
            connection
                .execute_batch("CREATE TABLE refused(a INTEGER)")
                .is_err()
        );
        drop(connection);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[test]
    fn remote_database_call_has_a_deadline() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (release_tx, release_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (_socket, _) = listener.accept().unwrap();
            let _ = release_rx.recv();
        });

        let runtime = DbRuntime::with_operation_timeout(Some(Duration::from_millis(100))).unwrap();
        let connector = crate::remote_https::https_connector();
        let database = runtime
            .block_on(
                libsql::Builder::new_remote(format!("http://{address}"), "test-token".into())
                    .connector(connector)
                    .build(),
            )
            .unwrap();
        let connection = database.connect().unwrap();
        let error = runtime
            .block_on_database("execute", connection.execute("SELECT 1", ()))
            .expect_err("blackholed remote request must time out");
        assert!(
            is_database_operation_timeout(&error),
            "unexpected: {error:#}"
        );
        assert!(error.to_string().contains("100ms"), "unexpected: {error:#}");

        release_tx.send(()).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn capture_reads_allow_writers_and_stale_promotion_cannot_overwrite_them() {
        let path = std::env::temp_dir().join(format!(
            "capture_upgrade_{:032x}.db",
            rand::random::<u128>()
        ));
        let reader = Connection::open(&path).unwrap();
        reader.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        reader
            .execute_batch_bounded(
                "CREATE TABLE counter(value INTEGER); INSERT INTO counter VALUES(1)",
                1_000_000,
            )
            .unwrap();
        let writer = Connection::open(&path).unwrap();
        let mut capture = reader.transaction_capture().unwrap();
        let before: i64 = capture
            .query_row("SELECT value FROM counter", (), |r| Ok(r.get(0)?))
            .unwrap();
        assert_eq!(before, 1);
        // Another writer completes while the capture read transaction is open.
        writer
            .execute_bounded("UPDATE counter SET value=2", (), 1_000_000)
            .unwrap();
        capture.ensure_mutation_receipt().unwrap();
        let error = capture
            .execute_bounded(
                "UPDATE counter SET value=?1",
                params![before + 10],
                1_000_000,
            )
            .unwrap_err();
        assert!(error.chain().any(|e| matches!(e.downcast_ref::<libsql::Error>(), Some(libsql::Error::SqliteFailure(code, _)) if code & 0xff == 5)), "{error:#}");
        drop(capture);
        let retry = reader.transaction_immediate().unwrap();
        let fresh: i64 = retry
            .query_row("SELECT value FROM counter", (), |r| Ok(r.get(0)?))
            .unwrap();
        assert_eq!(fresh, 2);
        retry
            .execute_bounded(
                "UPDATE counter SET value=?1",
                params![fresh + 10],
                1_000_000,
            )
            .unwrap();
        retry.commit().unwrap();
        assert_eq!(
            reader
                .query_row("SELECT value FROM counter", (), |r| Ok(r.get::<i64>(0)?))
                .unwrap(),
            12
        );
        drop(reader);
        drop(writer);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[test]
    fn primary_snapshot_does_not_wait_on_a_held_write_lock() {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_snapshot_lock_{:032x}.db",
            rand::random::<u128>()
        ));
        let writer = Connection::open(&path).unwrap();
        writer
            .execute_batch("PRAGMA busy_timeout=5000; PRAGMA journal_mode=WAL;")
            .unwrap();
        let held = writer.transaction_immediate().unwrap();
        let reader = Connection::open(&path).unwrap();
        reader.execute_batch("PRAGMA busy_timeout=5000;").unwrap();
        let started = std::time::Instant::now();
        let snapshot = reader.transaction_primary_snapshot().unwrap();
        let value: i64 = snapshot
            .query_row("SELECT 1", (), |row| Ok(row.get(0)?))
            .unwrap();
        assert!(!SqlExecutor::is_write_transaction(&snapshot));
        snapshot.finish_read().unwrap();
        assert_eq!(value, 1);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "deferred primary snapshot waited on the writer lock: {:?}",
            started.elapsed()
        );
        let consistent = reader.transaction_consistent_read().unwrap();
        let consistent_value: i64 = consistent
            .query_row("SELECT 1", (), |row| Ok(row.get(0)?))
            .unwrap();
        assert_eq!(consistent_value, 1);
        assert!(!SqlExecutor::is_write_transaction(&consistent));
        consistent.finish_read().unwrap();
        let mut refused = reader.transaction_primary_snapshot().unwrap();
        assert!(
            refused
                .ensure_mutation_receipt()
                .unwrap_err()
                .to_string()
                .contains("write lock")
        );
        refused.finish_read().unwrap();
        held.commit().unwrap();
        drop(reader);
        drop(writer);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[test]
    fn hrana_transaction_timeout_is_a_retryable_interactive_timeout() {
        let timeout = anyhow::anyhow!(
            "Hrana: `cursor error`: `error at step 0: (error code: TRANSACTION_TIMEOUT) `Transaction timed out``"
        );
        assert!(is_remote_interactive_timeout(&timeout));
        assert!(!is_database_operation_timeout(&timeout));
        let rollback = anyhow::anyhow!(
            "Hrana: `api error: `SQLite error: cannot rollback - no transaction is active``"
        );
        assert!(is_remote_interactive_timeout(&rollback));
        assert!(remote_read_rollback_is_complete(&rollback));
        assert!(!is_remote_interactive_timeout(&anyhow::anyhow!(
            "database is locked"
        )));
        let client_deadline =
            database_operation_timed_out("commit transaction", Duration::from_secs(5));
        assert!(
            client_deadline
                .to_string()
                .to_ascii_lowercase()
                .contains("transaction timed out"),
            "the client deadline text overlaps the provider phrase: {client_deadline}"
        );
        assert!(is_database_operation_timeout(&client_deadline));
        assert!(
            !is_remote_interactive_timeout(&client_deadline),
            "our deadline must not be treated as a dead stream"
        );
        let abandoned = database_caller_abandoned("commit transaction");
        assert!(is_database_operation_timeout(&abandoned));
        assert!(
            !is_remote_interactive_timeout(&abandoned),
            "a caller leaving must not be treated as a dead stream"
        );
    }
}
