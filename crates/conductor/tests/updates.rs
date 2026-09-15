//! Local process update/rollback. Provider HTTP is a fixture, not live Bunny.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use jmodguard_conductor::release;
use jmodguard_conductor::update_adapter::{Adapter, DrainContext, INVENTORY_FORMAT, Inventory, RECOVER_FORMAT};
use jmodguard_conductor::{Conductor, config::Config, router_for, update};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tower::ServiceExt;

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
const SECRET: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const STANDIN: &str = env!("CARGO_BIN_EXE_jmodguard-update-standin");

struct Env {
    conductor: Arc<Conductor>,
    router: axum::Router,
    root: std::path::PathBuf,
    _discord: tokio::task::JoinHandle<()>,
    _feed: tokio::task::JoinHandle<()>,
    _provider: tokio::task::JoinHandle<()>,
}

impl Drop for Env {
    fn drop(&mut self) {
        self._discord.abort();
        self._feed.abort();
        self._provider.abort();
        if let Ok(raw) = std::fs::read_to_string(self.root.join("update/inventory.json"))
            && let Ok(inventory) = serde_json::from_str::<Inventory>(&raw)
        {
            for row in inventory.components.values() {
                if let Some(pid) = row.pid {
                    unsafe {
                        libc::kill(pid as i32, libc::SIGKILL);
                        libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG);
                    }
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn digest(ch: char) -> String {
    format!("sha256:{}", ch.to_string().repeat(64))
}

fn unsigned(release_id: &str, bot: char, api: char, conductor: char, frontend: char) -> Value {
    json!({
        "format": release::FORMAT,
        "release_id": release_id,
        "channel": "stable",
        "version": release_id.trim_start_matches('v'),
        "source_commit": SHA,
        "published_at": "2026-09-15T00:00:00Z",
        "components": {
            "bot": {"digest": digest(bot), "platform": "linux/amd64", "size": 10},
            "api": {"digest": digest(api), "platform": "linux/amd64", "size": 11},
            "conductor": {"digest": digest(conductor), "platform": "linux/amd64", "size": 12},
            "frontend": {
                "sha256": digest(frontend),
                "files": {"index.html": "a", "app.js": "b", "style.css": "c"}
            }
        },
        "schema": {
            "core_family": "jmodguard-core-1",
            "core_version": 12,
            "guild_family": "jmodguard-guild-1",
            "guild_version": 8
        },
        "settings_schema": 1,
        "backup_format": "JMODGUARD-BACKUP-1",
        "compatibility": {"min_conductor": "0.1.0", "rollback": "matching-backup"},
        "notes": "fixture"
    })
}

fn signed(release_id: &str, bot: char, api: char, conductor: char, frontend: char) -> String {
    release::sign_manifest(
        unsigned(release_id, bot, api, conductor, frontend),
        "rp1",
        SECRET,
    )
    .unwrap()
}

async fn discord_stub(owner: u64) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route(
            "/token",
            axum::routing::post(|| async {
                axum::Json(json!({ "access_token": "conductor-token", "token_type": "Bearer" }))
            }),
        )
        .route(
            "/me",
            axum::routing::get(move || async move {
                axum::Json(json!({
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

async fn feed_stub() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let current = signed("v0.2.0", '1', '2', '3', '4');
    let fail = signed("v0.2.1", '1', 'e', '3', '4');
    let old = {
        let mut body = unsigned("v0.0.1", '1', '2', '3', '4');
        body["schema"]["core_version"] = json!(1);
        release::sign_manifest(body, "rp1", SECRET).unwrap()
    };
    let index = json!({
        "format": release::INDEX_FORMAT,
        "releases": [
            {"release_id": "v0.2.0", "channel": "stable", "path": "v0.2.0.json"},
            {"release_id": "v0.2.1", "channel": "stable", "path": "v0.2.1.json"},
            {"release_id": "v0.0.1", "channel": "stable", "path": "old.json"}
        ]
    })
    .to_string();
    let app = axum::Router::new()
        .route(
            "/index.json",
            axum::routing::get({
                let index = index.clone();
                move || async move { index }
            }),
        )
        .route(
            "/v0.2.0.json",
            axum::routing::get({
                let current = current.clone();
                move || async move { current }
            }),
        )
        .route(
            "/v0.2.1.json",
            axum::routing::get({
                let fail = fail.clone();
                move || async move { fail }
            }),
        )
        .route(
            "/old.json",
            axum::routing::get({
                let old = old.clone();
                move || async move { old }
            }),
        );
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/index.json"), task)
}

async fn provider_stub() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = Arc::new(Mutex::new(BTreeMap::<String, Value>::new()));
    let app = axum::Router::new()
        .route(
            "/apps/{name}/desired",
            axum::routing::post({
                let state = Arc::clone(&state);
                move |axum::extract::Path(name): axum::extract::Path<String>, axum::Json(body): axum::Json<Value>| {
                    let state = Arc::clone(&state);
                    async move {
                        let digest = body.get("digest").and_then(Value::as_str).unwrap_or_default().to_string();
                        let ready = body.get("ready").and_then(Value::as_bool).unwrap_or(false);
                        state.lock().unwrap().insert(
                            name,
                            json!({ "desired": digest, "observed": digest, "ready": ready, "generation": 1 }),
                        );
                        axum::Json(json!({ "ok": true }))
                    }
                }
            }),
        )
        .route(
            "/apps/{name}",
            axum::routing::get({
                let state = Arc::clone(&state);
                move |axum::extract::Path(name): axum::extract::Path<String>| {
                    let state = Arc::clone(&state);
                    async move {
                        axum::Json(state.lock().unwrap().get(&name).cloned().unwrap_or(json!({
                            "desired": "",
                            "observed": "",
                            "ready": false
                        })))
                    }
                }
            }),
        );
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), task)
}

fn place_process_artifact(
    cache: &std::path::Path,
    digest: &str,
    fail_ready: bool,
    hold_drain: bool,
) {
    let hex = digest.trim_start_matches("sha256:");
    let dest = cache.join(hex);
    std::fs::copy(STANDIN, &dest).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dest).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dest, perms).unwrap();
    }
    let mut env_lines = String::new();
    if fail_ready {
        env_lines.push_str("JMODGUARD_FAIL_READY=1\n");
    }
    if hold_drain {
        env_lines.push_str("JMODGUARD_HOLD_DRAIN=1\n");
    }
    if !env_lines.is_empty() {
        std::fs::write(cache.join(format!("{hex}.env")), env_lines).unwrap();
    }
}

fn place_frontend(cache: &std::path::Path, digest: &str, marker: &str) {
    let dir = cache.join(digest.trim_start_matches("sha256:"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("index.html"), format!("index-{marker}")).unwrap();
    std::fs::write(dir.join("app.js"), format!("js-{marker}")).unwrap();
    std::fs::write(dir.join("style.css"), format!("css-{marker}")).unwrap();
}

async fn setup(seed_previous: bool) -> Env {
    let (discord, discord_task) = discord_stub(42).await;
    let (feed, feed_task) = feed_stub().await;
    let (provider, provider_task) = provider_stub().await;
    let root = std::env::temp_dir().join(format!(
        "jmodguard-conductor-update-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let update_root = root.join("update");
    let cache = update_root.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    for ch in ['a', 'b', 'c', '1', '2', '3'] {
        place_process_artifact(&cache, &digest(ch), false, false);
    }
    place_process_artifact(&cache, &digest('e'), true, false);
    place_frontend(&cache, &digest('d'), "old");
    place_frontend(&cache, &digest('4'), "new");
    let mut keys = BTreeMap::new();
    keys.insert("rp1".into(), SECRET.into());
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
        backup_root: None,
        backup_key: None,
        capture_stores: Vec::new(),
        evidence_root: None,
        restore_target: None,
        guild_stores: Vec::new(),
        release_feed: Some(feed),
        release_keys: keys,
        installed_release: Some("v0.1.0".into()),
        update_root: Some(update_root.clone()),
        artifact_root: Some(cache.clone()),
        update_provider: Some(provider),
    };
    let conductor = Conductor::start(cfg, false).await.unwrap();
    let store = conductor.store().unwrap();
    let (mut settings, revision, _) = store.load_settings().await.unwrap();
    settings.discovery_channels = vec!["stable".into()];
    store.save_settings(&settings, revision).await.unwrap();
    if seed_previous {
        let adapter = Adapter::from_config(&conductor.cfg, conductor.http.clone()).unwrap();
        let mut inventory = Inventory {
            format: INVENTORY_FORMAT.into(),
            release_id: "v0.1.0".into(),
            components: BTreeMap::new(),
        };
        adapter.stage(&digest('d')).unwrap();
        inventory.components.insert(
            "frontend".into(),
            adapter
                .activate("frontend", &digest('d'), None)
                .await
                .unwrap(),
        );
        for (name, ch) in [("api", 'b'), ("bot", 'a'), ("conductor", 'c')] {
            adapter.stage(&digest(ch)).unwrap();
            let record = adapter.activate(name, &digest(ch), None).await.unwrap();
            adapter.wait_ready(name, &record).await.unwrap();
            inventory.components.insert(name.into(), record);
        }
        adapter.save_inventory(&inventory).unwrap();
    }
    let router = router_for(Arc::clone(&conductor));
    Env {
        conductor,
        router,
        root,
        _discord: discord_task,
        _feed: feed_task,
        _provider: provider_task,
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
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn login(env: &Env) -> String {
    let login = env
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
    let (status, _, session) = {
        let response = env
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/oauth/callback?code=ok&state={state}"))
                    .header("cookie", format!("jmodguard_conductor_oauth_state={state}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
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
    };
    assert_eq!(status, StatusCode::SEE_OTHER, "login failed");
    session.unwrap().split(';').next().unwrap().to_string()
}

fn frontend_marker(env: &Env) -> String {
    std::fs::read_to_string(env.root.join("update/current/frontend/index.html")).unwrap_or_default()
}

#[tokio::test]
async fn successful_update_and_code_only_rollback() {
    let env = setup(true).await;
    assert!(update::updates_available(&env.conductor.cfg));
    let cookie = login(&env).await;
    let (status, body) = call(&env.router, "GET", "/version", Some(&cookie), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let version: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(version["update_available"], true);
    let (status, body) = call(&env.router, "GET", "/api/updates", Some(&cookie), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let catalog: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(catalog["update_available"], true);
    let (status, plan) = call(
        &env.router,
        "GET",
        "/api/updates/plan?release_id=v0.2.0&preop=skip",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{plan}");
    assert!(plan.contains("Exclusive Gateway"));
    let (status, body) = call(
        &env.router,
        "POST",
        "/api/updates",
        Some(&cookie),
        Some(r#"{"release_id":"v0.2.0","preop":"skip","confirm_interruption":true}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = serde_json::from_str::<Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    env.conductor.tick_job().await.unwrap();
    let job = env
        .conductor
        .store()
        .unwrap()
        .get_job(&id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.status, "succeeded", "{:?}", job.last_error);
    assert_eq!(frontend_marker(&env), "index-new");
    let installed = env
        .conductor
        .store()
        .unwrap()
        .meta_get("installed_release")
        .await
        .unwrap();
    assert_eq!(installed.as_deref(), Some("v0.2.0"));
    let (mut settings, revision, _) = env
        .conductor
        .store()
        .unwrap()
        .load_settings()
        .await
        .unwrap();
    settings.discovery_interval_secs = 600;
    env.conductor
        .store()
        .unwrap()
        .save_settings(&settings, revision)
        .await
        .unwrap();
    let (status, body) = call(
        &env.router,
        "POST",
        &format!("/api/updates/{id}/rollback"),
        Some(&cookie),
        Some(r#"{"confirm_rollback":true}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    env.conductor.tick_job().await.unwrap();
    assert_eq!(frontend_marker(&env), "index-old");
    let settings = env.conductor.settings().await.unwrap();
    assert_eq!(settings.discovery_interval_secs, 600);
    let installed = env
        .conductor
        .store()
        .unwrap()
        .meta_get("installed_release")
        .await
        .unwrap();
    assert_eq!(installed.as_deref(), Some("v0.1.0"));
}

#[tokio::test]
async fn missing_artifacts_failed_readiness_backup_and_old_instance() {
    {
        let env = setup(true).await;
        let cookie = login(&env).await;
        std::fs::remove_file(
            env.root
                .join("update/cache")
                .join(digest('1').trim_start_matches("sha256:")),
        )
        .unwrap();
        let (status, body) = call(
            &env.router,
            "POST",
            "/api/updates",
            Some(&cookie),
            Some(r#"{"release_id":"v0.2.0","preop":"skip","confirm_interruption":true}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let id = serde_json::from_str::<Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        env.conductor.tick_job().await.unwrap();
        let job = env
            .conductor
            .store()
            .unwrap()
            .get_job(&id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.status, "failed");
        assert!(
            job.last_error.as_deref().unwrap_or("").contains("missing"),
            "{:?}",
            job.last_error
        );
    }

    {
        let env = setup(true).await;
        let cookie = login(&env).await;
        let (status, body) = call(
            &env.router,
            "POST",
            "/api/updates",
            Some(&cookie),
            Some(r#"{"release_id":"v0.2.1","preop":"skip","confirm_interruption":true}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let id = serde_json::from_str::<Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        env.conductor.tick_job().await.unwrap();
        let job = env
            .conductor
            .store()
            .unwrap()
            .get_job(&id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.status, "failed", "{:?}", job.last_error);
        assert!(
            job.last_error
                .as_deref()
                .unwrap_or("")
                .contains("readiness"),
            "{:?}",
            job.last_error
        );
        let adapter = Adapter::from_config(&env.conductor.cfg, env.conductor.http.clone()).unwrap();
        let inventory = adapter.load_inventory().unwrap();
        assert_eq!(inventory.components["api"].digest, digest('b'));
        assert_eq!(
            inventory.release_id,
            "v0.1.0",
            "release_id must not advance until the job commits"
        );
        adapter
            .wait_ready("api", &inventory.components["api"])
            .await
            .unwrap();
    }

    {
        let env = setup(false).await;
        let cookie = login(&env).await;
        let (status, body) = call(
            &env.router,
            "POST",
            "/api/updates",
            Some(&cookie),
            Some(r#"{"release_id":"v0.2.0","preop":"fresh","confirm_interruption":true}"#),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("checkpoint capture"), "{body}");
        let payload = json!({
            "release_id": "v0.2.0",
            "preop": "fresh",
            "rollback": false,
            "phase": "planned",
            "plan": {
                "desired": {
                    "frontend": digest('4'),
                    "api": digest('2'),
                    "bot": digest('1'),
                    "conductor": digest('3')
                },
                "previous": {},
                "installed": "v0.1.0"
            }
        });
        let id = env
            .conductor
            .store()
            .unwrap()
            .insert_job("release_update", &payload, 1)
            .await
            .unwrap();
        let err = update::run(&env.conductor, &id, &payload.to_string())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("backup")
                || err.to_string().contains("checkpoint")
                || err.to_string().contains("capture")
                || err.to_string().contains("not configured"),
            "{err:#}"
        );
    }

    let env = setup(true).await;
    let adapter = Adapter::from_config(&env.conductor.cfg, env.conductor.http.clone()).unwrap();
    let inventory = adapter.load_inventory().unwrap();
    let bot = inventory.components["bot"].clone();
    adapter.drain("bot", &bot, DrainContext::Apply).await.unwrap();
    adapter.confirm_dead("bot", &bot).await.unwrap();
    let zombie = adapter
        .activate("bot", &digest('a'), Some(&bot))
        .await
        .unwrap();
    adapter.wait_ready("bot", &zombie).await.unwrap();
    let err = adapter.confirm_dead("bot", &bot).await.unwrap_err();
    assert!(err.to_string().contains("still"), "{err:#}");
}

#[tokio::test]
async fn interrupted_job_fail_closed_and_conductor_recover_command() {
    {
        let env = setup(true).await;
        let cookie = login(&env).await;
        let (status, body) = call(
            &env.router,
            "POST",
            "/api/updates",
            Some(&cookie),
            Some(r#"{"release_id":"v0.2.0","preop":"skip","confirm_interruption":true}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let id = serde_json::from_str::<Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let claimed = env
            .conductor
            .store()
            .unwrap()
            .claim_job()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, id);
        let restarted = Conductor::start(env.conductor.cfg.clone(), false)
            .await
            .unwrap();
        let job = restarted
            .store()
            .unwrap()
            .get_job(&id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.status, "failed");
        assert!(
            job.last_error
                .as_deref()
                .unwrap_or("")
                .contains("interrupted")
        );
        let adapter = Adapter::from_config(&env.conductor.cfg, env.conductor.http.clone()).unwrap();
        let inventory = adapter.load_inventory().unwrap();
        assert_eq!(inventory.release_id, "v0.1.0");
        adapter
            .wait_ready("bot", &inventory.components["bot"])
            .await
            .unwrap();
        drop(restarted);
    }

    let env = setup(true).await;
    let adapter = Adapter::from_config(&env.conductor.cfg, env.conductor.http.clone()).unwrap();
    let inventory = adapter.load_inventory().unwrap();
    let conductor = inventory.components["conductor"].clone();
    adapter
        .write_recover(&jmodguard_conductor::update_adapter::RecoverInventory {
            format: jmodguard_conductor::update_adapter::RECOVER_FORMAT.into(),
            job_id: "job-recover".into(),
            release_id: "v0.2.0".into(),
            previous_release_id: "v0.1.0".into(),
            previous: BTreeMap::from([
                ("frontend".into(), digest('d')),
                ("api".into(), digest('b')),
                ("bot".into(), digest('a')),
                ("conductor".into(), digest('c')),
            ]),
            desired: BTreeMap::from([
                ("frontend".into(), digest('4')),
                ("api".into(), digest('2')),
                ("bot".into(), digest('1')),
                ("conductor".into(), digest('3')),
            ]),
            phase: "deploying".into(),
            component: "conductor".into(),
            status: "replacing".into(),
        })
        .unwrap();
    adapter
        .drain("conductor", &conductor, DrainContext::Apply)
        .await
        .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_jmodguard-conductor"))
        .arg("recover-update")
        .env_clear()
        .env("JMODGUARD_CONDUCTOR_UPDATE_ROOT", env.root.join("update"))
        .env(
            "JMODGUARD_CONDUCTOR_ARTIFACT_ROOT",
            env.root.join("update/cache"),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["status"], "recovered");
    let restored = adapter.load_inventory().unwrap();
    adapter
        .wait_ready("conductor", &restored.components["conductor"])
        .await
        .unwrap();
    assert_eq!(restored.components["conductor"].digest, digest('c'));
}

#[tokio::test]
async fn update_shares_idle_mutex_and_refuses_incompatible() {
    let env = setup(true).await;
    let cookie = login(&env).await;
    env.conductor
        .store()
        .unwrap()
        .insert_job("backup_capture", &json!({ "kind": "scheduled" }), 3)
        .await
        .unwrap();
    let (status, body) = call(
        &env.router,
        "POST",
        "/api/updates",
        Some(&cookie),
        Some(r#"{"release_id":"v0.2.0","preop":"skip","confirm_interruption":true}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("already running"), "{body}");
    let (status, body) = call(
        &env.router,
        "GET",
        "/api/updates/plan?release_id=v0.0.1",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("older schema"), "{body}");
}

#[tokio::test]
async fn plan_preflight_reflects_preop_query() {
    let env = setup(true).await;
    let cookie = login(&env).await;
    let (status, body) = call(
        &env.router,
        "GET",
        "/api/updates/plan?release_id=v0.2.0&preop=skip",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("preop=skip"),
        "plan checkpoint text should match skip preop: {body}"
    );
    let (status, body) = call(
        &env.router,
        "GET",
        "/api/updates/plan?release_id=v0.2.0&preop=fresh",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("fresh R4 checkpoint"),
        "plan checkpoint text should match fresh preop: {body}"
    );
}

#[tokio::test]
async fn uncertain_recover_blocks_new_apply_and_job_retry() {
    let env = setup(true).await;
    let adapter = Adapter::from_config(&env.conductor.cfg, env.conductor.http.clone()).unwrap();
    adapter
        .write_recover(&jmodguard_conductor::update_adapter::RecoverInventory {
            format: RECOVER_FORMAT.into(),
            job_id: "job-uncertain".into(),
            release_id: "v0.2.0".into(),
            previous_release_id: "v0.1.0".into(),
            previous: BTreeMap::from([
                ("bot".into(), digest('a')),
                ("api".into(), digest('b')),
            ]),
            desired: BTreeMap::new(),
            phase: "deploying".into(),
            component: "bot".into(),
            status: "uncertain".into(),
        })
        .unwrap();
    let cookie = login(&env).await;
    let (status, body) = call(
        &env.router,
        "POST",
        "/api/updates",
        Some(&cookie),
        Some(r#"{"release_id":"v0.2.0","preop":"skip","confirm_interruption":true}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("uncertain"), "{body}");
    let payload = json!({
        "release_id": "v0.2.0",
        "preop": "skip",
        "rollback": false,
        "phase": "planned",
        "plan": {
            "desired": { "bot": digest('1') },
            "previous": {},
            "installed": "v0.1.0"
        }
    });
    let job_id = env
        .conductor
        .store()
        .unwrap()
        .insert_job("release_update", &payload, 1)
        .await
        .unwrap();
    let claimed = env
        .conductor
        .store()
        .unwrap()
        .claim_job()
        .await
        .unwrap()
        .expect("job");
    assert_eq!(claimed.id, job_id);
    env.conductor
        .store()
        .unwrap()
        .finish_job(&job_id, false, Some("failed"))
        .await
        .unwrap();
    let (status, body) = call(
        &env.router,
        "POST",
        &format!("/api/jobs/{job_id}/retry"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("uncertain"), "{body}");
}

#[tokio::test]
async fn recover_update_fails_when_confirm_dead_still_serving() {
    let env = setup(true).await;
    let adapter = Adapter::from_config(&env.conductor.cfg, env.conductor.http.clone()).unwrap();
    let live = adapter.load_inventory().unwrap().components["bot"].clone();
    let mut inventory = adapter.load_inventory().unwrap();
    inventory.components.insert(
        "bot".into(),
        jmodguard_conductor::update_adapter::ComponentRecord {
            digest: live.digest.clone(),
            kind: "process".into(),
            pid: None,
            port: live.port,
            generation: live.generation,
        },
    );
    adapter.save_inventory(&inventory).unwrap();
    adapter
        .write_recover(&jmodguard_conductor::update_adapter::RecoverInventory {
            format: RECOVER_FORMAT.into(),
            job_id: "job-drain".into(),
            release_id: "v0.2.0".into(),
            previous_release_id: "v0.1.0".into(),
            previous: BTreeMap::from([("bot".into(), digest('a'))]),
            desired: BTreeMap::from([("bot".into(), digest('1'))]),
            phase: "deploying".into(),
            component: "bot".into(),
            status: "replacing".into(),
        })
        .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_jmodguard-conductor"))
        .arg("recover-update")
        .env_clear()
        .env("JMODGUARD_CONDUCTOR_UPDATE_ROOT", env.root.join("update"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(combined.contains("still serving health"), "{combined}");
    adapter.wait_ready("bot", &live).await.unwrap();
}

#[tokio::test]
async fn apply_requires_inventory_when_health_urls_configured() {
    let env = setup(false).await;
    let store = env.conductor.store().unwrap();
    let (mut settings, revision, _) = store.load_settings().await.unwrap();
    settings.bot_health_url = "http://127.0.0.1:9/health".into();
    store.save_settings(&settings, revision).await.unwrap();
    let cookie = login(&env).await;
    let (status, body) = call(
        &env.router,
        "POST",
        "/api/updates",
        Some(&cookie),
        Some(r#"{"release_id":"v0.2.0","preop":"skip","confirm_interruption":true}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = serde_json::from_str::<Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    env.conductor.tick_job().await.unwrap();
    let job = store.get_job(&id).await.unwrap().unwrap();
    assert_eq!(job.status, "failed");
    assert!(
        job.last_error
            .as_deref()
            .unwrap_or("")
            .contains("inventory has no bot"),
        "{:?}",
        job.last_error
    );
}

#[tokio::test]
async fn conductor_reconciles_installed_meta_from_inventory_on_start() {
    let env = setup(true).await;
    let adapter = Adapter::from_config(&env.conductor.cfg, env.conductor.http.clone()).unwrap();
    let mut inventory = adapter.load_inventory().unwrap();
    inventory.release_id = "v0.1.0".into();
    adapter.save_inventory(&inventory).unwrap();
    env.conductor
        .store()
        .unwrap()
        .meta_set("installed_release", "v0.2.0")
        .await
        .unwrap();
    let restarted = Conductor::start(env.conductor.cfg.clone(), false)
        .await
        .unwrap();
    let meta = restarted
        .store()
        .unwrap()
        .meta_get("installed_release")
        .await
        .unwrap();
    assert_eq!(meta.as_deref(), Some("v0.1.0"));
}
