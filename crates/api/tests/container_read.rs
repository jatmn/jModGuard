//! Private container-use readings on the loopback listener.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use jmodguard::config::ControlSecret;
use jmodguard::db::{self, Db, ResourceProcess};
use jmodguard::read_link::{
    CONTAINER_READ_PATH, RESOURCE_READ_PATH, ResourceReadResponse, seal_read, seal_read_route,
    spawn_container_read_listener, try_read_link,
};
use jmodguard::resources::{MINUTE_SECS, RECENT_MINUTES, Rollup, publish_recent};
use jmodguard::set_process_ready;
use jmodguard_api::config::ApiProcessConfig;
use jmodguard_api::process_ready_test_lock::ProcessReadyTestGuard;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;
use tower::ServiceExt;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_web_router_returns_404_for_container_read() {
    let path = std::env::temp_dir().join(format!(
        "container_read_404_{:032x}",
        rand::random::<u128>()
    ));
    let db = jmodguard::db::Db::open(&path, 2).unwrap();
    let mut cfg = ApiProcessConfig::default();
    cfg.bot.control_secret = ControlSecret::new("x".repeat(32));
    let app = jmodguard_api::startup::build_state(cfg, db).await.unwrap();
    let router = jmodguard_api::web::router(app);
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(CONTAINER_READ_PATH)
                .body(Body::from("bad"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let _ = std::fs::remove_file(path);
}

#[test]
fn loopback_listener_rejects_a_public_bind_without_crashing() {
    assert!(spawn_container_read_listener("0.0.0.0:8083", &"x".repeat(32)).is_none());
    assert!(spawn_container_read_listener("10.0.0.1:8083", &"x".repeat(32)).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_container_use_returns_json_for_a_sealed_request() {
    let secret = "y".repeat(32);
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let bound = spawn_container_read_listener(&addr.to_string(), &secret).expect("listener");
    assert_eq!(bound, addr);
    let _ready_guard = ProcessReadyTestGuard::acquire().await;
    set_process_ready(true);
    let body = seal_read(secret.as_bytes()).unwrap();
    let mut stream = TcpStream::connect(addr).unwrap();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
        path = CONTAINER_READ_PATH,
        len = body.len(),
    );
    stream.write_all(request.as_bytes()).unwrap();
    stream.write_all(&body).unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf);
    assert!(text.contains("HTTP/1.1 200"), "{text}");
    let (_, body) = text.split_once("\r\n\r\n").expect("http body");
    let json: jmodguard::read_link::ContainerReadResponse =
        serde_json::from_str(body.trim()).expect("json body");
    assert_eq!(
        json.copy,
        format!("api pid {}", std::process::id()),
        "copy labels the answering API process"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loopback_listener_rejects_oversized_content_length_without_buffering_body() {
    let secret = "z".repeat(32);
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let bound = spawn_container_read_listener(&addr.to_string(), &secret).expect("listener");
    assert_eq!(bound, addr);
    let _ready_guard = ProcessReadyTestGuard::acquire().await;
    set_process_ready(true);
    let mut stream = TcpStream::connect(addr).unwrap();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 5000\r\nConnection: close\r\n\r\n",
        path = CONTAINER_READ_PATH,
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf);
    assert!(text.contains("HTTP/1.1 413"), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_container_use_accepts_a_delayed_body() {
    let secret = "d".repeat(32);
    let addr = spawn_container_read_listener("127.0.0.1:0", &secret).expect("listener");
    let _ready_guard = ProcessReadyTestGuard::acquire().await;
    set_process_ready(true);
    let body = seal_read(secret.as_bytes()).unwrap();
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let headers = format!(
        "POST {CONTAINER_READ_PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len(),
    );
    stream.write_all(headers.as_bytes()).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    // Read even if the peer has closed, so failure reports the missing HTTP 200.
    let body_write = stream.write_all(&body);
    let mut response = Vec::new();
    let response_read = stream.read_to_end(&mut response);
    let text = String::from_utf8_lossy(&response);
    assert!(
        text.starts_with("HTTP/1.1 200 OK\r\n"),
        "expected HTTP 200 for delayed body: {text:?}; write: {body_write:?}; read: {response_read:?}"
    );
    body_write.unwrap();
    response_read.unwrap();
    let (_, json) = text.split_once("\r\n\r\n").expect("http body");
    let _: jmodguard::read_link::ContainerReadResponse = serde_json::from_str(json).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_resource_rollups_select_unsaved_minutes_and_replay_failed_writes() {
    let secret = "r".repeat(32);
    let addr = spawn_container_read_listener("127.0.0.1:0", &secret).expect("listener");
    let _ready_guard = ProcessReadyTestGuard::acquire().await;
    set_process_ready(true);
    let first = 1_800_000_000;
    let minutes: Vec<_> = (0..RECENT_MINUTES)
        .map(|index| {
            let mut minute = Rollup::empty(first + index as i64 * MINUTE_SECS, MINUTE_SECS);
            minute.cpu_avg_tenths = Some(108);
            minute.cpu_max_tenths = Some(219);
            minute.memory_avg_bytes = Some(32 * 1024 * 1024);
            minute.memory_max_bytes = Some(39 * 1024 * 1024);
            minute.data.minutes = 1;
            minute.data.cpu_samples = 12;
            minute.data.memory_samples = 12;
            minute.data.starts.push(first * 1000);
            minute
                .data
                .counters
                .insert("api_requests".into(), 107 + index as u64);
            minute.data.labelled.insert(
                "http_requests".into(),
                [
                    ("POST /api/me".into(), 25),
                    ("GET /api/guilds/config".into(), 37),
                    ("POST /api/audit/events".into(), 45),
                ]
                .into(),
            );
            minute.data.latency = vec![0, 12, 11, 47, 35, 2, 0, 0, 0, 0, 0, 0, 0];
            publish_recent(minute.clone());
            minute
        })
        .collect();
    let last = minutes.last().unwrap().start_secs;
    let link = try_read_link(&addr.to_string(), &secret)
        .unwrap()
        .expect("read link");
    let fetch = |cursor| {
        let link = link.clone();
        async move {
            tokio::task::spawn_blocking(move || match cursor {
                Some(saved) => link.fetch_resource_rollups_after_blocking(Some(saved)),
                None => link.fetch_resource_rollups_blocking(),
            })
            .await
            .unwrap()
            .expect("resource rollups")
        }
    };
    let full = fetch(None).await;
    assert_eq!(
        full.minutes, minutes,
        "old clients receive the full bounded buffer"
    );
    assert_eq!(full.copy, format!("api pid {}", std::process::id()));
    assert_eq!(
        fetch(Some(first - MINUTE_SECS)).await,
        full,
        "bounded catch-up"
    );
    let one = fetch(Some(last - MINUTE_SECS)).await;
    assert_eq!(
        one.minutes.len(),
        1,
        "only the unsaved minute belongs in the reply"
    );
    assert_eq!(one.minutes[0], minutes[RECENT_MINUTES - 1]);
    let idle = fetch(Some(last)).await;
    assert!(idle.minutes.is_empty());
    assert_eq!(idle.copy, full.copy);
    let encoded_len = |response: &ResourceReadResponse| serde_json::to_vec(response).unwrap().len();
    eprintln!(
        "Resource reply JSON: full={} bytes one={} bytes idle={} bytes",
        encoded_len(&full),
        encoded_len(&one),
        encoded_len(&idle)
    );
    assert!(encoded_len(&one) < encoded_len(&full) / 20);
    assert!(encoded_len(&idle) < encoded_len(&one));

    // Use deferred referential integrity to fail the transaction at COMMIT,
    // after the real recorder has executed its minute and hour writes.
    let directory =
        std::env::temp_dir().join(format!("resource_cursor_{:032x}", rand::random::<u128>()));
    std::fs::create_dir_all(&directory).unwrap();
    let db = Db::open(&directory.join("core.db"), 1).unwrap();
    let write = |rollups: Vec<Rollup>| {
        db::record_resource_rollups(
            &db,
            rollups
                .into_iter()
                .map(|minute| (ResourceProcess::Api, minute))
                .collect(),
            None,
            last + MINUTE_SECS,
        )
    };
    let unsaved = RECENT_MINUTES - 3;
    let saved = write(full.minutes[..unsaved].to_vec())
        .await
        .unwrap()
        .api_minute_starts
        .into_iter()
        .max()
        .unwrap();
    let pending = fetch(Some(saved)).await;
    assert_eq!(pending.minutes, minutes[unsaved..]);
    db.with_conn(|conn| {
        conn.execute_batch_bounded(
            "CREATE TABLE missing_parent (id INTEGER PRIMARY KEY);
             CREATE TABLE deferred_reference (
                 id INTEGER REFERENCES missing_parent(id) DEFERRABLE INITIALLY DEFERRED
             );
             CREATE TRIGGER fail_resource_commit AFTER INSERT ON resource_rollups
                 WHEN NEW.process = 'api' AND NEW.span_secs = 60
             BEGIN INSERT INTO deferred_reference VALUES (NEW.start_secs); END;",
            64 * 1024,
        )
    })
    .await
    .unwrap();
    let error = write(pending.minutes.clone()).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("FOREIGN KEY constraint failed"),
        "{error:#}"
    );
    let kept = db::resource_records(&db, MINUTE_SECS, first, last + MINUTE_SECS)
        .await
        .unwrap();
    assert_eq!(kept.api, minutes[..unsaved]);
    db.with_conn(|conn| conn.execute_batch_bounded("DROP TRIGGER fail_resource_commit;", 4096))
        .await
        .unwrap();

    let replay = fetch(Some(saved)).await;
    assert_eq!(
        replay, pending,
        "failed commit leaves every pending minute available"
    );
    let write = write(replay.minutes).await.unwrap();
    assert_eq!(write.minute_rows_inserted, 3);
    assert_eq!(
        write.api_minute_starts,
        minutes[unsaved..]
            .iter()
            .map(|m| m.start_secs)
            .collect::<Vec<_>>()
    );
    assert!(
        fetch(write.api_minute_starts.into_iter().max())
            .await
            .minutes
            .is_empty()
    );
    drop(db);
    std::fs::remove_dir_all(directory).unwrap();

    // A mismatched route and an invalid proof are refused by the real listener.
    let route_mismatch = seal_read(secret.as_bytes()).unwrap();
    let invalid_cursor = seal_read_route(secret.as_bytes(), RESOURCE_READ_PATH).unwrap();
    for (body, header) in [
        (route_mismatch, ""),
        (
            invalid_cursor,
            "X-JModGuard-Saved-Through: 1800000000:invalid\r\n",
        ),
    ] {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let request = format!(
            "POST {RESOURCE_READ_PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n{header}Connection: close\r\n\r\n",
            body.len(),
        );
        stream.write_all(request.as_bytes()).unwrap();
        stream.write_all(&body).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 401"), "{text}");
    }
}
