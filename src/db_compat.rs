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
#[error("remote database commit outcome is indeterminate: {0}")]
pub struct DatabaseCommitIndeterminate(#[source] anyhow::Error);

pub(crate) fn is_database_operation_timeout(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<DatabaseOperationTimedOut>().is_some())
}

pub(crate) fn is_database_commit_indeterminate(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<DatabaseCommitIndeterminate>()
            .is_some()
    })
}

/// True when Bunny Database / Turso closed an interactive Hrana transaction.
///
/// This is distinct from the client-side [`DatabaseOperationTimedOut`] deadline.
/// The provider kills idle or long interactive transactions (often around five
/// seconds) and the next statement fails at cursor step 0 with
/// `TRANSACTION_TIMEOUT`. ROLLBACK after that then reports that no transaction
/// is active.
pub(crate) fn is_remote_interactive_timeout(error: &anyhow::Error) -> bool {
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

#[derive(Clone)]
pub struct DbRuntime {
    handle: tokio::runtime::Handle,
    _thread: Arc<RuntimeThread>,
    operation_timeout: Option<Duration>,
    // Shared by all connections for one physical Db pool. A discovered bound
    // violation pauses growth until that runtime is restarted and repaired.
    growth_paused: Arc<AtomicBool>,
}

impl DbRuntime {
    pub(crate) fn growth_paused(&self) -> bool {
        self.growth_paused.load(Ordering::Acquire)
    }
    pub fn new() -> Result<Self> {
        Self::with_operation_timeout(None)
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
        match self.operation_timeout {
            Some(timeout) => {
                match self.block_on(async move { tokio::time::timeout(timeout, future).await }) {
                    Ok(result) => result
                        .map_err(Into::into)
                        .with_context(|| format!("remote database {operation} failed")),
                    Err(_) => Err(database_operation_timed_out(operation, timeout)),
                }
            }
            None => self.block_on(future).map_err(Into::into),
        }
    }

    fn is_remote(&self) -> bool {
        self.operation_timeout.is_some()
    }
}

pub struct Connection {
    inner: libsql::Connection,
    database: Arc<libsql::Database>,
    runtime: DbRuntime,
    tainted: Arc<AtomicBool>,
    config_version: Arc<std::sync::atomic::AtomicU64>,
    capacity_limits: CapacityLimits,
}

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
        })
    }

    /// Start a pooled remote operation without the previous operation's
    /// autocommit cursor baton. The server may expire it while the lease is
    /// idle. libSQL reset clears local stream state without sending SQL or
    /// replaying a failed request; explicit transactions use separate streams.
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

    /// Starts a write transaction at the primary immediately. Remote libSQL
    /// can reject a deferred transaction when it must upgrade from a replica
    /// read to a primary write while another writer is active.
    pub fn transaction_immediate(&self) -> Result<Transaction> {
        let inner = self.runtime.block_on_database(
            "begin immediate transaction",
            self.inner
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate),
        )?;
        let mut transaction = self.wrap_transaction(inner, None);
        transaction.ensure_mutation_receipt()?;
        Ok(transaction)
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
        let inner = self.runtime.block_on_database(
            "begin primary read transaction",
            self.inner
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate),
        )?;
        Ok(self.wrap_transaction(inner, None))
    }

    fn wrap_transaction(
        &self,
        inner: libsql::Transaction,
        mutation_receipt: Option<String>,
    ) -> Transaction {
        Transaction {
            inner: Some(inner),
            database: Arc::clone(&self.database),
            runtime: self.runtime.clone(),
            connection_tainted: Arc::clone(&self.tainted),
            connection_config_version: Arc::clone(&self.config_version),
            pending_config_version: std::sync::atomic::AtomicU64::new(0),
            mutation_receipt,
            capacity_dirty: AtomicBool::new(false),
            capacity_poisoned: AtomicBool::new(false),
            write_prepared: AtomicBool::new(false),
            stream_exhausted: AtomicBool::new(false),
            capacity_reservation: Mutex::new(None),
            capacity_limits: self.capacity_limits,
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

    pub fn is_autocommit(&self) -> bool {
        self.inner.is_autocommit()
    }

    pub fn is_tainted(&self) -> bool {
        self.tainted.load(Ordering::Acquire)
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
        })
    }

    fn record_config_version(&self, version: u64) {
        self.config_version.fetch_max(version, Ordering::SeqCst);
    }
}

pub struct Statement {
    inner: libsql::Statement,
    runtime: DbRuntime,
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
        let mapped = self.runtime.block_on_database("query", async {
            let mut cursor = self.inner.query(params).await?;
            let mut rows = Vec::new();
            while let Some(row) = cursor.next().await? {
                rows.push(map(&row));
            }
            Ok::<_, anyhow::Error>(rows)
        })?;
        Ok(MappedRows(mapped.into_iter()))
    }
}

pub struct Transaction {
    inner: Option<libsql::Transaction>,
    database: Arc<libsql::Database>,
    runtime: DbRuntime,
    connection_tainted: Arc<AtomicBool>,
    connection_config_version: Arc<std::sync::atomic::AtomicU64>,
    pending_config_version: std::sync::atomic::AtomicU64,
    mutation_receipt: Option<String>,
    capacity_dirty: AtomicBool,
    capacity_poisoned: AtomicBool,
    write_prepared: AtomicBool,
    /// The provider already closed this Hrana stream. Drop must not send
    /// ROLLBACK on it; the connection is tainted and the next open uses a new one.
    stream_exhausted: AtomicBool,
    capacity_reservation: Mutex<Option<CapacityReservation>>,
    capacity_limits: CapacityLimits,
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
        if is_remote_interactive_timeout(error) {
            self.stream_exhausted.store(true, Ordering::Release);
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
            self.runtime
                .block_on_database(
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
        self.map_stream(
            self.runtime
                .block_on_database("transaction internal query", async {
                    let mut rows = self.inner().query(sql, params).await?;
                    let row = rows.next().await?.ok_or(QueryReturnedNoRows)?;
                    map(&row)
                }),
        )
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
        self.map_stream(
            self.runtime
                .block_on_database("transaction execute_batch", self.inner().execute_batch(sql)),
        )?;
        Ok(())
    }

    pub fn execute_batch_bounded(&self, sql: &str, max_growth_bytes: u64) -> Result<()> {
        anyhow::ensure!(batch_kind(sql)?, "bounded batch must contain a mutation");
        self.reserve_growth(max_growth_bytes)?;
        self.map_stream(
            self.runtime
                .block_on_database("transaction bounded batch", self.inner().execute_batch(sql)),
        )?;
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
        if self.capacity_dirty.load(Ordering::Acquire) {
            let actual = crate::capacity::sample(&self)?;
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
                // an indeterminate outcome.
                let _rollback = self.runtime.block_on_database(
                    "rollback after failed commit",
                    self.inner().execute_batch("ROLLBACK"),
                );
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
        let result = self.runtime.block_on_database(
            "finish read transaction",
            self.inner().execute_batch("ROLLBACK"),
        );
        let inner = self.inner.take().expect("transaction already completed");
        drop_transaction_without_runtime(inner);
        match result {
            Ok(_) => Ok(()),
            // The rows were already collected. The provider closing the stream
            // (TRANSACTION_TIMEOUT) or rejecting ROLLBACK because it already
            // dropped the transaction does not make that snapshot indeterminate.
            Err(error) if remote_read_rollback_is_complete(&error) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn inner(&self) -> &libsql::Transaction {
        self.inner.as_ref().expect("transaction already completed")
    }

    fn reconcile_mutation_receipt(&self, receipt: &str) -> Result<bool> {
        let connection = self.database.connect()?;
        let mut transaction = Some(self.runtime.block_on_database(
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
        // Use a borrowed SQL rollback instead of libsql::Transaction::rollback,
        // whose consuming future drops the transaction inside Tokio. libsql's
        // Drop callback would then spawn a second, unbounded best-effort
        // rollback. Once this bounded request ends, discard the libsql wrapper
        // where no Tokio runtime is current so that callback cannot enqueue
        // work outside the pool/timeout boundary.
        let result = self.runtime.block_on_database(
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
        true
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
        let inner = self
            .runtime
            .block_on_database("transaction prepare", self.inner().prepare(sql))?;
        Ok(Statement {
            inner,
            runtime: self.runtime.clone(),
        })
    }

    fn record_config_version(&self, version: u64) {
        self.pending_config_version
            .fetch_max(version, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
