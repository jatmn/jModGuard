//! Private container-use readings for the bot.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use jmodguard::config::ControlSecret;
use jmodguard::read_link::{CONTAINER_READ_PATH, seal_read};
use jmodguard::set_process_ready;
use jmodguard_api::config::ApiProcessConfig;
use tower::ServiceExt;

#[tokio::test]
async fn private_container_use_requires_a_valid_mac() {
    let path =
        std::env::temp_dir().join(format!("container_read_{:032x}.db", rand::random::<u128>()));
    let db = jmodguard::db::Db::open(&path, 2).unwrap();
    let mut cfg = ApiProcessConfig::default();
    cfg.bot.control_secret = ControlSecret::new("x".repeat(32));
    let app = jmodguard_api::startup::build_state(cfg, db).await.unwrap();
    set_process_ready(true);
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
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    set_process_ready(false);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn private_container_use_returns_json_for_a_sealed_request() {
    let path =
        std::env::temp_dir().join(format!("container_read_{:032x}.db", rand::random::<u128>()));
    let db = jmodguard::db::Db::open(&path, 2).unwrap();
    let mut cfg = ApiProcessConfig::default();
    let secret = "y".repeat(32);
    cfg.bot.control_secret = ControlSecret::new(&secret);
    let app = jmodguard_api::startup::build_state(cfg, db).await.unwrap();
    set_process_ready(true);
    let router = jmodguard_api::web::router(app);
    let body = seal_read(secret.as_bytes()).unwrap();
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(CONTAINER_READ_PATH)
                .header("content-type", "application/octet-stream")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    set_process_ready(false);
    let _ = std::fs::remove_file(path);
}
