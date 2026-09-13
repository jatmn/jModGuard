//! Conductor HTTP surface. Guild API cookies never authenticate this origin.
use axum::extract::{Path, Query, State};
use axum::http::header::{CACHE_CONTROL, COOKIE, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use std::sync::Arc;

use crate::settings::SettingsPatch;
use crate::state::{Conductor, Operator, parse_session_cookie};
use crate::store::{now_ms, random_token};

const SESSION_COOKIE: &str = "jmodguard_conductor_session";
const OAUTH_COOKIE: &str = "jmodguard_conductor_oauth_state";
const NO_STORE: HeaderValue = HeaderValue::from_static("no-store");

pub fn router(state: Option<Arc<Conductor>>) -> Router {
    match state {
        Some(state) if state.operations_available() => operational_router(state),
        Some(state) => status_router(Some(state)),
        None => status_router(None),
    }
}

fn status_router(state: Option<Arc<Conductor>>) -> Router {
    let available = state
        .as_ref()
        .map(|s| s.operations_available())
        .unwrap_or(false);
    let version = serde_json::json!({
        "service": "conductor",
        "version": env!("CARGO_PKG_VERSION"),
        "operations_available": available,
    });
    let ready_state = state.clone();
    let router = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/health/", get(|| async { "ok" }))
        .route(
            "/ready",
            get(move || {
                let ready_state = ready_state.clone();
                async move {
                    if ready_state
                        .as_ref()
                        .is_some_and(|s| s.operations_available())
                    {
                        (StatusCode::OK, Json(serde_json::json!({ "code": "ready" })))
                            .into_response()
                    } else {
                        (
                            StatusCode::SERVICE_UNAVAILABLE,
                            Json(serde_json::json!({ "code": "control_not_configured" })),
                        )
                            .into_response()
                    }
                }
            }),
        )
        .route(
            "/version",
            get(move || {
                let version = version.clone();
                async move { Json(version) }
            }),
        )
        .layer(axum::middleware::map_response(
            |mut response: Response| async move {
                response.headers_mut().insert(CACHE_CONTROL, NO_STORE);
                response
            },
        ));
    if let Some(state) = state {
        router.with_state(state)
    } else {
        router.with_state(Arc::new(placeholder_state()))
    }
}

fn operational_router(state: Arc<Conductor>) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/health/", get(|| async { "ok" }))
        .route("/ready", get(ready))
        .route("/version", get(version))
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        .route("/login", get(login))
        .route("/logout", post(logout))
        .route("/oauth/callback", get(callback))
        .route("/api/me", get(api_me))
        .route("/api/settings", get(get_settings).patch(patch_settings))
        .route("/api/status", get(api_status))
        .route("/api/telemetry", get(api_telemetry))
        .route("/api/telemetry/lease", post(api_lease))
        .route("/api/jobs", get(api_jobs))
        .route("/api/jobs/{id}/retry", post(api_retry_job))
        .route("/api/notifications/test", post(api_notify_test))
        .route("/api/notifications/health", get(api_notify_health))
        .layer(axum::middleware::map_response(
            |mut response: Response| async move {
                response.headers_mut().insert(CACHE_CONTROL, NO_STORE);
                response
            },
        ))
        .with_state(state)
}

fn placeholder_state() -> Conductor {
    Conductor {
        cfg: crate::config::Config {
            bind: "127.0.0.1:0".parse().expect("static"),
            log_filter: "info".into(),
            shutdown_grace: std::time::Duration::from_secs(1),
            db_path: None,
            public_url: String::new(),
            owner_id: 0,
            oauth_client_id: String::new(),
            oauth_client_secret: String::new(),
            bot_health_url: String::new(),
            api_health_url: String::new(),
            oauth_token_url: String::new(),
            oauth_me_url: String::new(),
        },
        store: None,
        http: reqwest::Client::new(),
        started: std::time::Instant::now(),
    }
}

async fn ready(State(state): State<Arc<Conductor>>) -> Response {
    if state.operations_available() {
        (StatusCode::OK, Json(serde_json::json!({ "code": "ready" }))).into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "code": "control_not_configured" })),
        )
            .into_response()
    }
}

async fn version(State(state): State<Arc<Conductor>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "service": "conductor",
        "version": env!("CARGO_PKG_VERSION"),
        "operations_available": state.operations_available(),
        "backup_available": false,
        "update_available": false,
    }))
}

async fn index() -> impl IntoResponse {
    static_file(
        include_str!("../frontend/index.html"),
        "text/html; charset=utf-8",
    )
}
async fn app_js() -> impl IntoResponse {
    static_file(
        include_str!("../frontend/app.js"),
        "application/javascript; charset=utf-8",
    )
}
async fn style_css() -> impl IntoResponse {
    static_file(
        include_str!("../frontend/style.css"),
        "text/css; charset=utf-8",
    )
}
fn static_file(content: &'static str, content_type: &'static str) -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, content_type)], content)
}

async fn login(State(state): State<Arc<Conductor>>) -> Response {
    let token = random_token();
    let store = match state.store() {
        Ok(store) => store,
        Err(error) => return error_response(StatusCode::SERVICE_UNAVAILABLE, error),
    };
    if let Err(error) = store.put_oauth_state(&token).await {
        return error_response(StatusCode::BAD_REQUEST, error);
    }
    let url = state.authorize_url(&token);
    let mut response = Redirect::to(&url).into_response();
    append_cookie(
        &mut response,
        OAUTH_COOKIE,
        &token,
        state.cfg.cookie_secure(),
        600,
    );
    response
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn callback(
    State(state): State<Arc<Conductor>>,
    Query(query): Query<CallbackQuery>,
    headers: HeaderMap,
) -> Response {
    if query.error.is_some() {
        return error_page("Discord login was cancelled.");
    }
    let Some(code) = query.code.filter(|s| !s.is_empty()) else {
        return error_page("missing OAuth code");
    };
    let Some(state_token) = query.state.filter(|s| !s.is_empty()) else {
        return error_page("missing OAuth state");
    };
    let cookie = cookie_value(
        headers.get(COOKIE).and_then(|v| v.to_str().ok()),
        OAUTH_COOKIE,
    );
    if cookie.as_deref() != Some(state_token.as_str()) {
        return error_page("OAuth state mismatch");
    }
    let Ok(store) = state.store() else {
        return error_page("control store is not configured");
    };
    match store.take_oauth_state(&state_token).await {
        Ok(true) => {}
        _ => return error_page("OAuth state expired"),
    }
    match state.complete_login(&code).await {
        Ok(session_id) => {
            let mut response = Redirect::to("/").into_response();
            append_cookie(
                &mut response,
                SESSION_COOKIE,
                &session_id,
                state.cfg.cookie_secure(),
                86400,
            );
            clear_cookie(&mut response, OAUTH_COOKIE, state.cfg.cookie_secure());
            response
        }
        Err(error) => error_page(&error.to_string()),
    }
}

async fn logout(State(state): State<Arc<Conductor>>, headers: HeaderMap) -> Response {
    if let Some(id) = parse_session_cookie(headers.get(COOKIE).and_then(|v| v.to_str().ok()))
        && let Ok(store) = state.store()
    {
        let _ = store.revoke_session(&id).await;
        let _ = store.audit("session", "logout", &id).await;
    }
    let mut response = Json(serde_json::json!({ "ok": true })).into_response();
    clear_cookie(&mut response, SESSION_COOKIE, state.cfg.cookie_secure());
    response
}

async fn api_me(State(state): State<Arc<Conductor>>, headers: HeaderMap) -> Response {
    match operator(&state, &headers).await {
        Ok(op) => Json(serde_json::json!({
            "user": { "id": op.operator_id },
            "is_conductor_operator": true,
            "preview": false,
        }))
        .into_response(),
        Err(error) => auth_error(error),
    }
}

async fn get_settings(State(state): State<Arc<Conductor>>, headers: HeaderMap) -> Response {
    if let Err(error) = operator(&state, &headers).await {
        return auth_error(error);
    }
    let store = match state.store() {
        Ok(store) => store,
        Err(error) => return error_response(StatusCode::SERVICE_UNAVAILABLE, error),
    };
    match store.load_settings().await {
        Ok((mut settings, revision, updated_ms)) => {
            let _ = settings.validate();
            Json(serde_json::json!({
                "revision": revision,
                "updated_ms": updated_ms,
                "settings": settings.public_view(),
            }))
            .into_response()
        }
        Err(error) => error_response(StatusCode::BAD_REQUEST, error),
    }
}

async fn patch_settings(
    State(state): State<Arc<Conductor>>,
    headers: HeaderMap,
    Json(patch): Json<SettingsPatch>,
) -> Response {
    let operator = match operator(&state, &headers).await {
        Ok(op) => op,
        Err(error) => return auth_error(error),
    };
    let expected = patch.revision.unwrap_or(0);
    match apply_settings(&state, &operator, patch, expected).await {
        Ok(body) => Json(body).into_response(),
        Err(error) => error_response(StatusCode::BAD_REQUEST, error),
    }
}

async fn apply_settings(
    state: &Conductor,
    operator: &Operator,
    patch: SettingsPatch,
    expected: i64,
) -> anyhow::Result<serde_json::Value> {
    let store = state.store()?;
    let (mut settings, revision, _) = store.load_settings().await?;
    anyhow::ensure!(
        expected == 0 || expected == revision,
        "settings revision mismatch"
    );
    settings.apply_patch(patch)?;
    let next = store.save_settings(&settings, revision).await?;
    store
        .audit(&operator.operator_id, "settings_patch", &next.to_string())
        .await?;
    Ok(serde_json::json!({
        "revision": next,
        "settings": settings.public_view(),
        "restart_required": [],
    }))
}

async fn api_status(State(state): State<Arc<Conductor>>, headers: HeaderMap) -> Response {
    if let Err(error) = operator(&state, &headers).await {
        return auth_error(error);
    }
    match status_payload(&state).await {
        Ok(body) => Json(body).into_response(),
        Err(error) => error_response(StatusCode::BAD_REQUEST, error),
    }
}

async fn status_payload(state: &Conductor) -> anyhow::Result<serde_json::Value> {
    let settings = state.settings().await?;
    Ok(serde_json::json!({
        "conductor": state.resource_sample("current"),
        "bot": state.probe(&settings.bot_health_url).await,
        "api": state.probe(&settings.api_health_url).await,
        "jobs": state.store()?.list_jobs(12).await?,
        "unavailable": ["backups", "updates"],
    }))
}

#[derive(Deserialize)]
struct TelemetryQuery {
    from_ms: Option<i64>,
    to_ms: Option<i64>,
}

async fn api_telemetry(
    State(state): State<Arc<Conductor>>,
    headers: HeaderMap,
    Query(query): Query<TelemetryQuery>,
) -> Response {
    if let Err(error) = operator(&state, &headers).await {
        return auth_error(error);
    }
    let to = query.to_ms.unwrap_or_else(now_ms);
    let from = query
        .from_ms
        .unwrap_or_else(|| to.saturating_sub(48 * 3600 * 1000));
    let store = match state.store() {
        Ok(store) => store,
        Err(error) => return error_response(StatusCode::SERVICE_UNAVAILABLE, error),
    };
    match store.telemetry_range(from, to, 512).await {
        Ok(rows) => Json(serde_json::json!({ "samples": rows })).into_response(),
        Err(error) => error_response(StatusCode::BAD_REQUEST, error),
    }
}

async fn api_lease(State(state): State<Arc<Conductor>>, headers: HeaderMap) -> Response {
    let operator = match operator(&state, &headers).await {
        Ok(op) => op,
        Err(error) => return auth_error(error),
    };
    match state.settings().await {
        Ok(settings) => {
            let until = now_ms()
                + i64::try_from(settings.lease_secs.saturating_mul(1000)).unwrap_or(30_000);
            let store = match state.store() {
                Ok(store) => store,
                Err(error) => return error_response(StatusCode::SERVICE_UNAVAILABLE, error),
            };
            if let Err(error) = store.put_lease(&operator.session_id, until).await {
                return error_response(StatusCode::BAD_REQUEST, error);
            }
            Json(serde_json::json!({ "until_ms": until, "live": true })).into_response()
        }
        Err(error) => error_response(StatusCode::BAD_REQUEST, error),
    }
}

async fn api_jobs(State(state): State<Arc<Conductor>>, headers: HeaderMap) -> Response {
    if let Err(error) = operator(&state, &headers).await {
        return auth_error(error);
    }
    let store = match state.store() {
        Ok(store) => store,
        Err(error) => return error_response(StatusCode::SERVICE_UNAVAILABLE, error),
    };
    match store.list_jobs(50).await {
        Ok(jobs) => Json(serde_json::json!({ "jobs": jobs })).into_response(),
        Err(error) => error_response(StatusCode::BAD_REQUEST, error),
    }
}

async fn api_retry_job(
    State(state): State<Arc<Conductor>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let operator = match operator(&state, &headers).await {
        Ok(op) => op,
        Err(error) => return auth_error(error),
    };
    let store = match state.store() {
        Ok(store) => store,
        Err(error) => return error_response(StatusCode::SERVICE_UNAVAILABLE, error),
    };
    match store.replay_job(&id).await {
        Ok(true) => {
            let _ = store.audit(&operator.operator_id, "job_retry", &id).await;
            Json(serde_json::json!({ "ok": true, "id": id })).into_response()
        }
        Ok(false) => error_response(
            StatusCode::BAD_REQUEST,
            anyhow::anyhow!("job is not failed or does not exist"),
        ),
        Err(error) => error_response(StatusCode::BAD_REQUEST, error),
    }
}

async fn api_notify_test(State(state): State<Arc<Conductor>>, headers: HeaderMap) -> Response {
    let operator = match operator(&state, &headers).await {
        Ok(op) => op,
        Err(error) => return auth_error(error),
    };
    match enqueue_notify_test(&state, &operator).await {
        Ok(id) => Json(serde_json::json!({ "id": id, "state": "queued" })).into_response(),
        Err(error) => error_response(StatusCode::BAD_REQUEST, error),
    }
}

async fn enqueue_notify_test(state: &Conductor, operator: &Operator) -> anyhow::Result<String> {
    let id = state
        .store()?
        .insert_job(
            "notify_test",
            &serde_json::json!({
                "category": "health",
                "message": "jModGuard conductor test notification",
                "severity": "info",
            }),
            5,
        )
        .await?;
    state
        .store()?
        .audit(&operator.operator_id, "notify_test", &id)
        .await?;
    Ok(id)
}

async fn api_notify_health(State(state): State<Arc<Conductor>>, headers: HeaderMap) -> Response {
    if let Err(error) = operator(&state, &headers).await {
        return auth_error(error);
    }
    let store = match state.store() {
        Ok(store) => store,
        Err(error) => return error_response(StatusCode::SERVICE_UNAVAILABLE, error),
    };
    match store.recent_notify_receipts(20).await {
        Ok(receipts) => Json(serde_json::json!({ "receipts": receipts })).into_response(),
        Err(error) => error_response(StatusCode::BAD_REQUEST, error),
    }
}

async fn operator(state: &Conductor, headers: &HeaderMap) -> anyhow::Result<Operator> {
    state
        .require_operator(headers.get(COOKIE).and_then(|v| v.to_str().ok()))
        .await
}

fn auth_error(error: anyhow::Error) -> Response {
    let status = if error.to_string().contains("not authenticated")
        || error.to_string().contains("expired")
    {
        StatusCode::UNAUTHORIZED
    } else {
        StatusCode::FORBIDDEN
    };
    (
        status,
        Json(serde_json::json!({ "error": error.to_string() })),
    )
        .into_response()
}

fn error_response(status: StatusCode, error: anyhow::Error) -> Response {
    (
        status,
        Json(serde_json::json!({ "error": format!("{error:#}") })),
    )
        .into_response()
}

fn error_page(msg: &str) -> Response {
    let body = format!(
        "<html><body style='font-family:sans-serif;padding:40px'><h2>jModGuard conductor</h2><p>{}</p><p><a href='/login'>Try again</a></p></body></html>",
        msg.replace('<', "&lt;").replace('>', "&gt;")
    );
    (StatusCode::BAD_REQUEST, body).into_response()
}

fn cookie_value(header: Option<&str>, name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    for part in header.unwrap_or("").split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix(&prefix) {
            return Some(value.to_string());
        }
    }
    None
}

fn append_cookie(response: &mut Response, name: &str, value: &str, secure: bool, max_age: i64) {
    let secure = if secure { "; Secure" } else { "" };
    let value =
        format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}");
    if let Ok(header) = HeaderValue::from_str(&value) {
        response.headers_mut().append(SET_COOKIE, header);
    }
}

fn clear_cookie(response: &mut Response, name: &str, secure: bool) {
    append_cookie(response, name, "", secure, 0);
}
