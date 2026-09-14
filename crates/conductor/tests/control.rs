//! Control-store login, sessions, jobs and isolation. Local HTTP fixtures only.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use jmodguard_conductor::{Conductor, config::Config, router_for, store::ControlStore};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

struct Control {
    conductor: Arc<Conductor>,
    router: axum::Router,
    db: std::path::PathBuf,
    _discord: tokio::task::JoinHandle<()>,
}

impl Drop for Control {
    fn drop(&mut self) {
        self._discord.abort();
        let _ = std::fs::remove_file(&self.db);
        let _ = std::fs::remove_file(format!("{}-wal", self.db.display()));
        let _ = std::fs::remove_file(format!("{}-shm", self.db.display()));
    }
}

async fn discord_stub(owner: u64) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route(
            "/token",
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({ "access_token": "conductor-token", "token_type": "Bearer" }))
            }),
        )
        .route(
            "/me",
            axum::routing::get(move || async move {
                axum::Json(serde_json::json!({
                    "id": owner.to_string(),
                    "username": "operator",
                    "global_name": "Operator",
                }))
            }),
        );
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), task)
}

async fn setup(owner: u64) -> Control {
    let (discord, task) = discord_stub(owner).await;
    let db = std::env::temp_dir().join(format!(
        "jmodguard-conductor-control-{}-{}.db",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        log_filter: "warn".into(),
        shutdown_grace: Duration::from_secs(1),
        db_path: Some(db.clone()),
        public_url: "http://127.0.0.1:8081".into(),
        owner_id: owner,
        oauth_client_id: "cid".into(),
        oauth_client_secret: "csecret".into(),
        bot_health_url: String::new(),
        api_health_url: String::new(),
        oauth_token_url: format!("{discord}/token"),
        oauth_me_url: format!("{discord}/me"),
        backup_root: None,
        backup_key: None,
        capture_stores: Vec::new(),
        evidence_root: None,
        restore_target: None,
    };
    let conductor = Conductor::start(cfg, false).await.unwrap();
    let router = router_for(Arc::clone(&conductor));
    Control {
        conductor,
        router,
        db,
        _discord: task,
    }
}

async fn call(
    router: &axum::Router,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: Option<&str>,
) -> (StatusCode, String, Option<String>) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let request = builder
        .body(Body::from(body.unwrap_or("").to_string()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let set_cookie = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("jmodguard_conductor_session="))
        .map(|v| v.to_string());
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    (
        status,
        String::from_utf8_lossy(&bytes).into_owned(),
        set_cookie,
    )
}

async fn login_set_cookie(control: &Control) -> String {
    let (status, _, _) = call(&control.router, "GET", "/login", None, None).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let login = control
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let oauth = login
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.contains("jmodguard_conductor_oauth_state="))
        .unwrap();
    let state = oauth
        .split(';')
        .next()
        .unwrap()
        .trim_start_matches("jmodguard_conductor_oauth_state=");
    let (status, _, session) = call(
        &control.router,
        "GET",
        &format!("/oauth/callback?code=ok&state={state}"),
        Some(&format!("jmodguard_conductor_oauth_state={state}")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "login failed");
    session.unwrap()
}

async fn login(control: &Control) -> String {
    login_set_cookie(control)
        .await
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn unconfigured_router_keeps_login_closed() {
    let router = jmodguard_conductor::router();
    let (status, body, _) = call(&router, "GET", "/ready", None, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("control_not_configured"));
    let (status, _, _) = call(&router, "POST", "/login", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn owner_login_manages_settings_and_rejects_other_users() {
    let control = setup(42).await;
    let cookie = login(&control).await;
    let (status, me, _) = call(&control.router, "GET", "/api/me", Some(&cookie), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(me.contains("\"42\""));
    let (status, settings, _) =
        call(&control.router, "GET", "/api/settings", Some(&cookie), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(settings.contains("session_idle_secs"));
    assert!(!settings.contains("csecret"));
    let (status, patched, _) = call(
        &control.router,
        "PATCH",
        "/api/settings",
        Some(&cookie),
        Some(r#"{"revision":1,"session_idle_secs":1800,"notify_webhook_url":"http://127.0.0.1/hook"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert!(patched.contains("1800"));
    assert!(patched.contains("notify_webhook_configured"));
    assert!(!patched.contains("127.0.0.1/hook"));
}

#[tokio::test]
async fn guild_session_cookie_does_not_authenticate_conductor() {
    let control = setup(42).await;
    let (status, body, _) = call(
        &control.router,
        "GET",
        "/api/me",
        Some("jmodguard_session=guild-cookie"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("not authenticated"));
}

#[tokio::test]
async fn session_revocation_and_restart_reuse_durable_store() {
    let control = setup(42).await;
    let cookie = login(&control).await;
    let (status, _, _) = call(&control.router, "POST", "/logout", Some(&cookie), None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = call(&control.router, "GET", "/api/me", Some(&cookie), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let cookie = login(&control).await;
    let restarted = Conductor::start(
        Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            log_filter: "warn".into(),
            shutdown_grace: Duration::from_secs(1),
            db_path: Some(control.db.clone()),
            public_url: "http://127.0.0.1:8081".into(),
            owner_id: 42,
            oauth_client_id: "cid".into(),
            oauth_client_secret: "csecret".into(),
            bot_health_url: String::new(),
            api_health_url: String::new(),
            oauth_token_url: String::new(),
            oauth_me_url: String::new(),
            backup_root: None,
            backup_key: None,
            capture_stores: Vec::new(),
            evidence_root: None,
            restore_target: None,
        },
        false,
    )
    .await
    .unwrap();
    let router = router_for(restarted);
    let (status, me, _) = call(&router, "GET", "/api/me", Some(&cookie), None).await;
    assert_eq!(status, StatusCode::OK, "{me}");
}

#[tokio::test]
async fn session_cookie_uses_the_applied_absolute_lifetime() {
    let control = setup(42).await;
    let cookie = login(&control).await;
    let (status, patched, _) = call(
        &control.router,
        "PATCH",
        "/api/settings",
        Some(&cookie),
        Some(r#"{"revision":1,"session_absolute_secs":172800}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{patched}");

    let issued = login_set_cookie(&control).await;
    assert!(issued.contains("Max-Age=172800"), "{issued}");
}

#[tokio::test]
async fn failed_notification_job_can_be_replayed() {
    let control = setup(42).await;
    let store = control.conductor.store().unwrap();
    let id = store
        .insert_job("notify_test", &serde_json::json!({}), 1)
        .await
        .unwrap();
    let claimed = store.claim_job().await.unwrap().unwrap();
    assert_eq!(claimed.attempts, 1);
    store
        .finish_job(&id, false, Some("delivery failed"))
        .await
        .unwrap();
    assert!(store.replay_job(&id).await.unwrap());
    let replayed = store
        .list_jobs(8)
        .await
        .unwrap()
        .into_iter()
        .find(|job| job.id == id)
        .unwrap();
    assert_eq!(replayed.status, "queued");
    assert_eq!(replayed.attempts, 0);
    assert!(replayed.last_error.is_none());
}

#[tokio::test]
async fn interrupted_running_job_fails_closed_for_manual_replay() {
    let db = std::env::temp_dir().join(format!(
        "jmodguard-conductor-recovery-{}-{}.db",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    let store = ControlStore::open(&db).await.unwrap();
    let id = store
        .insert_job("notify_test", &serde_json::json!({}), 3)
        .await
        .unwrap();
    assert_eq!(store.claim_job().await.unwrap().unwrap().status, "running");
    drop(store);

    let reopened = ControlStore::open(&db).await.unwrap();
    let recovered = reopened
        .list_jobs(8)
        .await
        .unwrap()
        .into_iter()
        .find(|job| job.id == id)
        .unwrap();
    assert_eq!(recovered.status, "failed");
    assert!(
        recovered
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("outcome unknown"))
    );
    drop(reopened);
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_file(format!("{}-wal", db.display()));
    let _ = std::fs::remove_file(format!("{}-shm", db.display()));
}

#[tokio::test]
async fn live_lease_is_operator_only_and_telemetry_is_bounded() {
    let control = setup(42).await;
    let cookie = login(&control).await;
    let (status, body, _) = call(
        &control.router,
        "POST",
        "/api/telemetry/lease",
        Some(&cookie),
        Some("{}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("live"));
    let (status, _, _) = call(
        &control.router,
        "POST",
        "/api/telemetry/lease",
        None,
        Some("{}"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body, _) = call(
        &control.router,
        "GET",
        "/api/telemetry?from_ms=1&to_ms=9",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("samples"));
}
