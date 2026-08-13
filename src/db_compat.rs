//! Small synchronous facade over libSQL's async connection API.
//!
//! The data layer predates remote database support and intentionally performs
//! each logical operation inside a blocking closure. Keeping that boundary
//! lets local SQLite and Bunny Database share the exact same SQL paths while
//! the libSQL runtime handles either transport underneath.

use anyhow::{Context as _, Result};
use libsql::params::IntoParams;
use std::fmt::Display;
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};

pub use libsql::{Row, Value, params, params_from_iter};

#[derive(Debug, thiserror::Error)]
#[error("query returned no rows")]
struct QueryReturnedNoRows;

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
}

impl DbRuntime {
    pub fn new() -> Result<Self> {
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
}

pub struct Connection {
    inner: libsql::Connection,
    runtime: DbRuntime,
}

pub trait SqlExecutor {
    fn execute(&self, sql: &str, params: impl IntoParams + Send) -> Result<usize>;

    fn query_row<T, P, F>(&self, sql: &str, params: P, map: F) -> Result<T>
    where
        T: Send,
        P: IntoParams + Send,
        F: FnOnce(&Row) -> Result<T> + Send;

    fn prepare(&self, sql: &str) -> Result<Statement>;

    fn is_autocommit(&self) -> bool;
}

impl Connection {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let runtime = DbRuntime::new()?;
        let path = path.as_ref().to_path_buf();
        let database = runtime.block_on(libsql::Builder::new_local(path).build())?;
        let inner = database.connect()?;
        Ok(Self { inner, runtime })
    }

    pub fn from_database(database: &libsql::Database, runtime: DbRuntime) -> Result<Self> {
        Ok(Self {
            inner: database.connect()?,
            runtime,
        })
    }

    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        self.runtime.block_on(self.inner.execute_batch(sql))?;
        Ok(())
    }

    pub fn transaction(&self) -> Result<Transaction> {
        let inner = self.runtime.block_on(self.inner.transaction())?;
        Ok(Transaction {
            inner,
            runtime: self.runtime.clone(),
        })
    }

    pub fn pragma_update<T: Display>(
        &self,
        _schema: Option<&str>,
        pragma: &str,
        value: T,
    ) -> Result<()> {
        self.execute_batch(&format!("PRAGMA {pragma} = {value}"))
    }

    pub fn last_insert_rowid(&self) -> i64 {
        self.inner.last_insert_rowid()
    }

    pub fn is_autocommit(&self) -> bool {
        self.inner.is_autocommit()
    }
}

impl SqlExecutor for Connection {
    fn execute(&self, sql: &str, params: impl IntoParams + Send) -> Result<usize> {
        Ok(self.runtime.block_on(self.inner.execute(sql, params))? as usize)
    }

    fn query_row<T, P, F>(&self, sql: &str, params: P, map: F) -> Result<T>
    where
        T: Send,
        P: IntoParams + Send,
        F: FnOnce(&Row) -> Result<T> + Send,
    {
        self.runtime.block_on(async {
            let mut rows = self.inner.query(sql, params).await?;
            let row = rows.next().await?.ok_or(QueryReturnedNoRows)?;
            map(&row)
        })
    }

    fn prepare(&self, sql: &str) -> Result<Statement> {
        let inner = self.runtime.block_on(self.inner.prepare(sql))?;
        Ok(Statement {
            inner,
            runtime: self.runtime.clone(),
        })
    }

    fn is_autocommit(&self) -> bool {
        self.inner.is_autocommit()
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
        let mapped = self.runtime.block_on(async {
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
    inner: libsql::Transaction,
    runtime: DbRuntime,
}

impl Transaction {
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        self.runtime.block_on(self.inner.execute_batch(sql))?;
        Ok(())
    }

    pub fn pragma_update<T: Display>(
        &self,
        _schema: Option<&str>,
        pragma: &str,
        value: T,
    ) -> Result<()> {
        self.execute_batch(&format!("PRAGMA {pragma} = {value}"))
    }

    pub fn last_insert_rowid(&self) -> i64 {
        self.inner.last_insert_rowid()
    }

    pub fn commit(self) -> Result<()> {
        self.runtime.block_on(self.inner.commit())?;
        Ok(())
    }
}

impl SqlExecutor for Transaction {
    fn execute(&self, sql: &str, params: impl IntoParams + Send) -> Result<usize> {
        Ok(self.runtime.block_on(self.inner.execute(sql, params))? as usize)
    }

    fn query_row<T, P, F>(&self, sql: &str, params: P, map: F) -> Result<T>
    where
        T: Send,
        P: IntoParams + Send,
        F: FnOnce(&Row) -> Result<T> + Send,
    {
        self.runtime.block_on(async {
            let mut rows = self.inner.query(sql, params).await?;
            let row = rows.next().await?.ok_or(QueryReturnedNoRows)?;
            map(&row)
        })
    }

    fn prepare(&self, sql: &str) -> Result<Statement> {
        let inner = self.runtime.block_on(self.inner.prepare(sql))?;
        Ok(Statement {
            inner,
            runtime: self.runtime.clone(),
        })
    }

    fn is_autocommit(&self) -> bool {
        self.inner.is_autocommit()
    }
}
