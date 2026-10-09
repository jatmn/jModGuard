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
        }
    }
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
        let lease_owner = Arc::new(Mutex::new(None));
        Db {
            path: PathBuf::new(),
            remote: true,
            read_only: false,
            _database: Arc::clone(&database),
            runtime: runtime.clone(),
            startup_deferred: false,
            connections: Arc::new(ConnectionPool {
                idle: Mutex::new(connections),
                database,
                runtime,
                remote: true,
                read_only: false,
                busy_timeout_ms: 5_000,
                lease_owner: Arc::clone(&lease_owner),
                commit_lease_probe: Arc::new(Mutex::new(None)),
            }),
            permits: Arc::new(tokio::sync::Semaphore::new(2)),
            permit_timeout: Some(Duration::from_secs(3)),
            required_config_version: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            config_write_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            config_floor_lock: Arc::new(parking_lot::Mutex::new(())),
            settings_write_lock: Arc::new(tokio::sync::Mutex::new(())),
            lease_owner,
            #[cfg(test)]
            before_final_lease_reread: Arc::new(Mutex::new(None)),
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
    if hold_immediate_begin(&path, &input, &state).await {
        // The lock was taken when the request arrived. This response is that
        // same BEGIN, so it must not be reported as a second writer.
        state.lock().unwrap().skip_immediate_lock = true;
    } else if park_request(&path, &input, &state) {
        std::future::pending::<()>().await;
    }
    let supplied_baton = input["baton"].as_str();
    let mut state = state.lock().unwrap();
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
    json!({
        "type": "batch",
        "result": {
            "step_results": [{
                "cols": [],
                "rows": [],
                "affected_row_count": 0,
                "last_insert_rowid": null
            }],
            "step_errors": [null]
        }
    })
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
