//! Public install CLI, HTTP checklist, and local resource samples.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use jmodguard_conductor::{Conductor, config::Config, router_for};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tower::ServiceExt;

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn scratch(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "jmodguard-install-it-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn conductor_cmd() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_jmodguard-conductor"));
    cmd.env_clear().env("JMODGUARD_CONDUCTOR_LOG", "warn");
    cmd
}

fn rss_bytes(pid: u32) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb.saturating_mul(1024));
        }
    }
    None
}

struct Control {
    conductor: Arc<Conductor>,
    router: axum::Router,
    db: PathBuf,
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

async fn setup(owner: u64) -> Control {
    let (discord, task) = discord_stub(owner).await;
    let db = std::env::temp_dir().join(format!(
        "jmodguard-install-control-{}-{}.db",
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
        guild_stores: Vec::new(),
        release_feed: None,
        release_keys: Default::default(),
        installed_release: None,
        update_root: None,
        artifact_root: None,
        update_provider: None,
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
) -> (StatusCode, String) {
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
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn login(control: &Control) -> String {
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
    let response = control
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/oauth/callback?code=ok&state={state}"))
                .header("cookie", format!("jmodguard_conductor_oauth_state={state}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("jmodguard_conductor_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

#[test]
fn init_local_and_validate_install_cli_use_public_artifacts() {
    let root = scratch("cli");
    let init = conductor_cmd()
        .args(["init-local", "--root", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let stdout = String::from_utf8_lossy(&init.stdout);
    assert!(stdout.contains("\"private_ops_required\": false"));
    assert!(root.join("plan.json").is_file());
    assert!(!root.join("restore").exists());
    let validate = conductor_cmd()
        .args([
            "validate-install",
            "--plan",
            root.join("plan.json").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        validate.status.success(),
        "{}",
        String::from_utf8_lossy(&validate.stderr)
    );
    let report = String::from_utf8_lossy(&validate.stdout);
    assert!(report.contains("\"ok\": true"));
    assert!(report.contains("LicenseRef-PolyForm-Noncommercial-1.0.0"));
    let unknown = conductor_cmd().args(["deploy-bunny"]).output().unwrap();
    assert!(!unknown.status.success());
    let help = conductor_cmd().args(["help"]).output().unwrap();
    assert!(help.status.success());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn recover_update_without_update_root_fails_closed() {
    let started = Instant::now();
    let output = conductor_cmd().args(["recover-update"]).output().unwrap();
    assert!(!output.status.success());
    assert!(started.elapsed() < Duration::from_secs(5));
    let err = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(err.contains("UPDATE_ROOT") || err.contains("update_root") || err.contains("recover"));
}

#[test]
fn idle_conductor_rss_is_measured_locally() {
    let root = scratch("rss");
    std::fs::write(root.join("config.toml"), "not valid TOML [").unwrap();
    let log = std::fs::File::create(root.join("process.log")).unwrap();
    let mut child = conductor_cmd()
        .current_dir(&root)
        .env("JMODGUARD_CONDUCTOR_BIND", "127.0.0.1:0")
        .env("JMODGUARD_CONDUCTOR_LOG", "info")
        .env("JMODGUARD_CONDUCTOR_SHUTDOWN_SECS", "1")
        .env("JMODGUARD_CONFIG", root.join("config.toml"))
        .stdout(log)
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let addr = loop {
        let text = std::fs::read_to_string(root.join("process.log")).unwrap();
        if let Some((address, _)) = text
            .split_once("conductor listening on ")
            .and_then(|(_, rest)| rest.split_once(';'))
            && let Ok(addr) = address.parse::<SocketAddr>()
            && TcpStream::connect(addr).is_ok()
        {
            break addr;
        }
        assert!(child.try_wait().unwrap().is_none(), "conductor exited");
        assert!(Instant::now() < deadline, "conductor did not listen");
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write!(
        stream,
        "GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut health = String::new();
    stream.read_to_string(&mut health).unwrap();
    assert!(health.contains("ok"));
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write!(
        stream,
        "GET /version HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut version = String::new();
    stream.read_to_string(&mut version).unwrap();
    assert!(version.contains("\"private_ops_required\":false"));
    assert!(version.contains("LicenseRef-PolyForm-Noncommercial-1.0.0"));
    let idle = rss_bytes(child.id()).expect("VmRSS");
    assert!(idle > 0, "idle RSS must be observed");
    assert!(
        idle < 256 * 1024 * 1024,
        "idle conductor RSS {idle} exceeds the local debug bound"
    );
    let payload = root.join("tiny-core.db");
    std::fs::write(&payload, vec![0_u8; 4096]).unwrap();
    let backup_input = std::fs::metadata(&payload).unwrap().len();
    assert_eq!(backup_input, 4096);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn install_api_requires_operator_and_rejects_rds() {
    let control = setup(42).await;
    let (status, body) = call(&control.router, "GET", "/api/install", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let cookie = login(&control).await;
    let (status, body) = call(&control.router, "GET", "/api/install", Some(&cookie), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("\"private_ops_required\":false"));
    assert!(body.contains("docs/INSTALL.md"));
    assert!(body.contains("Live Bunny Database"));
    assert!(body.contains("crates/conductor/tests/updates.rs"));
    let (status, plan_body) = call(
        &control.router,
        "GET",
        "/api/install/plan",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{plan_body}");
    assert!(plan_body.contains("\"profile\":\"local\""));
    let sample = control.conductor.resource_sample("dashboard");
    let rss = sample["rss_bytes"].as_u64().unwrap();
    assert!(rss > 0 && rss < 256 * 1024 * 1024);
    let (status, err) = call(
        &control.router,
        "POST",
        "/api/install/validate",
        Some(&cookie),
        Some(
            r#"{"profile":"aws","public_origin":"https://admin.example","core_database":"sqlite","guild_database":"sqlite","core_url":"postgres://db.rds.amazonaws.com/jmodguard"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(err.contains("unsupported database engine"));
    let (status, err) = call(
        &control.router,
        "POST",
        "/api/install/validate",
        Some(&cookie),
        Some(
            r#"{"profile":"bunny","public_origin":"https://admin.example","core_database":"libsql","guild_database":"libsql","core_url":"libsql://core.lite.bunnydb.net?jwt=secret","guild_url":"libsql://guild.lite.bunnydb.net","api_embedded_ui":false,"frontend_storage":true}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(err.contains("must not include a query string"));
    let (status, err) = call(
        &control.router,
        "POST",
        "/api/install/validate",
        Some(&cookie),
        Some(
            r#"{"profile":"bunny","public_origin":"https://admin.example","core_database":"libsql","guild_database":"libsql","core_url":"libsql://core.lite.bunnydb.net#jwt=secret","guild_url":"libsql://guild.lite.bunnydb.net","api_embedded_ui":false,"frontend_storage":true}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(err.contains("must not include a fragment"));
    let root = scratch("http-local");
    let plan = serde_json::json!({
        "profile": "local",
        "public_origin": "http://127.0.0.1:8081",
        "core_database": "sqlite",
        "guild_database": "sqlite",
        "backup_root": root.join("backup"),
        "evidence_root": root.join("evidence"),
        "restore_target": root.join("restore"),
        "api_embedded_ui": true,
        "frontend_storage": false,
        "private_ops_required": false,
        "license": "LicenseRef-PolyForm-Noncommercial-1.0.0"
    });
    let (status, ok) = call(
        &control.router,
        "POST",
        "/api/install/validate",
        Some(&cookie),
        Some(&plan.to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ok}");
    assert!(ok.contains("\"ok\":true"));
    let (status, mixed) = call(
        &control.router,
        "POST",
        "/api/install/validate",
        Some(&cookie),
        Some(
            r#"{"profile":"aws","public_origin":"https://admin.example","core_database":"sqlite","guild_database":"libsql","guild_url":""}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{mixed}");
    assert!(mixed.contains("guild libSQL URL must be set"));
    let (status, overlap) = call(
        &control.router,
        "POST",
        "/api/install/validate",
        Some(&cookie),
        Some(
            &serde_json::json!({
                "profile": "local",
                "public_origin": "http://127.0.0.1:8081",
                "core_database": "sqlite",
                "guild_database": "sqlite",
                "backup_root": root.join("backup"),
                "evidence_root": root.join("evidence/../backup"),
                "api_embedded_ui": true,
                "frontend_storage": false
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{overlap}");
    assert!(overlap.contains("must not overlap"));
    let (status, aurora) = call(
        &control.router,
        "POST",
        "/api/install/validate",
        Some(&cookie),
        Some(
            r#"{"profile":"bunny","public_origin":"https://admin.example","core_database":"libsql","guild_database":"libsql","core_url":"libsql://core.aurora-test.lite.bunnydb.net","guild_url":"libsql://guild.lite.bunnydb.net","api_embedded_ui":false,"frontend_storage":true}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{aurora}");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn validate_install_succeeds_after_restore_directory_exists() {
    let root = scratch("restore-revalidate");
    let init = conductor_cmd()
        .args(["init-local", "--root", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));
    std::fs::create_dir_all(root.join("restore")).unwrap();
    let validate = conductor_cmd()
        .args([
            "validate-install",
            "--plan",
            root.join("plan.json").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        validate.status.success(),
        "{}",
        String::from_utf8_lossy(&validate.stderr)
    );
    let report = String::from_utf8_lossy(&validate.stdout);
    assert!(report.contains("\"ok\": true"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn public_acceptance_matrix_files_still_cover_isolation_update_move_backup() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for (path, needle) in [
        ("tests/process.rs", "standalone_process_reports_liveness"),
        ("tests/updates.rs", "recover_update"),
        ("tests/moves.rs", "move"),
        ("tests/backup.rs", "restore"),
        (
            "../integration-tests/tests/dual_process_smoke.rs",
            "smoke_dual_process",
        ),
    ] {
        let text = std::fs::read_to_string(root.join(path)).unwrap();
        assert!(text.contains(needle), "{path} missing {needle}");
    }
}
