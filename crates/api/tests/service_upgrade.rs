//! Read-only service-upgrade routes go through the normal router and the bot writer.
use jmodguard::config::{ApiProcessConfig, BotProcessConfig, ControlSecret};
use jmodguard::db::{self, Db};
use jmodguard_api::web::state::ApiState;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

async fn owner_cookie(store: &tower_sessions::MemoryStore, uid: u64) -> String {
    let session = tower_sessions::Session::new(None, Arc::new(store.clone()), None);
    session.insert("discord_user_id", uid).await.unwrap();
    session.save().await.unwrap();
    format!("jmodguard_session={}", session.id().unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn read_only_service_upgrade_routes_use_the_writer_link() {
    let path = std::env::temp_dir().join(format!(
        "service-upgrade-{:032x}.db",
        rand::random::<u128>()
    ));
    let bot_db = Db::open(&path, 2).unwrap();
    let mut bot_cfg = BotProcessConfig::default();
    bot_cfg.db.path = path.clone();
    bot_cfg.bot.control_bind = "127.0.0.1:0".into();
    bot_cfg.bot.control_secret = ControlSecret::new("x".repeat(32));
    let bot = jmodguard_bot::state::BotState::new(bot_cfg, bot_db).unwrap();
    bot.activation.mark_lock_held(true);
    assert!(
        db::claim_active_bot(
            &bot.db,
            bot.activation.instance(),
            jmodguard::control::now_ms(),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap()
    );
    let bound = jmodguard_bot::control_server::spawn(Arc::clone(&bot))
        .unwrap()
        .expect("control listener");
    let lease = "ab".repeat(16);
    assert!(
        db::claim_update_lease(&bot.db, "bot", &lease, chrono::Utc::now().timestamp(), 240,)
            .await
            .unwrap()
    );

    let mut cfg = ApiProcessConfig::default();
    cfg.db.path = path.clone();
    cfg.db.pool_size = 2;
    cfg.bot.owner_id = 123;
    cfg.bot.control_target = bound.to_string();
    cfg.bot.control_secret = ControlSecret::new("x".repeat(32));
    cfg.web.base_url = "https://api.example.test".into();
    let opts = cfg.db_open_options();
    assert!(opts.read_only, "production API options stay read-only");
    assert!(!opts.migrate);
    let flag = Arc::new(AtomicBool::new(false));
    db::arm_writer_open_tripwire(Arc::clone(&flag));
    let probe = Arc::new(db::StartupProbe::default());
    let _guard = db::enter_startup_probe(Some(Arc::clone(&probe)));
    let db = jmodguard_api::startup::open_database(&cfg).await.unwrap();
    assert!(db.is_read_only());
    let api = ApiState::new(cfg, db).unwrap();
    unsafe { std::env::set_var("JMODGUARD_UPDATE_APPLY_ENABLED", "1") };
    let store = tower_sessions::MemoryStore::default();
    let owner_cookie = owner_cookie(&store, 123).await;
    api.install_session_store(store);
    let router = jmodguard_api::web::router(Arc::clone(&api));

    let check = tower::ServiceExt::oneshot(
        router.clone(),
        axum::http::Request::builder()
            .method("POST")
            .uri("/api/service-upgrade/check")
            .header("cookie", &owner_cookie)
            .header("origin", "https://api.example.test")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::json!({"channel":"stable"}).to_string(),
            ))
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(check.status(), axum::http::StatusCode::CONFLICT);
    let check_body = axum::body::to_bytes(check.into_body(), 4096).await.unwrap();
    let check_text = String::from_utf8(check_body.to_vec()).unwrap();
    assert!(
        check_text.contains("another update operation is running"),
        "{check_text}"
    );
    assert!(!check_text.contains("read-only"), "{check_text}");

    let apply = tower::ServiceExt::oneshot(
        router.clone(),
        axum::http::Request::builder()
            .method("POST")
            .uri("/api/service-upgrade")
            .header("cookie", &owner_cookie)
            .header("origin", "https://api.example.test")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::json!({
                    "bot_digest": format!("sha256:{}", "a".repeat(64)),
                    "api_digest": format!("sha256:{}", "b".repeat(64)),
                    "confirmed": true
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(apply.status(), axum::http::StatusCode::CONFLICT);
    let apply_body = axum::body::to_bytes(apply.into_body(), 4096).await.unwrap();
    let apply_text = String::from_utf8(apply_body.to_vec()).unwrap();
    assert!(
        apply_text.contains("check again") || apply_text.contains("no checked candidate"),
        "{apply_text}"
    );
    assert!(!apply_text.contains("read-only"), "{apply_text}");

    let cancel = tower::ServiceExt::oneshot(
        router,
        axum::http::Request::builder()
            .method("POST")
            .uri("/api/service-upgrade/cancel")
            .header("cookie", &owner_cookie)
            .header("origin", "https://api.example.test")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::json!({"id":"missing"}).to_string(),
            ))
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(cancel.status(), axum::http::StatusCode::CONFLICT);
    let cancel_body = axum::body::to_bytes(cancel.into_body(), 4096)
        .await
        .unwrap();
    let cancel_text = String::from_utf8(cancel_body.to_vec()).unwrap();
    assert!(
        cancel_text.contains("no coordinated upgrade"),
        "{cancel_text}"
    );
    assert!(!cancel_text.contains("read-only"), "{cancel_text}");
    assert_eq!(probe.immediate(), 0);
    assert!(!flag.load(Ordering::SeqCst));
    drop(api);
    drop(bot);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    jmodguard::tenant::remove_sidecars(&path);
}
