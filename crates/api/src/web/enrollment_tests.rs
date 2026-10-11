//! Authenticated HTTP regressions with local OAuth responses and real SQLite.
use super::*;
use crate::db;
use axum::body::Body;
use axum::http::Request;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tower::ServiceExt;

struct DiscordStub {
    guilds: std::sync::Mutex<Vec<oauth::DiscordGuild>>,
    failed: AtomicBool,
    calls: AtomicUsize,
    token_calls: AtomicUsize,
    refresh_failed: AtomicBool,
    refresh_rate_limited: AtomicBool,
    refresh_server_failed: AtomicBool,
    refresh_server_retry_after: std::sync::Mutex<Option<String>>,
    still_unauthorized: AtomicBool,
    me_unauthorized: AtomicBool,
    spent_budget: AtomicBool,
    rate_limited: AtomicBool,
    /// Discord marked the 429 as the shared global limit, which also closes `oauth.me`.
    global_scope: AtomicBool,
    me_calls: AtomicUsize,
    retry_after: std::sync::Mutex<String>,
    body_retry_after: std::sync::Mutex<Option<f64>>,
    /// First page is a full 200-guild page; the following page is HTTP 429.
    limit_on_next_page: AtomicBool,
    /// `fixture-token` is rejected until `/token` issues `refreshed-token`.
    unauthorized_until_refresh: AtomicBool,
    retire_during_lookup: std::sync::Mutex<Option<db::Db>>,
    replace_destination_during_lookup: std::sync::Mutex<Option<db::Db>>,
}
#[derive(Clone, Copy)]
enum TokenTransport {
    Normal,
    Closed,
    Timeout,
}

struct Fixture {
    app: Arc<ApiState>,
    router: Router,
    cookie: String,
    store: MemoryStore,
    token_server: Option<tokio::task::JoinHandle<()>>,
    discord: Arc<DiscordStub>,
    server: tokio::task::JoinHandle<()>,
    path: std::path::PathBuf,
}
fn managed_guild(id: u64) -> oauth::DiscordGuild {
    oauth::DiscordGuild {
        id: id.to_string(),
        name: "fixture".into(),
        icon: None,
        owner: false,
        permissions: Some(oauth::PermissionsField::Number((1 << 5) | (1 << 2))),
    }
}
impl Fixture {
    async fn new(operator: bool) -> Self {
        Self::with_oauth_client_id(operator, "42").await
    }
    async fn with_oauth_client_id(operator: bool, oauth_client_id: &str) -> Self {
        Self::with_token_transport(operator, oauth_client_id, TokenTransport::Normal).await
    }
    async fn with_token_transport(
        operator: bool,
        oauth_client_id: &str,
        token_transport: TokenTransport,
    ) -> Self {
        let discord = Arc::new(DiscordStub {
            guilds: std::sync::Mutex::new(vec![managed_guild(10), managed_guild(20)]),
            failed: AtomicBool::new(false),
            calls: AtomicUsize::new(0),
            token_calls: AtomicUsize::new(0),
            refresh_failed: AtomicBool::new(false),
            refresh_rate_limited: AtomicBool::new(false),
            refresh_server_failed: AtomicBool::new(false),
            refresh_server_retry_after: std::sync::Mutex::new(None),
            still_unauthorized: AtomicBool::new(false),
            me_unauthorized: AtomicBool::new(false),
            spent_budget: AtomicBool::new(false),
            rate_limited: AtomicBool::new(false),
            global_scope: AtomicBool::new(false),
            me_calls: AtomicUsize::new(0),
            retry_after: std::sync::Mutex::new("2".into()),
            body_retry_after: std::sync::Mutex::new(None),
            limit_on_next_page: AtomicBool::new(false),
            unauthorized_until_refresh: AtomicBool::new(false),
            retire_during_lookup: std::sync::Mutex::new(None),
            replace_destination_during_lookup: std::sync::Mutex::new(None),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handler = Arc::clone(&discord);
        let token_handler = Arc::clone(&discord);
        let me_handler = Arc::clone(&discord);
        let routes = Router::new()
            .route(
                "/token",
                axum::routing::post(move || {
                    let handler = Arc::clone(&token_handler);
                    async move {
                        handler.token_calls.fetch_add(1, Ordering::SeqCst);
                        if handler.refresh_rate_limited.load(Ordering::SeqCst) {
                            let mut headers = HeaderMap::new();
                            let retry_after = handler.retry_after.lock().unwrap().clone();
                            if !retry_after.is_empty() {
                                headers.insert(axum::http::header::RETRY_AFTER, retry_after.parse().unwrap());
                            }
                            let mut body = serde_json::json!({"error": "rate limited"});
                            if let Some(seconds) = *handler.body_retry_after.lock().unwrap() {
                                body["retry_after"] = serde_json::json!(seconds);
                            }
                            return (StatusCode::TOO_MANY_REQUESTS, headers, Json(body)).into_response();
                        }
                        if handler.refresh_server_failed.load(Ordering::SeqCst) {
                            let mut headers = HeaderMap::new();
                            if let Some(wait) = handler.refresh_server_retry_after.lock().unwrap().as_ref() {
                                headers.insert(axum::http::header::RETRY_AFTER, wait.parse().unwrap());
                            }
                            if handler.spent_budget.load(Ordering::SeqCst) {
                                headers.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
                                headers.insert("x-ratelimit-reset-after", HeaderValue::from_static("30"));
                            }
                            return (StatusCode::SERVICE_UNAVAILABLE, headers).into_response();
                        }
                        if handler.refresh_failed.load(Ordering::SeqCst) {
                            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "invalid_grant"}))).into_response();
                        }
                        Json(serde_json::json!({
                            "access_token": "refreshed-token",
                            "refresh_token": "refreshed-refresh",
                            "expires_in": 3600,
                        })).into_response()
                    }
                }),
            )
            .route(
                "/me",
                get(move || {
                    let handler = Arc::clone(&me_handler);
                    async move {
                        handler.me_calls.fetch_add(1, Ordering::SeqCst);
                        if handler.me_unauthorized.load(Ordering::SeqCst) {
                            return StatusCode::FORBIDDEN.into_response();
                        }
                        Json(serde_json::json!({
                            "id": "42",
                            "username": "live-profile",
                            "global_name": null,
                            "avatar": null
                        })).into_response()
                    }
                }),
            )
            .route(
            "/guilds",
            get(move |headers: HeaderMap, Query(query): Query<std::collections::HashMap<String, String>>| {
                let handler = Arc::clone(&handler);
                async move {
                    let bearer = headers
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("");
                    if handler.unauthorized_until_refresh.load(Ordering::SeqCst)
                        && bearer != "Bearer refreshed-token"
                        || handler.still_unauthorized.load(Ordering::SeqCst)
                    {
                        handler.calls.fetch_add(1, Ordering::SeqCst);
                        return (
                            StatusCode::UNAUTHORIZED,
                            axum::http::HeaderMap::new(),
                            Json(serde_json::json!({"error": "invalid token"})),
                        )
                            .into_response();
                    }
                    assert!(
                        bearer == "Bearer fixture-token" || bearer == "Bearer refreshed-token",
                        "{bearer}"
                    );
                    let call = handler.calls.fetch_add(1, Ordering::SeqCst);
                    if call == 0 {
                        let db = handler
                            .replace_destination_during_lookup
                            .lock()
                            .unwrap()
                            .take();
                        if let Some(db) = db {
                            db::enroll_guild(&db, 20, 101).await.unwrap();
                        }
                    }
                    let retire = handler.retire_during_lookup.lock().unwrap().take();
                    if let Some(db) = retire {
                        db::retire_guild_enrollment(&db, 10, 100).await.unwrap();
                    }
                    if handler.failed.load(Ordering::SeqCst) {
                        return (
                            StatusCode::SERVICE_UNAVAILABLE,
                            axum::http::HeaderMap::new(),
                            Json(serde_json::json!({"error":"offline"})),
                        )
                            .into_response();
                    }
                    if handler.limit_on_next_page.load(Ordering::SeqCst) && query.contains_key("after")
                        || handler.rate_limited.load(Ordering::SeqCst)
                    {
                        let mut headers = axum::http::HeaderMap::new();
                        let retry_after = handler.retry_after.lock().unwrap().clone();
                        if !retry_after.is_empty() {
                            headers.insert(
                                axum::http::header::RETRY_AFTER,
                                retry_after.parse().unwrap(),
                            );
                        }
                        if handler.global_scope.load(Ordering::SeqCst) {
                            headers.insert(
                                axum::http::HeaderName::from_static("x-ratelimit-scope"),
                                axum::http::HeaderValue::from_static("global"),
                            );
                        }
                        let mut body = serde_json::json!({"message": "You are being rate limited."});
                        if handler.global_scope.load(Ordering::SeqCst) {
                            body["global"] = serde_json::json!(true);
                        }
                        if let Some(seconds) = *handler.body_retry_after.lock().unwrap() {
                            body["retry_after"] = serde_json::json!(seconds);
                        }
                        return (StatusCode::TOO_MANY_REQUESTS, headers, Json(body)).into_response();
                    }
                    let guilds = if handler.limit_on_next_page.load(Ordering::SeqCst) {
                        (0..200).map(managed_guild).collect::<Vec<_>>()
                    } else {
                        handler.guilds.lock().unwrap().clone()
                    };
                    let mut headers = HeaderMap::new();
                    if handler.spent_budget.load(Ordering::SeqCst) {
                        headers.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
                        headers.insert("x-ratelimit-reset-after", HeaderValue::from_static("30"));
                    }
                    (
                        StatusCode::OK,
                        headers,
                        Json(serde_json::json!(guilds)),
                    )
                        .into_response()
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, routes).await.unwrap();
        });
        let (token_endpoint, token_server) = if !matches!(token_transport, TokenTransport::Normal) {
            let listener = tokio::net::TcpListener::bind((address.ip(), 0))
                .await
                .unwrap();
            let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
            let handler = Arc::clone(&discord);
            let server = tokio::spawn(async move {
                let mut connections = Vec::new();
                loop {
                    let (connection, _) = listener.accept().await.unwrap();
                    handler.token_calls.fetch_add(1, Ordering::SeqCst);
                    if matches!(token_transport, TokenTransport::Timeout) {
                        connections.push(connection);
                    } else {
                        drop(connection);
                    }
                }
            });
            (endpoint, Some(server))
        } else {
            (format!("http://{address}/token"), None)
        };
        let path = std::env::temp_dir().join(format!(
            "jmodguard_api_enrollment_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = db::Db::open(&path, 2).unwrap();
        let mut cfg = crate::config::ApiProcessConfig::default();
        cfg.bot.owner_id = if operator { 42 } else { 99 };
        cfg.web.oauth_client_id = oauth_client_id.into();
        let mut app = ApiState::new(cfg, db).unwrap();
        Arc::get_mut(&mut Arc::get_mut(&mut app).unwrap().oauth)
            .unwrap()
            .guilds_endpoint = Some(format!("http://{address}/guilds"));
        Arc::get_mut(&mut Arc::get_mut(&mut app).unwrap().oauth)
            .unwrap()
            .token_endpoint = Some(token_endpoint);
        Arc::get_mut(&mut Arc::get_mut(&mut app).unwrap().oauth)
            .unwrap()
            .me_endpoint = Some(format!("http://{address}/me"));
        if matches!(token_transport, TokenTransport::Timeout) {
            Arc::get_mut(&mut Arc::get_mut(&mut app).unwrap().oauth)
                .unwrap()
                .http = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(1))
                .build()
                .unwrap();
        }
        tests::seed_auth_caches(&app, 42, vec![managed_guild(10), managed_guild(20)]);
        let store = MemoryStore::default();
        let session = Session::new(None, Arc::new(store.clone()), None);
        session.insert(SESSION_USER, 42u64).await.unwrap();
        session
            .insert(SESSION_TOKEN, "fixture-token")
            .await
            .unwrap();
        session
            .insert(SESSION_REFRESH, "fixture-refresh")
            .await
            .unwrap();
        session.save().await.unwrap();
        let cookie = format!("{SESSION_COOKIE_NAME}={}", session.id().unwrap());
        let router = router_with_store(Arc::clone(&app), store.clone());
        Self {
            app,
            router,
            cookie,
            store,
            token_server,
            discord,
            server,
            path,
        }
    }
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: &str,
    ) -> (StatusCode, serde_json::Value) {
        let guild = path
            .split('/')
            .nth(3)
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0);
        let token = if guild > 0 {
            db::guild_enrollment(&self.app.db, guild)
                .await
                .ok()
                .flatten()
                .map(|e| e.id)
        } else {
            None
        };
        let mut value: serde_json::Value =
            serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
        if path.ends_with("/copy") {
            let source = value["from_guild_id"].as_i64().unwrap_or(0);
            value["from_enrollment_id"] = serde_json::json!(
                db::guild_enrollment(&self.app.db, source)
                    .await
                    .ok()
                    .flatten()
                    .map(|e| e.id)
            );
        }
        let owned = if value.is_null() {
            body.to_owned()
        } else {
            value.to_string()
        };
        self.request_token(method, path, &owned, token.as_deref())
            .await
    }
    async fn request_preview(
        &self,
        method: &str,
        path: &str,
        body: &str,
        preview: &str,
    ) -> (StatusCode, serde_json::Value) {
        let guild = path
            .split('/')
            .nth(3)
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0);
        let token = if guild > 0 {
            db::guild_enrollment(&self.app.db, guild)
                .await
                .ok()
                .flatten()
                .map(|e| e.id)
        } else {
            None
        };
        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("cookie", &self.cookie)
                    .header("content-type", "application/json")
                    .header("x-jmodguard-enrollment", token.as_deref().unwrap_or(""))
                    .header("x-jmodguard-preview", preview)
                    .body(Body::from(body.to_owned()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 512 * 1024)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                serde_json::Value::String(String::from_utf8(bytes.to_vec()).unwrap())
            }),
        )
    }
    async fn request_token(
        &self,
        method: &str,
        path: &str,
        body: &str,
        token: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("cookie", &self.cookie)
                    .header("content-type", "application/json")
                    .header("x-jmodguard-enrollment", token.unwrap_or(""))
                    .body(Body::from(body.to_owned()))
                    .unwrap(),
            )
            .await
            .unwrap();
        super::assert_uncacheable_headers(response.headers());
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 512 * 1024)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                serde_json::Value::String(String::from_utf8(bytes.to_vec()).unwrap())
            }),
        )
    }
    async fn enroll(&self, guild: i64) {
        db::enroll_guild(&self.app.db, guild, 100).await.unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
        if let Some(server) = &self.token_server {
            server.abort();
        }
        crate::tenant::remove_sidecars(&self.path);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
        }
    }
}

#[tokio::test]
async fn retired_and_unregistered_guild_routes_deny_even_installation_owner() {
    for operator in [false, true] {
        let f = Fixture::new(operator).await;
        f.enroll(10).await;
        // Verify denial while placed-guild payload awaits asynchronous erasure.
        f.app
            .tenant
            .activate(db::guild_enrollment(&f.app.db, 10).await.unwrap().unwrap())
            .await
            .unwrap()
            .unwrap();
        db::patch_settings(&f.app.db, 10, Some(Some("private".into())), None, None)
            .await
            .unwrap();
        db::retire_guild_enrollment(&f.app.db, 10, 100)
            .await
            .unwrap();
        for guild in [10, 20] {
            for suffix in [
                "config",
                "rules/anything",
                "badwords",
                "commands",
                "commands/anything",
                "templates",
                "templates/message_edit",
                "logchannels",
                "logchannels/message_edit",
                "events",
                "bans",
                "stats",
                "cases",
                "case-delete-roles",
            ] {
                let (status, body) = f
                    .request("GET", &format!("/api/guilds/{guild}/{suffix}"), "")
                    .await;
                assert_eq!(status, StatusCode::CONFLICT, "{suffix}: {body}");
                assert_eq!(body["code"], "guild_enrollment_inactive");
                assert!(!body.to_string().contains("private"));
            }
            for suffix in ["config", "case-delete-roles"] {
                let (status, body) = f
                    .request("POST", &format!("/api/guilds/{guild}/{suffix}"), "")
                    .await;
                assert_eq!(status, StatusCode::CONFLICT, "POST {suffix}: {body}");
                assert_eq!(body["code"], "guild_enrollment_inactive");
            }
        }
        for (method, suffix, body) in [
            (
                "PUT",
                "settings",
                r#"{"prefix":"changed","bot_name":"changed"}"#,
            ),
            ("DELETE", "badwords/1", ""),
            ("DELETE", "commands/anything", r#"{}"#),
            ("DELETE", "rules/anything", r#"{}"#),
            ("DELETE", "templates/message_edit", r#"{}"#),
            (
                "PUT",
                "logfilters/message_edit",
                r#"{"users":["123"],"channels":[]}"#,
            ),
            (
                "DELETE",
                "logchannels/message_edit",
                r#"{"request_id":"1000_clear"}"#,
            ),
            ("POST", "testregex", r#"{"pattern":"a","sample":"a"}"#),
            (
                "POST",
                "unban",
                r#"{"request_id":"1000_unban","user_id":"99"}"#,
            ),
            ("DELETE", "cases/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", ""),
            (
                "PUT",
                "case-delete-roles",
                r#"{"roles":[],"revision":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}"#,
            ),
        ] {
            let (status, value) = f
                .request(method, &format!("/api/guilds/10/{suffix}"), body)
                .await;
            assert_eq!(status, StatusCode::CONFLICT, "{method} {suffix}: {value}");
            assert_eq!(value["code"], "guild_enrollment_inactive");
        }
        assert_eq!(
            db::get_settings(&f.app.db, 10).await.unwrap().prefix,
            "private"
        );
        assert!(
            db::nickname_reconcile_status(&f.app.db, 10)
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn current_enrollment_allows_access_but_live_permission_revocation_wins_over_cache() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    let (status, _) = f
        .request("PUT", "/api/guilds/10/settings", r#"{"prefix":"current"}"#)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        f.request("GET", "/api/guilds/10/config", "").await.0,
        StatusCode::OK
    );
    expire_auth_cache(&f.app, GUILD_AUTH_CACHE_TTL).await;
    f.discord.guilds.lock().unwrap().clear();
    let (status, body) = f
        .request("PUT", "/api/guilds/10/settings", r#"{"prefix":"revoked"}"#)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("cannot manage"));
    assert_eq!(
        db::get_settings(&f.app.db, 10).await.unwrap().prefix,
        "current"
    );
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn copy_requires_both_enrollments_even_with_global_destination() {
    let f = Fixture::new(true).await;
    f.enroll(10).await;
    f.enroll(20).await;
    db::patch_settings(&f.app.db, 10, Some(Some("source".into())), None, None)
        .await
        .unwrap();
    assert_eq!(
        f.request(
            "POST",
            "/api/guilds/20/copy",
            r#"{"from_guild_id":10,"sections":["settings"]}"#
        )
        .await
        .0,
        StatusCode::OK
    );
    db::retire_guild_enrollment(&f.app.db, 10, 100)
        .await
        .unwrap();
    for destination in [0, 20] {
        let (status, body) = f
            .request(
                "POST",
                &format!("/api/guilds/{destination}/copy"),
                r#"{"from_guild_id":10,"sections":["settings"]}"#,
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
    }
    let (status, _) = f
        .request(
            "POST",
            "/api/guilds/10/copy",
            r#"{"from_guild_id":20,"sections":["settings"]}"#,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_ne!(
        db::get_settings(&f.app.db, 0).await.unwrap().prefix,
        "source"
    );
}

#[tokio::test]
async fn unavailable_membership_database_fails_closed_without_leaking_details() {
    let f = Fixture::new(true).await;
    f.enroll(10).await;
    f.app
        .db
        .with_conn(|conn| {
            conn.execute_batch_bounded(
                "ALTER TABLE guild_enrollments RENAME TO private_fixture_table",
                64 * 1024 * 1024,
            )
        })
        .await
        .unwrap();
    let (status, body) = f.request("GET", "/api/guilds/10/config", "").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["code"], "guild_access_unverified");
    assert!(!body.to_string().contains("table"));
    assert_eq!(
        f.request("GET", "/api/guilds/0/config", "").await.0,
        StatusCode::OK,
        "global owner scope needs no guild enrollment"
    );
    f.app
        .db
        .with_conn(|conn| {
            conn.execute_batch_bounded(
                "ALTER TABLE private_fixture_table RENAME TO guild_enrollments",
                64 * 1024 * 1024,
            )
        })
        .await
        .unwrap();
    assert_eq!(
        f.request("GET", "/api/guilds/10/config", "").await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn discord_failure_never_uses_cached_authority_or_retires_membership() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    f.discord.failed.store(true, Ordering::SeqCst);
    let (status, _) = f.request("GET", "/api/guilds/10/config", "").await;
    assert!(!status.is_success());
    assert_ne!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(
        !f.request("GET", "/api/guilds/10/config", "")
            .await
            .0
            .is_success()
    );
    assert_eq!(
        f.discord.calls.load(Ordering::SeqCst),
        2,
        "failed fetch must not populate the authorization cache"
    );
    assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), 0);
    assert!(db::guild_enrollment(&f.app.db, 10).await.unwrap().is_some());
}

#[tokio::test]
async fn guild_list_rate_limit_is_temporary_and_does_not_refresh() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    f.discord.rate_limited.store(true, Ordering::SeqCst);
    *f.discord.body_retry_after.lock().unwrap() = Some(4.5);
    let response = f
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/guilds/10/cases/case-1")
                .header("cookie", &f.cookie)
                .header("content-type", "application/json")
                .header(
                    "x-jmodguard-enrollment",
                    db::guild_enrollment(&f.app.db, 10)
                        .await
                        .unwrap()
                        .unwrap()
                        .id,
                )
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .unwrap(),
        "5"
    );
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["code"], "discord_rate_limited");
    assert_eq!(body["retry_after"], 5);
    assert!(!body["error"].as_str().unwrap().contains("400"));
    assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), 1);
    let (status, _) = f.request("POST", "/api/guilds/10/cases/case-1", "{}").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        f.discord.calls.load(Ordering::SeqCst),
        1,
        "cooldown must not send another guild-list request"
    );
    assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), 0);
    assert!(db::guild_enrollment(&f.app.db, 10).await.unwrap().is_some());
}

#[tokio::test]
async fn guild_list_rate_limit_on_later_page_does_not_authorize() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    f.discord.limit_on_next_page.store(true, Ordering::SeqCst);
    let (status, body) = f.request("POST", "/api/guilds/10/cases/case-1", "{}").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["code"], "discord_rate_limited");
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), 2);
    assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), 0);
    let (again, _) = f.request("POST", "/api/guilds/10/cases/case-1", "{}").await;
    assert_eq!(again, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn concurrent_guild_checks_share_one_rate_limit() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    f.discord.rate_limited.store(true, Ordering::SeqCst);
    let first = f.request("POST", "/api/guilds/10/cases/case-1", "{}");
    let second = f.request("GET", "/api/guilds/10/config", "");
    let ((left, _), (right, _)) = tokio::join!(first, second);
    assert_eq!(left, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(right, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn display_keeps_the_last_guild_list_while_discord_is_rate_limited() {
    let f = Fixture::new(false).await;
    f.discord.rate_limited.store(true, Ordering::SeqCst);
    let (status, body) = f.request("POST", "/api/me", "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body["guilds"].as_array().unwrap().is_empty());
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), 0);
}

fn profile_older_than_user_ttl() -> std::time::Instant {
    std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(120))
        .expect("profile cache must be older than the 60s user ttl")
}

fn age_user_cache(app: &ApiState, user_id: u64) {
    let user = {
        app.user_cache
            .get(&user_id)
            .expect("seeded user")
            .value()
            .1
            .clone()
    };
    app.user_cache
        .insert(user_id, (profile_older_than_user_ttl(), user));
}

fn age_guild_cache(app: &ApiState, user_id: u64) {
    let guilds = {
        app.guild_cache
            .get(&user_id)
            .expect("seeded guilds")
            .value()
            .1
            .clone()
    };
    app.guild_cache
        .insert(user_id, (profile_older_than_user_ttl(), guilds));
}

#[tokio::test]
async fn display_keeps_cached_guilds_when_the_user_cache_is_cold_during_a_global_cooldown() {
    let f = Fixture::new(false).await;
    *f.discord.retry_after.lock().unwrap() = "120".into();
    f.discord.rate_limited.store(true, Ordering::SeqCst);
    f.discord.global_scope.store(true, Ordering::SeqCst);
    let limited = f
        .app
        .oauth
        .my_guilds(42, "fixture-token")
        .await
        .unwrap_err();
    assert!(matches!(
        limited,
        oauth::GuildListError::RateLimited { fresh: true, .. }
    ));
    age_user_cache(&f.app, 42);
    assert!(
        f.app
            .guild_cache
            .get(&42)
            .expect("warm guild list")
            .value()
            .0
            .elapsed()
            < std::time::Duration::from_secs(60)
    );

    let guild_calls = f.discord.calls.load(Ordering::SeqCst);
    let (status, body) = f.request("POST", "/api/me", "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body["guilds"].as_array().unwrap().is_empty());
    assert_eq!(body["user"]["id"], "42");
    assert_eq!(body["user"]["username"], "tester");
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), guild_calls);
    assert_eq!(f.discord.me_calls.load(Ordering::SeqCst), 0);

    f.app.user_cache.remove(&42);
    let (status, body) = f.request("POST", "/api/me", "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body["guilds"].as_array().unwrap().is_empty());
    assert_eq!(body["user"]["id"], "42");
    assert_eq!(body["user"]["username"], "");
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), guild_calls);
    assert_eq!(f.discord.me_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn display_keeps_cached_guilds_when_a_global_limit_blocks_the_profile_refresh() {
    let f = Fixture::new(false).await;
    *f.discord.retry_after.lock().unwrap() = "120".into();
    f.discord.rate_limited.store(true, Ordering::SeqCst);
    f.discord.global_scope.store(true, Ordering::SeqCst);
    age_user_cache(&f.app, 42);
    age_guild_cache(&f.app, 42);

    let (status, body) = f.request("POST", "/api/me", "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body["guilds"].as_array().unwrap().is_empty());
    assert_eq!(body["user"]["id"], "42");
    assert_eq!(body["guilds"][0]["id"], "10");
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.discord.me_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn display_refreshes_a_cold_user_when_discord_is_not_rate_limited() {
    let f = Fixture::new(false).await;
    age_user_cache(&f.app, 42);
    let (status, body) = f.request("POST", "/api/me", "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body["guilds"].as_array().unwrap().is_empty());
    assert_eq!(body["user"]["username"], "live-profile");
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.discord.me_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stale_display_fetches_once_then_serves_the_cached_list() {
    let f = Fixture::new(false).await;
    let stale = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(120))
        .expect("display cooldown test needs a monotonic clock older than the cache ttl");
    f.app
        .guild_cache
        .insert(42, (stale, vec![managed_guild(10)]));
    f.discord.rate_limited.store(true, Ordering::SeqCst);
    let (status, body) = f.request("POST", "/api/me", "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["guilds"][0]["id"], "10");
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), 1);
    let (again, _) = f.request("POST", "/api/me", "").await;
    assert_eq!(again, StatusCode::OK);
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn guild_list_cooldown_survives_a_new_api_process() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    f.app.guild_cache.clear();
    f.discord.rate_limited.store(true, Ordering::SeqCst);
    let (status, _) = f.request("POST", "/api/guilds/10/cases/case-1", "{}").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), 1);

    let endpoint = f.app.oauth.guilds_endpoint.clone().unwrap();
    let cfg = (*f.app.cfg).clone();
    let db = db::Db::open(&f.path, 2).unwrap();
    let mut restarted = ApiState::new(cfg, db).unwrap();
    std::sync::Arc::get_mut(&mut std::sync::Arc::get_mut(&mut restarted).unwrap().oauth)
        .unwrap()
        .guilds_endpoint = Some(endpoint);
    let err = restarted
        .oauth
        .my_guilds(42, "fixture-token")
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        oauth::GuildListError::RateLimited { fresh: false, .. }
    ));
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn owner_administration_does_not_fetch_guilds_while_rate_limited() {
    let f = Fixture::new(true).await;
    f.app.guild_cache.clear();
    f.app.user_cache.clear();
    f.discord.rate_limited.store(true, Ordering::SeqCst);
    let (status, body) = f.request("POST", "/api/updates", "").await;
    assert_ne!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_ne!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_ne!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), 0);

    let blocked = Fixture::new(false).await;
    blocked.app.guild_cache.clear();
    blocked.app.user_cache.clear();
    blocked.discord.rate_limited.store(true, Ordering::SeqCst);
    let (status, _) = blocked.request("POST", "/api/updates", "").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(blocked.discord.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn expired_guild_token_refreshes_once_and_server_errors_do_not() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    f.discord
        .unauthorized_until_refresh
        .store(true, Ordering::SeqCst);
    let (status, _) = f.request("GET", "/api/guilds/10/config", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), 2);

    expire_auth_cache(&f.app, GUILD_AUTH_CACHE_TTL).await;
    f.discord.failed.store(true, Ordering::SeqCst);
    f.discord
        .unauthorized_until_refresh
        .store(false, Ordering::SeqCst);
    let (status, _) = f.request("GET", "/api/guilds/10/config", "").await;
    assert!(!status.is_success());
    assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn revoked_permission_stays_denied_after_token_refresh() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    f.discord
        .unauthorized_until_refresh
        .store(true, Ordering::SeqCst);
    expire_auth_cache(&f.app, GUILD_AUTH_CACHE_TTL).await;
    f.discord.guilds.lock().unwrap().clear();
    let (status, _) = f
        .request("PUT", "/api/guilds/10/settings", r#"{"prefix":"nope"}"#)
        .await;
    assert!(!status.is_success());
    assert_ne!(
        db::get_settings(&f.app.db, 10).await.unwrap().prefix,
        "nope"
    );
    assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retirement_during_discord_authorization_blocks_the_settings_write() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    *f.discord.retire_during_lookup.lock().unwrap() = Some(f.app.db.clone());
    let (status, body) = f
        .request("PUT", "/api/guilds/10/settings", r#"{"prefix":"too late"}"#)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_ne!(
        db::get_settings(&f.app.db, 10).await.unwrap().prefix,
        "too late"
    );
}

#[tokio::test]
async fn out_of_range_guild_ids_cannot_wrap_into_database_scopes() {
    let f = Fixture::new(true).await;
    f.discord
        .guilds
        .lock()
        .unwrap()
        .push(managed_guild(u64::MAX));
    let (status, body) = f
        .request(
            "PUT",
            &format!("/api/guilds/{}/settings", u64::MAX),
            r#"{"prefix":"overflow"}"#,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid guild id");
    assert_ne!(
        db::get_settings(&f.app.db, -1).await.unwrap().prefix,
        "overflow"
    );
}

#[tokio::test]
async fn copy_cannot_write_destination_replaced_after_its_authorization() {
    let f = Fixture::new(true).await;
    f.enroll(10).await;
    f.enroll(20).await;
    db::patch_settings(&f.app.db, 10, Some(Some("source".into())), None, None)
        .await
        .unwrap();
    *f.discord.replace_destination_during_lookup.lock().unwrap() = Some(f.app.db.clone());
    let (status, body) = f
        .request(
            "POST",
            "/api/guilds/20/copy",
            r#"{"from_guild_id":10,"sections":["settings"]}"#,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "guild_enrollment_changed");
    assert_ne!(
        db::get_settings(&f.app.db, 20).await.unwrap().prefix,
        "source"
    );
}

#[tokio::test]
async fn configuration_crud_routes_use_scoped_writes_for_current_guild_and_global() {
    let f = Fixture::new(true).await;
    f.enroll(10).await;
    let rule = crate::defaults::default_rules().remove(0);
    let mut command = crate::defaults::default_commands().remove(0);
    command.name = "fixture".into();
    for (guild, scope) in [(10, "guild"), (10, "global"), (0, "global")] {
        let base = format!("/api/guilds/{guild}");
        let rule_path = format!("{base}/rules/{}", crate::types::RULE_IDS[0]);
        for (method, path, body) in [
            (
                "PUT",
                rule_path.clone(),
                serde_json::json!({"scope":scope,"data":rule}).to_string(),
            ),
            (
                "POST",
                format!("{base}/commands"),
                serde_json::json!({"scope":scope,"data":command}).to_string(),
            ),
            (
                "PUT",
                format!("{base}/commands/fixture"),
                serde_json::json!({"scope":scope,"data":command}).to_string(),
            ),
            (
                "PUT",
                format!("{base}/templates/message_edit"),
                serde_json::json!({"scope":scope,"template":"fixture"}).to_string(),
            ),
        ] {
            let (status, body) = f.request(method, &path, &body).await;
            assert_eq!(status, StatusCode::OK, "{method} {path}: {body}");
        }
        let (status, body) = f
            .request(
                "POST",
                &format!("{base}/badwords"),
                &serde_json::json!({"scope":scope,"pattern":"fixture"}).to_string(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let id = body["id"].as_i64().unwrap();
        for path in [
            rule_path,
            format!("{base}/commands/fixture"),
            format!("{base}/templates/message_edit"),
            format!("{base}/badwords/{id}"),
        ] {
            let (status, body) = f
                .request(
                    "DELETE",
                    &path,
                    &serde_json::json!({"scope":scope}).to_string(),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{path}: {body}");
        }
    }
}

#[tokio::test]
async fn guild_manager_cannot_mutate_global_configuration_or_probe_foreign_badwords() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    f.enroll(20).await;
    let global = db::add_badword(&f.app.db, 0, "global", None).await.unwrap();
    let other = db::add_badword(&f.app.db, 20, "other", None).await.unwrap();
    for id in [global, other, i64::MAX] {
        let (status, body) = f
            .request("DELETE", &format!("/api/guilds/10/badwords/{id}"), "")
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "badword not found");
    }
    let (status, _) = f
        .request(
            "PUT",
            "/api/guilds/10/templates/message_edit",
            r#"{"scope":"global","template":"rejected"}"#,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        db::get_badword_primary(&f.app.db, global)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        db::get_badword_primary(&f.app.db, other)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn configuration_edits_reject_missing_and_replaced_page_memberships() {
    let f = Fixture::new(true).await;
    f.enroll(10).await;
    let (status, loaded) = f.request("GET", "/api/guilds/10/config", "").await;
    assert_eq!(status, StatusCode::OK);
    let old = loaded["enrollment_id"].as_str().unwrap();
    db::enroll_guild(&f.app.db, 10, 101).await.unwrap();
    let rule = crate::defaults::default_rules().remove(0);
    let command = crate::defaults::default_commands().remove(0);
    let version = db::get_config_version(&f.app.db).await.unwrap();
    for scope in ["guild", "global"] {
        let scope_body = serde_json::json!({"scope":scope}).to_string();
        for (method, suffix, body) in [
            ("PUT", "settings", r#"{"prefix":"stale"}"#.to_owned()),
            (
                "PUT",
                "rules/flood",
                serde_json::json!({"scope":scope,"data":rule}).to_string(),
            ),
            ("DELETE", "rules/flood", scope_body.clone()),
            (
                "POST",
                "badwords",
                serde_json::json!({"scope":scope,"pattern":"stale"}).to_string(),
            ),
            ("DELETE", "badwords/1", String::new()),
            (
                "POST",
                "commands",
                serde_json::json!({"scope":scope,"data":command}).to_string(),
            ),
            (
                "PUT",
                "commands/fixture",
                serde_json::json!({"scope":scope,"data":command}).to_string(),
            ),
            ("DELETE", "commands/fixture", scope_body.clone()),
            (
                "PUT",
                "templates/message_edit",
                serde_json::json!({"scope":scope,"template":"stale"}).to_string(),
            ),
            ("DELETE", "templates/message_edit", scope_body),
            (
                "POST",
                "copy",
                r#"{"from_guild_id":"20","sections":["settings"]}"#.to_owned(),
            ),
        ] {
            for token in [None, Some(old)] {
                let (status, body) = f
                    .request_token(method, &format!("/api/guilds/10/{suffix}"), &body, token)
                    .await;
                assert_eq!(status, StatusCode::CONFLICT, "{method} {suffix}: {body}");
                assert_eq!(body["code"], "guild_enrollment_changed");
            }
        }
    }
    assert_eq!(db::get_config_version(&f.app.db).await.unwrap(), version);
    let (_, fresh) = f.request("GET", "/api/guilds/10/config", "").await;
    assert_ne!(fresh["enrollment_id"], loaded["enrollment_id"]);
    let (status, body) = f
        .request_token(
            "PUT",
            "/api/guilds/10/settings",
            r#"{"prefix":"fresh"}"#,
            fresh["enrollment_id"].as_str(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["config_patch"]["kind"], "settings");
    assert_eq!(body["config_patch"]["scope"], "guild");
    assert_eq!(body["config_patch"]["settings"]["prefix"], "fresh");
    assert_eq!(body["config_patch"]["prefix_present"], true);
    assert_eq!(body["config_patch"]["prefix"], "fresh");
    assert_eq!(
        db::get_settings(&f.app.db, 10).await.unwrap().prefix,
        "fresh"
    );
    let (status, body) = f
        .request_token(
            "PUT",
            "/api/guilds/10/settings",
            r#"{"bot_name":"Fresh Guard"}"#,
            fresh["enrollment_id"].as_str(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["config_patch"]["bot_name_present"], true);
    assert_eq!(
        body["config_patch"]["nickname_reconcile"]["state"],
        "pending"
    );
    assert_eq!(
        body["config_patch"]["nickname_reconcile"]["attempt_count"],
        0
    );
    assert!(body["config_patch"]["nickname_reconcile"]["last_error"].is_null());
    let rule = crate::defaults::default_rules().remove(0);
    let (status, body) = f
        .request_token(
            "PUT",
            "/api/guilds/10/rules/mention_spam",
            &serde_json::json!({"scope":"guild","data":rule.clone()}).to_string(),
            fresh["enrollment_id"].as_str(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["config_patch"]["kind"], "rule");
    assert_eq!(body["config_patch"]["id"], "mention_spam");
    assert_eq!(body["config_patch"]["scope"], "guild");
    assert_eq!(body["config_patch"]["value"], serde_json::json!(rule));
}

#[tokio::test]
async fn copy_preserves_the_source_selected_by_the_browser() {
    let f = Fixture::new(true).await;
    f.enroll(10).await;
    f.enroll(20).await;
    let (_, source) = f.request("GET", "/api/guilds/10/config", "").await;
    let (_, dest) = f.request("GET", "/api/guilds/20/config", "").await;
    db::enroll_guild(&f.app.db, 10, 101).await.unwrap();
    let version = db::get_config_version(&f.app.db).await.unwrap();
    for guild in [0, 20] {
        for token in [serde_json::Value::Null, source["enrollment_id"].clone()] {
            let body = serde_json::json!({"from_guild_id":"10", "from_enrollment_id":token, "sections":["settings"]});
            let (status, body) = f
                .request_token(
                    "POST",
                    &format!("/api/guilds/{guild}/copy"),
                    &body.to_string(),
                    dest["enrollment_id"].as_str(),
                )
                .await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert_eq!(body["code"], "guild_enrollment_changed");
        }
    }
    assert_eq!(db::get_config_version(&f.app.db).await.unwrap(), version);
    let (_, source) = f.request("GET", "/api/guilds/10/config", "").await;
    let body = serde_json::json!({"from_guild_id":"10", "from_enrollment_id":source["enrollment_id"], "sections":["settings"]});
    let (status, body) = f
        .request_token(
            "POST",
            "/api/guilds/20/copy",
            &body.to_string(),
            dest["enrollment_id"].as_str(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn configuration_snapshot_checks_its_captured_membership() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    let old = db::guild_enrollment(&f.app.db, 10).await.unwrap().unwrap();
    db::enroll_guild(&f.app.db, 10, 101).await.unwrap();
    let result =
        db::load_enrolled_web_config_snapshot(&f.app.db, db::ConfigurationScope::Guild(old)).await;
    assert!(result.err().unwrap().is::<db::EnrollmentChanged>());
    let current = db::guild_enrollment(&f.app.db, 10).await.unwrap().unwrap();
    db::load_enrolled_web_config_snapshot(&f.app.db, db::ConfigurationScope::Guild(current))
        .await
        .unwrap();
}

fn action_key(label: &str) -> String {
    format!("{}_{}", chrono::Utc::now().timestamp(), label)
}

#[tokio::test]
async fn member_sync_controls_require_an_existing_job_and_missing_job_receipt_fails() {
    use jmodguard_bot::action_worker::{DiscordActions, Outcome, WorkerLimits};

    struct Allowed;
    #[async_trait::async_trait]
    impl DiscordActions for Allowed {
        fn bot_user_id(&self) -> Option<i64> {
            Some(42)
        }
        async fn membership_matches(&self, _: &db::BotAction) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn authorized(&self, _: &db::BotAction) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn valid_log_channel(&self, _: i64, _: i64) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn unban(&self, _: i64, _: i64, _: &str) -> Outcome {
            panic!("not an unban")
        }
    }

    let f = Fixture::new(false).await;
    f.enroll(10).await;
    let enrollment = db::guild_enrollment(&f.app.db, 10).await.unwrap().unwrap();
    f.app
        .tenant
        .activate(enrollment.clone())
        .await
        .unwrap()
        .unwrap();
    let (store, scope) = f.app.tenant.live(enrollment).await.unwrap();
    let path = "/api/guilds/10/member-sync/actions";
    let body =
        serde_json::json!({"request_id":action_key("unadmitted"),"operation":"pause"}).to_string();
    let (status, rejected) = f.request("POST", path, &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{rejected}");
    assert_eq!(rejected["code"], "member_sync_missing_job");
    let (_, actions) = f.request("GET", "/api/guilds/10/actions", "").await;
    assert_eq!(actions.as_array().unwrap().len(), 0);

    let seeded = store
        .initialize_member_directory(scope.clone(), 42, chrono::Utc::now().timestamp_millis())
        .await
        .unwrap();
    assert!(seeded.run_id.is_some());
    let body =
        serde_json::json!({"request_id":action_key("queued"),"operation":"pause"}).to_string();
    let (status, queued) = f.request("POST", path, &body).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{queued}");
    assert_eq!(queued["state"], "pending");

    let mut guild_path = f.path.as_os_str().to_owned();
    guild_path.push(".guild.db");
    let guild_db = libsql::Builder::new_local(std::path::PathBuf::from(guild_path))
        .build()
        .await
        .unwrap();
    guild_db
        .connect()
        .unwrap()
        .execute("DELETE FROM guild_member_sync WHERE guild_id=10", ())
        .await
        .unwrap();
    drop(guild_db);

    jmodguard_bot::action_worker::drain_with_tenant(
        &f.app.db,
        &f.app.tenant,
        &Allowed,
        &WorkerLimits::default(),
    )
    .await
    .unwrap();
    let (_, actions) = f.request("GET", "/api/guilds/10/actions", "").await;
    assert_eq!(actions[0]["id"], queued["id"]);
    assert_eq!(actions[0]["state"], "failed");
    assert_eq!(actions[0]["outcome"], "member_sync_missing_job");

    let body =
        serde_json::json!({"request_id":action_key("missing"),"operation":"retry"}).to_string();
    let (status, rejected) = f.request("POST", path, &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{rejected}");
    assert_eq!(rejected["code"], "member_sync_missing_job");
    let still_missing = store
        .initialize_member_directory(scope, 42, chrono::Utc::now().timestamp_millis())
        .await
        .unwrap();
    assert!(still_missing.run_id.is_none());
    assert_eq!(still_missing.status, "needs_attention");
}

#[tokio::test]
async fn member_sync_controls_reject_invalid_operations_before_queueing() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    let enrollment = db::guild_enrollment(&f.app.db, 10).await.unwrap().unwrap();
    f.app
        .tenant
        .activate(enrollment.clone())
        .await
        .unwrap()
        .unwrap();
    let (store, scope) = f.app.tenant.live(enrollment).await.unwrap();
    store
        .initialize_member_directory(scope.clone(), 42, chrono::Utc::now().timestamp_millis())
        .await
        .unwrap();
    let path = "/api/guilds/10/member-sync/actions";
    let reconcile = serde_json::json!({
        "request_id": action_key("reconcile-early"),
        "operation": "reconcile"
    })
    .to_string();
    let (status, rejected) = f.request("POST", path, &reconcile).await;
    assert_eq!(status, StatusCode::CONFLICT, "{rejected}");
    assert_eq!(rejected["code"], "member_sync_invalid_operation");

    let mut guild_path = f.path.as_os_str().to_owned();
    guild_path.push(".guild.db");
    let guild_db = libsql::Builder::new_local(std::path::PathBuf::from(guild_path))
        .build()
        .await
        .unwrap();
    guild_db
        .connect()
        .unwrap()
        .execute(
            "UPDATE guild_member_sync SET status='needs_attention',status_reason='permission_denied',request_after_ms=? WHERE guild_id=10",
            libsql::params![chrono::Utc::now().timestamp_millis() + 60_000],
        )
        .await
        .unwrap();
    drop(guild_db);

    let resume = serde_json::json!({
        "request_id": action_key("resume-needs-attention"),
        "operation": "resume"
    })
    .to_string();
    let (status, rejected) = f.request("POST", path, &resume).await;
    assert_eq!(status, StatusCode::CONFLICT, "{rejected}");
    assert_eq!(rejected["code"], "member_sync_invalid_operation");

    let retry = serde_json::json!({
        "request_id": action_key("retry-needs-attention"),
        "operation": "retry"
    })
    .to_string();
    let (status, queued) = f.request("POST", path, &retry).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{queued}");
    assert_eq!(queued["state"], "pending");
}

#[tokio::test]
async fn member_sync_retry_admits_audit_pagination_stall_after_initial_complete() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    let enrollment = db::guild_enrollment(&f.app.db, 10).await.unwrap().unwrap();
    f.app
        .tenant
        .activate(enrollment.clone())
        .await
        .unwrap()
        .unwrap();
    let enrollment_id = enrollment.id.clone();
    let (store, scope) = f.app.tenant.live(enrollment).await.unwrap();
    store
        .initialize_member_directory(scope.clone(), 42, chrono::Utc::now().timestamp_millis())
        .await
        .unwrap();
    let completed_at = chrono::Utc::now().timestamp_millis();
    let mut guild_path = f.path.as_os_str().to_owned();
    guild_path.push(".guild.db");
    let guild_db_path = std::path::PathBuf::from(guild_path.clone());
    let guild_db = libsql::Builder::new_local(guild_db_path)
        .build()
        .await
        .unwrap();
    let conn = guild_db.connect().unwrap();
    conn.execute(
        "UPDATE guild_member_seed SET initial_completed_at_ms=?1 WHERE guild_id=10",
        libsql::params![completed_at],
    )
    .await
    .unwrap();
    conn.execute(
        "UPDATE guild_member_sync SET phase='complete',status='complete',completed_at_ms=?1,last_success_at_ms=?1 WHERE guild_id=10",
        libsql::params![completed_at],
    )
    .await
    .unwrap();
    conn.execute(
        "INSERT INTO guild_member_audit_recovery(guild_id,enrollment_id,bot_id,run_id,started_at_ms,status,status_reason,generation,processed)
         VALUES(10,?1,42,'audit-run',?2,'needs_attention','pagination_stalled',0,0)
         ON CONFLICT(guild_id) DO UPDATE SET status='needs_attention',status_reason='pagination_stalled',lease_owner=NULL,lease_until_ms=NULL",
        libsql::params![enrollment_id.as_str(), completed_at],
    )
    .await
    .unwrap();
    drop(conn);
    drop(guild_db);

    let path = "/api/guilds/10/member-sync/actions";
    let retry = serde_json::json!({
        "request_id": action_key("retry-audit-stall"),
        "operation": "retry"
    })
    .to_string();
    let (status, queued) = f.request("POST", path, &retry).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{queued}");
    assert_eq!(queued["state"], "pending");

    let guild_db = libsql::Builder::new_local(std::path::PathBuf::from(guild_path))
        .build()
        .await
        .unwrap();
    guild_db
        .connect()
        .unwrap()
        .execute(
            "UPDATE guild_member_audit_recovery SET status='complete',status_reason=NULL WHERE guild_id=10",
            (),
        )
        .await
        .unwrap();
    drop(guild_db);

    let noop = serde_json::json!({
        "request_id": action_key("retry-complete-no-stall"),
        "operation": "retry"
    })
    .to_string();
    let (status, rejected) = f.request("POST", path, &noop).await;
    assert_eq!(status, StatusCode::CONFLICT, "{rejected}");
    assert_eq!(rejected["code"], "member_sync_invalid_operation");
}

#[tokio::test]
async fn member_sync_status_uses_persisted_bot_identity_not_oauth_client_id() {
    let f = Fixture::with_oauth_client_id(false, "999999999999999990").await;
    f.enroll(10).await;
    let enrollment = db::guild_enrollment(&f.app.db, 10).await.unwrap().unwrap();
    f.app
        .tenant
        .activate(enrollment.clone())
        .await
        .unwrap()
        .unwrap();
    let (store, scope) = f.app.tenant.live(enrollment).await.unwrap();
    store
        .initialize_member_directory(scope.clone(), 42, chrono::Utc::now().timestamp_millis())
        .await
        .unwrap();
    let (status, body) = f.request("GET", "/api/guilds/10/member-sync", "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["run_id"].as_str().is_some(), "{body}");
}

#[tokio::test]
async fn queued_log_change_is_applied_only_by_worker_and_receipt_is_replayable() {
    use jmodguard_bot::action_worker::{DiscordActions, Outcome, WorkerLimits};
    struct Allowed;
    #[async_trait::async_trait]
    impl DiscordActions for Allowed {
        async fn membership_matches(&self, _: &db::BotAction) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn authorized(&self, _: &db::BotAction) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn valid_log_channel(&self, _: i64, _: i64) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn unban(&self, _: i64, _: i64, _: &str) -> Outcome {
            panic!("not an unban")
        }
    }
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    let body = serde_json::json!({"request_id":action_key("set"),"channel_id":"123"}).to_string();
    let (status, queued) = f
        .request("PUT", "/api/guilds/10/logchannels/message_edit", &body)
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{queued}");
    assert_eq!(queued["state"], "pending");
    assert!(queued.get("claim").is_none());
    assert!(
        db::list_log_channels(&f.app.db, 10)
            .await
            .unwrap()
            .is_empty()
    );
    let (_, replay) = f
        .request("PUT", "/api/guilds/10/logchannels/message_edit", &body)
        .await;
    assert_eq!(replay["id"], queued["id"]);
    jmodguard_bot::action_worker::drain_with_tenant(
        &f.app.db,
        &f.app.tenant,
        &Allowed,
        &WorkerLimits::default(),
    )
    .await
    .unwrap();
    let (status, history) = f.request("GET", "/api/guilds/10/actions", "").await;
    assert_eq!(status, StatusCode::OK, "{history}");
    assert_eq!(history[0]["state"], "succeeded");
    let (status, posted) = f.request("POST", "/api/guilds/10/actions", "").await;
    assert_eq!(status, StatusCode::OK, "{posted}");
    assert_eq!(posted, history);
    let (status, mapping) = f
        .request("POST", "/api/guilds/10/logchannels/message_edit", "")
        .await;
    assert_eq!(status, StatusCode::OK, "{mapping}");
    assert_eq!(mapping["guild_override"], "123");
    assert_eq!(
        db::list_log_channels(&f.app.db, 10).await.unwrap(),
        vec![("message_edit".into(), 123)]
    );
    let (_, replay) = f
        .request("PUT", "/api/guilds/10/logchannels/message_edit", &body)
        .await;
    assert_eq!(replay["id"], queued["id"]);
    assert_eq!(replay["state"], "succeeded");

    let clear = serde_json::json!({"request_id":action_key("clear")}).to_string();
    let (status, queued) = f
        .request("DELETE", "/api/guilds/10/logchannels/message_edit", &clear)
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{queued}");
    assert_eq!(queued["state"], "pending");
    assert_eq!(db::list_log_channels(&f.app.db, 10).await.unwrap().len(), 1);
    jmodguard_bot::action_worker::drain_with_tenant(
        &f.app.db,
        &f.app.tenant,
        &Allowed,
        &WorkerLimits::default(),
    )
    .await
    .unwrap();
    assert!(
        db::list_log_channels(&f.app.db, 10)
            .await
            .unwrap()
            .is_empty()
    );
    let (_, replay) = f
        .request("DELETE", "/api/guilds/10/logchannels/message_edit", &clear)
        .await;
    assert_eq!(replay["id"], queued["id"]);
    assert_eq!(replay["state"], "succeeded");
}

#[tokio::test]
async fn actions_require_page_membership_and_reject_rebinding_saved_requests() {
    let f = Fixture::new(true).await;
    f.enroll(10).await;
    let (_, config) = f.request("GET", "/api/guilds/10/config", "").await;
    let old = config["enrollment_id"].as_str();
    let body = serde_json::json!({"request_id":action_key("unban"),"user_id":"99"}).to_string();
    let (_, queued) = f.request("POST", "/api/guilds/10/unban", &body).await;
    db::enroll_guild(&f.app.db, 10, 101).await.unwrap();
    for token in [None, old] {
        for (method, path, body) in [
            ("POST", "/api/guilds/10/unban".to_owned(), body.clone()),
            (
                "PUT",
                "/api/guilds/10/logchannels/message_edit".into(),
                serde_json::json!({"request_id":action_key("log"),"channel_id":"123"}).to_string(),
            ),
            (
                "DELETE",
                "/api/guilds/10/logchannels/message_edit".into(),
                serde_json::json!({"request_id":action_key("clear")}).to_string(),
            ),
            ("GET", "/api/guilds/10/actions".into(), String::new()),
            (
                "DELETE",
                format!("/api/guilds/10/actions/{}", queued["id"].as_str().unwrap()),
                String::new(),
            ),
        ] {
            let (status, body) = f.request_token(method, &path, &body, token).await;
            assert_eq!(status, StatusCode::CONFLICT, "{method} {path}: {body}");
        }
    }
    let (status, rejected) = f.request("POST", "/api/guilds/10/unban", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{rejected}");
    assert_eq!(rejected["code"], "action_enrollment_changed");
    assert!(
        db::recent_bot_actions(&f.app.db, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn action_visibility_cancellation_and_admission_follow_current_action_permissions() {
    let f = Fixture::new(true).await;
    f.enroll(10).await;
    f.enroll(20).await;
    let (_, unban) = f
        .request(
            "POST",
            "/api/guilds/10/unban",
            &serde_json::json!({"request_id":action_key("ban"),"user_id":"99"}).to_string(),
        )
        .await;
    let (_, log) = f
        .request(
            "PUT",
            "/api/guilds/10/logchannels/message_edit",
            &serde_json::json!({"request_id":action_key("log"),"channel_id":"123"}).to_string(),
        )
        .await;
    let mut manage_only = managed_guild(10);
    manage_only.permissions = Some(oauth::PermissionsField::Number(1 << 5));
    expire_auth_cache(&f.app, GUILD_AUTH_CACHE_TTL).await;
    *f.discord.guilds.lock().unwrap() = vec![manage_only];
    let (status, history) = f.request("GET", "/api/guilds/10/actions", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(history.as_array().unwrap().len(), 1);
    assert_eq!(history[0]["id"], log["id"]);
    let path = format!("/api/guilds/10/actions/{}", unban["id"].as_str().unwrap());
    assert_eq!(
        f.request("DELETE", &path, "").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.request(
            "POST",
            "/api/guilds/10/unban",
            &serde_json::json!({"request_id":action_key("denied"),"user_id":"100"}).to_string()
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let path = format!("/api/guilds/10/actions/{}", log["id"].as_str().unwrap());
    assert_eq!(f.request("DELETE", &path, "").await.0, StatusCode::OK);
    let mut ban_only = managed_guild(10);
    ban_only.permissions = Some(oauth::PermissionsField::Number(1 << 2));
    expire_auth_cache(&f.app, GUILD_AUTH_CACHE_TTL).await;
    *f.discord.guilds.lock().unwrap() = vec![ban_only];
    let (_, history) = f.request("GET", "/api/guilds/10/actions", "").await;
    assert_eq!(history.as_array().unwrap().len(), 1);
    assert_eq!(history[0]["id"], unban["id"]);
    assert_eq!(
        f.request("DELETE", &path, "").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.request("GET", "/api/guilds/20/actions", "").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.request("GET", "/api/guilds/0/actions", "").await.0,
        StatusCode::BAD_REQUEST
    );
    expire_auth_cache(&f.app, GUILD_AUTH_CACHE_TTL).await;
    f.discord.guilds.lock().unwrap().clear();
    assert_eq!(
        f.request("GET", "/api/guilds/10/actions", "").await.0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn claimed_work_and_stuck_log_channels_can_be_cancelled_for_recovery() {
    let f = Fixture::new(true).await;
    f.enroll(10).await;
    let (_, unban) = f
        .request(
            "POST",
            "/api/guilds/10/unban",
            &serde_json::json!({"request_id":action_key("claimed"),"user_id":"99"}).to_string(),
        )
        .await;
    let claimed = db::claim_bot_action(&f.app.db, chrono::Utc::now().timestamp())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, unban["id"].as_str().unwrap());
    let path = format!("/api/guilds/10/actions/{}", claimed.id);
    let (status, body) = f.request("DELETE", &path, "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, again) = f
        .request(
            "POST",
            "/api/guilds/10/unban",
            &serde_json::json!({"request_id":action_key("claimed-retry"),"user_id":"99"})
                .to_string(),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{again}");
    assert_eq!(again["state"], "pending");
    let pending_unban_path = format!("/api/guilds/10/actions/{}", again["id"].as_str().unwrap());
    assert_eq!(
        f.request("DELETE", &pending_unban_path, "").await.0,
        StatusCode::OK
    );

    let (_, log) = f
        .request(
            "PUT",
            "/api/guilds/10/logchannels/message_edit",
            &serde_json::json!({"request_id":action_key("log-run"),"channel_id":"123"}).to_string(),
        )
        .await;
    let running = db::claim_bot_action(&f.app.db, chrono::Utc::now().timestamp())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(running.id, log["id"].as_str().unwrap());
    assert!(
        db::begin_bot_action(&f.app.db, &running, chrono::Utc::now().timestamp())
            .await
            .unwrap()
    );
    let log_path = format!("/api/guilds/10/actions/{}", running.id);
    let (status, body) = f.request("DELETE", &log_path, "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, newer) = f
        .request(
            "PUT",
            "/api/guilds/10/logchannels/message_edit",
            &serde_json::json!({"request_id":action_key("log-retry"),"channel_id":"124"})
                .to_string(),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{newer}");
    assert_eq!(newer["state"], "pending");
    assert_ne!(newer["id"], log["id"]);
    let pending_log_path = format!("/api/guilds/10/actions/{}", newer["id"].as_str().unwrap());
    assert_eq!(
        f.request("DELETE", &pending_log_path, "").await.0,
        StatusCode::OK
    );

    let (_, running_unban_job) = f
        .request(
            "POST",
            "/api/guilds/10/unban",
            &serde_json::json!({"request_id":action_key("running-unban"),"user_id":"99"})
                .to_string(),
        )
        .await;
    let running_unban = db::claim_bot_action(&f.app.db, chrono::Utc::now().timestamp())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(running_unban.id, running_unban_job["id"].as_str().unwrap());
    assert!(
        db::begin_bot_action(&f.app.db, &running_unban, chrono::Utc::now().timestamp())
            .await
            .unwrap()
    );
    let unban_running_path = format!("/api/guilds/10/actions/{}", running_unban.id);
    let (status, body) = f.request("DELETE", &unban_running_path, "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("only unstarted work can be cancelled"),
        "{body}"
    );
}

#[tokio::test]
async fn action_requests_require_ids_reject_forged_authority_and_report_expiry() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    for (method, path, body) in [
        (
            "POST",
            "/api/guilds/10/unban",
            serde_json::json!({"user_id":"99"}),
        ),
        (
            "PUT",
            "/api/guilds/10/logchannels/message_edit",
            serde_json::json!({"channel_id":"123"}),
        ),
        (
            "DELETE",
            "/api/guilds/10/logchannels/message_edit",
            serde_json::json!({}),
        ),
        (
            "POST",
            "/api/guilds/10/unban",
            serde_json::json!({"request_id":action_key("forged"),"user_id":"99","actor_id":"123"}),
        ),
    ] {
        assert_eq!(
            f.request(method, path, &body.to_string()).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    for (key, status, code) in [
        (
            "invalid".to_owned(),
            StatusCode::BAD_REQUEST,
            "action_request_invalid",
        ),
        (
            format!("{}_old", chrono::Utc::now().timestamp() - 10 * 86400),
            StatusCode::GONE,
            "action_request_expired",
        ),
    ] {
        let (actual, body) = f
            .request(
                "POST",
                "/api/guilds/10/unban",
                &serde_json::json!({"request_id":key,"user_id":"99"}).to_string(),
            )
            .await;
        assert_eq!(actual, status, "{body}");
        assert_eq!(body["code"], code);
    }
    assert!(
        db::recent_bot_actions(&f.app.db, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn moderation_pages_require_loaded_membership_and_current_permissions() {
    let f = Fixture::new(true).await;
    f.enroll(10).await;
    let old = db::guild_enrollment(&f.app.db, 10).await.unwrap().unwrap();
    db::insert_event(
        &f.app.db,
        10,
        "member_ban",
        Some(123),
        None,
        &serde_json::json!({"reason":"fixture"}),
    )
    .await
    .unwrap();
    db::record_ban(&f.app.db, 10, 123, None, "fixture")
        .await
        .unwrap();
    for path in ["events?user=123&limit=1", "bans?limit=1", "stats"] {
        let path = format!("/api/guilds/10/{path}");
        let (status, body) = f.request_token("GET", &path, "", None).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], "guild_enrollment_changed");
        assert_eq!(
            f.request_token("GET", &path, "", Some(&old.id)).await.0,
            StatusCode::OK
        );
    }
    db::enroll_guild(&f.app.db, 10, 101).await.unwrap();
    for route in ["events", "bans", "stats"] {
        let path = format!("/api/guilds/10/{route}");
        let (status, body) = f.request_token("GET", &path, "", Some(&old.id)).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "guild_enrollment_changed");
        assert_eq!(f.request("GET", &path, "").await.0, StatusCode::OK);
        assert_eq!(
            f.request("GET", &format!("/api/guilds/0/{route}"), "")
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    expire_auth_cache(&f.app, GUILD_AUTH_CACHE_TTL).await;
    f.discord.guilds.lock().unwrap().clear();
    for route in ["events", "bans", "stats"] {
        assert_eq!(
            f.request("GET", &format!("/api/guilds/10/{route}"), "")
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
}

#[tokio::test]
async fn moderation_event_user_filter_rejects_ids_outside_database_range() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    let (status, body) = f
        .request("GET", "/api/guilds/10/events?user=18446744073709551615", "")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid user id");
}

fn configuration_read_routes() -> Vec<String> {
    vec![
        format!("rules/{}", crate::types::RULE_IDS[0]),
        "badwords".into(),
        "commands".into(),
        "commands/fixture".into(),
        "templates".into(),
        "templates/fixture".into(),
        "logchannels".into(),
        "logchannels/fixture".into(),
    ]
}

#[tokio::test]
async fn individual_configuration_reads_require_page_membership_and_fresh_guild_permission() {
    let f = Fixture::new(true).await;
    f.enroll(10).await;
    let original = db::guild_enrollment(&f.app.db, 10).await.unwrap().unwrap();
    for suffix in configuration_read_routes() {
        let path = format!("/api/guilds/10/{suffix}");
        let (status, body) = f.request_token("GET", &path, "", None).await;
        assert_eq!(status, StatusCode::CONFLICT, "{path}: {body}");
        assert_eq!(body["code"], "guild_enrollment_changed");
        assert_eq!(
            f.request_token("GET", &path, "", Some(&original.id))
                .await
                .0,
            StatusCode::OK
        );
    }
    db::enroll_guild(&f.app.db, 10, 101).await.unwrap();
    for suffix in configuration_read_routes() {
        let path = format!("/api/guilds/10/{suffix}");
        let (status, body) = f.request_token("GET", &path, "", Some(&original.id)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], "guild_enrollment_changed");
        assert_eq!(f.request("GET", &path, "").await.0, StatusCode::OK);
    }
    expire_auth_cache(&f.app, GUILD_AUTH_CACHE_TTL).await;
    f.discord.guilds.lock().unwrap().clear();
    for suffix in configuration_read_routes() {
        assert_eq!(
            f.request("GET", &format!("/api/guilds/10/{suffix}"), "")
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        // Explicit owner global context remains independent of any guild membership.
        assert_eq!(
            f.request_token("GET", &format!("/api/guilds/0/{suffix}"), "", None)
                .await
                .0,
            StatusCode::OK
        );
    }
}

#[tokio::test]
async fn individual_configuration_reads_keep_global_context_owner_only() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    for suffix in configuration_read_routes() {
        assert_eq!(
            f.request("GET", &format!("/api/guilds/0/{suffix}"), "")
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            f.request("GET", &format!("/api/guilds/10/{suffix}"), "")
                .await
                .0,
            StatusCode::OK
        );
    }
}

#[tokio::test]
async fn individual_configuration_reads_preserve_inheritance_and_log_channel_wire_shapes() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    for (guild, label) in [(0, "global"), (10, "guild"), (20, "foreign")] {
        db::upsert_template(&f.app.db, guild, "fixture", label)
            .await
            .unwrap();
        let mut command = crate::defaults::default_commands()[0].clone();
        command.name = "fixture".into();
        command.description = label.into();
        db::upsert_command(&f.app.db, guild, "fixture", &command)
            .await
            .unwrap();
        db::upsert_log_channel(&f.app.db, guild, "fixture", 1329331947876479006 + guild)
            .await
            .unwrap();
        db::add_badword(&f.app.db, guild, label, None)
            .await
            .unwrap();
    }
    let (_, template) = f
        .request("GET", "/api/guilds/10/templates/fixture", "")
        .await;
    assert_eq!(
        template,
        serde_json::json!({"effective":"guild","guild_override":"guild","global":"global"})
    );
    let (_, command) = f
        .request("GET", "/api/guilds/10/commands/FIXTURE", "")
        .await;
    assert_eq!(command["effective"]["description"], "guild");
    assert_eq!(command["global"]["description"], "global");
    db::delete_template(&f.app.db, 10, "fixture").await.unwrap();
    let (_, inherited) = f
        .request("GET", "/api/guilds/10/templates/fixture", "")
        .await;
    assert_eq!(inherited["effective"], "global");
    assert!(inherited["guild_override"].is_null());
    let (_, channels) = f.request("GET", "/api/guilds/10/logchannels", "").await;
    assert_eq!(
        channels,
        serde_json::json!({"guild":[{"event":"fixture","channel_id":"1329331947876479016"}],"global":[]})
    );
    let (_, channel) = f
        .request("GET", "/api/guilds/10/logchannels/fixture", "")
        .await;
    assert_eq!(
        channel,
        serde_json::json!({"effective":"1329331947876479016","guild_override":"1329331947876479016","global":null})
    );
    let (_, absent) = f
        .request("GET", "/api/guilds/10/logchannels/absent", "")
        .await;
    assert_eq!(
        absent,
        serde_json::json!({"effective":null,"guild_override":null,"global":null})
    );
    let (_, words) = f.request("GET", "/api/guilds/10/badwords", "").await;
    let patterns: Vec<_> = words
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["pattern"].as_str().unwrap())
        .collect();
    assert!(
        patterns.contains(&"guild")
            && patterns.contains(&"global")
            && !patterns.contains(&"foreign")
    );
}

#[tokio::test]
async fn per_event_log_ignores_are_saved_in_the_authorized_guild_bundle() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    let body = serde_json::json!({"users":[" 000123 ","9007199254740993"],"channels":["000456"]})
        .to_string();
    let (status, saved) = f
        .request("PUT", "/api/guilds/10/logfilters/message_edit", &body)
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(
        saved["ignore"],
        serde_json::json!({"users":["123","9007199254740993"],"channels":["456"]})
    );
    let (_, bundle) = f.request("POST", "/api/guilds/10/config", "").await;
    let edit = bundle["log_channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["event"] == "message_edit")
        .unwrap();
    assert_eq!(edit["ignore"], saved["ignore"]);
    for invalid in [
        serde_json::json!({"users":["OtherBot"],"channels":[]}),
        serde_json::json!({"users":["0"],"channels":[]}),
        serde_json::json!({"users":[],"channels":["staff"]}),
        serde_json::json!({"users":[],"channels":["18446744073709551616"]}),
    ] {
        let (status, _) = f
            .request(
                "PUT",
                "/api/guilds/10/logfilters/message_edit",
                &invalid.to_string(),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    let deleted = serde_json::json!({"users":[],"channels":[]}).to_string();
    let (status, _) = f
        .request("PUT", "/api/guilds/10/logfilters/message_edit", &deleted)
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, bundle) = f.request("POST", "/api/guilds/10/config", "").await;
    let edit = bundle["log_channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["event"] == "message_edit")
        .unwrap();
    assert_eq!(
        edit["ignore"],
        serde_json::json!({"users":[],"channels":[]})
    );
}

#[tokio::test]
async fn cases_are_isolated_and_deletion_requires_administrator() {
    use crate::db::guild_data::{
        CaseAction, CaseData, CaseEvidence, CaseId, CaseOutcome, CaseSource,
    };
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    f.enroll(20).await;
    let left = db::guild_enrollment(&f.app.db, 10).await.unwrap().unwrap();
    let right = db::guild_enrollment(&f.app.db, 20).await.unwrap().unwrap();
    f.app.tenant.activate(left.clone()).await.unwrap().unwrap();
    f.app.tenant.activate(right.clone()).await.unwrap().unwrap();
    let tag = jmodguard::ban_reason::BanAttemptTag::new();
    f.app
        .tenant
        .record_selected_case(
            left.clone(),
            CaseData {
                ban_attempt_tag: Some(tag.clone()),
                source: CaseSource::Application {
                    operation_id: CaseId::new(),
                },
                action: CaseAction::Ban,
                outcome: CaseOutcome::Succeeded,
                subject_id: 1,
                actor_id: None,
                occurred_at_ms: 1,
                reason: Some("secret".into()),
                evidence: CaseEvidence::Unavailable,
            },
        )
        .await
        .unwrap();
    let (status, body) = f.request("GET", "/api/guilds/10/cases", "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids = body["cases"].as_array().unwrap();
    assert_eq!(ids.len(), 1);
    assert_eq!(body["can_delete"], false);
    let (status, other) = f.request("GET", "/api/guilds/20/cases", "").await;
    assert_eq!(status, StatusCode::OK, "{other}");
    assert!(other["cases"].as_array().unwrap().is_empty());
    let id = ids[0].as_str().unwrap();
    let (status, detail) = f
        .request("GET", &format!("/api/guilds/10/cases/{id}"), "")
        .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["case"]["reason"], "secret");
    assert!(detail["case"].get("ban_attempt_tag").is_none());
    assert!(!detail.to_string().contains(tag.as_str()));
    let note_url = format!("/api/guilds/10/cases/{id}/history");
    let note = serde_json::json!({"entry_id":"a".repeat(32),"text":"one intent"}).to_string();
    let (status, first) = f.request("POST", &note_url, &note).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    // Discard the successful response, as a client with a lost response would.
    let (status, replay) = f.request("POST", &note_url, &note).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    let (_, entries) = f.request("GET", &note_url, "").await;
    assert_eq!(entries["history"].as_array().unwrap().len(), 1);
    let conflict = serde_json::json!({"entry_id":"a".repeat(32),"text":"changed"}).to_string();
    assert_eq!(
        f.request("POST", &note_url, &conflict).await.0,
        StatusCode::BAD_REQUEST
    );

    let (status, leaked) = f
        .request("GET", &format!("/api/guilds/20/cases/{id}"), "")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{leaked}");
    assert!(!leaked.to_string().contains("secret"));
    let (status, denied) = f
        .request("DELETE", &format!("/api/guilds/10/cases/{id}"), "")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{denied}");
}

#[tokio::test]
async fn administrator_can_delete_cases_and_offline_stores_fail_closed() {
    use crate::db::guild_data::{
        CaseAction, CaseData, CaseEvidence, CaseId, CaseOutcome, CaseSource,
    };
    use crate::db::guild_stores::{StoreId, StorePolicy, assign_initial_placement, register_store};
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    let enrollment = db::guild_enrollment(&f.app.db, 10).await.unwrap().unwrap();
    f.app
        .tenant
        .activate(enrollment.clone())
        .await
        .unwrap()
        .unwrap();
    f.app
        .tenant
        .record_selected_case(
            enrollment.clone(),
            CaseData {
                ban_attempt_tag: None,
                source: CaseSource::Application {
                    operation_id: CaseId::new(),
                },
                action: CaseAction::Kick,
                outcome: CaseOutcome::Succeeded,
                subject_id: 2,
                actor_id: None,
                occurred_at_ms: 1,
                reason: Some("private-reason".into()),
                evidence: CaseEvidence::Unavailable,
            },
        )
        .await
        .unwrap();
    let (status, body) = f.request("GET", "/api/guilds/10/cases", "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["cases"][0].as_str().unwrap().to_owned();
    expire_auth_cache(&f.app, GUILD_AUTH_CACHE_TTL).await;
    f.discord.guilds.lock().unwrap()[0].owner = true;
    f.app.guild_cache.clear();
    let (status, deleted) = f
        .request("DELETE", &format!("/api/guilds/10/cases/{id}"), "")
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted["removed"], true);
    assert_eq!(deleted["cleanup_pending"], false);
    assert_eq!(deleted["discord_action_reversed"], false);
    let (status, missing) = f
        .request("GET", &format!("/api/guilds/10/cases/{id}"), "")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{missing}");
    assert!(!missing.to_string().contains("private-reason"));

    let ghost = Fixture::new(false).await;
    ghost.enroll(10).await;
    let store = StoreId::new();
    register_store(&ghost.app.db, store.clone(), StorePolicy::Shared)
        .await
        .unwrap();
    assign_initial_placement(&ghost.app.db, 10, store)
        .await
        .unwrap();
    let (status, offline) = ghost.request("GET", "/api/guilds/10/cases", "").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{offline}");
    assert_eq!(offline["code"], "guild_store_offline");
    assert!(!offline.to_string().contains("private-reason"));

    let (status, stats) = ghost.request("GET", "/api/guilds/10/stats", "").await;
    assert_eq!(status, StatusCode::OK, "{stats}");
    assert_eq!(stats["events"], 0);
    assert_eq!(stats["warns"], 0);
    assert_eq!(stats["bans"], 0);
    assert!(
        stats.get("cases").is_none(),
        "offline tenant stats omit only their unavailable case count: {stats}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn case_delete_reports_cleanup_pending_when_object_cleanup_fails() {
    use crate::db::SqlExecutor;
    use crate::db::guild_data::{
        CaseAction, CaseData, CaseEvidence, CaseId, CaseOutcome, CaseSource, EvidenceCapture,
        EvidenceOrigin, EvidenceRequest,
    };
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let f = Fixture::new(false).await;
    f.enroll(10).await;
    let enrollment = db::guild_enrollment(&f.app.db, 10).await.unwrap().unwrap();
    f.app
        .tenant
        .activate(enrollment.clone())
        .await
        .unwrap()
        .unwrap();
    f.app
        .tenant
        .record_selected_case(
            enrollment.clone(),
            CaseData {
                ban_attempt_tag: None,
                source: CaseSource::Application {
                    operation_id: CaseId::new(),
                },
                action: CaseAction::Kick,
                outcome: CaseOutcome::Succeeded,
                subject_id: 2,
                actor_id: None,
                occurred_at_ms: 1,
                reason: Some("private-reason".into()),
                evidence: CaseEvidence::Unavailable,
            },
        )
        .await
        .unwrap();
    let (status, body) = f.request("GET", "/api/guilds/10/cases", "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["cases"][0].as_str().unwrap().to_owned();
    let case = CaseId::try_from(id.clone()).unwrap();
    let (store, scope) = f.app.tenant.live(enrollment.clone()).await.unwrap();
    let storage = CaseId::try_from(db::evidence_storage_id(&f.app.db, "").await.unwrap()).unwrap();
    let evidence_id = CaseId::new();
    store
        .reserve_case_evidence(
            scope.clone(),
            case.clone(),
            evidence_id.clone(),
            EvidenceRequest {
                storage_id: storage,
                bytes: 3,
                origin: EvidenceOrigin {
                    channel_id: 11,
                    message_id: 12,
                    attachment_id: 13,
                },
            },
            crate::tenant::evidence_limits(),
        )
        .await
        .unwrap();
    store
        .settle_evidence_capture(
            scope,
            case.clone(),
            evidence_id.clone(),
            EvidenceCapture::Stored {
                bytes: 3,
                sha256: "a".repeat(64),
            },
        )
        .await
        .unwrap();

    let mut root = f.path.as_os_str().to_owned();
    root.push(".evidence");
    let object = std::path::PathBuf::from(root)
        .join("evidence")
        .join("10")
        .join(&enrollment.id)
        .join(case.as_str())
        .join(format!("{}.data", evidence_id.as_str()));
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(object.parent().unwrap())
        .unwrap();
    std::fs::write(&object, b"bad").unwrap();
    std::fs::set_permissions(&object, std::fs::Permissions::from_mode(0o644)).unwrap();

    expire_auth_cache(&f.app, GUILD_AUTH_CACHE_TTL).await;
    f.discord.guilds.lock().unwrap()[0].owner = true;
    f.app.guild_cache.clear();
    let (status, deleted) = f
        .request("DELETE", &format!("/api/guilds/10/cases/{id}"), "")
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted["removed"], true);
    assert_eq!(deleted["cleanup_pending"], true);
    assert_eq!(deleted["discord_action_reversed"], false);

    let (status, listed) = f.request("GET", "/api/guilds/10/cases", "").await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(listed["cases"].as_array().unwrap().is_empty());
    let intents: i64 = f
        .app
        .db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM backup_cleanup_intents WHERE kind='case'",
                (),
                |r| Ok(r.get(0)?),
            )
        })
        .await
        .unwrap();
    assert_eq!(intents, 0);
}

#[tokio::test]
async fn case_delete_and_retry_ignore_retired_backup_queue() {
    use crate::db::SqlExecutor;
    use crate::db::guild_data::*;
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    let e = db::guild_enrollment(&f.app.db, 10).await.unwrap().unwrap();
    f.app.tenant.activate(e.clone()).await.unwrap();
    f.app
        .tenant
        .record_selected_case(
            e,
            CaseData {
                ban_attempt_tag: None,
                source: CaseSource::Application {
                    operation_id: CaseId::new(),
                },
                action: CaseAction::Warning,
                outcome: CaseOutcome::Succeeded,
                subject_id: 1,
                actor_id: None,
                occurred_at_ms: 1,
                reason: None,
                evidence: CaseEvidence::Unavailable,
            },
        )
        .await
        .unwrap();
    let (_, listed) = f.request("GET", "/api/guilds/10/cases", "").await;
    let id = listed["cases"][0].as_str().unwrap();
    let url = format!("/api/guilds/10/cases/{id}");
    let (denied, _) = f
        .request("GET", "/api/guilds/10/cases?removed=true", "")
        .await;
    assert_ne!(denied, StatusCode::OK);
    expire_auth_cache(&f.app, GUILD_AUTH_CACHE_TTL).await;
    f.discord.guilds.lock().unwrap()[0].owner = true;
    f.app.guild_cache.clear();
    f.app.db.with_conn(|c|{c.execute_batch_bounded("CREATE TRIGGER review_fail BEFORE INSERT ON backup_cleanup_intents BEGIN SELECT RAISE(ABORT,'injected'); END", 64 * 1024 * 1024)?;Ok(())}).await.unwrap();
    let (status, first) = f.request("DELETE", &url, "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["removed"], true);
    let (status, second) = f.request("DELETE", &url, "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second["removed"], false);
    let count: i64 = f
        .app
        .db
        .with_conn(|c| {
            c.query_row("SELECT count(*) FROM backup_cleanup_intents", (), |r| {
                Ok(r.get(0)?)
            })
        })
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(first["cleanup_pending"], false);
    assert_eq!(second["cleanup_pending"], false);
    let (_, removed) = f
        .request("GET", "/api/guilds/10/cases?removed=true", "")
        .await;
    assert_eq!(removed["cases"][0], id);
    let (_, active) = f.request("GET", "/api/guilds/10/cases", "").await;
    assert!(active["cases"].as_array().unwrap().is_empty());
    f.request("DELETE", &url, "").await;
    let count: i64 = f
        .app
        .db
        .with_conn(|c| {
            c.query_row("SELECT count(*) FROM backup_cleanup_intents", (), |r| {
                Ok(r.get(0)?)
            })
        })
        .await
        .unwrap();
    assert_eq!(count, 0);
    f.request(
        "DELETE",
        &format!("/api/guilds/10/cases/{}", "f".repeat(32)),
        "",
    )
    .await;
    let count: i64 = f
        .app
        .db
        .with_conn(|c| {
            c.query_row("SELECT count(*) FROM backup_cleanup_intents", (), |r| {
                Ok(r.get(0)?)
            })
        })
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn owner_preview_cannot_write_and_does_not_inherit_owner_authority() {
    let f = Fixture::new(true).await;
    f.enroll(10).await;
    let (status, launched) = f
        .request(
            "POST",
            "/api/preview",
            r#"{"guild_id":"10","profile":"administrator"}"#,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{launched}");
    let token = launched["token"].as_str().expect("preview token");
    let (status, me) = f.request_preview("GET", "/api/me", "", token).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    assert_eq!(me["is_bot_owner"], false);
    assert_eq!(me["preview"]["guild_id"], "10");
    let (status, denied) = f
        .request_preview(
            "PUT",
            "/api/guilds/10/settings",
            r#"{"prefix":"preview"}"#,
            token,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denied["code"], "preview_read_only");
    let (status, me) = f.request_preview("POST", "/api/me", "", token).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    let (status, cases) = f
        .request_preview("POST", "/api/guilds/10/cases", "", token)
        .await;
    assert_ne!(status, StatusCode::FORBIDDEN, "{cases}");
    assert_ne!(cases["code"].as_str(), Some("preview_read_only"));
    let (status, note) = f
        .request_preview(
            "POST",
            "/api/guilds/10/cases/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/history",
            r#"{"entry_id":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","text":"no"}"#,
            token,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{note}");
    assert_eq!(note["code"], "preview_read_only");
    for path in ["/api/database-usage", "/api/database-usage/core"] {
        let (status, denied) = f.request_preview("POST", path, "", token).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {denied}");
        assert_eq!(denied["code"], "preview_read_only");
    }
    let (status, _global) = f
        .request_preview("PUT", "/api/guilds/0/settings", r#"{"prefix":"!"}"#, token)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, saved) = f
        .request("PUT", "/api/guilds/10/settings", r#"{"prefix":"owner"}"#)
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let (status, config) = f
        .request_preview("GET", "/api/guilds/10/config", "", token)
        .await;
    assert_eq!(status, StatusCode::OK, "{config}");
}

#[tokio::test]
async fn preview_profiles_apply_distinct_permission_views() {
    for (profile, can_delete) in [
        ("administrator", true),
        ("moderator", false),
        ("member", false),
    ] {
        let f = Fixture::new(true).await;
        f.enroll(10).await;
        let enrollment = db::guild_enrollment(&f.app.db, 10).await.unwrap().unwrap();
        f.app
            .tenant
            .activate(enrollment)
            .await
            .unwrap()
            .expect("tenant store");
        let (status, launched) = f
            .request(
                "POST",
                "/api/preview",
                &format!(r#"{{"guild_id":"10","profile":"{profile}"}}"#),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{profile}: {launched}");
        let token = launched["token"].as_str().expect("preview token");

        let (status, me) = f.request_preview("GET", "/api/me", "", token).await;
        assert_eq!(status, StatusCode::OK, "{profile}: {me}");
        assert_eq!(me["preview"]["profile"], profile);

        let (status, cases) = f
            .request_preview("GET", "/api/guilds/10/cases", "", token)
            .await;
        if profile == "member" {
            assert!(me["guilds"].as_array().unwrap().is_empty(), "{me}");
            assert_eq!(status, StatusCode::BAD_REQUEST, "{cases}");
            assert!(
                cases["error"]
                    .as_str()
                    .unwrap_or("")
                    .contains("MANAGE_GUILD"),
                "{cases}"
            );
        } else {
            assert_eq!(me["guilds"][0]["manage"], true, "{profile}: {me}");
            assert_eq!(status, StatusCode::OK, "{profile}: {cases}");
            assert_eq!(cases["can_delete"], can_delete, "{profile}: {cases}");
        }
    }
}

#[tokio::test]
async fn expired_preview_is_removed_and_returns_a_distinct_lifecycle_error() {
    let f = Fixture::new(true).await;
    let token = "expired-preview".to_string();
    f.app.preview.insert(
        token.clone(),
        preview::PreviewGrant {
            token: token.clone(),
            actor_id: 42,
            guild_id: 10,
            profile: preview::PreviewProfile::Member,
            expires: std::time::Instant::now() - std::time::Duration::from_secs(1),
        },
    );
    let (status, body) = f.request_preview("GET", "/api/me", "", &token).await;
    assert_eq!(status, StatusCode::GONE);
    assert_eq!(body["code"], "preview_expired");
    assert!(!f.app.preview.contains_key(&token));
}

#[tokio::test]
async fn guild_user_cannot_launch_preview() {
    let f = Fixture::new(false).await;
    let (status, body) = f
        .request(
            "POST",
            "/api/preview",
            r#"{"guild_id":"10","profile":"administrator"}"#,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap_or("").contains("bot owner"));
}

// Age the production snapshot without sleeping or altering its success payload.
async fn expire_auth_cache(app: &ApiState, age: std::time::Duration) {
    let entries: Vec<_> = app
        .auth_sessions
        .iter()
        .map(|e| Arc::clone(e.value()))
        .collect();
    for entry in entries {
        if let Some((at, _)) = &mut entry.lock().await.guilds {
            *at = std::time::Instant::now().checked_sub(age).unwrap();
        }
    }
}

#[tokio::test]
async fn session_guild_requests_reuse_coalesce_and_refetch_after_expiry() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    let requests = (
        f.request("GET", "/api/guilds/10/config", ""),
        f.request("GET", "/api/guilds/10/config", ""),
        f.request("GET", "/api/guilds/10/config", ""),
    );
    let (a, b, c) = tokio::join!(requests.0, requests.1, requests.2);
    for (status, body) in [a, b, c] {
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.request("GET", "/api/guilds/10/config", "").await.0,
        StatusCode::OK
    );
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), 1);
    expire_auth_cache(&f.app, GUILD_AUTH_CACHE_TTL).await;
    assert_eq!(
        f.request("GET", "/api/guilds/10/config", "").await.0,
        StatusCode::OK
    );
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn bounded_auth_snapshot_handles_discord_and_local_rate_limits() {
    for budget in [false, true] {
        let f = Fixture::new(false).await;
        f.enroll(10).await;
        f.discord.spent_budget.store(budget, Ordering::SeqCst);
        assert_eq!(
            f.request("GET", "/api/guilds/10/config", "").await.0,
            StatusCode::OK
        );
        expire_auth_cache(&f.app, GUILD_AUTH_CACHE_TTL).await;
        f.discord.rate_limited.store(true, Ordering::SeqCst);
        assert_eq!(
            f.request("GET", "/api/guilds/10/config", "").await.0,
            StatusCode::OK
        );
        assert_eq!(
            f.discord.calls.load(Ordering::SeqCst),
            if budget { 1 } else { 2 }
        );
        // A fallback must not reset the successful snapshot's age.
        expire_auth_cache(&f.app, GUILD_AUTH_MAX_STALE).await;
        let (status, body) = f.request("GET", "/api/guilds/10/config", "").await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body["code"], "discord_rate_limited");
        let entries: Vec<_> = f
            .app
            .auth_sessions
            .iter()
            .map(|e| Arc::clone(e.value()))
            .collect();
        for entry in entries {
            entry.lock().await.guilds = None;
        }
        assert_eq!(
            f.request("GET", "/api/guilds/10/config", "").await.0,
            StatusCode::TOO_MANY_REQUESTS
        );
    }
}

#[tokio::test]
async fn concurrent_rejected_tokens_refresh_once_or_die_without_more_discord_calls() {
    for failure in ["refresh", "guild", "profile", "none"] {
        let f = Fixture::new(false).await;
        f.enroll(10).await;
        f.discord
            .unauthorized_until_refresh
            .store(true, Ordering::SeqCst);
        f.discord
            .refresh_failed
            .store(failure == "refresh", Ordering::SeqCst);
        f.discord
            .still_unauthorized
            .store(failure == "guild", Ordering::SeqCst);
        f.discord
            .me_unauthorized
            .store(failure == "profile", Ordering::SeqCst);
        if failure == "profile" {
            // Exercise profile recovery as the first rejection too.
            f.discord
                .unauthorized_until_refresh
                .store(false, Ordering::SeqCst);
            f.app.user_cache.clear();
        }
        let (a, b, c) = tokio::join!(
            f.request("GET", "/api/guilds/10/config", ""),
            f.request("GET", "/api/guilds/10/config", ""),
            f.request("GET", "/api/guilds/10/config", "")
        );
        for (status, body) in [a, b, c] {
            if failure == "none" {
                assert_eq!(status, StatusCode::OK, "{body}");
            } else {
                assert!(
                    status == StatusCode::UNAUTHORIZED || body["error"] == "not authenticated",
                    "{status}: {body}"
                );
                if status == StatusCode::UNAUTHORIZED {
                    assert_eq!(body["code"], "auth_lost");
                }
            }
        }
        assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), 1, "{failure}");
        let calls = f.discord.calls.load(Ordering::SeqCst);
        let me_calls = f.discord.me_calls.load(Ordering::SeqCst);
        let (status, body) = f.request("GET", "/api/guilds/10/config", "").await;
        if failure != "none" {
            assert!(!status.is_success(), "{body}");
        }
        assert_eq!(f.discord.calls.load(Ordering::SeqCst), calls);
        assert_eq!(f.discord.me_calls.load(Ordering::SeqCst), me_calls);
        assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn copy_uses_one_permission_snapshot() {
    let f = Fixture::new(true).await;
    f.enroll(10).await;
    f.enroll(20).await;
    let (status, body) = f
        .request(
            "POST",
            "/api/guilds/20/copy",
            r#"{"from_guild_id":10,"sections":["settings"]}"#,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn display_saves_live_profile_while_guilds_are_cooling_down() {
    let f = Fixture::new(false).await;
    f.discord.rate_limited.store(true, Ordering::SeqCst);
    let _ = f.app.oauth.my_guilds(42, "fixture-token").await;
    f.app.user_cache.clear();
    for _ in 0..3 {
        assert_eq!(f.request("POST", "/api/me", "").await.0, StatusCode::OK);
    }
    assert_eq!(f.discord.me_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn authorization_snapshots_are_not_shared_between_sessions_or_tokens() {
    let f = Fixture::new(false).await;
    let first = tests::session_for_user(42).await;
    first
        .insert(SESSION_TOKEN, "fixture-token")
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let second = tests::session_for_user(42).await;
    second
        .insert(SESSION_TOKEN, "refreshed-token")
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    require_auth_fresh(&f.app, &first)
        .await
        .unwrap_or_else(|error| panic!("{:#}", error.0));
    f.discord
        .guilds
        .lock()
        .unwrap_or_else(|error| panic!("{error}"))
        .clear();
    assert!(
        require_auth_fresh(&f.app, &second)
            .await
            .unwrap_or_else(|error| panic!("{:#}", error.0))
            .guilds
            .is_empty()
    );
    assert!(
        !require_auth_fresh(&f.app, &first)
            .await
            .unwrap_or_else(|error| panic!("{:#}", error.0))
            .guilds
            .is_empty()
    );
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn owner_routes_preserve_auth_loss_for_an_already_loaded_session() {
    let f = Fixture::new(true).await;
    let store = Arc::new(MemoryStore::default());
    let session = Session::new(None, store.clone(), None);
    session.insert(SESSION_USER, 42u64).await.unwrap();
    session
        .insert(SESSION_TOKEN, "fixture-token")
        .await
        .unwrap();
    session
        .insert(SESSION_REFRESH, "fixture-refresh")
        .await
        .unwrap();
    session.save().await.unwrap();
    // Separate requests hydrate separate records; Session::clone shares a
    // handle, which the first rejection now correctly clears.
    let stale = Session::new(session.id(), store.clone(), None);
    let stale_updates = Session::new(session.id(), store, None);
    assert_eq!(stale.get::<u64>(SESSION_USER).await.unwrap(), Some(42));
    assert_eq!(
        stale_updates.get::<u64>(SESSION_USER).await.unwrap(),
        Some(42)
    );
    f.discord
        .unauthorized_until_refresh
        .store(true, Ordering::SeqCst);
    f.discord.refresh_failed.store(true, Ordering::SeqCst);
    let error = require_auth_fresh(&f.app, &session).await.err().unwrap();
    assert_eq!(error.into_response().status(), StatusCode::UNAUTHORIZED);
    let usage = usage::overview(axum::extract::State(f.app.clone()), stale, HeaderMap::new())
        .await
        .unwrap_err();
    let updates = updates::status(axum::extract::State(f.app.clone()), stale_updates)
        .await
        .unwrap_err();
    for (status, body) in [usage, updates] {
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body.0["code"], "auth_lost");
    }
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn waiting_session_saves_the_new_access_and_rotated_refresh_tokens() {
    let f = Fixture::new(false).await;
    let store = Arc::new(MemoryStore::default());
    let first = Session::new(None, store.clone(), None);
    first.insert(SESSION_USER, 42u64).await.unwrap();
    first.insert(SESSION_TOKEN, "fixture-token").await.unwrap();
    first
        .insert(SESSION_REFRESH, "fixture-refresh")
        .await
        .unwrap();
    first.save().await.unwrap();
    let waiting = Session::new(first.id(), store.clone(), None);
    assert_eq!(
        waiting
            .get::<String>(SESSION_REFRESH)
            .await
            .unwrap()
            .as_deref(),
        Some("fixture-refresh")
    );
    f.discord
        .unauthorized_until_refresh
        .store(true, Ordering::SeqCst);
    require_auth_fresh(&f.app, &first)
        .await
        .unwrap_or_else(|error| panic!("{:#}", error.0));
    require_auth_fresh(&f.app, &waiting)
        .await
        .unwrap_or_else(|error| panic!("{:#}", error.0));
    waiting.save().await.unwrap();
    let reloaded = Session::new(first.id(), store, None);
    assert_eq!(
        reloaded
            .get::<String>(SESSION_TOKEN)
            .await
            .unwrap()
            .as_deref(),
        Some("refreshed-token")
    );
    assert_eq!(
        reloaded
            .get::<String>(SESSION_REFRESH)
            .await
            .unwrap()
            .as_deref(),
        Some("refreshed-refresh")
    );
    assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn temporary_refresh_rate_limit_keeps_session_and_allows_later_authorization() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    f.discord
        .unauthorized_until_refresh
        .store(true, Ordering::SeqCst);
    f.discord.refresh_rate_limited.store(true, Ordering::SeqCst);
    let (status, body) = f.request("GET", "/api/guilds/10/config", "").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["code"], "discord_rate_limited");
    assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), 1);
    assert_refresh_session_kept(&f).await;
    assert_refresh_wait(&f, std::time::Duration::from_secs(2)).await;
    // Clear only the shared budget first: the session hold must still suppress refresh.
    clear_oauth_budget(&f);
    let (a, b, c) = tokio::join!(
        f.request("GET", "/api/guilds/10/config", ""),
        f.request("GET", "/api/guilds/10/config", ""),
        f.request("GET", "/api/guilds/10/config", "")
    );
    for (status, body) in [a, b, c] {
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        assert_eq!(body["code"], "discord_rate_limited");
    }
    assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), 1);
    f.discord
        .refresh_rate_limited
        .store(false, Ordering::SeqCst);
    expire_refresh_hold(&f).await;
    let (a, b, c) = tokio::join!(
        f.request("GET", "/api/guilds/10/config", ""),
        f.request("GET", "/api/guilds/10/config", ""),
        f.request("GET", "/api/guilds/10/config", "")
    );
    for (status, body) in [a, b, c] {
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), 2);
    let state = refresh_state(&f).await;
    let state = state.lock().await;
    assert!(state.refresh_hold.is_none());
    assert!(state.refresh_backoff.is_zero());
}

async fn refresh_state(f: &Fixture) -> Arc<tokio::sync::Mutex<SessionAuth>> {
    let id = f.cookie.split_once('=').unwrap().1.parse().unwrap();
    Arc::clone(f.app.auth_sessions.get(&id).unwrap().value())
}

async fn assert_refresh_session_kept(f: &Fixture) {
    let id = f.cookie.split_once('=').unwrap().1.parse().unwrap();
    let session = Session::new(Some(id), Arc::new(f.store.clone()), None);
    assert!(session.get::<u64>(SESSION_USER).await.unwrap().is_some());
    assert_eq!(
        session
            .get::<String>(SESSION_TOKEN)
            .await
            .unwrap()
            .as_deref(),
        Some("fixture-token")
    );
    assert_eq!(
        session
            .get::<String>(SESSION_REFRESH)
            .await
            .unwrap()
            .as_deref(),
        Some("fixture-refresh")
    );
    let state = refresh_state(f).await;
    let state = state.lock().await;
    assert!(!state.dead);
    assert_eq!(state.token.as_deref(), Some("fixture-token"));
    assert_eq!(state.refresh.as_deref(), Some("fixture-refresh"));
}

async fn assert_refresh_wait(f: &Fixture, expected: std::time::Duration) {
    let state = refresh_state(f).await;
    let state = state.lock().await;
    let remaining = state.refresh_wait().unwrap().0;
    assert!(
        remaining <= expected && remaining > expected - std::time::Duration::from_secs(1),
        "{remaining:?}"
    );
}

fn clear_oauth_budget(f: &Fixture) {
    *f.app.oauth.guild_list.lock().unwrap() = oauth::GuildListGate::new();
}

// Like expire_auth_cache, move only the production deadline; no real sleeps.
async fn expire_refresh_hold(f: &Fixture) {
    let state = refresh_state(f).await;
    let mut state = state.lock().await;
    let (_, limited) = state.refresh_hold.unwrap();
    state.refresh_hold = Some((
        oauth::Cooldown::Until(std::time::Instant::now() - std::time::Duration::from_secs(1)),
        limited,
    ));
}

#[tokio::test]
async fn temporary_refresh_body_retry_after_and_local_budget_keep_sessions() {
    for local_budget in [false, true] {
        let f = Fixture::new(false).await;
        f.enroll(10).await;
        f.discord
            .unauthorized_until_refresh
            .store(true, Ordering::SeqCst);
        f.discord.refresh_rate_limited.store(true, Ordering::SeqCst);
        *f.discord.retry_after.lock().unwrap() = String::new();
        *f.discord.body_retry_after.lock().unwrap() = Some(30.0);
        if local_budget {
            // Another session spent the shared token budget before this request.
            assert!(
                f.app
                    .oauth
                    .refresh("fixture-refresh")
                    .await
                    .unwrap_err()
                    .is::<oauth::DiscordRateLimit>()
            );
        }
        let (status, body) = f.request("GET", "/api/guilds/10/config", "").await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        assert_eq!(body["code"], "discord_rate_limited");
        assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), 1);
        assert_refresh_session_kept(&f).await;
        assert_refresh_wait(&f, std::time::Duration::from_secs(30)).await;
        clear_oauth_budget(&f);
        assert_eq!(
            f.request("GET", "/api/guilds/10/config", "").await.0,
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), 1);
        f.discord
            .refresh_rate_limited
            .store(false, Ordering::SeqCst);
        expire_refresh_hold(&f).await;
        assert_eq!(
            f.request("GET", "/api/guilds/10/config", "").await.0,
            StatusCode::OK
        );
        assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn temporary_refresh_server_failures_back_off_and_reset_after_success() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    f.discord
        .unauthorized_until_refresh
        .store(true, Ordering::SeqCst);
    f.discord
        .refresh_server_failed
        .store(true, Ordering::SeqCst);
    for (attempt, seconds) in [5, 10, 20, 40, 60, 60].into_iter().enumerate() {
        if attempt != 0 {
            expire_refresh_hold(&f).await;
        }
        let (status, body) = f.request("GET", "/api/guilds/10/config", "").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_ne!(body["code"], "auth_lost");
        assert_refresh_session_kept(&f).await;
        assert_refresh_wait(&f, std::time::Duration::from_secs(seconds)).await;
        let calls = f.discord.token_calls.load(Ordering::SeqCst);
        assert_eq!(calls, attempt + 1);
        assert_eq!(
            f.request("GET", "/api/guilds/10/config", "").await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), calls);
    }
    f.discord
        .refresh_server_failed
        .store(false, Ordering::SeqCst);
    expire_refresh_hold(&f).await;
    assert_eq!(
        f.request("GET", "/api/guilds/10/config", "").await.0,
        StatusCode::OK
    );
    let state = refresh_state(&f).await;
    assert!(state.lock().await.refresh_hold.is_none());
    assert!(state.lock().await.refresh_backoff.is_zero());
    // A fresh consecutive-failure sequence starts at the minimum again.
    expire_auth_cache(&f.app, GUILD_AUTH_CACHE_TTL).await;
    f.discord.still_unauthorized.store(true, Ordering::SeqCst);
    f.discord
        .refresh_server_failed
        .store(true, Ordering::SeqCst);
    assert_eq!(
        f.request("GET", "/api/guilds/10/config", "").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_refresh_wait(&f, std::time::Duration::from_secs(5)).await;
}

#[tokio::test]
async fn temporary_refresh_transport_failures_keep_session_and_release_lock() {
    for transport in [TokenTransport::Closed, TokenTransport::Timeout] {
        let f = Fixture::with_token_transport(false, "42", transport).await;
        f.enroll(10).await;
        f.discord
            .unauthorized_until_refresh
            .store(true, Ordering::SeqCst);
        let (status, body) = f.request("GET", "/api/guilds/10/config", "").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_ne!(body["code"], "auth_lost");
        assert_refresh_session_kept(&f).await;
        assert_refresh_wait(&f, std::time::Duration::from_secs(5)).await;
        let calls = f.discord.token_calls.load(Ordering::SeqCst);
        assert!(calls > 0);
        assert_eq!(
            f.request("GET", "/api/guilds/10/config", "").await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), calls);
    }
}

#[tokio::test]
async fn temporary_refresh_hold_never_extends_permission_snapshot_or_copy_authority() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    f.enroll(20).await;
    assert_eq!(
        f.request("GET", "/api/guilds/10/config", "").await.0,
        StatusCode::OK
    );
    expire_auth_cache(&f.app, GUILD_AUTH_CACHE_TTL).await;
    f.discord
        .unauthorized_until_refresh
        .store(true, Ordering::SeqCst);
    f.discord.refresh_rate_limited.store(true, Ordering::SeqCst);
    *f.discord.retry_after.lock().unwrap() = "30".into();
    assert_eq!(
        f.request("GET", "/api/guilds/10/config", "").await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        f.request("GET", "/api/guilds/10/config", "").await.0,
        StatusCode::OK
    );
    expire_auth_cache(&f.app, GUILD_AUTH_MAX_STALE).await;
    f.discord.guilds.lock().unwrap().clear();
    assert_eq!(f.request("POST", "/api/me", "").await.0, StatusCode::OK);
    for (method, path, request) in [
        ("GET", "/api/guilds/10/config", ""),
        (
            "POST",
            "/api/guilds/20/copy",
            r#"{"from_guild_id":10,"sections":["settings"]}"#,
        ),
    ] {
        let (status, body) = f.request(method, path, request).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        assert_eq!(body["code"], "discord_rate_limited");
    }
    assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), 1);
    clear_oauth_budget(&f);
    expire_refresh_hold(&f).await;
    f.discord
        .refresh_rate_limited
        .store(false, Ordering::SeqCst);
    let (status, body) = f.request("GET", "/api/guilds/10/config", "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["error"],
        "you cannot manage this server (needs MANAGE_GUILD permission)"
    );
    assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn refresh_invalid_grant_drops_session_and_next_request_calls_no_discord() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    f.discord
        .unauthorized_until_refresh
        .store(true, Ordering::SeqCst);
    f.discord.refresh_failed.store(true, Ordering::SeqCst);
    let (status, body) = f.request("GET", "/api/guilds/10/config", "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["code"], "auth_lost");
    let calls = f.discord.calls.load(Ordering::SeqCst);
    let me_calls = f.discord.me_calls.load(Ordering::SeqCst);
    assert!(
        !f.request("GET", "/api/guilds/10/config", "")
            .await
            .0
            .is_success()
    );
    assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), calls);
    assert_eq!(f.discord.me_calls.load(Ordering::SeqCst), me_calls);
    let state = refresh_state(&f).await;
    let state = state.lock().await;
    assert!(state.dead && state.token.is_none() && state.refresh.is_none());
    let id = f.cookie.split_once('=').unwrap().1.parse().unwrap();
    let session = Session::new(Some(id), Arc::new(f.store.clone()), None);
    assert!(session.get::<u64>(SESSION_USER).await.unwrap().is_none());
}

#[tokio::test]
async fn temporary_refresh_server_retry_after_overrides_default_backoff() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    f.discord
        .unauthorized_until_refresh
        .store(true, Ordering::SeqCst);
    f.discord
        .refresh_server_failed
        .store(true, Ordering::SeqCst);
    *f.discord.refresh_server_retry_after.lock().unwrap() = Some("30".into());
    f.discord.spent_budget.store(true, Ordering::SeqCst);
    assert_eq!(
        f.request("GET", "/api/guilds/10/config", "").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_refresh_wait(&f, std::time::Duration::from_secs(30)).await;
    assert_refresh_session_kept(&f).await;
    // Other sessions still obey the token endpoint's existing shared budget.
    assert!(
        f.app
            .oauth
            .refresh("fixture-refresh")
            .await
            .unwrap_err()
            .is::<oauth::DiscordRateLimit>()
    );
    assert_eq!(
        f.request("GET", "/api/guilds/10/config", "").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn coordinator_authenticate_sign_out_cycles_release_entries() {
    let f = Fixture::new(false).await;
    for _ in 0..20 {
        let session = Session::new(None, Arc::new(f.store.clone()), None);
        store_login_session(&session, 42, "fixture-token", Some("fixture-refresh"))
            .await
            .unwrap();
        require_auth_fresh(&f.app, &session).await.ok().unwrap();
        assert_eq!(f.app.auth_sessions.len(), 1);
        let entry = Arc::clone(
            f.app
                .auth_sessions
                .get(&session.id().unwrap())
                .unwrap()
                .value(),
        );
        {
            let mut auth = entry.lock().await;
            assert!(auth.token.is_some() && auth.refresh.is_some() && auth.guilds.is_some());
            auth.hold_refresh(&anyhow::anyhow!("temporary failure"));
        }
        let mut headers = HeaderMap::new();
        let origin = reqwest::Url::parse(&f.app.cfg.web.base_url)
            .unwrap()
            .origin()
            .ascii_serialization();
        headers.insert(axum::http::header::ORIGIN, origin.parse().unwrap());
        let response = logout(
            session,
            axum::extract::State(Arc::clone(&f.app)),
            headers,
            Query(ReturnToQuery { return_to: None }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            f.app.auth_sessions.is_empty(),
            "sign-out retained a coordinator"
        );
        assert_coordinator_payload_released(&entry).await;
    }
}

// Move lifecycle timestamps like the existing snapshot/refresh deadline helpers.
fn allow_coordinator_sweep(app: &ApiState) {
    *app.auth_sessions_swept_at.lock().unwrap() = None;
}

async fn assert_coordinator_payload_released(entry: &tokio::sync::Mutex<SessionAuth>) {
    let auth = entry.lock().await;
    assert!(auth.dead);
    assert!(auth.token.is_none());
    assert!(auth.refresh.is_none());
    assert!(auth.guilds.is_none());
    assert!(auth.refresh_hold.is_none());
    assert!(auth.refresh_backoff.is_zero());
}

#[tokio::test]
async fn coordinator_oauth_rotation_retires_old_entry_and_payload() {
    let f = Fixture::new(false).await;
    let old_id = f.cookie.split_once('=').unwrap().1.parse().unwrap();
    let session = Session::new(Some(old_id), Arc::new(f.store.clone()), None);
    require_auth_fresh(&f.app, &session).await.ok().unwrap();
    let old_entry = refresh_state(&f).await;
    let waiting = Session::new(Some(old_id), Arc::new(f.store.clone()), None);
    assert_eq!(waiting.get::<u64>(SESSION_USER).await.unwrap(), Some(42));
    let mut headers = HeaderMap::new();
    headers.insert(
        COOKIE,
        format!("{COOKIE_OAUTH_STATE}=fixture").parse().unwrap(),
    );
    let response = callback_response(
        session.clone(),
        &f.app.cfg,
        &f.app.oauth,
        Some(&f.app),
        headers,
        CallbackQuery {
            code: Some("fixture".into()),
            state: Some("fixture".into()),
            error: None,
        },
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_ne!(session.id(), Some(old_id));
    assert!(!f.app.auth_sessions.contains_key(&old_id));
    assert_coordinator_payload_released(&old_entry).await;
    assert!(f.app.retired_auth_sessions.contains_key(&old_id));
    let calls = f.discord.calls.load(Ordering::SeqCst);
    assert!(require_auth_fresh(&f.app, &waiting).await.is_err());
    assert!(!f.app.auth_sessions.contains_key(&old_id));
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), calls);
    let old_session = Session::new(Some(old_id), Arc::new(f.store.clone()), None);
    assert!(
        old_session
            .get::<u64>(SESSION_USER)
            .await
            .unwrap()
            .is_none()
    );
    session.save().await.unwrap();
    require_auth_fresh(&f.app, &session).await.ok().unwrap();
    assert!(f.app.auth_sessions.contains_key(&session.id().unwrap()));
}

#[tokio::test]
async fn coordinator_sweep_bounds_idle_entries_without_splitting_active_owner() {
    let f = Fixture::new(false).await;
    let id = f.cookie.split_once('=').unwrap().1.parse().unwrap();
    let session = Session::new(Some(id), Arc::new(f.store.clone()), None);
    require_auth_fresh(&f.app, &session).await.ok().unwrap();
    let entry = refresh_state(&f).await;
    let mut refreshing = entry.lock().await;
    refreshing.last_used = Some(
        std::time::Instant::now()
            .checked_sub(AUTH_SESSION_IDLE_TTL)
            .unwrap(),
    );
    // Refresh holds this exact mutex across token recovery. A sweep must not
    // replace its owner, even when its recorded activity has expired.
    allow_coordinator_sweep(&f.app);
    f.app.sweep_auth_sessions();
    assert!(f.app.auth_sessions.contains_key(&id));
    drop(refreshing);
    // A loaded request queued on this Arc must also retain the same owner.
    allow_coordinator_sweep(&f.app);
    f.app.sweep_auth_sessions();
    assert!(Arc::ptr_eq(
        f.app.auth_sessions.get(&id).unwrap().value(),
        &entry
    ));
    let old_owner = Arc::downgrade(&entry);
    drop(entry);
    // The preceding sweep is rate-limited, even though the entry is now idle.
    f.app.sweep_auth_sessions();
    assert!(f.app.auth_sessions.contains_key(&id));
    allow_coordinator_sweep(&f.app);
    // Exercise the production sweep trigger, then recreate from the live store.
    require_auth_fresh(&f.app, &session).await.ok().unwrap();
    assert!(
        old_owner.upgrade().is_none(),
        "idle coordinator was retained"
    );
    let replacement = refresh_state(&f).await;
    assert!(replacement.lock().await.last_used.unwrap().elapsed() < AUTH_SESSION_IDLE_TTL);
    drop(replacement);
    // An abandoned record is bounded by coordinator inactivity, regardless of
    // whether MemoryStore has already removed it.
    let entry = refresh_state(&f).await;
    entry.lock().await.last_used = Some(
        std::time::Instant::now()
            .checked_sub(AUTH_SESSION_IDLE_TTL)
            .unwrap(),
    );
    drop(entry);
    session.delete().await.unwrap();
    allow_coordinator_sweep(&f.app);
    f.app.sweep_auth_sessions();
    assert!(f.app.auth_sessions.is_empty());
}

#[tokio::test]
async fn coordinator_sweep_keeps_fresh_dead_marker_until_named_ttl() {
    let f = Fixture::new(false).await;
    let id = f.cookie.split_once('=').unwrap().1.parse().unwrap();
    let stale_session = Session::new(Some(id), Arc::new(f.store.clone()), None);
    // Load before the permanent refusal deletes the store record.
    assert!(
        stale_session
            .get::<u64>(SESSION_USER)
            .await
            .unwrap()
            .is_some()
    );
    f.discord
        .unauthorized_until_refresh
        .store(true, Ordering::SeqCst);
    f.discord.refresh_failed.store(true, Ordering::SeqCst);
    let session = Session::new(Some(id), Arc::new(f.store.clone()), None);
    assert!(
        require_auth_fresh(&f.app, &session)
            .await
            .err()
            .unwrap()
            .0
            .is::<AuthLost>()
    );
    let entry = refresh_state(&f).await;
    assert_coordinator_payload_released(&entry).await;
    drop(entry);
    allow_coordinator_sweep(&f.app);
    f.app.sweep_auth_sessions();
    assert!(f.app.auth_sessions.contains_key(&id));
    let calls = f.discord.token_calls.load(Ordering::SeqCst);
    assert!(
        require_auth_fresh(&f.app, &stale_session)
            .await
            .err()
            .unwrap()
            .0
            .is::<AuthLost>()
    );
    assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), calls);
    let entry = refresh_state(&f).await;
    entry.lock().await.dead_since = Some(
        std::time::Instant::now()
            .checked_sub(AUTH_SESSION_DEAD_TTL)
            .unwrap(),
    );
    drop(entry);
    allow_coordinator_sweep(&f.app);
    f.app.sweep_auth_sessions();
    assert!(f.app.auth_sessions.is_empty());
    assert!(f.app.retired_auth_sessions.contains_key(&id));
    // Tombstone retirement metadata is independent of the 30-second payload marker.
    *f.app.retired_auth_sessions.get_mut(&id).unwrap() = std::time::Instant::now()
        .checked_sub(AUTH_SESSION_TOMBSTONE_TTL - std::time::Duration::from_secs(1))
        .unwrap();
    allow_coordinator_sweep(&f.app);
    f.app.sweep_auth_sessions();
    assert!(f.app.retired_auth_sessions.contains_key(&id));
    *f.app.retired_auth_sessions.get_mut(&id).unwrap() = std::time::Instant::now()
        .checked_sub(AUTH_SESSION_TOMBSTONE_TTL)
        .unwrap();
    // Rate limiting applies to the tombstone sweep too.
    f.app.sweep_auth_sessions();
    assert!(f.app.retired_auth_sessions.contains_key(&id));
    allow_coordinator_sweep(&f.app);
    f.app.sweep_auth_sessions();
    assert!(!f.app.retired_auth_sessions.contains_key(&id));
}

#[tokio::test]
async fn refresh_server_waits_are_capped_without_overflow() {
    for (rate_limited, header, body) in [
        (false, "3600".to_owned(), None),
        (false, u64::MAX.to_string(), None),
        (false, f64::MAX.to_string(), None),
        (true, u64::MAX.to_string(), None),
        (true, String::new(), Some(f64::MAX)),
    ] {
        let f = Fixture::new(false).await;
        f.app.guild_cache.clear();
        f.discord
            .unauthorized_until_refresh
            .store(true, Ordering::SeqCst);
        if rate_limited {
            f.discord.refresh_rate_limited.store(true, Ordering::SeqCst);
            *f.discord.retry_after.lock().unwrap() = header;
            *f.discord.body_retry_after.lock().unwrap() = body;
        } else {
            f.discord
                .refresh_server_failed
                .store(true, Ordering::SeqCst);
            *f.discord.refresh_server_retry_after.lock().unwrap() = Some(header);
        }
        let (status, response) = f.request("POST", "/api/me", "").await;
        assert_eq!(
            status,
            if rate_limited {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::BAD_REQUEST
            },
            "{response}"
        );
        assert_refresh_session_kept(&f).await;
        assert_refresh_wait(&f, REFRESH_RETRY_MAX).await;
        let state = refresh_state(&f).await;
        assert!(matches!(
            state.lock().await.refresh_hold.unwrap().0,
            oauth::Cooldown::Until(_)
        ));
        // Shared cooldowns also feed the hold; their larger remaining wait
        // must not reintroduce a process-lifetime session hold.
        if rate_limited {
            expire_refresh_hold(&f).await;
            assert_eq!(
                f.request("POST", "/api/me", "").await.0,
                StatusCode::TOO_MANY_REQUESTS
            );
            assert_refresh_wait(&f, REFRESH_RETRY_MAX).await;
            assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), 1);
        }
    }
}

#[tokio::test]
async fn refresh_hold_resets_when_existing_access_token_authorizes_again() {
    let f = Fixture::new(false).await;
    f.app.guild_cache.clear();
    f.discord
        .unauthorized_until_refresh
        .store(true, Ordering::SeqCst);
    f.discord
        .refresh_server_failed
        .store(true, Ordering::SeqCst);
    for seconds in [5, 10] {
        assert_eq!(
            f.request("POST", "/api/me", "").await.0,
            StatusCode::BAD_REQUEST
        );
        assert_refresh_wait(&f, std::time::Duration::from_secs(seconds)).await;
        expire_refresh_hold(&f).await;
    }
    f.discord
        .unauthorized_until_refresh
        .store(false, Ordering::SeqCst);
    assert_eq!(f.request("POST", "/api/me", "").await.0, StatusCode::OK);
    assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), 2);
    let state = refresh_state(&f).await;
    assert!(state.lock().await.refresh_hold.is_none());
    assert!(state.lock().await.refresh_backoff.is_zero());
    assert_eq!(state.lock().await.token.as_deref(), Some("fixture-token"));
    drop(state);
    expire_auth_cache(&f.app, GUILD_AUTH_CACHE_TTL).await;
    f.app.guild_cache.clear();
    f.discord
        .unauthorized_until_refresh
        .store(true, Ordering::SeqCst);
    assert_eq!(
        f.request("POST", "/api/me", "").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_refresh_wait(&f, REFRESH_RETRY_MIN).await;
    assert_eq!(f.discord.token_calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn retired_hydrated_handle_cannot_recreate_coordinator() {
    let f = Fixture::new(true).await;
    let id = f.cookie.split_once('=').unwrap().1.parse().unwrap();
    let waiting = Session::new(Some(id), Arc::new(f.store.clone()), None);
    assert_eq!(waiting.get::<u64>(SESSION_USER).await.unwrap(), Some(42));
    assert!(f.app.auth_sessions.is_empty());
    let mut headers = HeaderMap::new();
    let origin = reqwest::Url::parse(&f.app.cfg.web.base_url)
        .unwrap()
        .origin()
        .ascii_serialization();
    headers.insert(axum::http::header::ORIGIN, origin.parse().unwrap());
    let response = logout(
        Session::new(Some(id), Arc::new(f.store.clone()), None),
        axum::extract::State(Arc::clone(&f.app)),
        headers,
        Query(ReturnToQuery { return_to: None }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let calls = f.discord.calls.load(Ordering::SeqCst);
    assert!(
        require_auth_fresh(&f.app, &waiting).await.is_err(),
        "retired hydrated handle authorized again"
    );
    assert!(
        f.app.auth_sessions.is_empty(),
        "retired ID recreated a coordinator"
    );
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), calls);
    assert!(session_owner(&f.app, &waiting).await.is_err());
    assert!(waiting.is_empty().await);
    let fresh = Session::new(Some(id), Arc::new(f.store.clone()), None);
    let mut headers = HeaderMap::new();
    headers.insert(
        COOKIE,
        format!("{COOKIE_OAUTH_STATE}=fixture").parse().unwrap(),
    );
    let response = callback_response(
        fresh.clone(),
        &f.app.cfg,
        &f.app.oauth,
        Some(&f.app),
        headers,
        CallbackQuery {
            code: Some("fixture".into()),
            state: Some("fixture".into()),
            error: None,
        },
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_ne!(fresh.id(), Some(id));
    fresh.save().await.unwrap();
    require_auth_fresh(&f.app, &fresh).await.ok().unwrap();
    session_owner(&f.app, &fresh).await.ok().unwrap();
    assert!(f.app.retired_auth_sessions.contains_key(&id));
    assert!(
        !f.app
            .retired_auth_sessions
            .contains_key(&fresh.id().unwrap())
    );
}

#[tokio::test]
async fn retired_delayed_response_save_cannot_restore_authority() {
    for definitive_refusal in [false, true] {
        let f = Fixture::new(true).await;
        f.enroll(10).await;
        let id = f.cookie.split_once('=').unwrap().1.parse().unwrap();
        let hydrated = Arc::new(tokio::sync::Notify::new());
        let authenticate = Arc::new(tokio::sync::Notify::new());
        let authorized = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        let delayed = Router::new()
            .route(
                "/delayed",
                get({
                    let app = Arc::clone(&f.app);
                    let hydrated = Arc::clone(&hydrated);
                    let authenticate = Arc::clone(&authenticate);
                    let authorized = Arc::clone(&authorized);
                    let finish = Arc::clone(&finish);
                    move |session: Session| {
                        let (app, hydrated, authenticate, authorized, finish) = (
                            Arc::clone(&app),
                            Arc::clone(&hydrated),
                            Arc::clone(&authenticate),
                            Arc::clone(&authorized),
                            Arc::clone(&finish),
                        );
                        async move {
                            assert_eq!(session.get::<u64>(SESSION_USER).await.unwrap(), Some(42));
                            hydrated.notify_one();
                            authenticate.notified().await;
                            require_auth_fresh(&app, &session).await.ok().unwrap();
                            // The leader's rotated credentials modified this old snapshot.
                            assert!(session.is_modified());
                            authorized.notify_one();
                            finish.notified().await;
                            StatusCode::OK
                        }
                    }
                }),
            )
            .layer(
                SessionManagerLayer::new(f.store.clone())
                    .with_name(SESSION_COOKIE_NAME)
                    .with_secure(false)
                    .with_expiry(Expiry::OnInactivity(CookieDuration::seconds(
                        SESSION_INACTIVITY_SECS,
                    ))),
            );
        let request = Request::builder()
            .uri("/delayed")
            .header(COOKIE, &f.cookie)
            .body(Body::empty())
            .unwrap();
        let response = tokio::spawn(delayed.oneshot(request));
        hydrated.notified().await;
        f.discord
            .unauthorized_until_refresh
            .store(true, Ordering::SeqCst);
        let leader = Session::new(Some(id), Arc::new(f.store.clone()), None);
        require_auth_fresh(&f.app, &leader).await.ok().unwrap();
        authenticate.notify_one();
        authorized.notified().await;
        if definitive_refusal {
            expire_auth_cache(&f.app, GUILD_AUTH_CACHE_TTL).await;
            f.discord.still_unauthorized.store(true, Ordering::SeqCst);
            f.discord.refresh_failed.store(true, Ordering::SeqCst);
            assert_eq!(
                f.request("GET", "/api/guilds/10/config", "").await.0,
                StatusCode::UNAUTHORIZED
            );
            let entry = refresh_state(&f).await;
            entry.lock().await.dead_since = Some(
                std::time::Instant::now()
                    .checked_sub(AUTH_SESSION_DEAD_TTL)
                    .unwrap(),
            );
            drop(entry);
            allow_coordinator_sweep(&f.app);
            f.app.sweep_auth_sessions();
        } else {
            let mut headers = HeaderMap::new();
            let origin = reqwest::Url::parse(&f.app.cfg.web.base_url)
                .unwrap()
                .origin()
                .ascii_serialization();
            headers.insert(axum::http::header::ORIGIN, origin.parse().unwrap());
            assert_eq!(
                logout(
                    leader,
                    axum::extract::State(Arc::clone(&f.app)),
                    headers,
                    Query(ReturnToQuery { return_to: None })
                )
                .await
                .status(),
                StatusCode::OK
            );
        }
        assert!(f.app.auth_sessions.is_empty());
        use tower_sessions::SessionStore as _;
        assert!(f.store.load(&id).await.unwrap().is_none());
        finish.notify_one();
        assert_eq!(response.await.unwrap().unwrap().status(), StatusCode::OK);
        // Real response middleware has now saved the modified, nonempty record.
        assert!(f.store.load(&id).await.unwrap().is_some());
        let owner = Session::new(Some(id), Arc::new(f.store.clone()), None);
        assert_eq!(owner.get::<u64>(SESSION_USER).await.unwrap(), Some(42));
        let calls = (
            f.discord.calls.load(Ordering::SeqCst),
            f.discord.token_calls.load(Ordering::SeqCst),
            f.discord.me_calls.load(Ordering::SeqCst),
        );
        let (status, body) = f.request("GET", "/api/guilds/10/config", "").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        assert_eq!(body["code"], "auth_lost");
        assert!(f.store.load(&id).await.unwrap().is_none());
        // The independently hydrated owner handle must also reject and clear.
        assert!(
            session_owner(&f.app, &owner)
                .await
                .err()
                .unwrap()
                .0
                .is::<AuthLost>()
        );
        assert!(owner.is_empty().await);
        assert!(f.app.auth_sessions.is_empty());
        assert_eq!(
            (
                f.discord.calls.load(Ordering::SeqCst),
                f.discord.token_calls.load(Ordering::SeqCst),
                f.discord.me_calls.load(Ordering::SeqCst)
            ),
            calls
        );
    }
}

async fn blocker_request(
    f: &Fixture,
    method: &str,
    path: &str,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let origin = reqwest::Url::parse(&f.app.cfg.web.base_url)
        .unwrap()
        .origin()
        .ascii_serialization();
    let response = f
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header(COOKIE, &f.cookie)
                .header(axum::http::header::ORIGIN, origin)
                .header("content-type", "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

// Hold a modified response so the real session middleware writes its old row
// after a different request has deleted or rotated that row.
async fn delayed_session_save(
    f: &Fixture,
) -> (Arc<tokio::sync::Notify>, tokio::task::JoinHandle<()>) {
    let hydrated = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    let delayed = Router::new()
        .route(
            "/delayed",
            get({
                let hydrated = Arc::clone(&hydrated);
                let finish = Arc::clone(&finish);
                move |session: Session| {
                    let hydrated = Arc::clone(&hydrated);
                    let finish = Arc::clone(&finish);
                    async move {
                        assert!(session.get::<u64>(SESSION_USER).await.unwrap().is_some());
                        session.insert("delayed_response", true).await.unwrap();
                        hydrated.notify_one();
                        finish.notified().await;
                        StatusCode::OK
                    }
                }
            }),
        )
        .layer(
            SessionManagerLayer::new(f.store.clone())
                .with_name(SESSION_COOKIE_NAME)
                .with_secure(false)
                .with_expiry(Expiry::OnInactivity(CookieDuration::seconds(
                    SESSION_INACTIVITY_SECS,
                ))),
        );
    let request = Request::builder()
        .uri("/delayed")
        .header(COOKIE, &f.cookie)
        .body(Body::empty())
        .unwrap();
    let response = tokio::spawn(async move {
        assert_eq!(
            delayed.oneshot(request).await.unwrap().status(),
            StatusCode::OK
        );
    });
    hydrated.notified().await;
    (finish, response)
}

async fn oauth_callback_request(f: &Fixture, cookie: &str) -> Response {
    f.router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/oauth/callback?code=fixture&state=fixture")
                .header(COOKIE, format!("{cookie}; {COOKIE_OAUTH_STATE}=fixture"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn recovery_rejects_late_save(definitive_refusal: bool) {
    use tower_sessions::SessionStore as _;
    for (method, route, body) in [
        ("GET", "/api/service-upgrade", ""),
        (
            "POST",
            "/api/service-upgrade/check",
            r#"{"channel":"invalid"}"#,
        ),
        (
            "POST",
            "/api/service-upgrade",
            r#"{"bot_digest":"","api_digest":"","confirmed":false}"#,
        ),
        ("POST", "/api/service-upgrade/cancel", r#"{"id":""}"#),
    ] {
        let f = Fixture::new(true).await;
        let id = f.cookie.split_once('=').unwrap().1.parse().unwrap();
        let (finish, response) = delayed_session_save(&f).await;
        if definitive_refusal {
            f.app.guild_cache.clear();
            f.discord.still_unauthorized.store(true, Ordering::SeqCst);
            f.discord.refresh_failed.store(true, Ordering::SeqCst);
            let (status, body) = f.request("POST", "/api/me", "").await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
            assert_eq!(body["code"], "auth_lost");
            let entry = refresh_state(&f).await;
            entry.lock().await.dead_since = Some(
                std::time::Instant::now()
                    .checked_sub(AUTH_SESSION_DEAD_TTL)
                    .unwrap(),
            );
            drop(entry);
            allow_coordinator_sweep(&f.app);
            f.app.sweep_auth_sessions();
        } else {
            assert_eq!(
                blocker_request(&f, "POST", "/logout", "").await.0,
                StatusCode::OK
            );
        }
        assert!(f.store.load(&id).await.unwrap().is_none());
        assert!(f.app.auth_sessions.is_empty());
        finish.notify_one();
        response.await.unwrap();
        assert!(f.store.load(&id).await.unwrap().is_some());
        let calls = (
            f.discord.calls.load(Ordering::SeqCst),
            f.discord.token_calls.load(Ordering::SeqCst),
            f.discord.me_calls.load(Ordering::SeqCst),
        );
        let (status, body) = blocker_request(&f, method, route, body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {route}: {body}");
        assert_eq!(body["code"], "auth_lost");
        assert!(f.store.load(&id).await.unwrap().is_none());
        assert!(f.app.auth_sessions.is_empty());
        assert_eq!(
            calls,
            (
                f.discord.calls.load(Ordering::SeqCst),
                f.discord.token_calls.load(Ordering::SeqCst),
                f.discord.me_calls.load(Ordering::SeqCst),
            )
        );
    }
}

#[tokio::test]
async fn review_blocker_recovery_rejects_late_save_after_logout() {
    recovery_rejects_late_save(false).await;
}

#[tokio::test]
async fn review_blocker_recovery_rejects_late_save_after_refusal() {
    recovery_rejects_late_save(true).await;
}

#[tokio::test]
async fn review_blocker_oauth_rotation_rejects_late_save() {
    use tower_sessions::SessionStore as _;
    let f = Fixture::new(true).await;
    let id = f.cookie.split_once('=').unwrap().1.parse().unwrap();
    let (finish, response) = delayed_session_save(&f).await;
    let callback = oauth_callback_request(&f, &f.cookie).await;
    assert_eq!(callback.status(), StatusCode::SEE_OTHER);
    let new_cookie = callback
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|header| header.to_str().ok())
        .find(|cookie| cookie.starts_with(SESSION_COOKIE_NAME))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    assert_ne!(new_cookie, f.cookie);
    assert!(f.app.retired_auth_sessions.contains_key(&id));
    assert!(f.store.load(&id).await.unwrap().is_none());
    finish.notify_one();
    response.await.unwrap();
    assert!(f.store.load(&id).await.unwrap().is_some());
    let calls = f.discord.calls.load(Ordering::SeqCst);
    let (status, body) = f.request("POST", "/api/me", "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["code"], "auth_lost");
    assert!(f.store.load(&id).await.unwrap().is_none());
    assert!(f.app.auth_sessions.is_empty());
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), calls);
    let current = Session::new(
        Some(new_cookie.split_once('=').unwrap().1.parse().unwrap()),
        Arc::new(f.store.clone()),
        None,
    );
    require_auth_fresh(&f.app, &current).await.ok().unwrap();
}

#[tokio::test]
async fn review_blocker_logout_ignores_unknown_cookie() {
    use tower_sessions::SessionStore as _;
    let mut f = Fixture::new(true).await;
    let real_cookie = f.cookie.clone();
    let real_id = real_cookie.split_once('=').unwrap().1.parse().unwrap();
    for _ in 0..8 {
        let id = tower_sessions::session::Id::default();
        assert!(f.store.load(&id).await.unwrap().is_none());
        f.cookie = format!("{SESSION_COOKIE_NAME}={id}");
        assert_eq!(
            blocker_request(&f, "POST", "/logout", "").await.0,
            StatusCode::OK
        );
        assert!(
            f.app.retired_auth_sessions.is_empty(),
            "unknown cookie allocated a tombstone"
        );
    }
    // A persisted identity without a coordinator must still be fenced.
    f.cookie = real_cookie;
    assert!(f.app.auth_sessions.is_empty());
    assert_eq!(
        blocker_request(&f, "POST", "/logout", "").await.0,
        StatusCode::OK
    );
    assert!(f.app.retired_auth_sessions.contains_key(&real_id));
    assert!(f.store.load(&real_id).await.unwrap().is_none());
    // A coordinator also proves ownership if its store row has disappeared.
    let g = Fixture::new(true).await;
    let id = g.cookie.split_once('=').unwrap().1.parse().unwrap();
    let session = Session::new(Some(id), Arc::new(g.store.clone()), None);
    require_auth_fresh(&g.app, &session).await.ok().unwrap();
    g.store.delete(&id).await.unwrap();
    assert_eq!(
        blocker_request(&g, "POST", "/logout", "").await.0,
        StatusCode::OK
    );
    assert!(g.app.retired_auth_sessions.contains_key(&id));
    assert!(g.app.auth_sessions.is_empty());
}

#[tokio::test]
async fn review_blocker_oauth_rotation_ignores_unknown_cookie() {
    let f = Fixture::new(true).await;
    for _ in 0..2 {
        let id = tower_sessions::session::Id::default();
        let cookie = format!("{SESSION_COOKIE_NAME}={id}");
        let response = oauth_callback_request(&f, &cookie).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(
            f.app.retired_auth_sessions.is_empty(),
            "unknown OAuth cookie allocated a tombstone"
        );
        assert!(
            response
                .headers()
                .get_all("set-cookie")
                .iter()
                .any(|header| header.to_str().unwrap().starts_with(SESSION_COOKIE_NAME))
        );
    }
}

#[tokio::test]
async fn tombstone_margin_rejects_unseen_late_save_past_inactivity() {
    use tower_sessions::SessionStore as _;
    // Both owner-only recovery and Discord-backed authentication must fail.
    for route in ["/api/service-upgrade", "/api/me"] {
        let f = Fixture::new(true).await;
        let id = f.cookie.split_once('=').unwrap().1.parse().unwrap();
        let (finish, response) = delayed_session_save(&f).await;
        assert_eq!(
            blocker_request(&f, "POST", "/logout", "").await.0,
            StatusCode::OK
        );
        assert!(f.store.load(&id).await.unwrap().is_none());
        let retirement = tower_sessions::cookie::time::OffsetDateTime::now_utc();
        finish.notify_one();
        response.await.unwrap();
        let mut revived = f.store.load(&id).await.unwrap().unwrap();
        assert!(
            revived.expiry_date >= retirement + CookieDuration::seconds(SESSION_INACTIVITY_SECS)
        );
        // Advance both clocks consistently: retirement was seven days + one
        // second ago; the late response saved two minutes after retirement.
        // No request observes the restored row before the production sweep.
        *f.app.retired_auth_sessions.get_mut(&id).unwrap() = std::time::Instant::now()
            .checked_sub(AUTH_SESSION_IDLE_TTL + std::time::Duration::from_secs(1))
            .unwrap();
        revived.expiry_date =
            tower_sessions::cookie::time::OffsetDateTime::now_utc() + CookieDuration::seconds(119);
        f.store.save(&revived).await.unwrap();
        allow_coordinator_sweep(&f.app);
        f.app.sweep_auth_sessions();
        let calls = (
            f.discord.calls.load(Ordering::SeqCst),
            f.discord.token_calls.load(Ordering::SeqCst),
            f.discord.me_calls.load(Ordering::SeqCst),
        );
        let (status, body) = f.request("GET", route, "").await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "late-saved identity became authoritative: {body}"
        );
        assert_eq!(body["code"], "auth_lost");
        assert!(f.store.load(&id).await.unwrap().is_none());
        assert!(f.app.auth_sessions.is_empty());
        assert_eq!(
            calls,
            (
                f.discord.calls.load(Ordering::SeqCst),
                f.discord.token_calls.load(Ordering::SeqCst),
                f.discord.me_calls.load(Ordering::SeqCst)
            )
        );

        // After eight days and the row's own expiry, cleanup leaves an unknown
        // ID. MemoryStore itself filters expired records on load.
        revived.expiry_date =
            tower_sessions::cookie::time::OffsetDateTime::now_utc() - CookieDuration::seconds(1);
        f.store.save(&revived).await.unwrap();
        *f.app.retired_auth_sessions.get_mut(&id).unwrap() = std::time::Instant::now()
            .checked_sub(AUTH_SESSION_IDLE_TTL + std::time::Duration::from_secs(24 * 60 * 60 + 1))
            .unwrap();
        allow_coordinator_sweep(&f.app);
        f.app.sweep_auth_sessions();
        assert!(!f.app.retired_auth_sessions.contains_key(&id));
        assert!(f.store.load(&id).await.unwrap().is_none());
        let (status, body) = f.request("GET", "/api/service-upgrade", "").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"], "sign in to manage service upgrades");
        let unknown = Session::new(Some(id), Arc::new(f.store.clone()), None);
        let failure = session_owner(&f.app, &unknown).await.err().unwrap();
        assert_eq!(failure.0.to_string(), "not authenticated");
        assert!(body.get("code").is_none());
        assert!(f.app.auth_sessions.is_empty());
        assert_eq!(
            calls,
            (
                f.discord.calls.load(Ordering::SeqCst),
                f.discord.token_calls.load(Ordering::SeqCst),
                f.discord.me_calls.load(Ordering::SeqCst)
            )
        );
    }
}

#[tokio::test]
async fn known_bad_image_routes_require_current_page_and_live_permission() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    let hash = "ab".repeat(32);
    let body = serde_json::json!({"hash": hash, "note": null}).to_string();
    let old = db::guild_enrollment(&f.app.db, 10).await.unwrap().unwrap();
    let (status, _) = f
        .request_token("POST", "/api/guilds/10/known-bad-images", &body, None)
        .await;
    assert_ne!(status, StatusCode::OK);
    db::retire_guild_enrollment(&f.app.db, 10, 100)
        .await
        .unwrap();
    db::enroll_guild(&f.app.db, 10, 101).await.unwrap();
    let (status, error) = f
        .request_token(
            "POST",
            "/api/guilds/10/known-bad-images",
            &body,
            Some(&old.id),
        )
        .await;
    assert_ne!(status, StatusCode::OK);
    assert_eq!(error["code"], "guild_enrollment_changed");
    assert!(
        db::load_config_parts(&f.app.db, 10)
            .await
            .unwrap()
            .known_bad_images
            .is_empty()
    );
    assert_ne!(
        f.request("POST", "/api/guilds/0/known-bad-images", &body)
            .await
            .0,
        StatusCode::OK
    );
    expire_auth_cache(&f.app, GUILD_AUTH_CACHE_TTL).await;
    f.discord.guilds.lock().unwrap().clear();
    assert_ne!(
        f.request("POST", "/api/guilds/10/known-bad-images", &body)
            .await
            .0,
        StatusCode::OK
    );
    assert_ne!(
        f.request(
            "DELETE",
            &format!("/api/guilds/10/known-bad-images/{hash}"),
            ""
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn known_bad_image_http_add_list_remove_accepts_only_safe_metadata() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    let hash = "ab".repeat(32);
    let body =
        serde_json::json!({"hash": hash.to_uppercase(), "note": "confirmed spam"}).to_string();
    let (status, saved) = f
        .request("POST", "/api/guilds/10/known-bad-images", &body)
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["changed"], true);
    let (status, config) = f.request("POST", "/api/guilds/10/config", "").await;
    assert_eq!(status, StatusCode::OK);
    let rows = config["known_bad_images"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["hash"], hash);
    assert_eq!(rows[0]["actor"], "unknown");
    assert_eq!(rows[0]["added_via"], "webui");
    assert_eq!(rows[0].as_object().unwrap().len(), 5);
    for invalid in [
        serde_json::json!({"hash": "unknown"}),
        serde_json::json!({"note": "missing digest"}),
        serde_json::json!({"hash": hash, "payload": "unsupported"}),
    ] {
        assert_ne!(
            f.request(
                "POST",
                "/api/guilds/10/known-bad-images",
                &invalid.to_string()
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    assert_eq!(
        f.request("POST", "/api/guilds/10/known-bad-images", &body)
            .await
            .1["changed"],
        false
    );
    assert_eq!(
        f.request(
            "DELETE",
            &format!("/api/guilds/10/known-bad-images/{hash}"),
            ""
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(
        f.request("POST", "/api/guilds/10/config", "").await.1["known_bad_images"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn known_bad_image_delete_rate_limit_leaves_row_intact() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    let hash = "ab".repeat(32);
    let body = serde_json::json!({"hash": hash, "note": null}).to_string();
    assert_eq!(
        f.request("POST", "/api/guilds/10/known-bad-images", &body)
            .await
            .0,
        StatusCode::OK
    );
    expire_auth_cache(&f.app, GUILD_AUTH_MAX_STALE).await;
    f.discord.rate_limited.store(true, Ordering::SeqCst);
    let (status, body) = f
        .request(
            "DELETE",
            &format!("/api/guilds/10/known-bad-images/{hash}"),
            "",
        )
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["code"], "discord_rate_limited");
    assert_eq!(
        db::load_config_parts(&f.app.db, 10)
            .await
            .unwrap()
            .known_bad_images
            .len(),
        1
    );
}
