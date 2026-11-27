//! Loopback Hrana checks for the pooled remote connection lifecycle.
//! Requests traverse libSQL's actual HTTP client and cursor implementation.

use super::super::*;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server, StatusCode};
use serde_json::{Value as Json, json};
use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Notify;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadRoute {
    Primary,
    ReplicaRoutable,
}

struct HranaState {
    next_baton: usize,
    latest_baton: Option<String>,
    expired: HashSet<String>,
    expire_next_cursor: bool,
    expired_requests: usize,
    fail_next_commit: bool,
    drop_next_commit_response: bool,
    fail_next_cas: bool,
    fail_next_meta_probe: bool,
    reset_next_request: bool,
    fail_next_request: Option<&'static str>,
    fail_next_status: Option<StatusCode>,
    commit_timeouts: usize,
    rollback_requests: usize,
    in_transaction: bool,
    begin_sql: Vec<String>,
    next_cursor_error: Option<(&'static str, &'static str)>,
    timeout_cursor_sql: Option<&'static str>,
    hang_next_commit: bool,
    hang_next_statement: bool,
    hang_next_begin: bool,
    stall_reads: &'static [ReadRoute],
    read_begins: usize,
    begin_released: bool,
    begin_release: Arc<Notify>,
    skip_immediate_lock: bool,
    hang_rollback: bool,
    write_locked: bool,
    closes: usize,
    commit_hung: bool,
    statement_hung: bool,
    begin_hung: bool,
    rollback_started: bool,
    progress: Arc<Notify>,
    // Optional real SQLite backing for configuration transaction/transport tests.
    backing: Option<(DbRuntime, libsql::Connection)>,
    provider: Option<Arc<ProviderModel>>,
    before_next_receipt: Option<Box<dyn FnOnce() + Send>>,
    client_delays: Option<ClientDelays>,
    requests: usize,
    statements: Vec<String>,
}

impl HranaState {
    fn new() -> Self {
        Self {
            next_baton: 0,
            latest_baton: None,
            expired: HashSet::new(),
            expire_next_cursor: false,
            expired_requests: 0,
            fail_next_commit: false,
            drop_next_commit_response: false,
            fail_next_cas: false,
            fail_next_meta_probe: false,
            reset_next_request: false,
            fail_next_request: None,
            fail_next_status: None,
            commit_timeouts: 0,
            rollback_requests: 0,
            in_transaction: false,
            begin_sql: Vec::new(),
            next_cursor_error: None,
            timeout_cursor_sql: None,
            hang_next_commit: false,
            hang_next_statement: false,
            hang_next_begin: false,
            stall_reads: &[],
            read_begins: 0,
            begin_released: false,
            begin_release: Arc::new(Notify::new()),
            skip_immediate_lock: false,
            hang_rollback: false,
            write_locked: false,
            closes: 0,
            commit_hung: false,
            statement_hung: false,
            begin_hung: false,
            rollback_started: false,
            progress: Arc::new(Notify::new()),
            backing: None,
            provider: None,
            before_next_receipt: None,
            client_delays: None,
            requests: 0,
            statements: Vec::new(),
        }
    }
}

struct ClientDelays {
    clock: Arc<crate::db_compat::ManualClock>,
    requests: Vec<(&'static str, Duration)>,
}

// A scaled provider: one core writer, per-request latency, and an interactive
// transaction lifetime. The actual statements still execute on real SQLite.
struct ProviderModel {
    writer: Arc<tokio::sync::Semaphore>,
    transactions:
        Mutex<std::collections::HashMap<String, (tokio::sync::OwnedSemaphorePermit, Duration)>>,
    timed_out: Mutex<HashSet<String>>,
    delay: Duration,
    limit: Duration,
    clock: ProviderClock,
    timeouts: AtomicUsize,
    committed_durations: Mutex<Vec<Duration>>,
    stall_owned: AtomicBool,
}

enum ProviderClock {
    Wall(std::time::Instant),
    Modeled(Arc<Mutex<Duration>>),
}

impl ProviderModel {
    fn new(delay: Duration, limit: Duration) -> Arc<Self> {
        Self::with_clock(delay, limit, ProviderClock::Wall(std::time::Instant::now()))
    }

    fn with_clock(delay: Duration, limit: Duration, clock: ProviderClock) -> Arc<Self> {
        Arc::new(Self {
            writer: Arc::new(tokio::sync::Semaphore::new(1)),
            transactions: Mutex::new(std::collections::HashMap::new()),
            timed_out: Mutex::new(HashSet::new()),
            delay,
            limit,
            clock,
            timeouts: AtomicUsize::new(0),
            committed_durations: Mutex::new(Vec::new()),
            stall_owned: AtomicBool::new(false),
        })
    }

    fn now(&self) -> Duration {
        match &self.clock {
            ProviderClock::Wall(started) => started.elapsed(),
            ProviderClock::Modeled(elapsed) => *elapsed.lock().unwrap(),
        }
    }

    async fn charge_request_latency(&self) {
        match &self.clock {
            ProviderClock::Wall(_) => tokio::time::sleep(self.delay).await,
            ProviderClock::Modeled(elapsed) => *elapsed.lock().unwrap() += self.delay,
        }
    }
}

async fn provider_response(
    path: &str,
    input: &Json,
    state: &Arc<Mutex<HranaState>>,
    model: &ProviderModel,
) -> Response<Body> {
    let baton = {
        let mut state = state.lock().unwrap();
        state.requests += 1;
        match input["baton"].as_str() {
            Some(baton) => baton.to_owned(),
            None => {
                state.next_baton += 1;
                format!("provider-baton-{}", state.next_baton)
            }
        }
    };
    let requests = if path == "/v3/pipeline" {
        input["requests"].as_array().unwrap().clone()
    } else {
        vec![input.clone()]
    };
    let first_sql = requests.first().map(statement_sql).unwrap_or_default();
    if let Some(delays) = &mut state.lock().unwrap().client_delays {
        let first = &requests[0];
        let sql = first["sql"]
            .as_str()
            .unwrap_or(&first_sql)
            .to_ascii_uppercase();
        if let Some(index) = delays
            .requests
            .iter()
            .position(|(prefix, _)| sql.starts_with(prefix))
        {
            let (_, delay) = delays.requests.remove(index);
            delays.clock.advance(delay);
            if let ProviderClock::Modeled(elapsed) = &model.clock {
                *elapsed.lock().unwrap() += delay;
            }
        }
    }
    if first_sql.starts_with("INSERT INTO MUTATION_RECEIPTS") {
        let before_receipt = state.lock().unwrap().before_next_receipt.take();
        if let Some(write) = before_receipt {
            tokio::task::spawn_blocking(write).await.unwrap();
        }
    }
    let owns_transaction = model.transactions.lock().unwrap().contains_key(&baton);
    let transaction_edge = first_sql.starts_with("BEGIN")
        || first_sql.starts_with("COMMIT")
        || first_sql.starts_with("ROLLBACK");
    if owns_transaction && !transaction_edge && model.stall_owned.swap(false, Ordering::SeqCst) {
        {
            let mut guard = state.lock().unwrap();
            guard.statement_hung = true;
            guard.progress.notify_waiters();
        }
        std::future::pending::<()>().await;
    }
    model.charge_request_latency().await;
    let mut responses = Vec::new();
    let mut closed = false;
    for request in &requests {
        let sql = statement_sql(request);
        if request["type"] == "close" {
            let owned = model.transactions.lock().unwrap().remove(&baton);
            if owned.is_some() {
                let mut state = state.lock().unwrap();
                let (runtime, conn) = state.backing.as_ref().unwrap();
                runtime.block_on(conn.execute("ROLLBACK", ())).unwrap();
                state.in_transaction = false;
                state.write_locked = false;
            }
            drop(owned);
            closed = true;
            responses.push(json!({"type":"ok","response":{"type":"close"}}));
            continue;
        }
        if request["type"] == "describe" {
            responses.push(json!({"type":"ok","response":{"type":"describe","result":{"params":[],"cols":[{"name":"value","decltype":"INTEGER"}],"is_explain":false,"is_readonly":true}}}));
            continue;
        }
        if sql.is_empty() {
            let owns = model.transactions.lock().unwrap().contains_key(&baton);
            responses.push(
                json!({"type":"ok","response":{"type":"get_autocommit","is_autocommit":!owns}}),
            );
            continue;
        }
        let expired = model
            .transactions
            .lock()
            .unwrap()
            .get(&baton)
            .is_some_and(|(_, start)| model.now() - *start > model.limit);
        if expired {
            let owned = model.transactions.lock().unwrap().remove(&baton);
            let mut state = state.lock().unwrap();
            let (runtime, conn) = state.backing.as_ref().unwrap();
            runtime.block_on(conn.execute("ROLLBACK", ())).unwrap();
            state.in_transaction = false;
            state.write_locked = false;
            model.timed_out.lock().unwrap().insert(baton.clone());
            model.timeouts.fetch_add(1, Ordering::SeqCst);
            drop(owned);
        }
        if model.timed_out.lock().unwrap().contains(&baton) {
            let error = json!({"message":"Transaction timed out","code":"TRANSACTION_TIMEOUT"});
            if path == "/v3/cursor" {
                return body(
                    StatusCode::OK,
                    format!(
                        "{}\n{}\n",
                        json!({"baton":baton}),
                        json!({"type":"step_error","step":0,"error":error})
                    ),
                );
            }
            responses.push(json!({"type":"ok","response":{"type":"batch","result":{"step_results":[null],"step_errors":[error]}}}));
            continue;
        }
        let owns = model.transactions.lock().unwrap().contains_key(&baton);
        // Autocommit statements hold the writer for just their execution. BEGIN
        // retains it on that stream; a competing writer waits for the permit.
        let permit = if owns {
            None
        } else {
            Some(Arc::clone(&model.writer).acquire_owned().await.unwrap())
        };
        let (results, errors) = {
            let mut state = state.lock().unwrap();
            state.in_transaction = owns;
            state.write_locked = owns;
            let single = vec![request.clone()];
            let steps = request["batch"]["steps"].as_array().unwrap_or(&single);
            let mut results = Vec::<Json>::new();
            let mut errors = Vec::new();
            for (index, step) in steps.iter().enumerate() {
                // libSQL execute_batch chains each statement to the preceding
                // successful step. Preserve that condition and real SQL errors.
                if !step["condition"].is_null() {
                    assert!(index > 0);
                    assert_eq!(step["condition"], json!({"type":"ok","step":index - 1}));
                    if results[index - 1].is_null() {
                        results.push(Json::Null);
                        errors.push(Json::Null);
                        continue;
                    }
                }
                match try_backed_statement_result(step, &mut state) {
                    Ok(result) => {
                        results.push(result);
                        errors.push(Json::Null);
                    }
                    Err(error) => {
                        let code = if matches!(
                            &error,
                            libsql::Error::SqliteFailure(code, _) if code & 0xff == 5
                        ) {
                            "SQLITE_BUSY"
                        } else {
                            "SQLITE_ERROR"
                        };
                        results.push(Json::Null);
                        errors.push(json!({"message":error.to_string(),"code":code}));
                    }
                }
            }
            (results, errors)
        };
        if sql.starts_with("BEGIN") && !results[0].is_null() {
            state.lock().unwrap().begin_sql.push(sql.clone());
            model
                .transactions
                .lock()
                .unwrap()
                .insert(baton.clone(), (permit.unwrap(), model.now()));
        } else if (sql.starts_with("COMMIT") || sql.starts_with("ROLLBACK"))
            && !results[0].is_null()
        {
            let owned = model.transactions.lock().unwrap().remove(&baton);
            if sql.starts_with("COMMIT") {
                let (_, start) = owned.as_ref().expect("COMMIT must own a transaction");
                model
                    .committed_durations
                    .lock()
                    .unwrap()
                    .push(model.now() - *start);
                if std::mem::take(&mut state.lock().unwrap().drop_next_commit_response) {
                    let (sender, response) = Body::channel();
                    sender.abort();
                    return body(StatusCode::OK, response);
                }
            }
        }
        if path == "/v3/cursor" {
            let mut lines = vec![json!({"baton":baton}).to_string()];
            for (index, result) in results.iter().enumerate() {
                if !errors[index].is_null() {
                    lines.push(
                        json!({"type":"step_error","step":index,"error":errors[index]}).to_string(),
                    );
                    continue;
                }
                if result.is_null() {
                    continue;
                }
                lines.push(
                    json!({"type":"step_begin","step":index,"cols":result["cols"]}).to_string(),
                );
                for row in result["rows"].as_array().unwrap() {
                    lines.push(json!({"type":"row","row":row}).to_string());
                }
                lines.push(json!({"type":"step_end","step":index,"affected_row_count":result["affected_row_count"]}).to_string());
            }
            return body(StatusCode::OK, format!("{}\n", lines.join("\n")));
        }
        responses.push(json!({"type":"ok","response":{"type":"batch","result":{"step_errors":errors,"step_results":results}}}));
    }
    body(
        StatusCode::OK,
        json!({"baton":if closed {None} else {Some(baton)},"results":responses}).to_string(),
    )
}

pub(in crate::db) struct HranaFixture {
    address: std::net::SocketAddr,
    state: Arc<Mutex<HranaState>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl HranaFixture {
    pub(in crate::db) fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    async fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(HranaState::new()));
        let service_state = Arc::clone(&state);
        let make_service = make_service_fn(move |_| {
            let state = Arc::clone(&service_state);
            async move {
                Ok::<_, Infallible>(service_fn(move |request| {
                    let state = Arc::clone(&state);
                    async move {
                        let reset = {
                            let mut guard = state.lock().unwrap();
                            std::mem::take(&mut guard.reset_next_request)
                        };
                        if reset {
                            Err(std::io::Error::new(
                                std::io::ErrorKind::ConnectionReset,
                                "connection reset",
                            ))
                        } else {
                            Ok::<_, std::io::Error>(respond(request, state).await)
                        }
                    }
                }))
            }
        });
        let (shutdown, receive) = tokio::sync::oneshot::channel();
        let server = Server::from_tcp(listener)
            .unwrap()
            .serve(make_service)
            .with_graceful_shutdown(async move {
                let _ = receive.await;
            });
        tokio::spawn(async move { server.await.unwrap() });
        Self {
            address,
            state,
            shutdown: Some(shutdown),
        }
    }

    pub(in crate::db) async fn delayed_database(
        path: &std::path::Path,
        delay: Duration,
        limit: Duration,
    ) -> (Self, Db) {
        Self::backed_database(path, Some(ProviderModel::new(delay, limit))).await
    }

    /// Account for each Hrana round trip without charging host scheduling to
    /// the provider lifetime. Paired databases can share one elapsed clock.
    pub(in crate::db) async fn modeled_latency_database(
        path: &std::path::Path,
        delay: Duration,
        limit: Duration,
        elapsed: Arc<Mutex<Duration>>,
    ) -> (Self, Db) {
        Self::backed_database(
            path,
            Some(ProviderModel::with_clock(
                delay,
                limit,
                ProviderClock::Modeled(elapsed),
            )),
        )
        .await
    }

    pub(in crate::db) async fn sqlite_backed_database(path: &std::path::Path) -> (Self, Db) {
        Self::backed_database(path, None).await
    }

    async fn backed_database(
        path: &std::path::Path,
        provider: Option<Arc<ProviderModel>>,
    ) -> (Self, Db) {
        let fixture = Self::start().await;
        let runtime = DbRuntime::new().unwrap();
        let backing = runtime
            .block_on(libsql::Builder::new_local(path).build())
            .unwrap()
            .connect()
            .unwrap();
        {
            let mut state = fixture.state.lock().unwrap();
            state.backing = Some((runtime, backing));
            state.provider = provider;
        }
        let db = fixture.db();
        (fixture, db)
    }

    pub(in crate::db) fn assert_committed_within_lifetime(&self, expected: usize) {
        let state = self.state.lock().unwrap();
        let model = state.provider.as_ref().unwrap();
        assert_eq!(model.timeouts.load(Ordering::SeqCst), 0);
        assert!(model.transactions.lock().unwrap().is_empty());
        let durations = model.committed_durations.lock().unwrap();
        assert_eq!(durations.len(), expected);
        for duration in durations.iter() {
            assert!(
                *duration >= model.delay,
                "latency must actually be injected"
            );
            assert!(*duration < model.limit, "transaction took {duration:?}");
        }
    }

    fn db(&self) -> Db {
        self.db_with_operation_timeout(Duration::from_secs(3))
    }

    pub(in crate::db) fn delay_client_requests(
        &self,
        db: &Db,
        requests: Vec<(&'static str, Duration)>,
    ) -> Arc<crate::db_compat::ManualClock> {
        let clock = install_clock(db);
        self.state.lock().unwrap().client_delays = Some(ClientDelays {
            clock: Arc::clone(&clock),
            requests,
        });
        clock
    }

    pub(in crate::db) fn assert_client_delays_used(&self) {
        let guard = self.state.lock().unwrap();
        let pending = &guard.client_delays.as_ref().unwrap().requests;
        assert!(pending.is_empty(), "unused client delays: {pending:?}");
    }

    pub(in crate::db) fn before_next_receipt(&self, write: impl FnOnce() + Send + 'static) {
        self.state.lock().unwrap().before_next_receipt = Some(Box::new(write));
    }

    pub(in crate::db) fn drop_next_commit_response(&self) {
        self.state.lock().unwrap().drop_next_commit_response = true;
    }

    pub(in crate::db) fn db_with_operation_timeout(&self, timeout: Duration) -> Db {
        let runtime = DbRuntime::with_operation_timeout(Some(timeout)).unwrap();
        let lease_runtime =
            DbRuntime::with_operation_timeout(Some(crate::db_compat::INTERACTIVE_READ_BUDGET))
                .unwrap();
        let database = runtime
            .block_on_database(
                "connect",
                libsql::Builder::new_remote(self.url(), "test".into())
                    .connector(crate::remote_https::https_connector())
                    .build(),
            )
            .unwrap();
        let database = Arc::new(database);
        let connections = (0..2)
            .map(|_| Connection::from_database(Arc::clone(&database), runtime.clone()).unwrap())
            .collect();
        let lease_state = Arc::new(Mutex::new(crate::db::ActiveLease {
            owner: None,
            generation: None,
        }));
        let launcher_fence = Arc::new(Mutex::new(None));
        Db {
            path: PathBuf::new(),
            remote: true,
            read_only: false,
            _database: Arc::clone(&database),
            runtime: runtime.clone(),
            lease_runtime: Some(lease_runtime.clone()),
            bootstrap_work: crate::db::BootstrapWork::Complete,
            connections: Arc::new(ConnectionPool {
                idle: Mutex::new(connections),
                database: Arc::clone(&database),
                runtime: runtime.clone(),
                remote: true,
                read_only: false,
                busy_timeout_ms: 5_000,
                lease_state: Arc::clone(&lease_state),
                generation_source: Mutex::new(None),
                launcher_fence: Arc::clone(&launcher_fence),
                commit_lease_probe: Arc::new(Mutex::new(None)),
                guild_store: false,
                outage: PrimaryOutageGate::default(),
            }),
            lease_connection: Arc::new(Mutex::new(Some(
                Connection::from_database(Arc::clone(&database), lease_runtime).unwrap(),
            ))),
            permits: Arc::new(tokio::sync::Semaphore::new(2)),
            permit_timeout: Some(Duration::from_secs(3)),
            required_config_version: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            config_write_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            config_floor_lock: Arc::new(parking_lot::Mutex::new(())),
            settings_write_lock: Arc::new(tokio::sync::Mutex::new(())),
            lease_state,
            launcher_fence,
            #[cfg(test)]
            before_final_lease_reread: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            during_leased_guild_commit: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            after_advised_due_read: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            after_advised_busy: Arc::new(Mutex::new(None)),
            #[cfg(debug_assertions)]
            test_config_snapshot: Arc::new(Mutex::new(None)),
        }
    }

    fn expire_latest(&self) {
        let mut state = self.state.lock().unwrap();
        let baton = state
            .latest_baton
            .clone()
            .expect("read must retain a baton");
        state.expired.insert(baton);
    }

    fn expire_next_cursor(&self) {
        self.state.lock().unwrap().expire_next_cursor = true;
    }

    fn expired_requests(&self) -> usize {
        self.state.lock().unwrap().expired_requests
    }

    fn fail_next_commit(&self) {
        self.state.lock().unwrap().fail_next_commit = true;
    }

    fn commit_timeouts(&self) -> usize {
        self.state.lock().unwrap().commit_timeouts
    }

    fn rollback_requests(&self) -> usize {
        self.state.lock().unwrap().rollback_requests
    }

    fn begin_sql(&self) -> Vec<String> {
        self.state.lock().unwrap().begin_sql.clone()
    }

    pub(in crate::db) fn immediate_begins(&self) -> usize {
        self.begin_sql()
            .iter()
            .filter(|sql| sql.contains("IMMEDIATE"))
            .count()
    }

    /// The next request whose SQL probes `sqlite_master` for `meta` returns 429.
    pub(in crate::db) fn fail_next_meta_probe(&self) {
        self.state.lock().unwrap().fail_next_meta_probe = true;
    }

    fn timeout_next_cursor(&self) {
        self.state.lock().unwrap().next_cursor_error =
            Some(("TRANSACTION_TIMEOUT", "Transaction timed out"));
    }

    pub(in crate::db) fn timeout_next_matching_cursor(&self, sql: &'static str) {
        self.state.lock().unwrap().timeout_cursor_sql = Some(sql);
    }

    /// SQL sent since the previous call, without transaction edges.
    pub(in crate::db) fn take_data_statements(&self) -> Vec<String> {
        self.take_statements()
            .into_iter()
            .filter(|sql| !is_transaction_edge(&sql.trim().to_ascii_uppercase()))
            .collect()
    }

    pub(in crate::db) fn take_statements(&self) -> Vec<String> {
        std::mem::take(&mut self.state.lock().unwrap().statements)
    }

    pub(in crate::db) fn read_begins(&self) -> usize {
        self.state.lock().unwrap().read_begins
    }

    fn hang_next_commit(&self) {
        self.state.lock().unwrap().hang_next_commit = true;
    }

    fn hang_next_statement(&self) {
        self.state.lock().unwrap().hang_next_statement = true;
    }

    fn hang_next_begin(&self) {
        self.state.lock().unwrap().hang_next_begin = true;
    }

    fn release_begin(&self) {
        let mut state = self.state.lock().unwrap();
        state.begin_released = true;
        state.begin_release.notify_waiters();
    }

    fn hang_rollback(&self) {
        self.state.lock().unwrap().hang_rollback = true;
    }

    fn write_locked(&self) -> bool {
        self.state.lock().unwrap().write_locked
    }

    fn closes(&self) -> usize {
        self.state.lock().unwrap().closes
    }

    async fn wait_until(&self, flag: impl Fn(&HranaState) -> bool) {
        loop {
            let progress = Arc::clone(&self.state.lock().unwrap().progress);
            let notified = progress.notified();
            if flag(&self.state.lock().unwrap()) {
                return;
            }
            notified.await;
        }
    }
}

impl Drop for HranaFixture {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

fn body(status: StatusCode, value: impl Into<Body>) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(value.into())
        .unwrap()
}

fn is_transaction_edge(sql: &str) -> bool {
    sql.starts_with("BEGIN") || sql.starts_with("COMMIT") || sql.starts_with("ROLLBACK")
}

/// Park this HTTP request forever after publishing the flag the test waits on.
/// The mutex is released before the park so a later ROLLBACK can still arrive.
fn park_request(path: &str, input: &Json, state: &Arc<Mutex<HranaState>>) -> bool {
    let mut guard = state.lock().unwrap();
    match path {
        "/v3/pipeline" => {
            let Some(requests) = input["requests"].as_array() else {
                return false;
            };
            for request in requests {
                let sql = statement_sql(request);
                if sql.is_empty() {
                    continue;
                }
                if sql.starts_with("COMMIT") && guard.hang_next_commit {
                    guard.hang_next_commit = false;
                    guard.commit_hung = true;
                    guard.progress.notify_waiters();
                    return true;
                }
                if sql.starts_with("ROLLBACK") && guard.hang_rollback {
                    guard.rollback_requests += 1;
                    guard.rollback_started = true;
                    guard.progress.notify_waiters();
                    return true;
                }
                if guard.hang_next_statement && !is_transaction_edge(&sql) {
                    guard.hang_next_statement = false;
                    guard.statement_hung = true;
                    guard.progress.notify_waiters();
                    return true;
                }
            }
            false
        }
        "/v3/cursor" => {
            let sql = statement_sql(input);
            if sql.is_empty() || is_transaction_edge(&sql) || !guard.hang_next_statement {
                return false;
            }
            guard.hang_next_statement = false;
            guard.statement_hung = true;
            guard.progress.notify_waiters();
            true
        }
        _ => false,
    }
}

async fn hold_immediate_begin(path: &str, input: &Json, state: &Arc<Mutex<HranaState>>) -> bool {
    let release = {
        let mut guard = state.lock().unwrap();
        if path != "/v3/pipeline" || !guard.hang_next_begin {
            return false;
        }
        let immediate = input["requests"].as_array().is_some_and(|requests| {
            requests.iter().any(|request| {
                let sql = statement_sql(request);
                sql.contains("BEGIN") && sql.contains("IMMEDIATE")
            })
        });
        if !immediate {
            return false;
        }
        guard.hang_next_begin = false;
        if guard.provider.is_none() {
            guard.write_locked = true;
            guard.in_transaction = true;
        }
        guard.begin_hung = true;
        guard.progress.notify_waiters();
        Arc::clone(&guard.begin_release)
    };
    loop {
        let notified = release.notified();
        if state.lock().unwrap().begin_released {
            return true;
        }
        notified.await;
    }
}

fn meta_probe_is_rate_limited(state: &Mutex<HranaState>, input: &Json) -> bool {
    let mut guard = state.lock().unwrap();
    if !guard.fail_next_meta_probe {
        return false;
    }
    let blob = input.to_string().to_ascii_lowercase();
    let probes_meta = blob.contains("select value from meta where key='restore_fence'");
    if !probes_meta {
        return false;
    }
    guard.fail_next_meta_probe = false;
    true
}

async fn respond(request: Request<Body>, state: Arc<Mutex<HranaState>>) -> Response<Body> {
    let path = request.uri().path().to_owned();
    let bytes = hyper::body::to_bytes(request.into_body()).await.unwrap();
    let input: Json = serde_json::from_slice(&bytes).unwrap();
    if meta_probe_is_rate_limited(&state, &input) {
        return body(
            StatusCode::TOO_MANY_REQUESTS,
            json!({"message":"rate limited","code":"HTTP_429"}).to_string(),
        );
    }
    if path == "/v3/pipeline"
        && let Some(route) = input["requests"].as_array().and_then(|requests| {
            requests.iter().find_map(|request| {
                let sql = statement_sql(request);
                if !sql.starts_with("BEGIN") {
                    return None;
                }
                if sql.contains("DEFERRED") {
                    Some(ReadRoute::Primary)
                } else if sql.contains("READONLY") {
                    Some(ReadRoute::ReplicaRoutable)
                } else {
                    None
                }
            })
        })
    {
        let (stall, status) = {
            let mut state = state.lock().unwrap();
            state.read_begins += 1;
            (
                state.stall_reads.contains(&route),
                state.fail_next_status.take(),
            )
        };
        if let Some(status) = status {
            return body(status, "fixture HTTP fault");
        }
        if stall {
            std::future::pending::<()>().await;
        }
    }
    let held_begin = hold_immediate_begin(&path, &input, &state).await;
    let provider = state.lock().unwrap().provider.clone();
    if let Some(provider) = provider {
        return provider_response(&path, &input, &state, &provider).await;
    }
    if held_begin {
        // The lock was taken when the request arrived. This response is that
        // same BEGIN, so it must not be reported as a second writer.
        state.lock().unwrap().skip_immediate_lock = true;
    } else if park_request(&path, &input, &state) {
        std::future::pending::<()>().await;
    }
    if let Some(message) = state.lock().unwrap().fail_next_request.take() {
        return body(StatusCode::BAD_REQUEST, message.to_owned());
    }
    let supplied_baton = input["baton"].as_str();
    let mut state = state.lock().unwrap();
    state.requests += 1;
    if supplied_baton.is_some_and(|baton| state.expired.contains(baton)) {
        state.expired_requests += 1;
        return body(
            StatusCode::BAD_REQUEST,
            json!({"message": "The stream has expired due to inactivity", "code": "STREAM_EXPIRED"})
                .to_string(),
        );
    }
    let baton = match supplied_baton {
        Some(baton) => baton.to_owned(),
        None => {
            state.next_baton += 1;
            format!("test-baton-{}", state.next_baton)
        }
    };
    state.latest_baton = Some(baton.clone());

    match path.as_str() {
        "/v3/pipeline" => {
            let requests = input["requests"].as_array().expect("pipeline requests");
            let drop_commit_response = state.drop_next_commit_response
                && requests
                    .iter()
                    .any(|request| statement_sql(request).starts_with("COMMIT"));
            let mut closed = false;
            let results: Vec<Json> = requests
                .iter()
                .map(|request| {
                    let response = match request["type"].as_str().expect("request type") {
                        "describe" => json!({
                            "type": "describe",
                            "result": {
                                "params": [],
                                "cols": [{"name": "value", "decltype": "INTEGER"}],
                                "is_explain": false,
                                "is_readonly": true
                            }
                        }),
                        "get_autocommit" => {
                            json!({"type": "get_autocommit", "is_autocommit": !state.in_transaction})
                        }
                        "batch" => pipeline_batch_response(request, &mut state),
                        "execute" => pipeline_batch_response(request, &mut state),
                        "close" => {
                            closed = true;
                            state.closes += 1;
                            state.write_locked = false;
                            state.in_transaction = false;
                            json!({"type": "close"})
                        }
                        other => panic!("unexpected Hrana pipeline operation: {other}"),
                    };
                    json!({"type": "ok", "response": response})
                })
                .collect();
            if drop_commit_response {
                state.drop_next_commit_response = false;
                state.expired.insert(baton);
                let (sender, response) = Body::channel();
                sender.abort();
                return body(StatusCode::OK, response);
            }
            body(
                StatusCode::OK,
                json!({"baton": if closed { None } else { Some(baton) }, "results": results})
                    .to_string(),
            )
        }
        "/v3/cursor" => {
            if state
                .timeout_cursor_sql
                .is_some_and(|sql| statement_sql(&input).contains(&sql.to_ascii_uppercase()))
            {
                state.timeout_cursor_sql = None;
                state.next_cursor_error = Some(("TRANSACTION_TIMEOUT", "Transaction timed out"));
            }
            if let Some((code, message)) = state.next_cursor_error.take() {
                if code == "TRANSACTION_TIMEOUT"
                    && let Some((runtime, conn)) = &state.backing
                    && !conn.is_autocommit()
                {
                    runtime.block_on(conn.execute("ROLLBACK", ())).unwrap();
                    state.in_transaction = false;
                    state.write_locked = false;
                }
                let lines = [
                    json!({"baton": baton}).to_string(),
                    json!({"type": "step_error", "step": 0, "error": {"message": message, "code": code}}).to_string(),
                ];
                return body(StatusCode::OK, format!("{}\n", lines.join("\n")));
            }
            if state.expire_next_cursor {
                state.expire_next_cursor = false;
                state.expired_requests += 1;
                return body(
                    StatusCode::BAD_REQUEST,
                    json!({"message": "The stream has expired due to inactivity", "code": "STREAM_EXPIRED"})
                        .to_string(),
                );
            }
            assert_eq!(input["batch"]["steps"].as_array().unwrap().len(), 1);
            if state.backing.is_some() {
                let result = backed_statement_result(&input, &mut state);
                let mut lines = vec![
                    json!({"baton": baton}).to_string(),
                    json!({"type": "step_begin", "step": 0, "cols": result["cols"]}).to_string(),
                ];
                for row in result["rows"].as_array().unwrap() {
                    lines.push(json!({"type": "row", "row": row}).to_string());
                }
                lines.push(json!({"type": "step_end", "step": 0, "affected_row_count": result["affected_row_count"]}).to_string());
                return body(StatusCode::OK, format!("{}\n", lines.join("\n")));
            }
            let lines = [
                json!({"baton": baton}).to_string(),
                json!({"type": "step_begin", "step": 0, "cols": [{"name": "value", "decltype": "INTEGER"}]}).to_string(),
                json!({"type": "row", "row": [{"type": "integer", "value": "1"}]}).to_string(),
                json!({"type": "step_end", "step": 0, "affected_row_count": 0}).to_string(),
            ];
            body(StatusCode::OK, format!("{}\n", lines.join("\n")))
        }
        other => panic!("unexpected Hrana route: {other}"),
    }
}

fn statement_sql(request: &Json) -> String {
    request["batch"]["steps"]
        .as_array()
        .and_then(|steps| steps.first())
        .and_then(|step| step["stmt"]["sql"].as_str())
        .or_else(|| request["stmt"]["sql"].as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_uppercase()
}

fn pipeline_batch_response(request: &Json, state: &mut HranaState) -> Json {
    let sql = statement_sql(request);
    if sql.starts_with("UPDATE META") && state.fail_next_cas {
        state.fail_next_cas = false;
        return json!({"type":"batch","result":{"step_results":[null],"step_errors":[{"message":"Transaction timed out","code":"TRANSACTION_TIMEOUT"}]}});
    }
    if sql.starts_with("ROLLBACK") {
        state.rollback_requests += 1;
        state.in_transaction = false;
        state.write_locked = false;
        state.progress.notify_waiters();
    } else if sql.starts_with("COMMIT") && state.fail_next_commit {
        state.fail_next_commit = false;
        state.commit_timeouts += 1;
        if let Some((runtime, conn)) = &state.backing {
            runtime.block_on(conn.execute("ROLLBACK", ())).unwrap();
        }
        state.in_transaction = false;
        state.write_locked = false;
        return json!({
            "type": "batch",
            "result": {
                "step_results": [null],
                "step_errors": [{
                    "message": "Transaction timed out",
                    "code": "TRANSACTION_TIMEOUT"
                }]
            }
        });
    } else if sql.starts_with("BEGIN") {
        if sql.contains("IMMEDIATE") && state.write_locked && !state.skip_immediate_lock {
            return json!({
                "type": "batch",
                "result": {
                    "step_results": [null],
                    "step_errors": [{
                        "message": "database is locked",
                        "code": "SQLITE_BUSY"
                    }]
                }
            });
        }
        if sql.contains("IMMEDIATE") {
            state.skip_immediate_lock = false;
            state.write_locked = true;
        }
        state.begin_sql.push(sql);
        state.in_transaction = true;
    } else if sql.starts_with("COMMIT") {
        state.in_transaction = false;
        state.write_locked = false;
    }
    let result = if state.backing.is_some() {
        backed_statement_result(request, state)
    } else {
        json!({"cols": [], "rows": [], "affected_row_count": 0, "last_insert_rowid": null})
    };
    json!({
        "type": "batch",
        "result": { "step_results": [result], "step_errors": [null] }
    })
}

// Use actual SQL for copy tests while retaining synthetic lifecycle fault fixtures.
fn backed_statement_result(request: &Json, state: &mut HranaState) -> Json {
    try_backed_statement_result(request, state).unwrap()
}

fn try_backed_statement_result(request: &Json, state: &mut HranaState) -> libsql::Result<Json> {
    let stmt = request["batch"]["steps"]
        .as_array()
        .and_then(|steps| steps.first())
        .map(|step| &step["stmt"])
        .unwrap_or(&request["stmt"]);
    let sql = stmt["sql"].as_str().unwrap();
    state.statements.push(sql.to_owned());
    let args = stmt["args"]
        .as_array()
        .map(|args| {
            args.iter()
                .map(|arg| match arg["type"].as_str().unwrap() {
                    "integer" => {
                        libsql::Value::Integer(arg["value"].as_str().unwrap().parse().unwrap())
                    }
                    "text" => libsql::Value::Text(arg["value"].as_str().unwrap().to_owned()),
                    "null" => libsql::Value::Null,
                    other => panic!("unsupported copy fixture parameter: {other}"),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let (runtime, conn) = state.backing.as_ref().unwrap();
    runtime.block_on(async {
        let prepared = conn.prepare(sql).await?;
        let cols = prepared.columns().into_iter().map(|col| json!({"name": col.name(), "decltype": col.decl_type()})).collect::<Vec<_>>();
        let mut values = Vec::new();
        let affected = if prepared.column_count() > 0 {
            let mut rows = prepared.query(args).await?;
            while let Some(row) = rows.next().await? {
                values.push((0..row.column_count()).map(|i| match row.get_value(i).unwrap() {
                    libsql::Value::Integer(n) => json!({"type": "integer", "value": n.to_string()}),
                    libsql::Value::Text(value) => json!({"type": "text", "value": value}),
                    libsql::Value::Null => json!({"type": "null"}),
                    other => panic!("unsupported copy fixture value: {other:?}"),
                }).collect::<Vec<_>>());
            }
            0
        } else {
            prepared.execute(args).await?
        };
        Ok(json!({"cols": cols, "rows": values, "affected_row_count": affected, "last_insert_rowid": null}))
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn full_custom_rule_copy_uses_one_hrana_insert_request() {
    let path = super::tmp_db_path("remote_custom_copy");
    let local = Db::open(&path, 2).unwrap();
    let source = ConfigurationScope::Guild(enroll_guild(&local, 10, 100).await.unwrap().unwrap());
    let destination =
        ConfigurationScope::Guild(enroll_guild(&local, 20, 100).await.unwrap().unwrap());
    let mut expected = Vec::new();
    for n in 0..crate::custom_rules::MAX_CUSTOM_RULES_PER_GUILD {
        let id = format!("custom:{n:016x}");
        let mut rule = crate::defaults::default_rules()[0].clone();
        rule.params = json!({"kind": "word", "words": [format!("word{n}")]});
        rule.dry_run = false;
        upsert_rule(&local, 10, &id, &rule).await.unwrap();
        rule.dry_run = true;
        expected.push((id, rule));
    }
    for (id, rule) in crate::types::RULE_IDS
        .iter()
        .zip(crate::defaults::default_rules())
    {
        upsert_rule(&local, 10, id, &rule).await.unwrap();
        expected.push((id.to_string(), rule));
    }
    let mut stale = crate::defaults::default_rules()[0].clone();
    stale.description = "destination only".into();
    upsert_rule(&local, 20, "old", &stale).await.unwrap();
    let fixture = HranaFixture::start().await;
    let runtime = DbRuntime::new().unwrap();
    let backing = runtime
        .block_on(libsql::Builder::new_local(&path).build())
        .unwrap()
        .connect()
        .unwrap();
    fixture.state.lock().unwrap().backing = Some((runtime, backing));
    let remote = fixture.db();
    copy_enrolled_guild_config(&remote, source, destination, vec!["rules".into()])
        .await
        .unwrap();
    {
        let state = fixture.state.lock().unwrap();
        let inserts = state
            .statements
            .iter()
            .filter(|sql| sql.starts_with("INSERT INTO rules "))
            .count();
        assert_eq!(inserts, 1, "full copy must send one rules INSERT request");
        assert!(
            state.requests <= 30,
            "copy HTTP requests must stay bounded: {}",
            state.requests
        );
        assert!(
            state
                .statements
                .iter()
                .any(|sql| sql.starts_with("BEGIN IMMEDIATE"))
        );
        assert!(state.statements.iter().any(|sql| sql.starts_with("COMMIT")));
        eprintln!(
            "full copy: {inserts} rules INSERT request; {} total HTTP requests",
            state.requests
        );
    }
    let copied = list_rules(&local, 20).await.unwrap();
    assert_eq!(copied.len(), expected.len());
    assert!(get_rule(&local, 20, "old").await.unwrap().is_none());
    for (id, rule) in expected {
        let actual = get_rule(&local, 20, &id).await.unwrap().unwrap();
        if crate::custom_rules::is_custom_id(&id) {
            assert!(actual.dry_run, "copied {id} must be dry-run");
        }
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(rule).unwrap(),
            "{id}"
        );
    }
    drop(remote);
    drop(fixture);
    drop(local);
    super::cleanup_db(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn maximal_badword_batch_commits_within_remote_transaction_lifetime() {
    let path = super::tmp_db_path("remote_badword_batch");
    let local = Db::open(&path, 2).unwrap();
    let request = ConfigurationScope::Guild(enroll_guild(&local, 10, 100).await.unwrap().unwrap());
    let scope = ConfigurationWriteScope::new(request.clone(), request).unwrap();
    let words = (0..32).map(|n| format!("word{n}")).collect::<Vec<_>>();
    let patterns = crate::normalize::badword_patterns(&words.join(", ")).unwrap();
    assert_eq!(patterns.len(), 32);
    let original = add_enrolled_badwords(&local, scope.clone(), "word0, word1", Some("original"))
        .await
        .unwrap();
    // A later duplicate row must not replace the canonical lowest-id row.
    add_badword(&local, 10, &patterns[0], Some("later"))
        .await
        .unwrap();
    let before = get_primary_versions(&local).await.unwrap();

    // Real libSQL HTTP client against SQLite, with modeled remote latency and
    // the provider's five-second interactive transaction lifetime.
    let (fixture, remote) = HranaFixture::modeled_latency_database(
        &path,
        Duration::from_millis(80),
        Duration::from_secs(5),
        Arc::default(),
    )
    .await;
    let saved = add_enrolled_badwords(&remote, scope.clone(), &words.join("\n"), Some("batch"))
        .await
        .expect("a maximal batch must commit inside the provider lifetime");
    fixture.assert_committed_within_lifetime(1);
    assert_eq!(
        saved.iter().map(|w| w.pattern.as_str()).collect::<Vec<_>>(),
        patterns.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert_eq!(saved[0].id, original[0].id);
    assert_eq!(saved[1].id, original[1].id);
    for (index, word) in saved.iter().enumerate() {
        let note = if index < 2 { "original" } else { "batch" };
        assert_eq!(word.note.as_deref(), Some(note), "{}", word.pattern);
        assert_eq!(word.guild_id, 10);
    }
    // Read independently from SQLite rather than trusting the HTTP response.
    let stored = list_badwords(&local, 10).await.unwrap();
    assert_eq!(stored.len(), 33);
    for word in &saved {
        let row = stored.iter().find(|row| row.id == word.id).unwrap();
        assert_eq!((&row.pattern, &row.note), (&word.pattern, &word.note));
    }
    let changed = get_primary_versions(&local).await.unwrap();
    assert_eq!(changed.config, before.config + 1);

    let repeated = add_enrolled_badwords(&remote, scope, &words.join(", "), Some("again"))
        .await
        .unwrap();
    fixture.assert_committed_within_lifetime(2);
    assert_eq!(
        repeated.iter().map(|w| w.id).collect::<Vec<_>>(),
        saved.iter().map(|w| w.id).collect::<Vec<_>>()
    );
    assert_eq!(list_badwords(&local, 10).await.unwrap().len(), 33);
    assert_eq!(get_primary_versions(&local).await.unwrap(), changed);
    drop(remote);
    drop(fixture);
    drop(local);
    super::cleanup_db(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn bot_config_proof_miss_keeps_the_full_snapshot_request_budget() {
    let path = super::tmp_db_path("remote_bot_config_proof");
    let local = Db::open(&path, 2).unwrap();
    add_badword(&local, 10, "original", None).await.unwrap();
    let (fixture, remote) = HranaFixture::modeled_latency_database(
        &path,
        Duration::from_millis(80),
        Duration::from_secs(5),
        Arc::default(),
    )
    .await;

    let requests = || fixture.state.lock().unwrap().requests;
    let before_cold = requests();
    let cold = read_bot_config(&remote, 10, false, None).await.unwrap();
    assert_eq!(cold.parts.unwrap().badwords, ["original"]);
    let full_read_requests = requests() - before_cold;
    let proof = cold.proof.unwrap();

    let before_hit = requests();
    let hit = read_bot_config(&remote, 10, false, Some(proof.clone()))
        .await
        .unwrap();
    assert!(hit.parts.is_none());
    assert_eq!((hit.version, hit.revision), (cold.version, cold.revision));
    assert_eq!(requests() - before_hit, full_read_requests);

    add_badword(&local, 10, "changed", None).await.unwrap();
    let before_miss = requests();
    let miss = read_bot_config(&remote, 10, false, Some(proof))
        .await
        .unwrap();
    assert_eq!(miss.version, cold.version + 1);
    assert_eq!(miss.parts.unwrap().badwords, ["original", "changed"]);
    assert_eq!(
        requests() - before_miss,
        full_read_requests,
        "an expired proof that misses must use the same Hrana budget as a full read"
    );

    drop(remote);
    drop(fixture);
    drop(local);
    super::cleanup_db(&path);
}

async fn enqueue_test_action(db: &Db, now: i64, user_id: i64) -> BotAction {
    enqueue_bot_action(
        db,
        10,
        42,
        &format!("{now}_queued"),
        crate::bot_actions::Action::Unban {
            user_id,
            reason: "queued".into(),
        },
        now,
        crate::bot_actions::Limits::default(),
    )
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advised_bot_action_claim_request_budget() {
    let request_delay = Duration::from_millis(50);
    for (name, advised, due, budget) in [
        ("no hint", false, true, 22),
        ("stale hint", true, false, 4),
        ("valid hint", true, true, 22),
    ] {
        let path = super::tmp_db_path("remote_action_claim");
        let local = Db::open(&path, 2).unwrap();
        let now = 100;
        let action = if due {
            enroll_guild(&local, 10, 1).await.unwrap();
            Some(enqueue_test_action(&local, now, 20).await)
        } else {
            None
        };
        let elapsed = Arc::new(Mutex::new(Duration::ZERO));
        let (fixture, remote) = HranaFixture::modeled_latency_database(
            &path,
            request_delay,
            Duration::from_secs(5),
            Arc::clone(&elapsed),
        )
        .await;

        let result = if advised {
            claim_advised_bot_action(&remote, now).await
        } else {
            claim_bot_action(&remote, now).await
        }
        .unwrap();

        let requests = fixture.state.lock().unwrap().requests;
        let time = *elapsed.lock().unwrap();
        let statements = fixture.take_data_statements();
        // Pruning runs every 256 receipts across the process. Report the
        // claim path's cost separately from that optional maintenance request.
        let prunes = statements
            .iter()
            .filter(|sql| sql.starts_with("DELETE FROM mutation_receipts"))
            .count();
        let claim_requests = requests - prunes;
        let claim_time = time - request_delay * u32::try_from(prunes).unwrap();
        eprintln!(
            "{name}: {claim_requests} requests, {} ms modeled ({prunes} prune requests excluded)",
            claim_time.as_millis()
        );
        assert_eq!(time, request_delay * u32::try_from(requests).unwrap());
        assert_eq!(claim_requests, budget, "{name} request budget");
        assert_eq!(fixture.read_begins(), usize::from(advised));
        assert_eq!(fixture.immediate_begins(), usize::from(!advised));
        assert_eq!(
            statements
                .iter()
                .filter(|sql| sql.contains("INSERT INTO mutation_receipts"))
                .count(),
            usize::from(due)
        );
        assert_eq!(
            result.as_ref().map(|job| &job.id),
            action.as_ref().map(|job| &job.id)
        );
        if let Some(result) = result {
            assert_eq!(
                bot_action(&local, 10, &result.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                "claimed"
            );
        }
        drop(remote);
        drop(fixture);
        drop(local);
        super::cleanup_db(&path);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advised_bot_action_claim_revalidates_after_busy_promotion() {
    for more_work in [false, true] {
        let path = super::tmp_db_path("remote_action_claim_busy");
        let local = Db::open(&path, 2).unwrap();
        let now = 100;
        enroll_guild(&local, 10, 1).await.unwrap();
        let first = enqueue_test_action(&local, now - 1, 20).await;
        let next = if more_work {
            Some(enqueue_test_action(&local, now, 21).await)
        } else {
            None
        };
        let (fixture, remote) = HranaFixture::modeled_latency_database(
            &path,
            Duration::from_millis(50),
            Duration::from_secs(5),
            Arc::default(),
        )
        .await;
        let writer_path = path.clone();
        let first_id = first.id.clone();
        fixture.state.lock().unwrap().before_next_receipt = Some(Box::new(move || {
            // An independent SQLite writer commits after the primary due
            // read but before the same transaction attempts its first write.
            let writer = Connection::open(&writer_path).unwrap();
            let updated = writer
                .execute_bounded(
                    "UPDATE bot_actions SET state='cancelled' WHERE id=?1 AND state='pending'",
                    params![first_id],
                    4096,
                )
                .unwrap();
            assert_eq!(updated, 1);
        }));

        let result = claim_advised_bot_action(&remote, now).await.unwrap();
        assert_eq!(
            fixture.read_begins(),
            2,
            "must refresh the primary read after the conflict"
        );
        assert_eq!(fixture.immediate_begins(), usize::from(more_work));
        assert_eq!(
            result.as_ref().map(|job| &job.id),
            next.as_ref().map(|job| &job.id)
        );
        let receipts = local
            .with_conn(|conn| {
                conn.query_row("SELECT count(*) FROM mutation_receipts", (), |row| {
                    Ok(row.get::<i64>(0)?)
                })
            })
            .await
            .unwrap();
        assert_eq!(receipts, i64::from(more_work));

        drop(remote);
        drop(fixture);
        drop(local);
        super::cleanup_db(&path);
    }
}

async fn advised_claim_fallback_fixture(
    path: &std::path::Path,
) -> (Db, HranaFixture, Db, BotAction) {
    let local = Db::open(path, 2).unwrap();
    enroll_guild(&local, 10, 1).await.unwrap();
    let action = enqueue_test_action(&local, 100, 20).await;
    let (fixture, _) = HranaFixture::modeled_latency_database(
        path,
        Duration::ZERO,
        Duration::from_secs(10),
        Arc::default(),
    )
    .await;
    let remote = fixture.db_with_operation_timeout(Duration::from_secs(5));
    (local, fixture, remote, action)
}

fn invalidate_advised_claim_snapshot(fixture: &HranaFixture, path: &std::path::Path, id: &str) {
    let path = path.to_owned();
    let id = id.to_owned();
    fixture.before_next_receipt(move || {
        // Commit between the due read and the first promotion write, keeping
        // the same action pending for the retry.
        let writer = Connection::open(&path).unwrap();
        assert_eq!(
            writer
                .execute_bounded(
                    "UPDATE bot_actions SET expires_at=expires_at+1 WHERE id=?1",
                    params![id],
                    4096,
                )
                .unwrap(),
            1
        );
    });
}

async fn assert_advised_action_pending(local: &Db, action: &BotAction) {
    let stored = bot_action(local, 10, &action.id).await.unwrap().unwrap();
    assert_eq!(stored.state, "pending");
    assert_eq!(stored.claim, None);
}

fn hold_advised_fallback_permits(
    remote: &Db,
    clock: &Arc<crate::db_compat::ManualClock>,
    spent: Duration,
) -> tokio::sync::oneshot::Receiver<tokio::sync::OwnedSemaphorePermit> {
    let permits = Arc::clone(&remote.permits);
    let clock = Arc::clone(clock);
    let (send, receive) = tokio::sync::oneshot::channel();
    *remote.after_advised_busy.lock().unwrap() = Some(Box::new(move || {
        let held = permits.try_acquire_many_owned(2).unwrap();
        clock.advance(spent);
        send.send(held).unwrap();
    }));
    receive
}

async fn assert_advised_fallback_checkout_timeout(
    error: anyhow::Error,
    local: &Db,
    fixture: &HranaFixture,
    remote: &Db,
    action: &BotAction,
) {
    assert!(is_database_operation_timeout(&error), "{error:#}");
    assert!(
        error.to_string().contains("waiting for connection"),
        "{error:#}"
    );
    assert_eq!(crate::db_compat::remote_unavailable_operation(&error), None);
    assert_eq!(remote.connections.outage.failure_count(), 0);
    assert_eq!(
        fixture.read_begins(),
        1,
        "only the promotion snapshot began"
    );
    assert_eq!(fixture.immediate_begins(), 0);
    fixture.assert_committed_within_lifetime(0);
    assert_advised_action_pending(local, action).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advised_claim_expired_busy_fallback_skips_permit_wait() {
    let path = super::tmp_db_path("remote_action_claim_expired_before_checkout");
    let (local, fixture, remote, action) = advised_claim_fallback_fixture(&path).await;
    invalidate_advised_claim_snapshot(&fixture, &path, &action.id);
    let clock = install_clock(&remote);
    let write_limit = remote.connections.runtime.operation_limit().unwrap();
    let held = hold_advised_fallback_permits(&remote, &clock, write_limit);

    let error = tokio::time::timeout(
        Duration::from_secs(2),
        claim_advised_bot_action(&remote, 100),
    )
    .await
    .expect("expired fallback waited for a pool permit")
    .expect_err("expired fallback claimed work");
    let held = held.await.unwrap();
    assert_eq!(remote.permits.available_permits(), 0);
    assert_advised_fallback_checkout_timeout(error, &local, &fixture, &remote, &action).await;
    drop(held);
    read_primary_fixture(&remote).await.unwrap();

    drop(remote);
    drop(fixture);
    drop(local);
    super::cleanup_db(&path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advised_claim_busy_fallback_permit_wait_uses_remaining_budget() {
    let path = super::tmp_db_path("remote_action_claim_checkout_deadline");
    let (local, fixture, remote, action) = advised_claim_fallback_fixture(&path).await;
    invalidate_advised_claim_snapshot(&fixture, &path, &action.id);
    let clock = install_clock(&remote);
    let write_limit = remote.connections.runtime.operation_limit().unwrap();
    let remaining = Duration::from_secs(1);
    let held = hold_advised_fallback_permits(&remote, &clock, write_limit - remaining);

    {
        let claim = claim_advised_bot_action(&remote, 100);
        tokio::pin!(claim);
        // The busy hook runs while polling the claim, so it proceeds to poll
        // checkout before the receiver below observes the held permits.
        let held = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                biased;
                result = &mut claim => panic!("claim returned before checkout queued: {result:?}"),
                held = held => held.unwrap(),
            }
        })
        .await
        .expect("promotion did not reach the busy fallback");
        clock.advance(remaining - Duration::from_millis(1));
        tokio::select! {
            biased;
            result = &mut claim => panic!("checkout returned before the deadline: {result:?}"),
            () = std::future::ready(()) => {}
        }
        clock.advance(Duration::from_millis(1));

        let error = tokio::time::timeout(Duration::from_secs(2), claim)
            .await
            .expect("queued fallback outlived the remaining write budget")
            .expect_err("fallback claimed work at the deadline");
        assert_advised_fallback_checkout_timeout(error, &local, &fixture, &remote, &action).await;
        drop(held);
    }
    read_primary_fixture(&remote).await.unwrap();

    drop(remote);
    drop(fixture);
    drop(local);
    super::cleanup_db(&path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advised_claim_busy_fallback_expiry_releases_primary_probe() {
    let path = super::tmp_db_path("remote_action_claim_expired_probe");
    let (local, fixture, remote, action) = advised_claim_fallback_fixture(&path).await;
    fixture.state.lock().unwrap().fail_next_status = Some(StatusCode::SERVICE_UNAVAILABLE);
    let failure = remote
        .with_ungated_conn(|conn| conn.transaction_primary_snapshot()?.finish_read())
        .await
        .unwrap_err();
    assert!(format!("{failure:#}").contains("status=503"), "{failure:#}");
    let read_begins = fixture.read_begins();
    let generation = match remote.connections.outage.admit().unwrap() {
        Admission::Work(generation) => generation,
        Admission::Probe(_) => panic!("the gate must be closed for the promotion"),
    };
    invalidate_advised_claim_snapshot(&fixture, &path, &action.id);

    let clock = install_clock(&remote);
    let write_limit = remote.connections.runtime.operation_limit().unwrap();
    let remaining = Duration::from_secs(1);
    let held = hold_advised_fallback_permits(&remote, &clock, write_limit - remaining);
    let pool = Arc::clone(&remote.connections);
    {
        let mut after_busy = remote.after_advised_busy.lock().unwrap();
        let hold_permits = after_busy.take().unwrap();
        *after_busy = Some(Box::new(move || {
            let gate = &pool.outage;
            // Publish an outage observed by another caller after the promotion.
            assert!(is_primary_outage(&gate.record_failure(
                generation,
                crate::db_compat::INTERACTIVE_READ_BUDGET,
                failure,
            )));
            gate.elapse_probe_cooldown();
            hold_permits();
        }));
    }

    {
        let claim = claim_advised_bot_action(&remote, 100);
        tokio::pin!(claim);
        let held = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                biased;
                result = &mut claim => panic!("claim returned before probe checkout queued: {result:?}"),
                held = held => held.unwrap(),
            }
        })
        .await
        .expect("promotion did not reach the busy fallback");
        let gate = &remote.connections.outage;
        assert!(
            gate.admit()
                .is_err_and(|error| error.is::<PrimaryBackoff>())
        );
        clock.advance(remaining);
        let error = tokio::time::timeout(Duration::from_secs(2), claim)
            .await
            .expect("the probe checkout outlived the write budget")
            .expect_err("the expired fallback claimed work");
        assert!(is_database_operation_timeout(&error), "{error:#}");
        assert!(
            error.to_string().contains("waiting for connection"),
            "{error:#}"
        );
        assert_eq!(gate.failure_count(), 1);
        assert_eq!(
            fixture.read_begins(),
            read_begins + 1,
            "only the promotion sent BEGIN"
        );
        assert_eq!(fixture.immediate_begins(), 0);
        assert_advised_action_pending(&local, &action).await;
        drop(held);
    }

    // The next caller must reach the primary immediately. A failure there
    // starts a fresh cooldown because that caller actually sent a probe.
    fixture.state.lock().unwrap().fail_next_status = Some(StatusCode::SERVICE_UNAVAILABLE);
    let error = read_primary_fixture(&remote).await.unwrap_err();
    assert!(format!("{error:#}").contains("status=503"), "{error:#}");
    assert!(is_primary_outage(&error), "{error:#}");
    assert_eq!(remote.connections.outage.failure_count(), 2);
    let error = read_primary_fixture(&remote).await.unwrap_err();
    assert!(error.is::<PrimaryBackoff>(), "{error:#}");

    remote.connections.outage.elapse_probe_cooldown();
    read_primary_fixture(&remote).await.unwrap();
    assert_eq!(remote.connections.outage.failure_count(), 0);

    drop(remote);
    drop(fixture);
    drop(local);
    super::cleanup_db(&path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advised_claim_busy_fallback_times_out_at_original_deadline() {
    let path = super::tmp_db_path("remote_action_claim_fallback_deadline");
    let (local, fixture, remote, action) = advised_claim_fallback_fixture(&path).await;
    invalidate_advised_claim_snapshot(&fixture, &path, &action.id);
    let clock = install_clock(&remote);
    let spent = Duration::from_secs(3);
    let write_limit = remote.connections.runtime.operation_limit().unwrap();
    let due_clock = Arc::clone(&clock);
    *remote.after_advised_due_read.lock().unwrap() = Some(Box::new(move || {
        due_clock.advance(spent);
    }));
    fixture.hang_next_begin();

    let (result, releasing) = {
        let claim = claim_advised_bot_action(&remote, 100);
        tokio::pin!(claim);
        tokio::select! {
            result = &mut claim => panic!("claim returned before the fallback BEGIN: {result:?}"),
            hung = tokio::time::timeout(Duration::from_secs(2), fixture.wait_until(|state| state.begin_hung)) => {
                if hung.is_err() {
                    fixture.release_begin();
                }
                hung.expect("fallback BEGIN was not sent");
            }
        }
        clock.advance(write_limit - spent);
        let releasing = tokio::time::timeout(Duration::from_secs(2), async {
            while remote.runtime.open_release_count() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        // Always unblock the fixture, including when testing the old behavior.
        fixture.release_begin();
        (claim.await, releasing)
    };
    releasing.expect("fallback BEGIN must give up at the original write deadline");
    let error = result.expect_err("the fallback cannot claim after the write deadline");
    assert!(is_database_operation_timeout(&error), "{error:#}");
    assert!(
        error.to_string().contains("begin immediate transaction"),
        "{error:#}"
    );
    assert_eq!(fixture.read_begins(), 2);
    assert_eq!(fixture.immediate_begins(), 1);
    fixture.assert_committed_within_lifetime(0);
    assert_advised_action_pending(&local, &action).await;

    drop(remote);
    drop(fixture);
    drop(local);
    super::cleanup_db(&path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advised_claim_expiry_before_busy_fallback_leaves_primary_pool_available() {
    let path = super::tmp_db_path("remote_action_claim_expired_fallback");
    let (local, fixture, remote, action) = advised_claim_fallback_fixture(&path).await;
    let clock = install_clock(&remote);
    let write_limit = remote.connections.runtime.operation_limit().unwrap();

    for _ in 0..2 {
        invalidate_advised_claim_snapshot(&fixture, &path, &action.id);
        clock.advance_at_operation("begin primary snapshot", write_limit);
        let error = claim_advised_bot_action(&remote, 100)
            .await
            .expect_err("the fallback must stop before opening an expired snapshot");
        assert!(is_database_operation_timeout(&error), "{error:#}");
        assert!(
            error.to_string().contains("begin primary snapshot"),
            "{error:#}"
        );
        assert_eq!(crate::db_compat::remote_unavailable_operation(&error), None);
        assert_advised_action_pending(&local, &action).await;
    }
    assert_eq!(
        fixture.read_begins(),
        2,
        "only the promotion snapshots began"
    );
    assert_eq!(fixture.immediate_begins(), 0);
    fixture.assert_committed_within_lifetime(0);
    read_primary_fixture(&remote)
        .await
        .expect("local fallback expiry must leave the primary pool available");

    drop(remote);
    drop(fixture);
    drop(local);
    super::cleanup_db(&path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_advised_due_check_leaves_time_to_prepare_the_claim() {
    let path = super::tmp_db_path("remote_slow_action_due");
    let local = Db::open(&path, 2).unwrap();
    enroll_guild(&local, 10, 1).await.unwrap();
    let action = enqueue_test_action(&local, 100, 20).await;
    let (fixture, _) = HranaFixture::modeled_latency_database(
        &path,
        Duration::ZERO,
        Duration::from_secs(5),
        Arc::default(),
    )
    .await;
    let remote = fixture.db_with_operation_timeout(Duration::from_secs(10));
    fixture.delay_client_requests(
        &remote,
        vec![
            (
                "SELECT EXISTS(SELECT 1 FROM BOT_ACTIONS",
                Duration::from_millis(3700),
            ),
            (
                "SELECT * FROM PRAGMA_PAGE_COUNT()",
                Duration::from_millis(600),
            ),
        ],
    );

    let claim = claim_advised_bot_action(&remote, 100)
        .await
        .expect("receipt preparation must have the ordinary claim deadline")
        .expect("the due action must be claimed");
    assert_eq!(claim.id, action.id);
    assert_eq!(fixture.read_begins(), 1);
    assert_eq!(fixture.immediate_begins(), 0);
    fixture.assert_committed_within_lifetime(1);
    fixture.assert_client_delays_used();

    drop(remote);
    drop(fixture);
    drop(local);
    super::cleanup_db(&path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capture_promotes_after_the_read_deadline_within_the_write_budget() {
    let path = super::tmp_db_path("remote_capture_promotion");
    let local = Db::open(&path, 2).unwrap();
    let (fixture, _) = HranaFixture::sqlite_backed_database(&path).await;
    let remote = fixture.db_with_operation_timeout(Duration::from_secs(5));
    let clock = install_clock(&remote);
    let deadline = remote.connections.runtime.capture_write_deadline();

    remote
        .with_conn(move |conn| {
            let mut tx = conn.transaction_capture_before(deadline, false)?;
            let _: i64 = tx.query_row("SELECT 1", (), |row| Ok(row.get(0)?))?;
            // The final read succeeded before the interactive deadline.
            clock.advance(crate::db_compat::INTERACTIVE_READ_BUDGET + Duration::from_millis(500));
            tx.prepare_capture_write()?;
            tx.commit()
        })
        .await
        .expect("promotion must use the remaining write budget");

    let receipts: i64 = local
        .with_conn(|conn| {
            conn.query_row("SELECT COUNT(*) FROM mutation_receipts", (), |row| {
                Ok(row.get(0)?)
            })
        })
        .await
        .unwrap();
    assert_eq!(receipts, 1);
    assert_eq!(fixture.read_begins(), 1);
    assert_eq!(fixture.immediate_begins(), 0);

    drop(remote);
    drop(fixture);
    drop(local);
    super::cleanup_db(&path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_capture_cannot_prepare_growth_or_reclaim_receipts() {
    let fixture = HranaFixture::start().await;
    let remote = fixture.db_with_operation_timeout(Duration::from_secs(5));
    let clock = install_clock(&remote);
    for reclaim in [false, true] {
        let clock = Arc::clone(&clock);
        let deadline = remote.runtime.capture_write_deadline();
        let error = remote
            .with_conn(move |conn| {
                let mut tx = conn.transaction_capture_before(deadline, false)?;
                let _: i64 = tx.query_row("SELECT 1", (), |row| Ok(row.get(0)?))?;
                // Preflight returned successfully, then the budget expired
                // before the first required write.
                clock.advance(Duration::from_secs(5));
                if reclaim {
                    tx.prepare_capture_reclaim()
                } else {
                    tx.prepare_capture_write()
                }
            })
            .await
            .unwrap_err();
        assert!(is_database_operation_timeout(&error), "{error:#}");
        assert!(
            error.to_string().contains("promote capture transaction"),
            "{error:#}"
        );
    }
    assert_eq!(fixture.read_begins(), 2);
    assert_eq!(fixture.immediate_begins(), 0);
    assert_eq!(fixture.rollback_requests(), 2);
}

async fn advised_claim_after_due_read(promotion_time: Duration, succeeds: bool) {
    let path = super::tmp_db_path("remote_action_claim_promotion_deadline");
    let local = Db::open(&path, 2).unwrap();
    enroll_guild(&local, 10, 1).await.unwrap();
    let action = enqueue_test_action(&local, 100, 20).await;
    let (fixture, _) = HranaFixture::modeled_latency_database(
        &path,
        Duration::ZERO,
        Duration::from_secs(10),
        Arc::default(),
    )
    .await;
    let remote = fixture.db_with_operation_timeout(Duration::from_secs(5));
    let clock = install_clock(&remote);
    let attempts = if succeeds { 1 } else { 2 };
    for _ in 0..attempts {
        let clock = Arc::clone(&clock);
        *remote.after_advised_due_read.lock().unwrap() = Some(Box::new(move || {
            // Advance only once the real due read has succeeded.
            clock.advance(promotion_time);
        }));

        let result = claim_advised_bot_action(&remote, 100).await;
        let stored = bot_action(&local, 10, &action.id).await.unwrap().unwrap();
        if succeeds {
            let claimed = result.unwrap().unwrap();
            assert_eq!(claimed.id, action.id);
            assert_eq!(stored.state, "claimed");
            assert!(stored.claim.is_some());
            assert_eq!(stored.claim, claimed.claim);
        } else {
            let error = result.expect_err("promotion must respect the original write deadline");
            assert!(is_database_operation_timeout(&error), "{error:#}");
            assert!(
                error.to_string().contains("promote capture transaction"),
                "{error:#}"
            );
            assert_eq!(stored.state, "pending");
            assert_eq!(stored.claim, None);
        }
    }
    assert_eq!(fixture.read_begins(), attempts);
    assert_eq!(fixture.immediate_begins(), 0);
    fixture.assert_committed_within_lifetime(usize::from(succeeds));
    let receipts = local
        .with_conn(|conn| {
            conn.query_row("SELECT count(*) FROM mutation_receipts", (), |row| {
                Ok(row.get::<i64>(0)?)
            })
        })
        .await
        .unwrap();
    assert_eq!(receipts, i64::from(succeeds));
    if !succeeds {
        read_primary_fixture(&remote)
            .await
            .expect("local promotion timeouts must leave the primary pool available");
    }

    drop(remote);
    drop(fixture);
    drop(local);
    super::cleanup_db(&path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advised_claim_promotes_after_the_interactive_deadline() {
    advised_claim_after_due_read(
        crate::db_compat::INTERACTIVE_READ_BUDGET + Duration::from_millis(500),
        true,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advised_claim_aborts_promotion_after_the_write_deadline() {
    advised_claim_after_due_read(Duration::from_millis(5500), false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advised_claim_expiry_before_begin_leaves_primary_pool_available() {
    let fixture = HranaFixture::start().await;
    let write_limit = Duration::from_secs(5);
    let remote = fixture.db_with_operation_timeout(write_limit);
    let clock = install_clock(&remote);

    for _ in 0..2 {
        clock.advance_at_operation("begin capture transaction", write_limit);
        let error = claim_advised_bot_action(&remote, 100).await.unwrap_err();
        assert!(is_database_operation_timeout(&error), "{error:#}");
        assert!(
            error.to_string().contains("begin capture transaction"),
            "{error:#}"
        );
    }
    assert!(fixture.begin_sql().is_empty(), "expired claims sent BEGIN");
    read_primary_fixture(&remote)
        .await
        .expect("local pre-BEGIN timeouts must leave the primary pool available");
}

fn read_one(conn: &mut Connection) -> Result<i64> {
    conn.query_row("SELECT 1", (), |row| Ok(row.get(0)?))
}

async fn sync_read(db: Db) -> Result<i64> {
    tokio::task::spawn_blocking(move || db.with_conn_sync(read_one))
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_remote_cursor_baton_is_reset_before_async_pool_reuse() {
    let fixture = HranaFixture::start().await;
    let db = fixture.db();
    assert_eq!(db.with_conn(read_one).await.unwrap(), 1);
    fixture.expire_latest();
    assert_eq!(db.with_conn(read_one).await.unwrap(), 1);
    assert_eq!(db.with_conn(read_one).await.unwrap(), 1);
    assert_eq!(fixture.expired_requests(), 0);
    assert_eq!(db.connections.idle.lock().unwrap().len(), 2);
    assert_eq!(db.permits.available_permits(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_remote_cursor_baton_is_reset_before_sync_pool_reuse() {
    let fixture = HranaFixture::start().await;
    let db = fixture.db();
    assert_eq!(sync_read(db.clone()).await.unwrap(), 1);
    fixture.expire_latest();
    assert_eq!(sync_read(db.clone()).await.unwrap(), 1);
    assert_eq!(sync_read(db.clone()).await.unwrap(), 1);
    assert_eq!(fixture.expired_requests(), 0);
    assert_eq!(db.connections.idle.lock().unwrap().len(), 2);
    assert_eq!(db.permits.available_permits(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_stream_error_discards_lease_without_replaying_closure() {
    let fixture = HranaFixture::start().await;
    let db = fixture.db();
    let executions = Arc::new(AtomicUsize::new(0));
    fixture.expire_next_cursor();
    let first_count = Arc::clone(&executions);
    let error = db
        .with_conn(move |conn| {
            first_count.fetch_add(1, Ordering::SeqCst);
            read_one(conn)
        })
        .await
        .expect_err("injected stream failure should reach caller");
    assert_eq!(error.to_string(), "remote database query failed");
    assert!(format!("{error:#}").contains("STREAM_EXPIRED"));
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert_eq!(db.connections.idle.lock().unwrap().len(), 1);
    assert_eq!(db.permits.available_permits(), 2);

    let second_count = Arc::clone(&executions);
    let value = db
        .with_conn(move |conn| {
            second_count.fetch_add(1, Ordering::SeqCst);
            read_one(conn)
        })
        .await
        .unwrap();
    assert_eq!(value, 1);
    assert_eq!(executions.load(Ordering::SeqCst), 2);
    assert_eq!(db.connections.idle.lock().unwrap().len(), 1);
    assert_eq!(db.permits.available_permits(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_commit_timeout_does_not_roll_back_the_closed_stream() {
    let fixture = HranaFixture::start().await;
    let db = fixture.db();
    fixture.fail_next_commit();
    let started = std::time::Instant::now();
    let error = db
        .with_conn(|conn| {
            let tx = conn.transaction_primary_write()?;
            tx.commit()
        })
        .await
        .expect_err("a timed-out commit must not report success");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "rollback on the closed stream must not wait out the client deadline"
    );
    assert!(is_database_commit_indeterminate(&error), "{error:#}");
    assert!(
        is_remote_interactive_timeout(&error),
        "commit timeout must stay an interactive-stream failure: {error:#}"
    );
    assert_eq!(fixture.commit_timeouts(), 1);
    assert_eq!(
        fixture.rollback_requests(),
        0,
        "COMMIT already closed the stream"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_commit_error_is_reconciled_against_durable_rows() {
    let path = super::tmp_db_path("remote_commit_receipt");
    let local = Db::open(&path, 2).unwrap();
    let request = ConfigurationScope::Guild(enroll_guild(&local, 10, 100).await.unwrap().unwrap());
    let scope = ConfigurationWriteScope::new(request.clone(), request).unwrap();
    let fixture = HranaFixture::start().await;
    let runtime = DbRuntime::new().unwrap();
    let backing = runtime
        .block_on(libsql::Builder::new_local(&path).build())
        .unwrap()
        .connect()
        .unwrap();
    fixture.state.lock().unwrap().backing = Some((runtime, backing));
    let remote = fixture.db();

    fixture.fail_next_commit();
    let error = add_enrolled_badwords(&remote, scope.clone(), "sample", None)
        .await
        .expect_err("a rejected commit with no receipt must fail");
    assert_eq!(fixture.commit_timeouts(), 1);
    assert!(is_remote_interactive_timeout(&error), "{error:#}");
    assert!(!is_database_commit_indeterminate(&error), "{error:#}");
    assert!(list_badwords(&local, 10).await.unwrap().is_empty());

    // Use a fresh primary-outage gate for the two lost-response cases.
    drop(remote);
    let remote = fixture.db();
    fixture.fail_next_commit();
    fixture.state.lock().unwrap().drop_next_commit_response = true;
    let error = add_enrolled_badwords(&remote, scope.clone(), "sample", None)
        .await
        .expect_err("a lost response after a rolled-back COMMIT must fail");
    assert_eq!(fixture.commit_timeouts(), 2);
    assert!(!fixture.state.lock().unwrap().drop_next_commit_response);
    assert!(
        matches!(
            error.downcast_ref::<libsql::Error>(),
            Some(libsql::Error::Hrana(_))
        ),
        "{error:#}"
    );
    assert!(!is_remote_interactive_timeout(&error), "{error:#}");
    assert!(!is_database_commit_indeterminate(&error), "{error:#}");
    assert!(list_badwords(&local, 10).await.unwrap().is_empty());

    fixture.state.lock().unwrap().drop_next_commit_response = true;
    let saved = add_enrolled_badwords(&remote, scope, "sample", None)
        .await
        .expect("a lost response after an applied COMMIT must reconcile as success");
    assert!(!fixture.state.lock().unwrap().drop_next_commit_response);
    let stored = list_badwords(&local, 10).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(saved.len(), 1);
    assert_eq!(
        stored[0].pattern,
        crate::normalize::badword_patterns("sample").unwrap()[0]
    );
    assert_eq!(stored[0].id, saved[0].id);

    drop(remote);
    drop(fixture);
    drop(local);
    super::cleanup_db(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn primary_snapshot_skips_rollback_when_the_cursor_stream_is_closed() {
    let fixture = HranaFixture::start().await;
    let db = fixture.db();
    fixture.timeout_next_cursor();
    let started = std::time::Instant::now();
    let error = db
        .with_conn(|conn| {
            let tx = conn.transaction_primary_snapshot()?;
            let query = tx.query_row("SELECT 1", (), |row| Ok(row.get::<i64>(0)?));
            let finished = tx.finish_read();
            query?;
            finished
        })
        .await
        .expect_err("a closed cursor must surface to the caller");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "finish_read must not roll back a closed stream for the client deadline"
    );
    assert!(
        is_remote_interactive_timeout(&error),
        "cursor timeout must stay an interactive-stream failure: {error:#}"
    );
    assert_eq!(fixture.rollback_requests(), 0);
    let begins = fixture.begin_sql();
    assert!(
        begins
            .iter()
            .any(|sql| sql.contains("DEFERRED") && !sql.contains("IMMEDIATE")),
        "primary snapshot must be deferred: {begins:?}"
    );
    assert_eq!(db.connections.idle.lock().unwrap().len(), 1);
    assert_eq!(db.permits.available_permits(), 2);
}

#[tokio::test(start_paused = true)]
async fn primary_stall_defers_pooled_reads_and_recovers_while_lease_retries() {
    #[derive(Clone)]
    struct Log(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Log {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let path = super::tmp_db_path("primary_outage");
    let local = Db::open(&path, 2).unwrap();
    let instance = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let refuse = Arc::new(AtomicBool::new(false));
    let now = crate::control::now_ms();
    assert!(
        claim_active_bot(&local, instance, now, Arc::clone(&refuse))
            .await
            .unwrap()
    );
    let fixture = HranaFixture::start().await;
    let runtime = DbRuntime::new().unwrap();
    let backing = runtime
        .block_on(libsql::Builder::new_local(&path).build())
        .unwrap()
        .connect()
        .unwrap();
    fixture.state.lock().unwrap().backing = Some((runtime, backing));
    let db = fixture.db_with_operation_timeout(Duration::from_secs(7));
    let other = db.clone();
    let launcher = UpdateControl::from_application(&db);
    launcher.arm_launcher_fence("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    assert!(launcher.renew_launcher_fence().await.unwrap());
    let launcher_deadline = launcher.launcher_fence_deadline().unwrap();
    db.arm_writer_term(instance, local.leased_generation().unwrap());
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let writer = Log(Arc::clone(&bytes));
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let capture = tracing::subscriber::set_default(subscriber);

    fixture.state.lock().unwrap().stall_reads = &[ReadRoute::Primary];
    let started = tokio::time::Instant::now().into_std();
    let (snapshot, versions) =
        tokio::join!(get_primary_versions(&db), get_primary_versions(&other));
    for error in [snapshot.unwrap_err(), versions.unwrap_err()] {
        assert!(is_database_operation_timeout(&error), "{error:#}");
        assert!(
            error.to_string().ends_with("timed out after 4s"),
            "{error:#}"
        );
    }
    assert_eq!(fixture.state.lock().unwrap().read_begins, 2);

    for _ in 0..3 {
        let (snapshot, versions) =
            tokio::join!(get_primary_versions(&db), get_primary_versions(&other));
        for error in [snapshot.unwrap_err(), versions.unwrap_err()] {
            assert!(
                error.to_string().contains("primary is backing off"),
                "{error:#}"
            );
        }
    }
    assert_eq!(
        fixture.state.lock().unwrap().read_begins,
        2,
        "deferred callers must not send database requests"
    );

    let deadline = started + Duration::from_secs(12);
    fixture.state.lock().unwrap().fail_next_cas = true;
    assert!(
        renew_active_bot(
            &db,
            instance,
            crate::control::now_ms(),
            deadline,
            Arc::clone(&refuse)
        )
        .await
        .is_err(),
        "first renewal attempt must reach the injected CAS error"
    );
    assert!(
        renew_active_bot(&db, instance, crate::control::now_ms(), deadline, refuse)
            .await
            .unwrap(),
        "lease retry must reach the database during the cooldown"
    );
    local
        .with_conn(|conn| {
            conn.execute_rows("DELETE FROM meta WHERE key='launcher_lease'", (), 1)?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        launcher.renew_launcher_fence_cas().await.unwrap(),
        "a missing fence must still be reacquired while ordinary work backs off"
    );
    assert!(launcher.launcher_fence_deadline().unwrap() > launcher_deadline);
    assert_eq!(fixture.state.lock().unwrap().read_begins, 2);

    fixture.state.lock().unwrap().stall_reads = &[];
    tokio::time::advance(Duration::from_secs(5)).await;
    let (probe_owner, contender) =
        tokio::join!(db.with_conn(|_| Ok(())), other.with_conn(|_| Ok(())));
    assert_ne!(probe_owner.is_ok(), contender.is_ok());
    assert_eq!(
        fixture.state.lock().unwrap().read_begins,
        3,
        "one caller may probe the primary after the cooldown"
    );
    assert_eq!(
        get_primary_versions(&db).await.unwrap(),
        get_primary_versions(&local).await.unwrap(),
        "regular reads must resume using the same remote pool"
    );

    drop(capture);
    let log = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    assert_eq!(log.matches("WARN ").count(), 1, "{log}");
    assert_eq!(log.matches("primary recovered").count(), 1, "{log}");
    for field in ["count=", "first_failure=", "last_failure=", "operations="] {
        assert!(log.contains(field), "{log}");
    }
    assert!(log.contains("count=2"), "{log}");
    assert!(log.contains("begin primary snapshot"), "{log}");
    drop(launcher);
    drop(other);
    drop(db);
    drop(fixture);
    drop(local);
    super::cleanup_db(&path);
}

#[tokio::test(start_paused = true)]
async fn version_poll_confirms_primary_health_after_ambiguous_read_failure() {
    let path = super::tmp_db_path("remote_version_routes");
    let local = Db::open(&path, 2).unwrap();
    let expected = get_primary_versions(&local).await.unwrap();
    let fixture = HranaFixture::start().await;
    let runtime = DbRuntime::new().unwrap();
    let backing = runtime
        .block_on(libsql::Builder::new_local(&path).build())
        .unwrap()
        .connect()
        .unwrap();
    fixture.state.lock().unwrap().backing = Some((runtime, backing));
    let db = fixture.db_with_operation_timeout(Duration::from_secs(7));

    fixture.state.lock().unwrap().stall_reads = &[ReadRoute::ReplicaRoutable, ReadRoute::Primary];
    let error = get_version_state(&db).await.unwrap_err();
    assert!(
        error.to_string().contains("begin consistent read"),
        "{error:#}"
    );
    assert!(!is_primary_outage(&error), "{error:#}");
    assert_eq!(
        fixture.state.lock().unwrap().read_begins,
        1,
        "an on-demand check must return after its first failed read"
    );

    fixture.state.lock().unwrap().stall_reads = &[ReadRoute::ReplicaRoutable];
    assert_eq!(poll_version_state(&db).await.unwrap(), expected);
    assert_eq!(fixture.state.lock().unwrap().read_begins, 3);
    assert_eq!(get_primary_versions(&db).await.unwrap(), expected);
    assert_eq!(fixture.state.lock().unwrap().read_begins, 4);

    // The same fallback applies when a READONLY statement fails after BEGIN.
    fixture.state.lock().unwrap().stall_reads = &[];
    fixture.timeout_next_cursor();
    assert_eq!(poll_version_state(&db).await.unwrap(), expected);
    assert_eq!(fixture.state.lock().unwrap().read_begins, 6);

    // Only the version poll sees this shared-endpoint outage. Its fallback
    // reaches the stalled primary and opens the gate for the next poll.
    fixture.state.lock().unwrap().stall_reads = &[ReadRoute::ReplicaRoutable, ReadRoute::Primary];
    let error = poll_version_state(&db).await.unwrap_err();
    assert!(is_primary_outage(&error), "{error:#}");
    assert!(
        error.to_string().contains("begin primary snapshot"),
        "{error:#}"
    );
    assert_eq!(fixture.state.lock().unwrap().read_begins, 8);
    let error = poll_version_state(&db).await.unwrap_err();
    assert!(
        error.to_string().contains("primary is backing off"),
        "{error:#}"
    );
    assert_eq!(fixture.state.lock().unwrap().read_begins, 8);

    fixture.state.lock().unwrap().stall_reads = &[];
    tokio::time::advance(Duration::from_secs(5)).await;
    assert_eq!(poll_version_state(&db).await.unwrap(), expected);
    assert_eq!(fixture.state.lock().unwrap().read_begins, 10);

    drop(db);
    drop(fixture);
    drop(local);
    super::cleanup_db(&path);
}

async fn set_version_meta(raw: &libsql::Connection, values: [Option<&str>; 4]) {
    let keys = [
        "config_version",
        "slash_registry_version",
        "config_revision",
        "slash_registry_revision",
    ];
    for (key, value) in keys.into_iter().zip(values) {
        raw.execute("DELETE FROM meta WHERE key=?1", libsql::params![key])
            .await
            .unwrap();
        if let Some(value) = value {
            raw.execute(
                "INSERT INTO meta(key,value) VALUES(?1,?2)",
                libsql::params![key, value],
            )
            .await
            .unwrap();
        }
    }
}

fn version(config: u64, slash: u64, config_revision: &str, slash_revision: &str) -> VersionState {
    VersionState {
        config,
        slash,
        config_revision: config_revision.into(),
        slash_revision: slash_revision.into(),
    }
}

/// Each version entry point, with the data statements its observation sent.
async fn observe_versions(fixture: &HranaFixture, db: &Db) -> Vec<(VersionState, Vec<String>)> {
    let mut observed = Vec::new();
    for route in 0..3 {
        fixture.state.lock().unwrap().statements.clear();
        let state = match route {
            0 => get_version_state(db).await,
            1 => poll_version_state(db).await,
            _ => get_primary_versions(db).await,
        }
        .unwrap();
        let data = fixture
            .state
            .lock()
            .unwrap()
            .statements
            .iter()
            .filter(|sql| !is_transaction_edge(&sql.trim().to_ascii_uppercase()))
            .cloned()
            .collect();
        observed.push((state, data));
    }
    observed
}

#[tokio::test(flavor = "multi_thread")]
async fn version_observation_reads_counters_and_revisions_in_one_statement() {
    let path = super::tmp_db_path("remote_version_statement");
    let local = Db::open(&path, 2).unwrap();
    let raw = libsql::Builder::new_local(&path)
        .build()
        .await
        .unwrap()
        .connect()
        .unwrap();
    let fixture = HranaFixture::start().await;
    let runtime = DbRuntime::new().unwrap();
    let backing = runtime
        .block_on(libsql::Builder::new_local(&path).build())
        .unwrap()
        .connect()
        .unwrap();
    fixture.state.lock().unwrap().backing = Some((runtime, backing));
    let db = fixture.db();

    let cases = [
        (
            "modern",
            [Some("7"), Some("3"), Some("c-rev"), Some("s-rev")],
            version(7, 3, "c-rev", "s-rev"),
        ),
        (
            "missing revisions",
            [Some("7"), Some("3"), None, None],
            version(7, 3, "legacy:7", "legacy:3"),
        ),
        (
            "missing everything",
            [None, None, None, None],
            version(0, 0, "legacy:0", "legacy:0"),
        ),
        (
            "mixed slash revision",
            [Some("12"), Some("junk"), None, Some("s-only")],
            version(12, 0, "legacy:12", "s-only"),
        ),
        (
            "mixed config revision",
            [Some("12"), Some("5"), Some("c-only"), None],
            version(12, 5, "c-only", "legacy:5"),
        ),
        // A restored or replaced database can keep the counter while
        // publishing a new revision; that must remain a visible change.
        (
            "new config revision, same counter",
            [Some("12"), Some("5"), Some("c-next"), None],
            version(12, 5, "c-next", "legacy:5"),
        ),
        (
            "new slash revision, same counter",
            [Some("12"), Some("5"), Some("c-next"), Some("s-next")],
            version(12, 5, "c-next", "s-next"),
        ),
    ];
    let mut previous: Option<VersionState> = None;
    for (label, values, expected) in cases {
        set_version_meta(&raw, values).await;
        for (route, (state, data)) in observe_versions(&fixture, &db)
            .await
            .into_iter()
            .enumerate()
        {
            assert_eq!(state, expected, "{label} route {route}");
            assert_eq!(
                data.len(),
                1,
                "{label} route {route}: one data statement per observation: {data:?}"
            );
        }
        if let Some(previous) = previous.replace(expected.clone()) {
            assert_ne!(previous, expected, "{label} must be observed as a change");
        }
    }

    drop(db);
    drop(fixture);
    drop(raw);
    drop(local);
    super::cleanup_db(&path);
}

struct ObservedGuildStats {
    counts: (i64, i64, i64),
    statements: Vec<String>,
}

/// The bot `guild_stats` and dashboard `enrolled_guild_stats` reads for each
/// guild, with the ordered SQL trace of each read.
async fn observe_guild_stats(
    fixture: &HranaFixture,
    db: &Db,
    scopes: &[GuildEnrollment],
) -> Vec<ObservedGuildStats> {
    let mut observed = Vec::new();
    for scope in scopes {
        for enrolled in [false, true] {
            fixture.state.lock().unwrap().statements.clear();
            let stats = if enrolled {
                enrolled_guild_stats(db, scope.clone()).await
            } else {
                guild_stats(db, scope.guild_id).await
            }
            .unwrap();
            let statements = std::mem::take(&mut fixture.state.lock().unwrap().statements);
            observed.push(ObservedGuildStats {
                counts: (stats.events, stats.warns, stats.bans),
                statements,
            });
        }
    }
    observed
}

#[tokio::test(flavor = "multi_thread")]
async fn guild_stats_read_all_counts_in_one_statement() {
    let path = super::tmp_db_path("remote_guild_stats");
    let local = Db::open(&path, 2).unwrap();
    let mut scopes = Vec::new();
    for guild in [10, 20, 30] {
        scopes.push(enroll_guild(&local, guild, 100).await.unwrap().unwrap());
    }
    let (fixture, db) = HranaFixture::sqlite_backed_database(&path).await;

    let check = |label: &str, observed: Vec<ObservedGuildStats>, expected: [(i64, i64, i64); 3]| {
        for (index, read) in observed.into_iter().enumerate() {
            let (guild, enrolled) = (index / 2, index % 2 == 1);
            let route = if enrolled { "dashboard" } else { "bot" };
            assert_eq!(
                read.counts, expected[guild],
                "{label} guild {guild} {route}"
            );
            let stats_sql = match read.statements.as_slice() {
                [stats] if !enrolled => stats,
                [begin, enrollment, stats, rollback] if enrolled => {
                    assert_eq!(begin.trim().trim_end_matches(';'), "BEGIN DEFERRED");
                    assert!(enrollment.contains("guild_enrollments"), "{enrollment}");
                    assert_eq!(rollback.trim().trim_end_matches(';'), "ROLLBACK");
                    stats
                }
                statements => {
                    panic!("{label} guild {guild} {route}: unexpected SQL trace {statements:?}");
                }
            };
            let stats_sql = stats_sql.to_ascii_uppercase();
            for table in ["EVENTS", "WARNS", "BANS"] {
                assert!(stats_sql.contains(&format!("FROM {table}")), "{stats_sql}");
            }
        }
    };

    check(
        "empty",
        observe_guild_stats(&fixture, &db, &scopes).await,
        [(0, 0, 0); 3],
    );

    let counted = [
        ("mod_action", json!({"action": "warn"})),
        ("member_ban", json!({})),
        ("automod", json!({"action": "ban"})),
        ("plan_executed", json!({"moderation": true})),
    ];
    let excluded = [
        ("automod", json!({"action": "ban", "dry_run": true})),
        ("automod", json!({"action": "log"})),
        ("message_edit", json!({})),
        ("plan_executed", json!({"moderation": false})),
    ];
    for (event, data) in counted.iter().chain(&excluded) {
        insert_event(&local, 10, event, Some(5), None, data)
            .await
            .unwrap();
    }
    for (event, data) in [
        ("timeout_add", json!({})),
        ("member_kick", json!({})),
        ("system", json!({})),
    ] {
        insert_event(&local, 20, event, None, None, &data)
            .await
            .unwrap();
    }
    local
        .with_conn(|conn| {
            conn.execute_batch_bounded(
                "INSERT INTO events (guild_id,event_type,data,created_at)
                 VALUES (10,'automod','invalid',''), (20,'plan_executed',NULL,'');",
                64 * 1024 * 1024,
            )
        })
        .await
        .unwrap();
    add_warn(&local, 10, 5, 9, "first").await.unwrap();
    add_warn(&local, 10, 6, 9, "second").await.unwrap();
    add_warn(&local, 20, 5, 9, "other guild").await.unwrap();
    record_ban(&local, 10, 5, Some(9), "active").await.unwrap();
    record_ban(&local, 10, 6, None, "lifted").await.unwrap();
    mark_unbanned(&local, 10, 6).await.unwrap();

    // Guild 30 stays empty beside populated guilds.
    let expected = [(4, 2, 2), (2, 1, 0), (0, 0, 0)];
    check(
        "mixed",
        observe_guild_stats(&fixture, &db, &scopes).await,
        expected,
    );
    for (scope, expected) in scopes.iter().zip(expected) {
        let stats = guild_stats(&local, scope.guild_id).await.unwrap();
        assert_eq!((stats.events, stats.warns, stats.bans), expected);
    }

    drop(db);
    drop(fixture);
    drop(local);
    super::cleanup_db(&path);
}

async fn read_primary_fixture(db: &Db) -> Result<()> {
    db.with_conn(|conn| {
        let tx = conn.transaction_primary_snapshot()?;
        tx.query_row("SELECT 1", (), |_| Ok(()))?;
        tx.finish_read()
    })
    .await
}

#[tokio::test(start_paused = true)]
async fn primary_gate_counts_gateway_errors_and_closed_http_connections() {
    for status in [
        Some(StatusCode::BAD_GATEWAY),
        Some(StatusCode::SERVICE_UNAVAILABLE),
        None,
    ] {
        let fixture = HranaFixture::start().await;
        let db = fixture.db();
        for reported in [false, true] {
            {
                let mut state = fixture.state.lock().unwrap();
                state.fail_next_status = status;
                state.reset_next_request = status.is_none();
            }
            let error = get_primary_versions(&db).await.unwrap_err();
            let expected = match status {
                Some(StatusCode::BAD_GATEWAY) => "status=502",
                Some(_) => "status=503",
                None => "connection closed before message completed",
            };
            assert!(format!("{error:#}").contains(expected), "{error:#}");
            assert_eq!(is_primary_outage(&error), reported, "{error:#}");
        }
        let error = get_primary_versions(&db).await.unwrap_err();
        assert!(
            error.to_string().contains("primary is backing off"),
            "{error:#}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn isolated_primary_errors_remain_visible_until_the_gate_reports_an_outage() {
    let fixture = HranaFixture::start().await;
    let db = fixture.db();

    for _ in 0..3 {
        fixture.timeout_next_cursor();
        let error = get_primary_versions(&db).await.unwrap_err();
        assert!(is_remote_interactive_timeout(&error), "{error:#}");
        assert!(
            !is_primary_outage(&error),
            "caller must still report this error: {error:#}"
        );
        tokio::time::advance(Duration::from_secs(15)).await;
    }

    for _ in 0..2 {
        fixture.state.lock().unwrap().fail_next_status = Some(StatusCode::FORBIDDEN);
        let error = get_primary_versions(&db).await.unwrap_err();
        assert!(format!("{error:#}").contains("status=403"), "{error:#}");
        assert!(!is_primary_outage(&error), "{error:#}");
    }
    for _ in 0..2 {
        fixture.state.lock().unwrap().next_cursor_error =
            Some(("SQLITE_ERROR", "no such table: example"));
        let error = read_primary_fixture(&db).await.unwrap_err();
        assert!(format!("{error:#}").contains("no such table"), "{error:#}");
        assert!(!is_primary_outage(&error), "{error:#}");
    }
    read_primary_fixture(&db).await.unwrap();
}

fn install_clock(db: &Db) -> Arc<crate::db_compat::ManualClock> {
    let clock = crate::db_compat::ManualClock::new();
    db.runtime.install_manual_clock(Some(Arc::clone(&clock)));
    clock
}

fn spawn_write(
    db: Db,
    body: impl FnOnce(&mut Connection) -> Result<()> + Send + 'static,
) -> std::sync::mpsc::Receiver<Result<()>> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(db.with_conn_sync(body));
    });
    receiver
}

#[tokio::test(flavor = "multi_thread")]
async fn api_deadline_mid_write_releases_the_lock_for_the_next_writer() {
    let fixture = HranaFixture::start().await;
    let db = fixture.db();
    let clock = install_clock(&db);
    fixture.hang_next_commit();
    let first = spawn_write(db.clone(), |conn| {
        let tx = conn.transaction_primary_write()?;
        tx.commit()
    });
    fixture.wait_until(|state| state.commit_hung).await;
    clock.advance(Duration::from_secs(3) + Duration::from_millis(1));
    let error = first
        .recv_timeout(Duration::from_secs(2))
        .expect("timed-out commit did not return")
        .expect_err("a timed-out commit must not report success");
    assert!(
        is_database_operation_timeout(&error),
        "client deadline must stay visible: {error:#}"
    );
    assert!(
        is_database_commit_indeterminate(&error),
        "lost commit stays indeterminate: {error:#}"
    );
    assert!(
        !is_remote_interactive_timeout(&error),
        "our deadline is not a dead stream: {error:#}"
    );
    assert_eq!(fixture.rollback_requests(), 1);
    assert!(
        !fixture.write_locked(),
        "ROLLBACK must drop the writer lock before the provider steals it"
    );

    let next = spawn_write(db, |conn| {
        let tx = conn.transaction_primary_write()?;
        tx.commit()
    });
    next.recv_timeout(Duration::from_secs(2))
        .expect("next writer stuck behind the abandoned lock")
        .expect("next writer must commit without waiting out a provider steal");
    assert!(!fixture.write_locked());
}

#[tokio::test(flavor = "multi_thread")]
async fn hung_rollback_cannot_outlive_the_short_release_deadline() {
    let fixture = HranaFixture::start().await;
    let db = fixture.db();
    let clock = install_clock(&db);
    fixture.hang_next_statement();
    fixture.hang_rollback();
    let call = spawn_write(db, |conn| {
        let tx = conn.transaction_primary_write()?;
        tx.execute("SELECT 1", ())?;
        tx.commit()
    });
    fixture.wait_until(|state| state.statement_hung).await;
    clock.advance(Duration::from_secs(3) + Duration::from_millis(1));
    fixture.wait_until(|state| state.rollback_started).await;
    assert!(
        call.try_recv().is_err(),
        "the API call must still be inside the short release"
    );
    clock.advance(Duration::from_millis(250));
    assert!(
        call.try_recv().is_err(),
        "release must not finish before its own deadline"
    );
    clock.advance(Duration::from_millis(100));
    let error = call
        .recv_timeout(Duration::from_secs(2))
        .expect("hung ROLLBACK held the API call open")
        .expect_err("the statement deadline must still fail the call");
    assert!(
        is_database_operation_timeout(&error),
        "unexpected: {error:#}"
    );
    assert!(fixture.rollback_requests() >= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn abandoned_write_releases_the_connection_for_the_next_writer() {
    let fixture = HranaFixture::start().await;
    let db = fixture.db();
    // One permit stays held here, so the next write can proceed only after the
    // abandoned call returns its connection.
    let held = Arc::clone(&db.permits).acquire_owned().await.unwrap();
    fixture.hang_next_commit();
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
    let writer = db.clone();
    let first = tokio::spawn(async move {
        let call = writer.with_conn(|conn| {
            let tx = conn.transaction_primary_write()?;
            tx.commit()
        });
        tokio::pin!(call);
        tokio::select! {
            biased;
            _ = cancel_rx => {}
            _ = &mut call => panic!("hung commit returned before the caller stopped waiting"),
        }
    });
    fixture.wait_until(|state| state.commit_hung).await;
    cancel_tx.send(()).unwrap();
    first.await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(2),
        fixture.wait_until(|state| state.rollback_requests >= 1 && !state.write_locked),
    )
    .await
    .expect("abandoned write kept the writer lock");

    let next = db.clone();
    let next = tokio::spawn(async move {
        next.with_conn(|conn| {
            let tx = conn.transaction_primary_write()?;
            tx.commit()
        })
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), next)
        .await
        .expect("next writer did not get a connection")
        .unwrap()
        .expect("next writer must commit");
    drop(held);
}

#[tokio::test(flavor = "multi_thread")]
async fn timed_out_begin_releases_the_lock_for_the_next_writer() {
    let fixture = HranaFixture::start().await;
    let db = fixture.db();
    let clock = install_clock(&db);
    // One spare permit would let the next write bypass a connection that is
    // still checked out. Hold it so the next write needs this call to return.
    let held = Arc::clone(&db.permits).acquire_owned().await.unwrap();
    fixture.hang_next_begin();
    let first = spawn_write(db.clone(), |conn| {
        let _tx = conn.transaction_immediate()?;
        Ok(())
    });
    fixture.wait_until(|state| state.begin_hung).await;
    clock.advance(Duration::from_secs(3) + Duration::from_millis(1));
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if db.runtime.open_release_count() >= 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("begin deadline never entered the short release");
    fixture.release_begin();
    let error = first
        .recv_timeout(Duration::from_secs(2))
        .expect("timed-out begin did not return")
        .expect_err("a timed-out begin must not report success");
    assert!(
        is_database_operation_timeout(&error),
        "client deadline must stay visible: {error:#}"
    );
    assert!(
        error.to_string().contains("begin immediate transaction"),
        "unexpected operation: {error:#}"
    );
    assert!(
        !is_remote_interactive_timeout(&error),
        "our deadline is not a dead stream: {error:#}"
    );
    assert!(
        fixture.rollback_requests() >= 1,
        "the open must roll back on the database runtime"
    );
    assert!(
        !fixture.write_locked(),
        "ROLLBACK must drop the writer lock before the next writer"
    );

    let next = spawn_write(db, |conn| {
        let tx = conn.transaction_primary_write()?;
        tx.commit()
    });
    next.recv_timeout(Duration::from_secs(2))
        .expect("next writer stuck behind the timed-out begin")
        .expect("next writer must commit");
    assert!(!fixture.write_locked());
    drop(held);
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_connection_close_lets_the_next_writer_take_the_lock() {
    let fixture = HranaFixture::start().await;
    let runtime = DbRuntime::with_operation_timeout(Some(Duration::from_secs(3))).unwrap();
    let database = runtime
        .block_on_database(
            "connect",
            libsql::Builder::new_remote(format!("http://{}", fixture.address), "test".into())
                .connector(crate::remote_https::https_connector())
                .build(),
        )
        .unwrap();
    let database = Arc::new(database);
    let connection = Connection::from_database(Arc::clone(&database), runtime.clone()).unwrap();
    {
        let tx = connection.transaction_primary_write().unwrap();
        tx.commit().unwrap();
    }
    runtime.close_connection(connection);
    assert!(
        fixture.closes() >= 1,
        "startup close must be sent on the database runtime"
    );
    let next = Connection::from_database(database, runtime).unwrap();
    let tx = next.transaction_primary_write().unwrap();
    tx.commit().unwrap();
    assert!(!fixture.write_locked());
}

#[tokio::test(flavor = "multi_thread")]
async fn lease_renewal_survives_provider_bulk_writer() {
    let path = super::tmp_db_path("lease_provider_contention");
    let local = Db::open(&path, 2).unwrap();
    let instance = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let refuse = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let initial = crate::control::now_ms();
    assert!(
        claim_active_bot(&local, instance, initial, Arc::clone(&refuse))
            .await
            .unwrap()
    );
    let fixture = HranaFixture::start().await;
    let runtime = DbRuntime::new().unwrap();
    let backing = runtime
        .block_on(libsql::Builder::new_local(&path).build())
        .unwrap()
        .connect()
        .unwrap();
    let model = ProviderModel::new(Duration::from_millis(60), Duration::from_millis(550));
    {
        let mut state = fixture.state.lock().unwrap();
        state.backing = Some((runtime, backing));
        state.provider = Some(Arc::clone(&model));
    }
    let remote = fixture.db();
    remote.arm_writer_term(instance, local.leased_generation().unwrap());
    let bulk = remote.clone();
    let began = Arc::new(tokio::sync::Semaphore::new(0));
    let signal = Arc::clone(&began);
    let writer = tokio::task::spawn_blocking(move || {
        bulk.with_conn_sync(move |conn| {
            for _ in 0..6 {
                // Real interactive core write transactions holding the provider
                // writer ~440 ms (0.8 of its scaled lifetime).
                let tx = conn.transaction_primary_write()?;
                signal.add_permits(1);
                std::thread::sleep(Duration::from_millis(380));
                tx.finish_read()?;
            }
            Ok(())
        })
    });
    began.acquire().await.unwrap().forget();
    let mut kept = 0;
    let mut failure = None;
    for _ in 0..4 {
        match renew_active_bot(
            &remote,
            instance,
            crate::control::now_ms(),
            std::time::Instant::now() + Duration::from_secs(10),
            Arc::clone(&refuse),
        )
        .await
        {
            Ok(true) => kept += 1,
            result => {
                failure = Some(format!("{result:?}"));
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    let writer_result = writer.await.unwrap();
    assert!(writer_result.is_ok(), "bulk writer: {writer_result:?}");
    assert_eq!(
        kept,
        4,
        "lease renewal lost under provider contention: {failure:?}; provider TRANSACTION_TIMEOUT count={}",
        model.timeouts.load(Ordering::SeqCst)
    );
    let state = fixture.state.lock().unwrap();
    let renewal_statements = state
        .statements
        .iter()
        .filter(|sql| sql.contains("'active_bot_lease'") && sql.starts_with("UPDATE"))
        .count();
    assert_eq!(renewal_statements, 4, "one statement per renewal attempt");
    assert_eq!(
        state.begin_sql.len(),
        6,
        "only bulk writer may open interactive transactions"
    );
    eprintln!("lease kept over 4 renewals; 4 CAS statements; 0 renewal BEGIN; provider timeouts=0");
    drop(state);
    let renewed = local
        .with_conn_sync(|conn| meta_string_conn(conn, "active_bot_lease"))
        .unwrap()
        .unwrap();
    let expiry = renewed.rsplit_once(' ').unwrap().1.parse::<i64>().unwrap();
    assert!(
        expiry > initial + ACTIVE_BOT_LEASE_TTL_MS,
        "authority must be extended durably"
    );
    drop(remote);
    drop(fixture);
    drop(local);
    super::cleanup_db(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn lease_renewal_bypasses_exhausted_core_pool() {
    let path = super::tmp_db_path("lease_pool_exhaustion");
    let db = Db::open(&path, 2).unwrap();
    let instance = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let refuse = Arc::new(std::sync::atomic::AtomicBool::new(false));
    assert!(
        claim_active_bot(&db, instance, crate::control::now_ms(), Arc::clone(&refuse))
            .await
            .unwrap()
    );
    let blocked = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let entered = Arc::new(tokio::sync::Semaphore::new(0));
    let mut holders = Vec::new();
    for _ in 0..2 {
        let db = db.clone();
        let blocked = Arc::clone(&blocked);
        let entered = Arc::clone(&entered);
        holders.push(tokio::spawn(async move {
            db.with_conn(move |_| {
                entered.add_permits(1);
                let (lock, ready) = &*blocked;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = ready.wait(released).unwrap();
                }
                Ok(())
            })
            .await
            .unwrap();
        }));
    }
    entered.acquire_many(2).await.unwrap().forget();
    assert_eq!(db.permits.available_permits(), 0);
    let renewed = tokio::time::timeout(
        Duration::from_millis(500),
        renew_active_bot(
            &db,
            instance,
            crate::control::now_ms(),
            std::time::Instant::now() + Duration::from_secs(10),
            refuse,
        ),
    )
    .await;
    *blocked.0.lock().unwrap() = true;
    blocked.1.notify_all();
    for holder in holders {
        holder.await.unwrap();
    }
    assert!(
        matches!(renewed, Ok(Ok(true))),
        "lease renewal waited for exhausted core pool: {renewed:?}"
    );
    drop(db);
    super::cleanup_db(&path);
}

// Claims create authority and may allocate metadata/receipts. Exercise refusal
// through the public owner with real SQLite, also behind the libSQL HTTP client.
async fn lease_claim_admission_cases(use_remote: bool, pause_growth: bool) {
    for takeover in [false, true] {
        let path = super::tmp_db_path("lease_claim_admission");
        let local = Db::open(&path, 2).unwrap();
        let fixture = if use_remote {
            let fixture = HranaFixture::start().await;
            let runtime = DbRuntime::new().unwrap();
            let backing = runtime
                .block_on(libsql::Builder::new_local(&path).build())
                .unwrap()
                .connect()
                .unwrap();
            fixture.state.lock().unwrap().backing = Some((runtime, backing));
            Some(fixture)
        } else {
            None
        };
        let db = fixture
            .as_ref()
            .map_or_else(|| local.clone(), HranaFixture::db);
        let owner = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let claimant = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let refuse = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let now = crate::control::now_ms();
        if takeover {
            assert!(
                claim_active_bot(&db, owner, now, Arc::clone(&refuse))
                    .await
                    .unwrap()
            );
        }
        let stored = || {
            local
                .with_conn_sync(|conn| {
                    Ok((
                        meta_string_conn(conn, "active_bot_lease")?,
                        meta_string_conn(conn, "active_bot_generation")?,
                        conn.query_row("SELECT COUNT(*) FROM mutation_receipts", (), |row| {
                            Ok(row.get::<i64>(0)?)
                        })?,
                    ))
                })
                .unwrap()
        };
        let before = stored();
        let generation = db.leased_generation();
        let expected_error = if pause_growth {
            // Exceed the real bound, even including both remote receipt allowances.
            // Rollback must leave only the shared runtime pause behind.
            let error = db
                .with_conn(|conn| {
                    let mut tx = conn.transaction_primary_write()?;
                    tx.ensure_mutation_receipt()?;
                    tx.execute_bounded(
                        "INSERT INTO meta(key,value) VALUES('underestimated',zeroblob(12582912))",
                        (),
                        0,
                    )?;
                    tx.commit()
                })
                .await
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("growth exceeded the reserved bound"),
                "{error:#}"
            );
            assert!(db.capacity_runtime_status().growth_paused);
            "database growth paused"
        } else {
            "capacity budget exhausted"
        };
        let constrain_capacity = || {
            if !pause_growth {
                // Remote errors discard a pooled connection; constrain its replacement too.
                db.with_conn_sync(|_| Ok(())).unwrap();
                let allocation = local
                    .with_conn_sync(|conn| crate::capacity::allocated_bytes(conn))
                    .unwrap();
                // Some free bytes remain, but not enough for the first remote
                // receipt (or the local metadata write's conservative allowance).
                for conn in db.connections.idle.lock().unwrap().iter_mut() {
                    conn.set_test_capacity_limits(allocation + 1, allocation + 16 * 1024 * 1024);
                }
                // Armed renewal uses its own connection and must face the same headroom.
                db.lease_connection
                    .lock()
                    .unwrap()
                    .as_mut()
                    .expect("lease connection initialized")
                    .set_test_capacity_limits(allocation + 1, allocation + 16 * 1024 * 1024);
            }
        };
        assert_eq!(stored(), before);
        constrain_capacity();
        assert!(
            !claim_active_bot(
                &db,
                claimant,
                now,
                Arc::new(std::sync::atomic::AtomicBool::new(true))
            )
            .await
            .unwrap()
        );
        if takeover {
            // A live foreign owner remains a read-only refusal, even under pressure.
            assert!(
                !claim_active_bot(&db, claimant, now, Arc::clone(&refuse))
                    .await
                    .unwrap()
            );
        }
        let claim_time = if takeover {
            now + ACTIVE_BOT_LEASE_TTL_MS
        } else {
            now
        };
        let error = claim_active_bot(&db, claimant, claim_time, Arc::clone(&refuse))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected_error), "{error:#}");
        assert_eq!(
            stored(),
            before,
            "refusal changed durable authority or receipts"
        );
        assert_eq!(
            db.leased_generation(),
            generation,
            "refusal armed a new writer term"
        );

        // A claimant without this armed owner/generation cannot use renewal to bypass admission.
        constrain_capacity();
        let error = renew_active_bot(
            &db,
            claimant,
            claim_time,
            std::time::Instant::now() + Duration::from_secs(10),
            Arc::clone(&refuse),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains(expected_error), "{error:#}");
        assert_eq!(stored(), before);
        assert_eq!(db.leased_generation(), generation);

        constrain_capacity();
        let error = db
            .with_conn(|conn| {
                conn.execute_rows(
                    "INSERT INTO meta(key,value) VALUES('ordinary','denied')",
                    (),
                    1,
                )?;
                Ok(())
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected_error), "{error:#}");
        assert_eq!(stored(), before);
        if takeover {
            // Renewal of this still-armed generation takes the separate CAS path.
            assert!(
                renew_active_bot(
                    &db,
                    owner,
                    now + 1,
                    std::time::Instant::now() + Duration::from_secs(10),
                    Arc::clone(&refuse)
                )
                .await
                .unwrap()
            );
            assert_eq!(
                stored(),
                (
                    Some(format!("{owner} {}", now + 1 + ACTIVE_BOT_LEASE_TTL_MS)),
                    before.1.clone(),
                    before.2
                )
            );
            assert_eq!(db.leased_generation(), generation);
            assert!(
                !renew_active_bot(
                    &db,
                    owner,
                    now + 1 + ACTIVE_BOT_LEASE_TTL_MS,
                    std::time::Instant::now() + Duration::from_secs(10),
                    Arc::clone(&refuse)
                )
                .await
                .unwrap(),
                "CAS must not revive expired authority"
            );
        }
        drop(db);
        drop(fixture);
        drop(local);
        super::cleanup_db(&path);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn local_lease_claims_obey_growth_pause() {
    lease_claim_admission_cases(false, true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_lease_claims_obey_growth_pause() {
    lease_claim_admission_cases(true, true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn local_lease_claims_obey_capacity_admission() {
    lease_claim_admission_cases(false, false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_lease_claims_obey_receipt_capacity_admission() {
    lease_claim_admission_cases(true, false).await;
}

// Exercise the public renewal owner through both libSQL backends. The remote
// variant uses the real HTTP client with the existing SQLite-backed Hrana fixture.
async fn lease_extend_only_cases(use_remote: bool) {
    let path = super::tmp_db_path("lease_extend_only");
    let local = Db::open(&path, 2).unwrap();
    let fixture = if use_remote {
        let fixture = HranaFixture::start().await;
        let runtime = DbRuntime::new().unwrap();
        let backing = runtime
            .block_on(libsql::Builder::new_local(&path).build())
            .unwrap()
            .connect()
            .unwrap();
        fixture.state.lock().unwrap().backing = Some((runtime, backing));
        Some(fixture)
    } else {
        None
    };
    let db = fixture
        .as_ref()
        .map_or_else(|| local.clone(), HranaFixture::db);
    let owner = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let foreign = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let refuse = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let now = crate::control::now_ms();
    assert!(
        claim_active_bot(&db, owner, now, Arc::clone(&refuse))
            .await
            .unwrap()
    );
    let generation = db.leased_generation().unwrap();
    let stored = || {
        local
            .with_conn_sync(|conn| {
                Ok((
                    meta_string_conn(conn, "active_bot_lease")?.unwrap(),
                    meta_string_conn(conn, "active_bot_generation")?.unwrap(),
                ))
            })
            .unwrap()
    };
    for (label, offset, expected_offset, matches) in [
        ("equal", 0, 0, true),
        ("earlier", -1, 0, true),
        ("backward clock", -30_000, 0, true),
        ("forward clock within lease", 1_000, 1_000, true),
        (
            "expired at exact boundary",
            ACTIVE_BOT_LEASE_TTL_MS + 1_000,
            1_000,
            false,
        ),
        (
            "forward clock beyond lease",
            ACTIVE_BOT_LEASE_TTL_MS + 10_000,
            1_000,
            false,
        ),
    ] {
        assert_eq!(
            renew_active_bot(
                &db,
                owner,
                now + offset,
                std::time::Instant::now() + Duration::from_secs(10),
                Arc::clone(&refuse)
            )
            .await
            .unwrap(),
            matches,
            "{label}"
        );
        assert_eq!(
            stored(),
            (
                format!(
                    "{owner} {}",
                    now + ACTIVE_BOT_LEASE_TTL_MS + expected_offset
                ),
                generation.to_string()
            ),
            "{label}"
        );
    }

    // An unarmed caller takes the interactive claim fallback even when this
    // holder already owns the row. That path must preserve the later expiry too.
    db.arm_writer_term(owner, 0);
    assert_eq!(db.leased_generation(), Some(0));
    assert!(
        renew_active_bot(
            &db,
            owner,
            now,
            std::time::Instant::now() + Duration::from_secs(10),
            Arc::clone(&refuse)
        )
        .await
        .unwrap()
    );
    assert_eq!(
        stored(),
        (
            format!("{owner} {}", now + ACTIVE_BOT_LEASE_TTL_MS + 1_000),
            generation.to_string()
        )
    );

    // Each ownership predicate must independently reject an otherwise valid
    // extension, including a same-holder term mismatch (ABA).
    for (label, holder, term) in [
        ("stale holder", foreign, generation),
        ("stale generation", owner, generation + 1),
    ] {
        db.arm_writer_term(holder, term);
        let before = stored();
        assert!(
            !renew_active_bot(
                &db,
                holder,
                now + 4_000,
                std::time::Instant::now() + Duration::from_secs(10),
                Arc::clone(&refuse)
            )
            .await
            .unwrap(),
            "{label}"
        );
        assert_eq!(stored(), before, "{label}");
    }
    db.arm_writer_term(owner, generation);
    let later = fixture
        .as_ref()
        .map_or_else(|| Db::open(&path, 2).unwrap(), HranaFixture::db);
    later.arm_writer_term(owner, generation);

    // Queue a renewal behind this handle's dedicated connection. Polling once
    // captures the term and submits its blocking job. Another handle writes
    // first, then the queued request must respect that new durable state.
    for (label, pending_offset, later_offset, holder, matches, expected_offset) in [
        (
            "overlapping renewals",
            2_000,
            3_000,
            owner,
            true,
            ACTIVE_BOT_LEASE_TTL_MS + 3_000,
        ),
        (
            "late write after takeover",
            ACTIVE_BOT_LEASE_TTL_MS + 5_000,
            ACTIVE_BOT_LEASE_TTL_MS + 4_000,
            foreign,
            false,
            2 * ACTIVE_BOT_LEASE_TTL_MS + 4_000,
        ),
        (
            "late write after same-holder takeover",
            2 * ACTIVE_BOT_LEASE_TTL_MS + 6_000,
            2 * ACTIVE_BOT_LEASE_TTL_MS + 5_000,
            owner,
            false,
            3 * ACTIVE_BOT_LEASE_TTL_MS + 5_000,
        ),
    ] {
        let blocked = db.clone();
        let (held_tx, held_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            let _slot = blocked.lease_connection.lock().unwrap();
            held_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        held_rx.await.unwrap();
        let mut pending = Box::pin(renew_active_bot(
            &db,
            owner,
            now + pending_offset,
            std::time::Instant::now() + Duration::from_secs(10),
            Arc::clone(&refuse),
        ));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(pending.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let later_result = if matches {
            renew_active_bot(
                &later,
                holder,
                now + later_offset,
                std::time::Instant::now() + Duration::from_secs(10),
                Arc::clone(&refuse),
            )
            .await
        } else {
            claim_active_bot(&later, holder, now + later_offset, Arc::clone(&refuse)).await
        };
        if !matches && holder == owner {
            // Re-arming even this same handle must not rebind the queued request.
            db.arm_writer_term(owner, later.leased_generation().unwrap());
        }
        release_tx.send(()).unwrap();
        blocker.await.unwrap();
        assert!(later_result.unwrap(), "{label}");
        assert_eq!(pending.await.unwrap(), matches, "{label}");
        assert_eq!(
            stored(),
            (
                format!("{holder} {}", now + expected_offset),
                later.leased_generation().unwrap().to_string()
            ),
            "{label}"
        );
    }
    drop(later);
    drop(db);
    drop(fixture);
    drop(local);
    super::cleanup_db(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn lease_extend_only_local() {
    lease_extend_only_cases(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn lease_extend_only_remote() {
    lease_extend_only_cases(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn lease_cas_fences_owner_expiry_restore_and_shutdown() {
    let path = super::tmp_db_path("lease_cas_fences");
    let db = Db::open(&path, 2).unwrap();
    let instance = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let foreign = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let refuse = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let now = crate::control::now_ms();
    assert!(
        claim_active_bot(&db, instance, now, Arc::clone(&refuse))
            .await
            .unwrap()
    );
    let generation = db.leased_generation();
    assert!(
        renew_active_bot(
            &db,
            instance,
            now + 1,
            std::time::Instant::now() + Duration::from_secs(10),
            Arc::clone(&refuse)
        )
        .await
        .unwrap()
    );
    assert_eq!(db.leased_generation(), generation);
    refuse.store(true, Ordering::Release);
    assert!(
        !renew_active_bot(
            &db,
            instance,
            now + 2,
            std::time::Instant::now() + Duration::from_secs(10),
            Arc::clone(&refuse)
        )
        .await
        .unwrap()
    );
    refuse.store(false, Ordering::Release);
    assert!(
        !renew_active_bot(
            &db,
            foreign,
            now + 3,
            std::time::Instant::now() + Duration::from_secs(10),
            Arc::clone(&refuse)
        )
        .await
        .unwrap()
    );
    assert!(
        !renew_active_bot(
            &db,
            instance,
            now + ACTIVE_BOT_LEASE_TTL_MS + 10_000,
            std::time::Instant::now() + Duration::from_secs(10),
            Arc::clone(&refuse)
        )
        .await
        .unwrap()
    );
    db.with_conn(|conn| {
        conn.set_expected_lease_owner(None);
        let tx = conn.transaction_immediate()?;
        write_meta_string_conn(&tx, "restore_fence", "1")?;
        tx.commit()
    })
    .await
    .unwrap();
    assert!(
        !renew_active_bot(
            &db,
            instance,
            now + 4,
            std::time::Instant::now() + Duration::from_secs(10),
            refuse
        )
        .await
        .unwrap()
    );
    drop(db);
    super::cleanup_db(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn launcher_cas_recovers_missing_row_then_resumes_cas() {
    let path = super::tmp_db_path("launcher_cas_missing");
    let local = Db::open(&path, 2).unwrap();
    let fixture = HranaFixture::start().await;
    let runtime = DbRuntime::new().unwrap();
    let backing = runtime
        .block_on(libsql::Builder::new_local(&path).build())
        .unwrap()
        .connect()
        .unwrap();
    fixture.state.lock().unwrap().backing = Some((runtime, backing));
    let remote = fixture.db();
    let control = UpdateControl::from_application(&remote);
    let owner = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    control.arm_launcher_fence(owner);
    assert!(control.renew_launcher_fence().await.unwrap());
    let before = control.launcher_fence_deadline().unwrap();
    local
        .with_conn(|conn| {
            conn.execute_rows("DELETE FROM meta WHERE key='launcher_lease'", (), 1)?;
            Ok(())
        })
        .await
        .unwrap();

    assert!(
        control.renew_launcher_fence_cas().await.unwrap(),
        "a renewal tick must reacquire a deleted launcher fence"
    );
    let recovered = control.launcher_fence_deadline().unwrap();
    assert!(recovered > before);
    let value = local
        .with_conn(|conn| meta_string_conn(conn, "launcher_lease"))
        .await
        .unwrap()
        .unwrap();
    let (stored_owner, expiry) = value.split_once(' ').unwrap();
    assert_eq!(stored_owner, owner);
    assert!(expiry.parse::<i64>().unwrap() > crate::control::now_ms());

    fixture.state.lock().unwrap().statements.clear();
    assert!(control.renew_launcher_fence_cas().await.unwrap());
    assert!(control.launcher_fence_deadline().unwrap() > recovered);
    {
        let state = fixture.state.lock().unwrap();
        assert_eq!(state.statements.len(), 1, "the next tick must use only CAS");
        // libSQL normalizes SQL before sending it over Hrana.
        assert!(state.statements[0].starts_with("UPDATE meta SET value"));
        assert!(state.statements[0].contains("'launcher_lease'"));
    }
    drop(control);
    drop(remote);
    drop(fixture);
    drop(local);
    super::cleanup_db(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn launcher_cas_preserves_existing_fence_semantics() {
    let path = super::tmp_db_path("launcher_cas");
    let db = Db::open(&path, 2).unwrap();
    let owner = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let foreign = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let control = UpdateControl::from_application(&db);
    control.arm_launcher_fence(owner);
    assert!(
        !control.renew_launcher_fence_cas().await.unwrap(),
        "missing row must use acquisition path"
    );
    assert!(control.renew_launcher_fence().await.unwrap());
    assert!(control.renew_launcher_fence_cas().await.unwrap());
    control.arm_launcher_fence(foreign);
    assert!(!control.renew_launcher_fence_cas().await.unwrap());
    control.arm_launcher_fence(owner);
    assert!(control.renew_launcher_fence().await.unwrap());
    let foreign_value = format!("{foreign} {}", crate::control::now_ms() + 60_000);
    let value = foreign_value.clone();
    db.with_conn(move |conn| {
        conn.set_expected_launcher(None);
        let tx = conn.transaction_immediate()?;
        write_meta_string_conn(&tx, "launcher_lease", &value)?;
        tx.commit()
    })
    .await
    .unwrap();
    let before = control.launcher_fence_deadline();
    assert!(!control.renew_launcher_fence_cas().await.unwrap());
    assert_eq!(control.launcher_fence_deadline(), before);
    assert_eq!(
        db.with_conn(|conn| meta_string_conn(conn, "launcher_lease"))
            .await
            .unwrap(),
        Some(foreign_value)
    );
    db.with_conn(move |conn| {
        conn.set_expected_launcher(None);
        let tx = conn.transaction_immediate()?;
        write_meta_string_conn(&tx, "launcher_lease", &format!("{foreign} 0"))?;
        tx.commit()
    })
    .await
    .unwrap();
    assert!(
        control.renew_launcher_fence_cas().await.unwrap(),
        "expired foreign fence may be taken"
    );
    db.with_conn(|conn| {
        conn.set_expected_launcher(None);
        let tx = conn.transaction_immediate()?;
        write_meta_string_conn(&tx, "restore_fence", "1")?;
        tx.commit()
    })
    .await
    .unwrap();
    assert!(!control.renew_launcher_fence_cas().await.unwrap());
    // A restore can remove the row while blocking its recreation.
    let restored = libsql::Builder::new_local(&path).build().await.unwrap();
    restored
        .connect()
        .unwrap()
        .execute("DELETE FROM meta WHERE key='launcher_lease'", ())
        .await
        .unwrap();
    let before = control.launcher_fence_deadline();
    assert!(!control.renew_launcher_fence_cas().await.unwrap());
    assert_eq!(control.launcher_fence_deadline(), before);
    assert!(
        db.with_conn(|conn| meta_string_conn(conn, "launcher_lease"))
            .await
            .unwrap()
            .is_none()
    );
    drop(control);
    drop(db);
    super::cleanup_db(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_lease_cas_discards_and_recreates_the_dedicated_stream() {
    let path = super::tmp_db_path("lease_cas_stream");
    let local = Db::open(&path, 2).unwrap();
    let instance = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let refuse = Arc::new(std::sync::atomic::AtomicBool::new(false));
    assert!(
        claim_active_bot(
            &local,
            instance,
            crate::control::now_ms(),
            Arc::clone(&refuse)
        )
        .await
        .unwrap()
    );
    let fixture = HranaFixture::start().await;
    let runtime = DbRuntime::new().unwrap();
    let backing = runtime
        .block_on(libsql::Builder::new_local(&path).build())
        .unwrap()
        .connect()
        .unwrap();
    fixture.state.lock().unwrap().backing = Some((runtime, backing));
    let remote = fixture.db();
    remote.arm_writer_term(instance, local.leased_generation().unwrap());
    fixture.state.lock().unwrap().fail_next_cas = true;
    let error = renew_active_bot(
        &remote,
        instance,
        crate::control::now_ms(),
        std::time::Instant::now() + Duration::from_secs(10),
        Arc::clone(&refuse),
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("TRANSACTION_TIMEOUT"));
    assert!(remote.lease_connection.lock().unwrap().is_none());
    assert!(
        renew_active_bot(
            &remote,
            instance,
            crate::control::now_ms(),
            std::time::Instant::now() + Duration::from_secs(10),
            Arc::clone(&refuse)
        )
        .await
        .unwrap()
    );
    assert!(remote.lease_connection.lock().unwrap().is_some());
    fixture.expire_latest();
    assert!(
        renew_active_bot(
            &remote,
            instance,
            crate::control::now_ms(),
            std::time::Instant::now() + Duration::from_secs(10),
            refuse
        )
        .await
        .unwrap()
    );
    assert_eq!(
        fixture.expired_requests(),
        0,
        "CAS must reset each remote stream before use"
    );
    assert_eq!(
        fixture.state.lock().unwrap().requests,
        3,
        "each CAS attempt is exactly one HTTP request"
    );
    assert!(fixture.begin_sql().is_empty());
    drop(remote);
    drop(fixture);
    drop(local);
    super::cleanup_db(&path);
}

// Scale 1:1: TTL 20s, margin 5s, first renewal at 5s leaves 10s.
// Keeping actual 3s/4.8s delays ensures the old *real* 2s cap is what fails.
async fn lease_window_provider_case(writer_wait: bool) {
    let path = super::tmp_db_path("lease_window_provider");
    let local = Db::open(&path, 2).unwrap();
    let instance = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let refuse = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let initial = crate::control::now_ms() - 5_000;
    assert!(
        claim_active_bot(&local, instance, initial, Arc::clone(&refuse))
            .await
            .unwrap()
    );
    let fixture = HranaFixture::start().await;
    let runtime = DbRuntime::new().unwrap();
    let backing = runtime
        .block_on(libsql::Builder::new_local(&path).build())
        .unwrap()
        .connect()
        .unwrap();
    let model = ProviderModel::new(
        if writer_wait {
            Duration::from_millis(300)
        } else {
            Duration::from_secs(3)
        },
        Duration::from_secs(5),
    );
    {
        let mut state = fixture.state.lock().unwrap();
        state.backing = Some((runtime, backing));
        state.provider = Some(Arc::clone(&model));
    }
    let remote = fixture.db();
    remote.arm_writer_term(instance, local.leased_generation().unwrap());
    let began = Arc::new(tokio::sync::Semaphore::new(0));
    let writer = if writer_wait {
        let bulk = remote.clone();
        let began = Arc::clone(&began);
        Some(tokio::task::spawn_blocking(move || {
            bulk.with_conn_sync(move |conn| {
                let tx = conn.transaction_primary_write()?;
                began.add_permits(1);
                // 4.5s hold + 0.3s COMMIT request = about 4.8s owning the writer.
                std::thread::sleep(Duration::from_millis(4_500));
                tx.finish_read()
            })
        }))
    } else {
        None
    };
    if writer_wait {
        began.acquire().await.unwrap().forget();
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let started = std::time::Instant::now();
    let result = renew_active_bot(
        &remote,
        instance,
        crate::control::now_ms(),
        deadline,
        refuse,
    )
    .await;
    let renewal_elapsed = started.elapsed();
    if let Some(writer) = writer {
        writer.await.unwrap().unwrap();
    }
    assert!(
        matches!(result, Ok(true)),
        "healthy CAS must fit the 10s authority window: {result:?}"
    );
    if writer_wait {
        assert!(
            renewal_elapsed > Duration::from_secs(2),
            "writer test must exceed the old two-second cap"
        );
    }
    assert!(std::time::Instant::now() < deadline);
    let renewed = local
        .with_conn_sync(|conn| meta_string_conn(conn, "active_bot_lease"))
        .unwrap()
        .unwrap();
    let expiry = renewed.rsplit_once(' ').unwrap().1.parse::<i64>().unwrap();
    assert!(
        expiry > initial + ACTIVE_BOT_LEASE_TTL_MS,
        "confirmed authority extended durably"
    );
    if !writer_wait {
        assert_eq!(model.timeouts.load(Ordering::SeqCst), 0);
    }
    // Loaded hosts may push the holder over the 5s provider lifetime; a killed
    // holder also releases the writer and must allow the autocommit CAS through.
    eprintln!(
        "healthy CAS confirmed; authority extended; 10s window; writer_wait={writer_wait}; provider timeouts={}",
        model.timeouts.load(Ordering::SeqCst)
    );
    drop(remote);
    drop(fixture);
    drop(local);
    super::cleanup_db(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn lease_window_three_second_response_extends_authority() {
    lease_window_provider_case(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn lease_window_four_point_eight_second_writer_extends_authority() {
    lease_window_provider_case(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stalled_snapshot_does_not_drop_the_lease_inside_the_ttl() {
    let path = super::tmp_db_path("lease_stalled_snapshot");
    let local = Db::open(&path, 2).unwrap();
    let instance = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let refuse = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let initial = crate::control::now_ms();
    assert!(
        claim_active_bot(&local, instance, initial, Arc::clone(&refuse))
            .await
            .unwrap()
    );
    let before = local
        .with_conn_sync(|conn| meta_string_conn(conn, "active_bot_lease"))
        .unwrap()
        .unwrap();
    let fixture = HranaFixture::start().await;
    let runtime = DbRuntime::new().unwrap();
    let backing = runtime
        .block_on(libsql::Builder::new_local(&path).build())
        .unwrap()
        .connect()
        .unwrap();
    let model = ProviderModel::new(Duration::ZERO, Duration::from_secs(5));
    {
        let mut state = fixture.state.lock().unwrap();
        state.backing = Some((runtime, backing));
        state.provider = Some(Arc::clone(&model));
    }
    // Longer than the lease TTL, so an unbounded pool call would starve renewal.
    let remote = fixture.db_with_operation_timeout(Duration::from_secs(30));
    remote.arm_writer_term(instance, local.leased_generation().unwrap());
    model.stall_owned.store(true, Ordering::SeqCst);
    let reader = remote.clone();
    let started = std::time::Instant::now();
    let read = tokio::task::spawn_blocking(move || {
        reader.with_conn_sync(|conn| {
            let tx = conn.transaction_primary_snapshot()?;
            tx.query_row("SELECT 1", (), |_| Ok(()))?;
            Ok(())
        })
    });
    fixture.wait_until(|state| state.statement_hung).await;

    let blocked = renew_active_bot(
        &remote,
        instance,
        crate::control::now_ms(),
        std::time::Instant::now() + Duration::from_secs(1),
        Arc::clone(&refuse),
    )
    .await
    .expect_err("a CAS blocked by the stalled snapshot must not report success");
    assert!(
        blocked
            .to_string()
            .contains("active-bot lease renewal timed out"),
        "timed-out renewal must stay retriable: {blocked:#}"
    );
    assert!(
        !is_database_operation_timeout(&blocked),
        "the activation loop exits on a database operation timeout: {blocked:#}"
    );
    assert!(remote.lease_connection.lock().unwrap().is_none());
    let during = local
        .with_conn_sync(|conn| meta_string_conn(conn, "active_bot_lease"))
        .unwrap()
        .unwrap();
    assert_eq!(during, before, "the timed-out CAS must not extend the row");

    let read_error = tokio::time::timeout(Duration::from_secs(8), read)
        .await
        .expect("stalled snapshot was not bounded inside the provider lifetime")
        .unwrap()
        .expect_err("the stalled snapshot must fail on the read budget");
    assert!(
        is_database_operation_timeout(&read_error),
        "snapshot deadline must be the client budget: {read_error:#}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "snapshot held the writer for {:?}",
        started.elapsed()
    );
    assert!(
        model.transactions.lock().unwrap().is_empty(),
        "ROLLBACK must release the provider writer before the lease TTL"
    );

    assert!(
        renew_active_bot(
            &remote,
            instance,
            crate::control::now_ms(),
            std::time::Instant::now() + Duration::from_secs(10),
            refuse,
        )
        .await
        .unwrap(),
        "renewal after the stalled snapshot releases"
    );
    assert!(
        started.elapsed() < Duration::from_millis(u64::try_from(ACTIVE_BOT_LEASE_TTL_MS).unwrap()),
        "lease renewal lost inside the TTL after {:?}",
        started.elapsed()
    );
    let renewed = local
        .with_conn_sync(|conn| meta_string_conn(conn, "active_bot_lease"))
        .unwrap()
        .unwrap();
    let expiry = renewed.rsplit_once(' ').unwrap().1.parse::<i64>().unwrap();
    assert!(expiry > initial + ACTIVE_BOT_LEASE_TTL_MS);
    assert!(remote.lease_connection.lock().unwrap().is_some());
    drop(remote);
    drop(fixture);
    drop(local);
    super::cleanup_db(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn prepared_snapshot_query_stops_inside_the_read_budget() {
    let path = super::tmp_db_path("lease_prepared_snapshot");
    let local = Db::open(&path, 2).unwrap();
    let instance = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let refuse = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let initial = crate::control::now_ms();
    assert!(
        claim_active_bot(&local, instance, initial, Arc::clone(&refuse))
            .await
            .unwrap()
    );
    let fixture = HranaFixture::start().await;
    let runtime = DbRuntime::new().unwrap();
    let backing = runtime
        .block_on(libsql::Builder::new_local(&path).build())
        .unwrap()
        .connect()
        .unwrap();
    let model = ProviderModel::new(Duration::ZERO, Duration::from_secs(5));
    {
        let mut state = fixture.state.lock().unwrap();
        state.backing = Some((runtime, backing));
        state.provider = Some(Arc::clone(&model));
    }
    let remote = fixture.db_with_operation_timeout(Duration::from_secs(30));
    let clock = install_clock(&remote);
    remote.arm_writer_term(instance, local.leased_generation().unwrap());
    let (prepared_tx, prepared_rx) = std::sync::mpsc::channel();
    let (go_tx, go_rx) = std::sync::mpsc::channel();
    let started = std::time::Instant::now();
    let read = spawn_write(remote.clone(), move |conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let mut stmt = tx.prepare("SELECT 1")?;
        prepared_tx.send(()).unwrap();
        go_rx.recv().unwrap();
        let rows = stmt.query_map((), |row| Ok(row.get::<i64>(0)?))?;
        rows.collect::<Result<Vec<_>>>()?;
        tx.finish_read()?;
        Ok(())
    });
    prepared_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("prepare must finish before the query is stalled");
    model.stall_owned.store(true, Ordering::SeqCst);
    go_tx.send(()).unwrap();
    fixture.wait_until(|state| state.statement_hung).await;

    let blocked = renew_active_bot(
        &remote,
        instance,
        crate::control::now_ms(),
        std::time::Instant::now() + Duration::from_secs(1),
        Arc::clone(&refuse),
    )
    .await
    .expect_err("a CAS blocked by the prepared snapshot must not report success");
    assert!(
        blocked
            .to_string()
            .contains("active-bot lease renewal timed out"),
        "timed-out renewal must stay retriable: {blocked:#}"
    );
    assert!(
        !is_database_operation_timeout(&blocked),
        "the activation loop exits on a database operation timeout: {blocked:#}"
    );
    assert!(remote.lease_connection.lock().unwrap().is_none());

    clock.advance(crate::db_compat::INTERACTIVE_READ_BUDGET + Duration::from_millis(50));
    let read_error = read
        .recv_timeout(Duration::from_secs(2))
        .expect("prepared query was not bounded inside the read budget")
        .expect_err("the stalled prepared query must fail on the read budget");
    assert!(
        is_database_operation_timeout(&read_error),
        "prepared query deadline must be the client budget: {read_error:#}"
    );
    assert!(
        !read_error.to_string().contains("30s"),
        "prepared query used the pool deadline: {read_error:#}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "prepared snapshot held the writer for {:?}",
        started.elapsed()
    );
    assert!(
        model.transactions.lock().unwrap().is_empty(),
        "ROLLBACK must release the provider writer before the lease TTL"
    );

    assert!(
        renew_active_bot(
            &remote,
            instance,
            crate::control::now_ms(),
            std::time::Instant::now() + Duration::from_secs(10),
            refuse,
        )
        .await
        .unwrap(),
        "renewal after the prepared snapshot releases"
    );
    assert!(
        started.elapsed() < Duration::from_millis(u64::try_from(ACTIVE_BOT_LEASE_TTL_MS).unwrap()),
        "lease renewal lost inside the TTL after {:?}",
        started.elapsed()
    );
    remote
        .with_conn_sync(|conn| {
            let tx = conn.transaction_primary_snapshot()?;
            tx.query_row("SELECT 1", (), |_| Ok(()))?;
            tx.finish_read()
        })
        .expect("pool must accept a new snapshot after the prepared query times out");
    drop(remote);
    drop(fixture);
    drop(local);
    super::cleanup_db(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_rollback_stops_inside_the_read_budget() {
    let fixture = HranaFixture::start().await;
    let db = fixture.db_with_operation_timeout(Duration::from_secs(30));
    let clock = install_clock(&db);
    let (selected_tx, selected_rx) = std::sync::mpsc::channel();
    let (go_tx, go_rx) = std::sync::mpsc::channel();
    let first_connection = Arc::new(std::sync::Mutex::new(0usize));
    let record_connection = Arc::clone(&first_connection);
    let call = spawn_write(db.clone(), move |conn| {
        *record_connection.lock().unwrap() = conn.pool_identity();
        let tx = conn.transaction_primary_snapshot()?;
        tx.query_row("SELECT 1", (), |_| Ok(()))?;
        selected_tx.send(()).unwrap();
        go_rx.recv().unwrap();
        tx.finish_read()
    });
    selected_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the snapshot read must succeed before ROLLBACK is stalled");
    fixture.hang_rollback();
    go_tx.send(()).unwrap();
    fixture.wait_until(|state| state.rollback_started).await;
    assert!(
        call.try_recv().is_err(),
        "ROLLBACK must still be waiting on the read budget"
    );
    clock.advance(crate::db_compat::INTERACTIVE_READ_BUDGET + Duration::from_millis(50));
    call.recv_timeout(Duration::from_secs(2))
        .expect("stalled ROLLBACK held finish_read open")
        .expect("a stalled final ROLLBACK must not fail the collected snapshot");
    assert!(fixture.rollback_requests() >= 1);
    assert_eq!(
        db.connections.idle.lock().unwrap().len(),
        1,
        "the timed-out rollback must not return its connection to the pool"
    );
    fixture.state.lock().unwrap().hang_rollback = false;
    let stale = *first_connection.lock().unwrap();
    let mut fresh = 0usize;
    db.with_conn_sync(|conn| {
        fresh = conn.pool_identity();
        let tx = conn.transaction_primary_snapshot()?;
        tx.query_row("SELECT 1", (), |_| Ok(()))?;
        tx.finish_read()
    })
    .expect("a later snapshot must run after the stalled ROLLBACK returns");
    assert_ne!(fresh, 0);
    assert_ne!(
        fresh, stale,
        "the next snapshot must not reuse the connection whose ROLLBACK timed out"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_rollback_after_expired_budget_uses_the_short_release() {
    let fixture = HranaFixture::start().await;
    let db = fixture.db_with_operation_timeout(Duration::from_secs(30));
    let clock = install_clock(&db);
    fixture.hang_rollback();
    let first_connection = Arc::new(std::sync::Mutex::new(0usize));
    let record_connection = Arc::clone(&first_connection);
    let snapshot_clock = Arc::clone(&clock);
    let call = spawn_write(db.clone(), move |conn| {
        *record_connection.lock().unwrap() = conn.pool_identity();
        let tx = conn.transaction_primary_snapshot()?;
        tx.query_row("SELECT 1", (), |_| Ok(()))?;
        snapshot_clock.advance(crate::db_compat::INTERACTIVE_READ_BUDGET);
        tx.finish_read()
    });
    tokio::time::timeout(
        Duration::from_secs(6),
        fixture.wait_until(|state| state.rollback_started),
    )
    .await
    .expect("expired snapshot never sent ROLLBACK");
    // Advance the rollback deadline only after the fixture has parked the request.
    let expected_release = Duration::from_millis(300);
    let last_tick = Duration::from_millis(1);
    clock.advance(expected_release - last_tick);
    assert!(
        matches!(
            call.recv_timeout(Duration::from_millis(150)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ),
        "ROLLBACK returned before the short release deadline"
    );
    clock.advance(last_tick);
    call.recv_timeout(Duration::from_secs(2))
        .expect("expired-budget ROLLBACK waited on the pool deadline")
        .expect("a stalled ROLLBACK must not fail the collected snapshot");
    assert_eq!(
        db.connections.idle.lock().unwrap().len(),
        1,
        "the short-release timeout must not return its connection to the pool"
    );
    fixture.state.lock().unwrap().hang_rollback = false;
    let stale = *first_connection.lock().unwrap();
    let mut fresh = 0usize;
    db.with_conn_sync(|conn| {
        fresh = conn.pool_identity();
        let tx = conn.transaction_primary_snapshot()?;
        tx.query_row("SELECT 1", (), |_| Ok(()))?;
        tx.finish_read()
    })
    .expect("a later snapshot must run on a fresh connection");
    assert_ne!(fresh, stale);
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_finish_reuses_the_connection_when_rollback_succeeds() {
    let fixture = HranaFixture::start().await;
    let db = fixture.db();
    let first_connection = Arc::new(std::sync::Mutex::new(0usize));
    let record_connection = Arc::clone(&first_connection);
    db.with_conn_sync(move |conn| {
        *record_connection.lock().unwrap() = conn.pool_identity();
        let tx = conn.transaction_primary_snapshot()?;
        tx.query_row("SELECT 1", (), |_| Ok(()))?;
        tx.finish_read()
    })
    .expect("a fast snapshot rollback must succeed");
    let stale = *first_connection.lock().unwrap();
    let mut fresh = 0usize;
    db.with_conn_sync(|conn| {
        fresh = conn.pool_identity();
        let tx = conn.transaction_primary_snapshot()?;
        tx.query_row("SELECT 1", (), |_| Ok(()))?;
        tx.finish_read()
    })
    .expect("the next snapshot must run");
    assert_eq!(
        fresh, stale,
        "a completed rollback must return the connection to the pool"
    );
    assert_eq!(db.connections.idle.lock().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn prepared_snapshot_closed_stream_skips_rollback() {
    let fixture = HranaFixture::start().await;
    let db = fixture.db();
    let (prepared_tx, prepared_rx) = std::sync::mpsc::channel();
    let (go_tx, go_rx) = std::sync::mpsc::channel();
    let started = std::time::Instant::now();
    let call = spawn_write(db.clone(), move |conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let mut stmt = tx.prepare("SELECT 1")?;
        prepared_tx.send(()).unwrap();
        go_rx.recv().unwrap();
        let query = stmt
            .query_map((), |row| Ok(row.get::<i64>(0)?))?
            .collect::<Result<Vec<_>>>();
        let finished = tx.finish_read();
        query?;
        finished
    });
    prepared_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("prepare must finish before the cursor is closed");
    fixture.timeout_next_cursor();
    go_tx.send(()).unwrap();
    let error = call
        .recv_timeout(Duration::from_secs(2))
        .expect("closed prepared cursor did not return")
        .expect_err("a provider-closed prepared query must surface");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "finish_read must not roll back a closed prepared stream"
    );
    assert!(
        is_remote_interactive_timeout(&error),
        "prepared cursor timeout must stay an interactive-stream failure: {error:#}"
    );
    assert_eq!(fixture.rollback_requests(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_constructor_isolates_the_lease_runtime() {
    let fixture = HranaFixture::start().await;
    let opts = DbOpenOptions {
        path: PathBuf::from("lease-runtime"),
        url: format!("http://{}", fixture.address),
        token: "test".into(),
        pool_size: 2,
        migrate: true,
        read_only: false,
        default_prefix: String::new(),
        retention_days: 0,
    };
    let remote = Db::open_with_bootstrap(&opts, |_| Ok(()), |_| Ok(())).unwrap();
    {
        let lease_runtime = remote.lease_runtime.as_ref().expect("lease runtime");
        let lease = remote.lease_connection.lock().unwrap();
        let lease_conn = lease.as_ref().expect("lease connection");
        assert!(
            lease_conn.runtime_same_owner(lease_runtime),
            "renewal must run on the dedicated lease runtime"
        );
        assert!(
            !lease_conn.runtime_same_owner(&remote.runtime),
            "renewal must not share the pool runtime"
        );
        assert_eq!(
            lease_runtime.operation_limit(),
            Some(crate::db_compat::INTERACTIVE_READ_BUDGET)
        );
        let idle = remote.connections.idle.lock().unwrap();
        assert!(
            idle.iter()
                .all(|conn| conn.runtime_same_owner(&remote.runtime))
        );
        assert!(
            idle.iter()
                .all(|conn| !conn.runtime_same_owner(lease_runtime))
        );
    }

    let path = super::tmp_db_path("lease_runtime_local");
    let local = Db::open(&path, 2).unwrap();
    {
        let lease_runtime = local.lease_runtime.as_ref().expect("local lease runtime");
        let lease = local.lease_connection.lock().unwrap();
        let lease_conn = lease.as_ref().expect("local lease connection");
        assert!(lease_conn.runtime_same_owner(&local.runtime));
        assert!(lease_conn.runtime_same_owner(lease_runtime));
        assert_eq!(lease_runtime.operation_limit(), None);
    }
    drop(local);
    drop(remote);
    drop(fixture);
    super::cleanup_db(&path);
}

fn open_remote_counted(fixture: &HranaFixture, read_only: bool) -> (Db, u64, u64) {
    let probe = Arc::new(StartupProbe::default());
    let _probe = enter_startup_probe(Some(probe.clone()));
    let opts = DbOpenOptions {
        path: PathBuf::from("lease-resources"),
        url: fixture.url(),
        token: "test".into(),
        pool_size: 2,
        migrate: !read_only,
        read_only,
        default_prefix: String::new(),
        retention_days: 0,
    };
    let db = Db::open_with_bootstrap(&opts, |_| Ok(()), |_| Ok(())).unwrap();
    (db, probe.runtime_starts(), probe.connection_opens())
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_read_only_open_builds_no_lease_runtime_or_connection() {
    let fixture = HranaFixture::start().await;
    let (read_only, runtimes, opens) = open_remote_counted(&fixture, true);
    assert_eq!(
        runtimes, 1,
        "read-only remote open starts only the pool runtime"
    );
    assert_eq!(
        opens, 3,
        "bootstrap plus two pooled connections, no lease stream"
    );
    assert!(read_only.lease_runtime.is_none());
    assert!(read_only.lease_connection.lock().unwrap().is_none());

    let (writable, runtimes, opens) = open_remote_counted(&fixture, false);
    assert_eq!(runtimes, 2, "writable remote open keeps the lease runtime");
    assert_eq!(
        opens, 4,
        "bootstrap, two pooled connections and the lease stream"
    );
    assert!(writable.lease_runtime.is_some());
    assert!(writable.lease_connection.lock().unwrap().is_some());

    let requests = fixture.state.lock().unwrap().requests;
    let error = read_only.with_lease_conn(|_| Ok(())).await.unwrap_err();
    assert_eq!(error.to_string(), "read-only database cannot renew a lease");
    assert_eq!(fixture.state.lock().unwrap().requests, requests);
    assert!(read_only.lease_connection.lock().unwrap().is_none());
    drop(read_only);
    drop(writable);
    drop(fixture);
}

#[tokio::test(flavor = "multi_thread")]
async fn launcher_window_three_second_response_renews() {
    // Scale 1:1; a 3s response must fit the unchanged 20s fence TTL.
    let path = super::tmp_db_path("launcher_window_provider");
    let local = Db::open(&path, 2).unwrap();
    let fixture = HranaFixture::start().await;
    let runtime = DbRuntime::new().unwrap();
    let backing = runtime
        .block_on(libsql::Builder::new_local(&path).build())
        .unwrap()
        .connect()
        .unwrap();
    fixture.state.lock().unwrap().backing = Some((runtime, backing));
    let remote = fixture.db();
    let control = UpdateControl::from_application(&remote);
    control.arm_launcher_fence("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    assert!(control.renew_launcher_fence().await.unwrap());
    let before = control.launcher_fence_deadline().unwrap();
    fixture.state.lock().unwrap().provider = Some(ProviderModel::new(
        Duration::from_secs(3),
        Duration::from_secs(5),
    ));
    assert!(control.renew_launcher_fence_cas().await.unwrap());
    assert!(control.launcher_fence_deadline().unwrap() > before);
    eprintln!("launcher 3s response confirmed; fence deadline extended");
    drop(control);
    drop(remote);
    drop(fixture);
    drop(local);
    super::cleanup_db(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn lease_window_expired_never_sends_cas() {
    let fixture = HranaFixture::start().await;
    let remote = fixture.db();
    let instance = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    remote.arm_writer_term(instance, 1);
    let result = renew_active_bot(
        &remote,
        instance,
        crate::control::now_ms(),
        std::time::Instant::now(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(fixture.state.lock().unwrap().requests, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn lease_window_fast_connection_reset_recovers_before_deadline() {
    let path = super::tmp_db_path("lease_reset_provider");
    let local = Db::open(&path, 2).unwrap();
    let instance = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let refuse = Arc::new(std::sync::atomic::AtomicBool::new(false));
    assert!(
        claim_active_bot(
            &local,
            instance,
            crate::control::now_ms(),
            Arc::clone(&refuse)
        )
        .await
        .unwrap()
    );
    let fixture = HranaFixture::start().await;
    let runtime = DbRuntime::new().unwrap();
    let backing = runtime
        .block_on(libsql::Builder::new_local(&path).build())
        .unwrap()
        .connect()
        .unwrap();
    {
        let mut state = fixture.state.lock().unwrap();
        state.backing = Some((runtime, backing));
        state.provider = Some(ProviderModel::new(
            Duration::from_millis(60),
            Duration::from_secs(5),
        ));
        state.reset_next_request = true;
    }
    let remote = fixture.db();
    remote.arm_writer_term(instance, local.leased_generation().unwrap());
    let started = std::time::Instant::now();
    let window = Duration::from_secs(10);
    let deadline = started + window;
    let error = renew_active_bot(
        &remote,
        instance,
        crate::control::now_ms(),
        deadline,
        Arc::clone(&refuse),
    )
    .await
    .unwrap_err();
    assert!(
        !is_database_operation_timeout(&error),
        "reset must be a non-timeout error: {error:#}"
    );
    assert!(remote.lease_connection.lock().unwrap().is_none());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        renew_active_bot(
            &remote,
            instance,
            crate::control::now_ms(),
            deadline,
            refuse
        )
        .await
        .unwrap()
    );
    assert!(
        started.elapsed() < window / 2,
        "retry finishes with ample authority; generous 5s slack"
    );
    eprintln!("fast connection reset retried; CAS confirmed well before 10s deadline");
    drop(remote);
    drop(fixture);
    drop(local);
    super::cleanup_db(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_runtime_outlives_temporary_callers_and_closes_idle_streams() {
    let fixture = HranaFixture::start().await;
    let db = fixture.db();
    let owner = db.runtime.clone();
    let expected_owner = owner.block_on(async { tokio::runtime::Handle::current().id() });
    let db = std::thread::spawn(move || {
        let caller = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        caller.block_on(async {
            db.with_conn_sync(|conn| {
                conn.query_row("SELECT 1", (), |row| {
                    assert_eq!(tokio::runtime::Handle::current().id(), expected_owner);
                    Ok(row.get::<i64>(0)?)
                })
            })
            .unwrap();
        });
        drop(caller);
        db
    })
    .join()
    .unwrap();
    assert_eq!(db.with_conn_sync(read_one).unwrap(), 1);
    // No ambient runtime remains on this thread when the idle connection drops.
    std::thread::spawn(move || drop(db)).join().unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while fixture.state.lock().unwrap().closes == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("idle stream cleanup must reach the server on its surviving owner");
    drop(owner);
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_runtime_stream_errors_discard_transactions_and_allow_reconnect() {
    for message in [
        "Received an invalid baton",
        "tried to send request while no runtime was available",
    ] {
        for prepared in [false, true] {
            let fixture = HranaFixture::start().await;
            let db = fixture.db();
            db.connections.idle.lock().unwrap().truncate(1);
            let state = Arc::clone(&fixture.state);
            let error = db
                .with_conn(move |conn| {
                    let tx = conn.transaction_primary_snapshot()?;
                    let query = if prepared {
                        let mut stmt = tx.prepare("SELECT 1")?;
                        state.lock().unwrap().fail_next_request = Some(message);
                        stmt.query_map((), |row| Ok(row.get::<i64>(0)?)).map(|_| ())
                    } else {
                        state.lock().unwrap().fail_next_request = Some(message);
                        tx.query_row("SELECT 1", (), |_| Ok(()))
                    };
                    let finished = tx.finish_read();
                    query?;
                    finished
                })
                .await
                .expect_err("stream failure must reach the caller without replay");
            assert!(format!("{error:#}").contains(message));
            assert_eq!(
                fixture.rollback_requests(),
                0,
                "lost streams cannot roll back"
            );
            assert_eq!(db.connections.idle.lock().unwrap().len(), 0);
            tokio::time::timeout(Duration::from_secs(2), async {
                while fixture.state.lock().unwrap().closes == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("discarded transaction stream must close on its owner");
            assert_eq!(db.with_conn(read_one).await.unwrap(), 1);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_runtime_failed_read_cleanup_is_nonfatal_and_discards_connection() {
    for message in [
        "Received an invalid baton",
        "tried to send request while no runtime was available",
    ] {
        let fixture = HranaFixture::start().await;
        let db = fixture.db();
        db.connections.idle.lock().unwrap().truncate(1);
        let state = Arc::clone(&fixture.state);
        let value = db
            .with_conn(move |conn| {
                let tx = conn.transaction_primary_snapshot()?;
                let value = tx.query_row("SELECT 1", (), |row| Ok(row.get::<i64>(0)?))?;
                state.lock().unwrap().fail_next_request = Some(message);
                tx.finish_read()?;
                Ok(value)
            })
            .await
            .expect("cleanup failure must not invalidate collected rows");
        assert_eq!(value, 1);
        assert_eq!(db.connections.idle.lock().unwrap().len(), 0);
        assert_eq!(db.with_conn(read_one).await.unwrap(), 1);
    }
}
