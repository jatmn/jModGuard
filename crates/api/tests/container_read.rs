//! Private container-use readings on the loopback listener.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use jmodguard::config::ControlSecret;
use jmodguard::read_link::{CONTAINER_READ_PATH, seal_read, spawn_container_read_listener};
use jmodguard::set_process_ready;
use jmodguard_api::config::ApiProcessConfig;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Mutex;
use std::time::Duration;
use tower::ServiceExt;

static PROCESS_READY_TESTS: Mutex<()> = Mutex::new(());

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
    let _ready_guard = PROCESS_READY_TESTS.lock().unwrap();
    set_process_ready(true);
    std::thread::sleep(Duration::from_millis(50));
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
    set_process_ready(false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loopback_listener_rejects_oversized_content_length_without_buffering_body() {
    let secret = "z".repeat(32);
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let bound = spawn_container_read_listener(&addr.to_string(), &secret).expect("listener");
    assert_eq!(bound, addr);
    let _ready_guard = PROCESS_READY_TESTS.lock().unwrap();
    set_process_ready(true);
    std::thread::sleep(Duration::from_millis(50));
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
    set_process_ready(false);
}
