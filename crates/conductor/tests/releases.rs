//! Signed Stable/Nightly discovery. Local HTTP fixtures only; apply stays inactive without an update root.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use jmodguard_conductor::release::{self, DiscoveredRelease};
use jmodguard_conductor::{Conductor, config::Config, router_for};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
const SECRET: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct Env {
    conductor: Arc<Conductor>,
    router: axum::Router,
    db: std::path::PathBuf,
    _discord: tokio::task::JoinHandle<()>,
    _feed: tokio::task::JoinHandle<()>,
}

impl Drop for Env {
    fn drop(&mut self) {
        self._discord.abort();
        self._feed.abort();
        let _ = std::fs::remove_file(&self.db);
        let _ = std::fs::remove_file(format!("{}-wal", self.db.display()));
        let _ = std::fs::remove_file(format!("{}-shm", self.db.display()));
    }
}

fn unsigned(channel: &str, release_id: &str, core_version: i64, missing: &[&str]) -> Value {
    let mut components = json!({
        "bot": {"digest": format!("sha256:{}", "1".repeat(64)), "platform": "linux/amd64", "size": 10},
        "api": {"digest": format!("sha256:{}", "2".repeat(64)), "platform": "linux/amd64", "size": 11},
        "conductor": {"digest": format!("sha256:{}", "3".repeat(64)), "platform": "linux/amd64", "size": 12},
        "frontend": {
            "sha256": format!("sha256:{}", "4".repeat(64)),
            "files": {"index.html": "a", "app.js": "b", "style.css": "c"}
        }
    });
    for name in missing {
        components.as_object_mut().unwrap().remove(*name);
    }
    json!({
        "format": release::FORMAT,
        "release_id": release_id,
        "channel": channel,
        "version": release_id.trim_start_matches('v'),
        "source_commit": SHA,
        "published_at": "2026-09-15T00:00:00Z",
        "components": components,
        "schema": {
            "core_family": "jmodguard-core-1",
            "core_version": core_version,
            "guild_family": "jmodguard-guild-1",
            "guild_version": release::GUILD_SCHEMA_VERSION
        },
        "settings_schema": 1,
        "backup_format": "JMODGUARD-BACKUP-1",
        "compatibility": {"min_conductor": "0.1.0", "rollback": "matching-backup"},
        "notes": "fixture"
    })
}

fn signed(channel: &str, release_id: &str, core_version: i64) -> String {
    release::sign_manifest(
        unsigned(channel, release_id, core_version, &[]),
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
    let stable = signed("stable", "v0.2.0", 12);
    let nightly = signed("nightly", "nightly-20260915-aaaaaaaaaaaa", 12);
    let old = signed("stable", "v0.0.1", 1);
    let unsigned_raw = unsigned("stable", "v0.3.0", 12, &[]).to_string();
    let index = json!({
        "format": release::INDEX_FORMAT,
        "releases": [
            {"release_id": "v0.2.0", "channel": "stable", "path": "v0.2.0.json", "published_at": "2026-09-15T00:00:00Z"},
            {"release_id": "nightly-20260915-aaaaaaaaaaaa", "channel": "nightly", "path": "nightly.json", "published_at": "2026-09-15T01:00:00Z"},
            {"release_id": "v0.0.1", "channel": "stable", "path": "old.json", "published_at": "2026-01-01T00:00:00Z"},
            {"release_id": "v0.3.0", "channel": "stable", "path": "unsigned.json", "published_at": "2026-09-16T00:00:00Z"},
            {"release_id": "partial", "channel": "stable", "path": "partial.json", "published_at": "2026-09-14T00:00:00Z"}
        ]
    })
    .to_string();
    let partial = unsigned("stable", "partial", 12, &["conductor", "frontend"]).to_string();
    let app = axum::Router::new()
        .route(
            "/index.json",
            axum::routing::get(move || {
                let index = index.clone();
                async move { index }
            }),
        )
        .route(
            "/v0.2.0.json",
            axum::routing::get(move || {
                let stable = stable.clone();
                async move { stable }
            }),
        )
        .route(
            "/nightly.json",
            axum::routing::get(move || {
                let nightly = nightly.clone();
                async move { nightly }
            }),
        )
        .route(
            "/old.json",
            axum::routing::get(move || {
                let old = old.clone();
                async move { old }
            }),
        )
        .route(
            "/unsigned.json",
            axum::routing::get(move || {
                let unsigned_raw = unsigned_raw.clone();
                async move { unsigned_raw }
            }),
        )
        .route(
            "/partial.json",
            axum::routing::get(move || {
                let partial = partial.clone();
                async move { partial }
            }),
        );
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/index.json"), task)
}

async fn setup(channels: &[&str]) -> Env {
    let (discord, discord_task) = discord_stub(42).await;
    let (feed, feed_task) = feed_stub().await;
    let db = std::env::temp_dir().join(format!(
        "jmodguard-conductor-release-{}-{}.db",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    let mut keys = std::collections::BTreeMap::new();
    keys.insert("rp1".into(), SECRET.into());
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        log_filter: "warn".into(),
        shutdown_grace: Duration::from_secs(1),
        db_path: Some(db.clone()),
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
        update_root: None,
        artifact_root: None,
        update_provider: None,
    };
    let conductor = Conductor::start(cfg, false).await.unwrap();
    if !channels.is_empty() {
        let store = conductor.store().unwrap();
        let (mut settings, revision, _) = store.load_settings().await.unwrap();
        settings.discovery_channels = channels.iter().map(|c| (*c).to_string()).collect();
        store.save_settings(&settings, revision).await.unwrap();
    }
    let router = router_for(Arc::clone(&conductor));
    Env {
        conductor,
        router,
        db,
        _discord: discord_task,
        _feed: feed_task,
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

#[test]
fn unsigned_unknown_key_and_incomplete_manifests_are_refused() {
    let body = unsigned("stable", "v0.2.0", 12, &[]);
    assert!(release::verify_manifest(&body.to_string(), &Default::default()).is_err());
    let signed = release::sign_manifest(body.clone(), "rp1", SECRET).unwrap();
    let mut keys = std::collections::BTreeMap::new();
    keys.insert("rp2".into(), SECRET.into());
    assert!(release::verify_manifest(&signed, &keys).is_err());
    let incomplete = unsigned("stable", "v0.2.0", 12, &["frontend"]);
    let signed_incomplete = release::sign_manifest(incomplete, "rp1", SECRET).unwrap();
    keys.insert("rp1".into(), SECRET.into());
    assert!(release::verify_manifest(&signed_incomplete, &keys).is_err());
}

#[test]
fn guild_and_settings_schema_compatibility() {
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        log_filter: "warn".into(),
        shutdown_grace: Duration::from_secs(1),
        db_path: None,
        public_url: String::new(),
        owner_id: 0,
        oauth_client_id: String::new(),
        oauth_client_secret: String::new(),
        bot_health_url: String::new(),
        api_health_url: String::new(),
        oauth_token_url: String::new(),
        oauth_me_url: String::new(),
        backup_root: None,
        backup_key: None,
        capture_stores: Vec::new(),
        evidence_root: None,
        restore_target: None,
        guild_stores: Vec::new(),
        release_feed: None,
        release_keys: Default::default(),
        installed_release: Some("v0.1.0".into()),
        update_root: None,
        artifact_root: None,
        update_provider: None,
    };
    let mut body = unsigned("stable", "v0.2.0", 12, &[]);
    assert_eq!(
        release::installed(&cfg)["guild_version"],
        json!(release::GUILD_SCHEMA_VERSION)
    );
    body["schema"]["guild_version"] = json!(10);
    let (ok, reason) = release::compatibility(&body, &cfg).unwrap();
    assert!(!ok);
    assert!(reason.unwrap().contains("guild schema"));
    body["schema"]["guild_version"] = json!(11);
    let (ok, reason) = release::compatibility(&body, &cfg).unwrap();
    assert!(!ok);
    assert!(reason.unwrap().contains("guild schema"));
    body = unsigned("stable", "v0.2.0", 12, &[]);
    let (ok, reason) = release::compatibility(&body, &cfg).unwrap();
    assert!(ok);
    assert!(reason.is_none());
    body["schema"]["guild_version"] = json!(1);
    let (ok, reason) = release::compatibility(&body, &cfg).unwrap();
    assert!(!ok);
    assert!(reason.unwrap().contains("guild schema"));
    body = unsigned("stable", "v0.2.0", 12, &[]);
    body["schema"]["guild_version"] = json!(8);
    let (ok, reason) = release::compatibility(&body, &cfg).unwrap();
    assert!(!ok);
    assert!(reason.unwrap().contains("guild schema"));
    body = unsigned("stable", "v0.2.0", 12, &[]);
    body["settings_schema"] = json!(0);
    let (ok, reason) = release::compatibility(&body, &cfg).unwrap();
    assert!(!ok);
    assert!(reason.unwrap().contains("settings schema"));
}

#[tokio::test]
async fn failed_scheduled_poll_advances_interval() {
    let (discord, discord_task) = discord_stub(42).await;
    let db = std::env::temp_dir().join(format!(
        "jmodguard-conductor-release-poll-{}-{}.db",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    let mut keys = std::collections::BTreeMap::new();
    keys.insert("rp1".into(), SECRET.into());
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        log_filter: "warn".into(),
        shutdown_grace: Duration::from_secs(1),
        db_path: Some(db.clone()),
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
        release_feed: Some("http://127.0.0.1:1/index.json".into()),
        release_keys: keys,
        installed_release: None,
        update_root: None,
        artifact_root: None,
        update_provider: None,
    };
    let conductor = Conductor::start(cfg, false).await.unwrap();
    let store = conductor.store().unwrap();
    let (mut settings, revision, _) = store.load_settings().await.unwrap();
    settings.discovery_interval_secs = 3600;
    store.save_settings(&settings, revision).await.unwrap();
    assert!(release::refresh_if_due(&conductor, false).await.is_err());
    assert!(
        store
            .meta_get("release_last_poll_ms")
            .await
            .unwrap()
            .is_some()
    );
    assert!(release::refresh_if_due(&conductor, false).await.is_ok());
    discord_task.abort();
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_file(format!("{}-wal", db.display()));
    let _ = std::fs::remove_file(format!("{}-shm", db.display()));
}

#[test]
fn older_schema_is_incompatible() {
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        log_filter: "warn".into(),
        shutdown_grace: Duration::from_secs(1),
        db_path: None,
        public_url: String::new(),
        owner_id: 0,
        oauth_client_id: String::new(),
        oauth_client_secret: String::new(),
        bot_health_url: String::new(),
        api_health_url: String::new(),
        oauth_token_url: String::new(),
        oauth_me_url: String::new(),
        backup_root: None,
        backup_key: None,
        capture_stores: Vec::new(),
        evidence_root: None,
        restore_target: None,
        guild_stores: Vec::new(),
        release_feed: None,
        release_keys: Default::default(),
        installed_release: Some("v0.1.0".into()),
        update_root: None,
        artifact_root: None,
        update_provider: None,
    };
    let (ok, reason) = release::compatibility(&unsigned("stable", "v0.0.1", 1, &[]), &cfg).unwrap();
    assert!(!ok);
    assert!(reason.unwrap().contains("older schema"));
    let (ok, reason) =
        release::compatibility(&unsigned("stable", "v0.2.0", 12, &[]), &cfg).unwrap();
    assert!(ok);
    assert!(reason.is_none());
}

#[tokio::test]
async fn catalog_lists_complete_signed_releases_and_refuses_apply() {
    let env = setup(&["stable", "nightly"]).await;
    assert!(release::discovery_available(&env.conductor.cfg));
    let cookie = login(&env).await;
    let (status, body) = call(&env.router, "GET", "/api/updates", Some(&cookie), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let catalog: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(catalog["available"], true);
    assert_eq!(catalog["update_available"], false);
    assert_eq!(catalog["installed"]["release_id"], "v0.1.0");
    let ids: Vec<&str> = catalog["releases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["release_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"v0.2.0"));
    assert!(ids.contains(&"nightly-20260915-aaaaaaaaaaaa"));
    assert!(ids.contains(&"v0.0.1"));
    assert!(!ids.contains(&"v0.3.0"));
    assert!(!ids.contains(&"partial"));
    let old = catalog["releases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["release_id"] == "v0.0.1")
        .unwrap();
    assert_eq!(old["compatible"], false);
    assert_eq!(catalog["available_count"], 2);
    let (status, apply) = call(
        &env.router,
        "POST",
        "/api/updates/apply",
        Some(&cookie),
        Some(r#"{"release_id":"v0.2.0"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{apply}");
    assert!(apply.contains("not configured"), "{apply}");
    let (status, apply) = call(
        &env.router,
        "POST",
        "/api/updates",
        Some(&cookie),
        Some(r#"{"release_id":"v0.2.0"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{apply}");
    assert!(apply.contains("not configured"), "{apply}");
}

#[tokio::test]
async fn forced_refresh_query_polls_immediately() {
    let env = setup(&["stable"]).await;
    let cookie = login(&env).await;
    let store = env.conductor.store().unwrap();
    store
        .meta_set(
            "release_last_poll_ms",
            &jmodguard_conductor::store::now_ms().to_string(),
        )
        .await
        .unwrap();
    let (status, body) = call(
        &env.router,
        "GET",
        "/api/updates?refresh=true",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let catalog: Value = serde_json::from_str(&body).unwrap();
    assert!(
        catalog["releases"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["release_id"] == "v0.2.0")
    );
}

#[tokio::test]
async fn stable_only_hides_nightly_and_unconfigured_discovery_stays_inactive() {
    let env = setup(&["stable"]).await;
    let cookie = login(&env).await;
    let (status, body) = call(&env.router, "GET", "/api/updates", Some(&cookie), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let catalog: Value = serde_json::from_str(&body).unwrap();
    let ids: Vec<&str> = catalog["releases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["release_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"v0.2.0"));
    assert!(!ids.iter().any(|id| id.starts_with("nightly-")));
    assert_eq!(catalog["available_count"], 1);

    let unconfigured = jmodguard_conductor::router();
    let (status, _) = call(&unconfigured, "GET", "/api/updates", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[test]
fn discovered_release_count_is_channel_and_compatibility_accurate() {
    let rows = vec![
        DiscoveredRelease {
            release_id: "v0.2.0".into(),
            channel: "stable".into(),
            version: "0.2.0".into(),
            source_commit: SHA.into(),
            published_at: "t".into(),
            notes: String::new(),
            compatible: true,
            blocked_reason: None,
            components: vec![
                "bot".into(),
                "api".into(),
                "conductor".into(),
                "frontend".into(),
            ],
        },
        DiscoveredRelease {
            release_id: "nightly-20260915-aaaaaaaaaaaa".into(),
            channel: "nightly".into(),
            version: "n".into(),
            source_commit: SHA.into(),
            published_at: "t".into(),
            notes: String::new(),
            compatible: true,
            blocked_reason: None,
            components: vec!["bot".into()],
        },
    ];
    assert_eq!(
        release::available_count(&rows, &["stable".into()], "v0.1.0"),
        1
    );
    assert_eq!(
        release::available_count(&rows, &["stable".into(), "nightly".into()], "v0.2.0"),
        1
    );
}
