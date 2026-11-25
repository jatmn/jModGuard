//! Read-only service-upgrade and update routes use the normal router and bot writer.
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use jmodguard::config::{ApiProcessConfig, BotProcessConfig, ControlSecret};
use jmodguard::db::{self, Db};
use jmodguard_api::web::state::ApiState;
use serde_json::{Value, json};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tower::ServiceExt;

const UPDATE_APPLY_ENABLED: &str = "JMODGUARD_UPDATE_APPLY_ENABLED";

/// The production router reads the rollout flag from the process environment.
fn in_rollout_test_process(test: &str, rollout: &str) -> bool {
    const CHILD: &str = "JMODGUARD_SERVICE_UPGRADE_TEST_CHILD";
    if std::env::var(CHILD).as_deref() == Ok(test) {
        assert_eq!(std::env::var(UPDATE_APPLY_ENABLED).as_deref(), Ok(rollout));
        return true;
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test])
        .env(CHILD, test)
        .env(UPDATE_APPLY_ENABLED, rollout)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed;"),
        "{test}: {}\n{stdout}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

async fn session_cookie(store: &tower_sessions::MemoryStore, uid: u64) -> String {
    let session = tower_sessions::Session::new(None, Arc::new(store.clone()), None);
    session.insert("discord_user_id", uid).await.unwrap();
    session.save().await.unwrap();
    format!("jmodguard_session={}", session.id().unwrap())
}

struct RecoveryPage {
    path: std::path::PathBuf,
    router: axum::Router,
    cookie: String,
    store: tower_sessions::MemoryStore,
    probe: Arc<db::StartupProbe>,
    guard: db::StartupProbeGuard,
    flag: Arc<AtomicBool>,
    api: Arc<ApiState>,
    bot: Arc<jmodguard_bot::state::BotState>,
}

impl RecoveryPage {
    async fn open() -> Self {
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
        let guard = db::enter_startup_probe(Some(Arc::clone(&probe)));
        let db = jmodguard_api::startup::open_database(&cfg).await.unwrap();
        assert!(db.is_read_only());
        let api = ApiState::new(cfg, db).unwrap();
        let store = tower_sessions::MemoryStore::default();
        let cookie = session_cookie(&store, api.cfg.bot.owner_id).await;
        api.install_session_store(store.clone());
        let router = jmodguard_api::web::router(Arc::clone(&api));
        Self {
            path,
            router,
            cookie,
            store,
            probe,
            guard,
            flag,
            api,
            bot,
        }
    }

    fn finish(self) {
        assert_eq!(self.probe.immediate(), 0);
        assert!(!self.flag.load(Ordering::SeqCst));
        let path = self.path.clone();
        drop(self.api);
        drop(self.bot);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        jmodguard::tenant::remove_sidecars(&path);
        drop(self.guard);
    }
}

async fn post_apply(page: &RecoveryPage) -> (axum::http::StatusCode, String) {
    let apply = tower::ServiceExt::oneshot(
        page.router.clone(),
        axum::http::Request::builder()
            .method("POST")
            .uri("/api/service-upgrade")
            .header("cookie", &page.cookie)
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
    let status = apply.status();
    let apply_body = axum::body::to_bytes(apply.into_body(), 4096).await.unwrap();
    (status, String::from_utf8(apply_body.to_vec()).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_service_upgrade_routes_use_the_writer_link() {
    if !in_rollout_test_process("read_only_service_upgrade_routes_use_the_writer_link", "1") {
        return;
    }
    let page = RecoveryPage::open().await;
    let router = page.router.clone();
    let owner_cookie = page.cookie.clone();

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

    let (status, apply_text) = post_apply(&page).await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
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
    page.finish();
}

async fn post_update(
    page: &RecoveryPage,
    route: &str,
    body: &Value,
    cookie: Option<&str>,
    origin: Option<&str>,
) -> (StatusCode, Value) {
    let mut request =
        Request::post(format!("/api/updates/{route}")).header("content-type", "application/json");
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    if let Some(origin) = origin {
        request = request.header("origin", origin);
    }
    let response = page
        .router
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn update_state(db: &Db) -> (Value, Option<u64>) {
    (
        serde_json::to_value(db::component_updates(db).await.unwrap()).unwrap(),
        db::update_alert_channel(db).await.unwrap(),
    )
}

async fn assert_update_route_guards(page: &RecoveryPage, route: &str, body: &Value) {
    let origin = page.api.cfg.web.base_url.as_str();
    let foreign = origin.replacen("https://", "http://", 1);
    let non_owner = session_cookie(&page.store, page.api.cfg.bot.owner_id + 1).await;
    let owner = Some(page.cookie.as_str());
    let before = update_state(&page.bot.db).await;
    for (cookie, origin, status, error) in [
        (
            None,
            Some(origin),
            StatusCode::UNAUTHORIZED,
            "sign in required",
        ),
        (
            Some(non_owner.as_str()),
            Some(origin),
            StatusCode::FORBIDDEN,
            "bot owner required",
        ),
        (
            owner,
            None,
            StatusCode::FORBIDDEN,
            "untrusted request origin",
        ),
        (
            owner,
            Some(foreign.as_str()),
            StatusCode::FORBIDDEN,
            "untrusted request origin",
        ),
    ] {
        let (actual_status, response) = post_update(page, route, body, cookie, origin).await;
        let after = update_state(&page.bot.db).await;
        assert_eq!(
            actual_status, status,
            "{route}: expected {error}, got {response}"
        );
        assert_eq!(response["error"], error, "{route}");
        assert_eq!(after, before, "{route}: {error}");
    }
}

async fn post_owner_update(page: &RecoveryPage, route: &str, body: &Value) -> (StatusCode, Value) {
    post_update(
        page,
        route,
        body,
        Some(&page.cookie),
        Some(&page.api.cfg.web.base_url),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_check_requires_owner_and_trusted_origin() {
    if !in_rollout_test_process("update_check_requires_owner_and_trusted_origin", "0") {
        return;
    }
    let page = RecoveryPage::open().await;
    assert_eq!(
        std::env::var(UPDATE_APPLY_ENABLED).as_deref(),
        Ok("0"),
        "opening the fixture must preserve the inherited rollout setting"
    );
    assert_update_route_guards(&page, "check", &json!({"component":"bot"})).await;
    let (status, response) =
        post_owner_update(&page, "check", &json!({"component":"unsupported"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
    assert_eq!(response["error"], "invalid component");
    page.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn update_policy_requires_owner_and_trusted_origin() {
    let page = RecoveryPage::open().await;
    let body = json!({"component":"api","channel":"stable","cadence":"off"});
    assert_update_route_guards(&page, "policy", &body).await;
    let (status, response) = post_owner_update(&page, "policy", &body).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(
        db::component_update(&page.bot.db, "api")
            .await
            .unwrap()
            .channel,
        "stable"
    );
    page.finish();
}

#[tokio::test(flavor = "multi_thread")]
async fn update_alert_channel_requires_owner_and_trusted_origin() {
    let page = RecoveryPage::open().await;
    let body = json!({"channel_id":"42"});
    assert_update_route_guards(&page, "alert-channel", &body).await;
    let (status, response) = post_owner_update(&page, "alert-channel", &body).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(
        db::update_alert_channel(&page.bot.db).await.unwrap(),
        Some(42)
    );
    page.finish();
}

async fn checked_api_candidate(
    page: &RecoveryPage,
    self_update_enabled: bool,
) -> db::ComponentRelease {
    let db = &page.bot.db;
    let now = chrono::Utc::now().timestamp();
    let schema = db::CORE_SCHEMA_VERSION;
    let guild_schema = db::guild_data::SCHEMA_VERSION;
    let candidate = db::ComponentRelease {
        component: "api".into(),
        channel: "latest".into(),
        release_id: "update-fixture".into(),
        source_sha: "a".repeat(40),
        published_at: chrono::Utc::now().to_rfc3339(),
        image: "ghcr.io/jatmn/jmodguard-api".into(),
        digest: format!("sha256:{}", "a".repeat(64)),
        min_schema: schema,
        max_schema: schema,
        result_schema: schema,
        min_api: 1,
        max_api: 1,
        launcher_protocol: 1,
        launcher: Some(db::LauncherCompatibility {
            revision: db::LAUNCHER_REVISION,
            min_revision: db::REQUIRED_LAUNCHER_REVISION,
        }),
        base_abi: "debian-bookworm-amd64".into(),
        upgrade: Some(db::UpgradeCompatibility {
            control_protocol: db::UPDATE_CONTROL_PROTOCOL,
            min_core: schema,
            min_guild: guild_schema,
            guild_schema,
        }),
    };
    for (role, enabled) in [("bot", true), ("api", self_update_enabled)] {
        db::record_component_running(
            db,
            role,
            None,
            db::ComponentObservation {
                source_sha: "b".repeat(40),
                executable_sha256: "c".repeat(64),
                admin_sha256: None,
                base_image_sha: "b".repeat(40),
                started_at: now,
                min_schema: schema,
                max_schema: schema,
                api_capability: 1,
                self_update_enabled: enabled,
                base_abi: candidate.base_abi.clone(),
                guild_schema: Some(guild_schema),
                update_protocol: db::UPDATE_CONTROL_PROTOCOL,
                launcher_revision: db::LAUNCHER_REVISION,
                baked_launcher_revision: db::LAUNCHER_REVISION,
            },
            now,
        )
        .await
        .unwrap();
    }
    db::record_update_check(db, "api", "latest", Some(candidate.clone()), None, now)
        .await
        .unwrap();
    candidate
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_install_requires_owner_and_trusted_origin() {
    if !in_rollout_test_process("update_install_requires_owner_and_trusted_origin", "1") {
        return;
    }
    let page = RecoveryPage::open().await;
    let candidate = checked_api_candidate(&page, true).await;
    let body = json!({"component":"api","digest":candidate.digest});
    assert_update_route_guards(&page, "install", &body).await;
    let (status, response) = post_owner_update(&page, "install", &body).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["queued"], true);
    let state = db::component_update(&page.bot.db, "api").await.unwrap();
    assert_eq!(state.state, "queued");
    assert_eq!(state.desired, Some(candidate));
    page.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_install_refuses_when_service_self_update_is_disabled() {
    if !in_rollout_test_process(
        "update_install_refuses_when_service_self_update_is_disabled",
        "1",
    ) {
        return;
    }
    let page = RecoveryPage::open().await;
    let candidate = checked_api_candidate(&page, false).await;
    let before = update_state(&page.bot.db).await;
    let (status, response) = post_owner_update(
        &page,
        "install",
        &json!({"component":"api","digest":candidate.digest}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{response}");
    assert_eq!(
        response["error"],
        "service self-update is disabled or unobserved"
    );
    assert_eq!(update_state(&page.bot.db).await, before);
    page.finish();
}
