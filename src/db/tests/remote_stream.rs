//! Loopback Hrana checks for the pooled remote connection lifecycle.
//! Requests traverse libSQL's actual HTTP client and cursor implementation.

use super::super::*;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server, StatusCode};
use serde_json::{Value as Json, json};
use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

struct HranaState {
    next_baton: usize,
    latest_baton: Option<String>,
    expired: HashSet<String>,
    expire_next_cursor: bool,
    expired_requests: usize,
    fail_next_commit: bool,
    fail_next_cas: bool,
    commit_timeouts: usize,
    rollback_requests: usize,
    in_transaction: bool,
    begin_sql: Vec<String>,
    timeout_next_cursor: bool,
    hang_next_commit: bool,
    hang_next_statement: bool,
    hang_next_begin: bool,
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
            fail_next_cas: false,
            commit_timeouts: 0,
            rollback_requests: 0,
            in_transaction: false,
            begin_sql: Vec::new(),
            timeout_next_cursor: false,
            hang_next_commit: false,
            hang_next_statement: false,
            hang_next_begin: false,
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
            requests: 0,
            statements: Vec::new(),
        }
    }
}

// A scaled provider: one core writer, per-request latency, and an interactive
// transaction lifetime. The actual statements still execute on real SQLite.
struct ProviderModel {
    writer: Arc<tokio::sync::Semaphore>,
    transactions: Mutex<
        std::collections::HashMap<String, (tokio::sync::OwnedSemaphorePermit, std::time::Instant)>,
    >,
    timed_out: Mutex<HashSet<String>>,
    delay: Duration,
    limit: Duration,
    timeouts: AtomicUsize,
}

impl ProviderModel {
    fn new(delay: Duration, limit: Duration) -> Arc<Self> {
        Arc::new(Self {
            writer: Arc::new(tokio::sync::Semaphore::new(1)),
            transactions: Mutex::new(std::collections::HashMap::new()),
            timed_out: Mutex::new(HashSet::new()),
            delay,
            limit,
            timeouts: AtomicUsize::new(0),
        })
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
    tokio::time::sleep(model.delay).await;
    let requests = if path == "/v3/pipeline" {
        input["requests"].as_array().unwrap().clone()
    } else {
        vec![input.clone()]
    };
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
            .is_some_and(|(_, start)| start.elapsed() > model.limit);
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
        let result = {
            let mut state = state.lock().unwrap();
            state.in_transaction = owns;
            state.write_locked = owns;
            backed_statement_result(request, &mut state)
        };
        if sql.starts_with("BEGIN") {
            state.lock().unwrap().begin_sql.push(sql.clone());
            model
                .transactions
                .lock()
                .unwrap()
                .insert(baton.clone(), (permit.unwrap(), std::time::Instant::now()));
        } else if sql.starts_with("COMMIT") || sql.starts_with("ROLLBACK") {
            model.transactions.lock().unwrap().remove(&baton);
        }
        if path == "/v3/cursor" {
            let mut lines = vec![
                json!({"baton":baton}).to_string(),
                json!({"type":"step_begin","step":0,"cols":result["cols"]}).to_string(),
            ];
            for row in result["rows"].as_array().unwrap() {
                lines.push(json!({"type":"row","row":row}).to_string());
            }
            lines.push(json!({"type":"step_end","step":0,"affected_row_count":result["affected_row_count"]}).to_string());
            return body(StatusCode::OK, format!("{}\n", lines.join("\n")));
        }
        responses.push(json!({"type":"ok","response":{"type":"batch","result":{"step_results":[result],"step_errors":[null]}}}));
    }
    body(
        StatusCode::OK,
        json!({"baton":if closed {None} else {Some(baton)},"results":responses}).to_string(),
    )
}

struct HranaFixture {
    address: std::net::SocketAddr,
    state: Arc<Mutex<HranaState>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl HranaFixture {
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
                    async move { Ok::<_, Infallible>(respond(request, state).await) }
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

    fn db(&self) -> Db {
        let runtime = DbRuntime::with_operation_timeout(Some(Duration::from_secs(3))).unwrap();
        let database = runtime
            .block_on_database(
                "connect",
                libsql::Builder::new_remote(format!("http://{}", self.address), "test".into())
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
            startup_deferred: false,
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
            }),
            lease_connection: Arc::new(Mutex::new(Some(
                Connection::from_database(Arc::clone(&database), runtime.clone()).unwrap(),
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
            fail_lease_cleanup: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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

    fn timeout_next_cursor(&self) {
        self.state.lock().unwrap().timeout_next_cursor = true;
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
        guard.write_locked = true;
        guard.in_transaction = true;
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

async fn respond(request: Request<Body>, state: Arc<Mutex<HranaState>>) -> Response<Body> {
    let path = request.uri().path().to_owned();
    let bytes = hyper::body::to_bytes(request.into_body()).await.unwrap();
    let input: Json = serde_json::from_slice(&bytes).unwrap();
    let provider = state.lock().unwrap().provider.clone();
    if let Some(provider) = provider {
        return provider_response(&path, &input, &state, &provider).await;
    }
    if hold_immediate_begin(&path, &input, &state).await {
        // The lock was taken when the request arrived. This response is that
        // same BEGIN, so it must not be reported as a second writer.
        state.lock().unwrap().skip_immediate_lock = true;
    } else if park_request(&path, &input, &state) {
        std::future::pending::<()>().await;
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
            body(
                StatusCode::OK,
                json!({"baton": if closed { None } else { Some(baton) }, "results": results})
                    .to_string(),
            )
        }
        "/v3/cursor" => {
            if state.timeout_next_cursor {
                state.timeout_next_cursor = false;
                let lines = [
                    json!({"baton": baton}).to_string(),
                    json!({"type": "step_error", "step": 0, "error": {"message": "Transaction timed out", "code": "TRANSACTION_TIMEOUT"}}).to_string(),
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
        let prepared = conn.prepare(sql).await.unwrap();
        let cols = prepared.columns().into_iter().map(|col| json!({"name": col.name(), "decltype": col.decl_type()})).collect::<Vec<_>>();
        let mut values = Vec::new();
        let affected = if prepared.column_count() > 0 {
            let mut rows = prepared.query(args).await.unwrap();
            while let Some(row) = rows.next().await.unwrap() {
                values.push((0..row.column_count()).map(|i| match row.get_value(i).unwrap() {
                    libsql::Value::Integer(n) => json!({"type": "integer", "value": n.to_string()}),
                    libsql::Value::Text(value) => json!({"type": "text", "value": value}),
                    libsql::Value::Null => json!({"type": "null"}),
                    other => panic!("unsupported copy fixture value: {other:?}"),
                }).collect::<Vec<_>>());
            }
            0
        } else {
            prepared.execute(args).await.unwrap()
        };
        json!({"cols": cols, "rows": values, "affected_row_count": affected, "last_insert_rowid": null})
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
            let tx = conn.transaction_primary_read()?;
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
        let tx = conn.transaction_primary_read()?;
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
        let tx = conn.transaction_primary_read()?;
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
        let tx = conn.transaction_primary_read()?;
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
            let tx = conn.transaction_primary_read()?;
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
            let tx = conn.transaction_primary_read()?;
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
        let tx = conn.transaction_primary_read()?;
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
        let tx = connection.transaction_primary_read().unwrap();
        tx.commit().unwrap();
    }
    runtime.close_connection(connection);
    assert!(
        fixture.closes() >= 1,
        "startup close must be sent on the database runtime"
    );
    let next = Connection::from_database(database, runtime).unwrap();
    let tx = next.transaction_primary_read().unwrap();
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
                let tx = conn.transaction_primary_read()?;
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
        renew_active_bot(&db, instance, crate::control::now_ms(), refuse),
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
        renew_active_bot(&db, instance, now + 1, Arc::clone(&refuse))
            .await
            .unwrap()
    );
    assert_eq!(db.leased_generation(), generation);
    refuse.store(true, Ordering::Release);
    assert!(
        !renew_active_bot(&db, instance, now + 2, Arc::clone(&refuse))
            .await
            .unwrap()
    );
    refuse.store(false, Ordering::Release);
    assert!(
        !renew_active_bot(&db, foreign, now + 3, Arc::clone(&refuse))
            .await
            .unwrap()
    );
    assert!(
        !renew_active_bot(&db, instance, now + 30_000, Arc::clone(&refuse))
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
        !renew_active_bot(&db, instance, now + 4, refuse)
            .await
            .unwrap()
    );
    drop(db);
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
    db.with_conn(move |conn| {
        conn.set_expected_launcher(None);
        let tx = conn.transaction_immediate()?;
        write_meta_string_conn(&tx, "launcher_lease", &format!("{owner} 0"))?;
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
            Arc::clone(&refuse)
        )
        .await
        .unwrap()
    );
    assert!(remote.lease_connection.lock().unwrap().is_some());
    fixture.expire_latest();
    assert!(
        renew_active_bot(&remote, instance, crate::control::now_ms(), refuse)
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
