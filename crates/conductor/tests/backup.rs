//! Local encrypted backup/restore. No live Bunny, Discord or provider account.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use jmodguard_conductor::config::{CaptureStore, Config};
use jmodguard_conductor::{Conductor, router_for};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

struct Harness {
    conductor: Arc<Conductor>,
    router: axum::Router,
    root: std::path::PathBuf,
    _discord: tokio::task::JoinHandle<()>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self._discord.abort();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn discord_stub(owner: u64) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route(
            "/token",
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({
                    "access_token": "conductor-token",
                    "token_type": "Bearer"
                }))
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

fn unique_root() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "jmodguard-backup-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ))
}

async fn setup(restore_exists: bool) -> Harness {
    let (discord, task) = discord_stub(42).await;
    let root = unique_root();
    std::fs::create_dir_all(&root).unwrap();
    let core = root.join("core.db");
    write_core(&core, true).await;
    let evidence = root.join("evidence");
    std::fs::create_dir_all(&evidence).unwrap();
    std::fs::write(evidence.join("note.bin"), b"case-note").unwrap();
    let backup_root = root.join("backups");
    let restore = root.join("restore-target");
    if restore_exists {
        std::fs::create_dir_all(&restore).unwrap();
    }
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        log_filter: "warn".into(),
        shutdown_grace: Duration::from_secs(1),
        db_path: Some(root.join("control.db")),
        public_url: "http://127.0.0.1:8081".into(),
        owner_id: 42,
        oauth_client_id: "cid".into(),
        oauth_client_secret: "csecret".into(),
        bot_health_url: String::new(),
        api_health_url: String::new(),
        oauth_token_url: format!("{discord}/token"),
        oauth_me_url: format!("{discord}/me"),
        backup_root: Some(backup_root),
        backup_key: Some([7u8; 32]),
        capture_stores: vec![CaptureStore {
            role: "core".into(),
            path: core,
        }],
        evidence_root: Some(evidence),
        restore_target: Some(restore),
    };
    let conductor = Conductor::start(cfg, false).await.unwrap();
    let router = router_for(Arc::clone(&conductor));
    Harness {
        conductor,
        router,
        root,
        _discord: task,
    }
}

async fn write_core(path: &std::path::Path, with_actions: bool) {
    let _ = std::fs::remove_file(path);
    let db = libsql::Builder::new_local(path).build().await.unwrap();
    let conn = db.connect().unwrap();
    conn.execute_batch(
        "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
         INSERT INTO meta(key,value) VALUES('schema_family','jmodguard-core-1');
         CREATE TABLE guild_settings(guild_id INTEGER, prefix TEXT);
         INSERT INTO guild_settings(guild_id, prefix) VALUES(10, '!');
         CREATE TABLE bot_actions(id INTEGER PRIMARY KEY, payload TEXT);
         CREATE TABLE backup_cleanup_intents(
           intent_id TEXT PRIMARY KEY,
           kind TEXT,
           guild_id INTEGER,
           enrollment_id TEXT,
           case_id TEXT,
           evidence_id TEXT,
           object_key TEXT,
           created_at INTEGER
         );",
    )
    .await
    .unwrap();
    if with_actions {
        conn.execute(
            "INSERT INTO bot_actions(id, payload) VALUES (1, 'pending-ban')",
            (),
        )
        .await
        .unwrap();
    }
}

async fn call(
    router: &axum::Router,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: Option<&[u8]>,
    content_type: Option<&str>,
) -> (StatusCode, Vec<u8>, Option<String>) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    let request = builder
        .body(Body::from(body.unwrap_or(b"").to_vec()))
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
    let bytes = axum::body::to_bytes(response.into_body(), 8_000_000)
        .await
        .unwrap();
    (status, bytes.to_vec(), set_cookie)
}

async fn tick_until_idle(h: &Harness) {
    for _ in 0..16 {
        h.conductor.tick_job().await.unwrap();
        if !h
            .conductor
            .store()
            .unwrap()
            .has_active_backup_job()
            .await
            .unwrap()
        {
            return;
        }
    }
}

async fn complete_archive_id(h: &Harness, cookie: &str) -> String {
    let (status, listed, _) =
        call(&h.router, "GET", "/api/backups", Some(cookie), None, None).await;
    assert_eq!(status, StatusCode::OK);
    let listed: serde_json::Value = serde_json::from_slice(&listed).unwrap();
    listed["archives"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["status"] == "complete")
        .expect("complete backup")["id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn login(h: &Harness) -> String {
    let login = h
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
    let (_, _, session) = call(
        &h.router,
        "GET",
        &format!("/oauth/callback?code=ok&state={state}"),
        Some(&format!("jmodguard_conductor_oauth_state={state}")),
        None,
        None,
    )
    .await;
    session.unwrap().split(';').next().unwrap().to_string()
}

#[tokio::test]
async fn backups_require_owner_session_and_stay_off_unconfigured_version() {
    let router = jmodguard_conductor::router();
    let (status, body, _) = call(&router, "GET", "/version", None, None, None).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("\"backup_available\":false"));
    let h = setup(false).await;
    let (status, _, _) = call(&h.router, "GET", "/api/backups", None, None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = call(
        &h.router,
        "GET",
        "/api/backups",
        Some("jmodguard_session=guild"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let cookie = login(&h).await;
    let (status, body, _) = call(&h.router, "GET", "/version", Some(&cookie), None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("\"backup_available\":true"));
}

#[tokio::test]
async fn manual_backup_excludes_pending_actions_and_restores_without_replay() {
    let h = setup(false).await;
    let cookie = login(&h).await;
    let (status, queued, _) = call(
        &h.router,
        "POST",
        "/api/backups",
        Some(&cookie),
        Some(br#"{"kind":"manual"}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&queued)
    );
    tick_until_idle(&h).await;
    let id = complete_archive_id(&h, &cookie).await;
    let (status, _, _) = call(
        &h.router,
        "POST",
        &format!("/api/backups/{id}/restore"),
        Some(&cookie),
        Some(br#"{"acknowledge_historical":false}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tick_until_idle(&h).await;
    let restored = h.root.join("restore-target").join("core.sqlite");
    assert!(restored.exists(), "restored store missing");
    let db = libsql::Builder::new_local(&restored).build().await.unwrap();
    let conn = db.connect().unwrap();
    let settings: i64 = conn
        .query("SELECT COUNT(*) FROM guild_settings WHERE guild_id=10", ())
        .await
        .unwrap()
        .next()
        .await
        .unwrap()
        .unwrap()
        .get(0)
        .unwrap();
    let actions = conn
        .query(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='bot_actions'",
            (),
        )
        .await
        .unwrap()
        .next()
        .await
        .unwrap()
        .unwrap()
        .get::<i64>(0)
        .unwrap();
    assert_eq!(settings, 1);
    assert_eq!(actions, 0);
    assert!(h.root.join("restore-target").join("COMPLETE").exists());
    assert!(
        h.root
            .join("restore-target")
            .join("evidence")
            .join("note.bin")
            .exists()
    );
}

#[tokio::test]
async fn historical_restore_requires_acknowledgment_and_rejects_corrupt_uploads() {
    let h = setup(false).await;
    let cookie = login(&h).await;
    let (status, _, _) = call(
        &h.router,
        "POST",
        "/api/backups",
        Some(&cookie),
        Some(br#"{"kind":"manual"}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tick_until_idle(&h).await;
    let id = complete_archive_id(&h, &cookie).await;
    std::fs::remove_file(h.root.join("core.db")).unwrap();
    let (status, _, _) = call(
        &h.router,
        "POST",
        &format!("/api/backups/{id}/restore"),
        Some(&cookie),
        Some(br#"{"acknowledge_historical":false}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tick_until_idle(&h).await;
    let jobs = h.conductor.store().unwrap().list_jobs(20).await.unwrap();
    let restore = jobs
        .iter()
        .find(|job| job.kind == "backup_restore")
        .expect("restore job");
    assert_eq!(restore.status, "failed");
    assert!(
        restore
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("historical restore acknowledgment required"),
        "{restore:?}"
    );
    assert!(!h.root.join("restore-target").exists());
    let (status, _, _) = call(
        &h.router,
        "POST",
        &format!("/api/backups/{id}/restore"),
        Some(&cookie),
        Some(br#"{"acknowledge_historical":true}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tick_until_idle(&h).await;
    assert!(h.root.join("restore-target").join("COMPLETE").exists());
    let (status, body, _) = call(
        &h.router,
        "POST",
        "/api/backups/upload",
        Some(&cookie),
        Some(b"not-an-archive"),
        Some("application/octet-stream"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("unsupported archive format"));
}

#[tokio::test]
async fn restore_refuses_an_existing_target_and_restores_quarantined_uploads() {
    let existing = setup(true).await;
    let cookie = login(&existing).await;
    let (status, _, _) = call(
        &existing.router,
        "POST",
        "/api/backups",
        Some(&cookie),
        Some(br#"{"kind":"manual"}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tick_until_idle(&existing).await;
    let id = complete_archive_id(&existing, &cookie).await;
    let (status, _, _) = call(
        &existing.router,
        "POST",
        &format!("/api/backups/{id}/restore"),
        Some(&cookie),
        Some(br#"{}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tick_until_idle(&existing).await;
    let jobs = existing
        .conductor
        .store()
        .unwrap()
        .list_jobs(20)
        .await
        .unwrap();
    assert!(
        jobs.iter()
            .any(|job| job.kind == "backup_restore" && job.status == "failed"),
        "{jobs:?}"
    );

    let h = setup(false).await;
    let cookie = login(&h).await;
    let (status, _, _) = call(
        &h.router,
        "POST",
        "/api/backups",
        Some(&cookie),
        Some(br#"{"kind":"manual"}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tick_until_idle(&h).await;
    let id = complete_archive_id(&h, &cookie).await;
    let (status, bytes, _) = call(
        &h.router,
        "GET",
        &format!("/api/backups/{id}/download"),
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(bytes.starts_with(jmodguard_conductor::backup::MAGIC));
    let (status, uploaded, _) = call(
        &h.router,
        "POST",
        "/api/backups/upload",
        Some(&cookie),
        Some(&bytes),
        Some("application/octet-stream"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&uploaded)
    );
    let uploaded: serde_json::Value = serde_json::from_slice(&uploaded).unwrap();
    let upload_id = uploaded["id"].as_str().unwrap();
    let (status, _, _) = call(
        &h.router,
        "POST",
        &format!("/api/backups/{upload_id}/restore"),
        Some(&cookie),
        Some(br#"{}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tick_until_idle(&h).await;
    assert!(h.root.join("restore-target").join("COMPLETE").exists());
}

#[tokio::test]
async fn known_removal_is_applied_and_cleanup_intents_are_acked() {
    let h = setup(false).await;
    let cookie = login(&h).await;
    let (status, _, _) = call(
        &h.router,
        "POST",
        "/api/backups",
        Some(&cookie),
        Some(br#"{"kind":"manual"}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tick_until_idle(&h).await;
    let core = h.root.join("core.db");
    let db = libsql::Builder::new_local(&core).build().await.unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "INSERT INTO backup_cleanup_intents(intent_id, kind, guild_id, object_key, created_at) VALUES('intent-1','guild',10,'note.bin',1)",
        (),
    )
        .await
        .unwrap();
    tick_until_idle(&h).await;
    let listed = h
        .conductor
        .store()
        .unwrap()
        .list_archives(20)
        .await
        .unwrap();
    assert!(
        listed
            .iter()
            .any(|row| row.status == "replaced" || row.scope_json.contains("sanitized")),
        "{listed:?}"
    );
}
