//! Loopback Hrana checks for the pooled remote connection lifecycle.
//! Requests traverse libSQL's actual HTTP client and cursor implementation.

use super::super::*;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server, StatusCode};
use serde_json::{Value as Json, json};
use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct HranaState {
    next_baton: usize,
    latest_baton: Option<String>,
    expired: HashSet<String>,
    expire_next_cursor: bool,
    expired_requests: usize,
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
        let state = Arc::new(Mutex::new(HranaState::default()));
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
        Db {
            path: PathBuf::new(),
            remote: true,
            _database: Arc::clone(&database),
            runtime: runtime.clone(),
            startup_deferred: false,
            connections: Arc::new(ConnectionPool {
                idle: Mutex::new(connections),
                database,
                runtime,
                remote: true,
            }),
            permits: Arc::new(tokio::sync::Semaphore::new(2)),
            permit_timeout: Some(Duration::from_secs(3)),
            required_config_version: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            config_write_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            config_floor_lock: Arc::new(parking_lot::Mutex::new(())),
            settings_write_lock: Arc::new(tokio::sync::Mutex::new(())),
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

async fn respond(request: Request<Body>, state: Arc<Mutex<HranaState>>) -> Response<Body> {
    let path = request.uri().path().to_owned();
    let bytes = hyper::body::to_bytes(request.into_body()).await.unwrap();
    let input: Json = serde_json::from_slice(&bytes).unwrap();
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
                            json!({"type": "get_autocommit", "is_autocommit": true})
                        }
                        "close" => {
                            closed = true;
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
