//! Conductor HTTP surface. Guild API cookies never authenticate this origin.
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE, COOKIE, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

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
        "backup_available": false,
        "capture_available": false,
        "moves_available": false,
        "update_available": false,
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
        .route("/api/backups", get(api_backups).post(api_backup_create))
        .route("/api/backups/upload", post(api_backup_upload))
        .route("/api/backups/{id}", get(api_backup_one))
        .route("/api/backups/{id}/download", get(api_backup_download))
        .route("/api/backups/{id}/portable", post(api_backup_portable))
        .route("/api/backups/{id}/restore", post(api_backup_restore))
        .route("/api/moves", get(api_moves).post(api_move_start))
        .route("/api/moves/plan", get(api_move_plan))
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
            backup_root: None,
            backup_key: None,
            capture_stores: Vec::new(),
            evidence_root: None,
            restore_target: None,
            guild_stores: Vec::new(),
        },
        store: None,
        backup: None,
        http: reqwest::Client::new(),
        started: std::time::Instant::now(),
        restore_passwords: std::sync::Mutex::new(std::collections::HashMap::new()),
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
        "backup_available": state.backup_available(),
        "capture_available": state.capture_available(),
        "moves_available": crate::guild_move::moves_available(&state.cfg),
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
        Ok((session_id, absolute_secs)) => {
            let mut response = Redirect::to("/").into_response();
            append_cookie(
                &mut response,
                SESSION_COOKIE,
                &session_id,
                state.cfg.cookie_secure(),
                i64::try_from(absolute_secs).unwrap_or(i64::MAX),
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
        "backup_available": state.backup_available(),
        "capture_available": state.capture_available(),
        "moves_available": crate::guild_move::moves_available(&state.cfg),
        "unavailable": ["updates"],
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

#[allow(clippy::result_large_err)] // Keeps handler call sites on their native response type.
async fn require_backup(state: &Conductor) -> Result<&crate::backup::BackupZone, Response> {
    state
        .backup_zone()
        .map_err(|error| error_response(StatusCode::SERVICE_UNAVAILABLE, error))
}

async fn api_backups(State(state): State<Arc<Conductor>>, headers: HeaderMap) -> Response {
    if let Err(error) = operator(&state, &headers).await {
        return auth_error(error);
    }
    if let Err(error) = require_backup(&state).await {
        return error;
    }
    let store = match state.store() {
        Ok(store) => store,
        Err(error) => return error_response(StatusCode::SERVICE_UNAVAILABLE, error),
    };
    let settings = match state.settings().await {
        Ok(settings) => settings,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error),
    };
    let failed_jobs = match store.list_jobs(50).await {
        Ok(jobs) => jobs
            .into_iter()
            .filter(|job| job.kind.starts_with("backup_") && job.status == "failed")
            .collect::<Vec<_>>(),
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error),
    };
    match store.list_archives(100).await {
        Ok(archives) => {
            let last = store.last_complete_ms().await.ok().flatten();
            let last_attempt = store
                .last_backup_schedule_activity_ms()
                .await
                .ok()
                .flatten();
            Json(serde_json::json!({
                "archives": archives.into_iter().filter(|row| row.status == "complete" || row.status == "failed" || row.status == "quarantined").collect::<Vec<_>>(),
                "failed_jobs": failed_jobs,
                "last_complete_ms": last,
                "overdue": state.capture_available()
                    && crate::backup::overdue(&settings, last_attempt),
                "backup_enabled": settings.backup_enabled,
                "interval_hours": settings.backup_interval_hours,
                "key_from_env": state.cfg.backup_key.is_some(),
                "capture_available": state.capture_available(),
            }))
            .into_response()
        }
        Err(error) => error_response(StatusCode::BAD_REQUEST, error),
    }
}

#[derive(Deserialize)]
struct BackupCreate {
    kind: Option<String>,
}

async fn api_backup_create(
    State(state): State<Arc<Conductor>>,
    headers: HeaderMap,
    Json(body): Json<BackupCreate>,
) -> Response {
    let operator = match operator(&state, &headers).await {
        Ok(op) => op,
        Err(error) => return auth_error(error),
    };
    if let Err(error) = require_backup(&state).await {
        return error;
    }
    if !state.capture_available() {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            anyhow::anyhow!("capture requires a configured core store"),
        );
    }
    let kind = match body.kind.as_deref().unwrap_or("manual") {
        kind @ ("manual" | "checkpoint") => kind,
        _ => {
            return error_response(
                StatusCode::BAD_REQUEST,
                anyhow::anyhow!("unsupported backup kind"),
            );
        }
    };
    match enqueue_backup(&state, &operator, kind).await {
        Ok(id) => Json(serde_json::json!({ "id": id, "state": "queued" })).into_response(),
        Err(error) => error_response(StatusCode::BAD_REQUEST, error),
    }
}

async fn enqueue_backup(
    state: &Conductor,
    operator: &Operator,
    kind: &str,
) -> anyhow::Result<String> {
    let id = state
        .store()?
        .insert_backup_job_if_idle("backup_capture", &serde_json::json!({ "kind": kind }), 3)
        .await?
        .ok_or_else(|| anyhow::anyhow!("a backup or restore job is already running"))?;
    state
        .store()?
        .audit(&operator.operator_id, "backup_create", &id)
        .await?;
    Ok(id)
}

async fn api_backup_one(
    State(state): State<Arc<Conductor>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(error) = operator(&state, &headers).await {
        return auth_error(error);
    }
    match state.store() {
        Ok(store) => match store.get_archive(&id).await {
            Ok(Some(row)) => Json(serde_json::json!({ "archive": row })).into_response(),
            Ok(None) => error_response(StatusCode::NOT_FOUND, anyhow::anyhow!("backup not found")),
            Err(error) => error_response(StatusCode::BAD_REQUEST, error),
        },
        Err(error) => error_response(StatusCode::SERVICE_UNAVAILABLE, error),
    }
}

async fn api_backup_download(
    State(state): State<Arc<Conductor>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(error) = operator(&state, &headers).await {
        return auth_error(error);
    }
    let zone = match require_backup(&state).await {
        Ok(zone) => zone,
        Err(error) => return error,
    };
    let row = match state.store() {
        Ok(store) => match store.get_archive(&id).await {
            Ok(Some(row)) if row.status == "complete" => row,
            Ok(_) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    anyhow::anyhow!("backup is not complete"),
                );
            }
            Err(error) => return error_response(StatusCode::BAD_REQUEST, error),
        },
        Err(error) => return error_response(StatusCode::SERVICE_UNAVAILABLE, error),
    };
    stream_file(
        zone.complete_path(&row.id),
        &format!("jmodguard-{id}.jmb"),
        None,
    )
    .await
}

#[derive(Deserialize)]
struct PortableBody {
    password: String,
}

async fn api_backup_portable(
    State(state): State<Arc<Conductor>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<PortableBody>,
) -> Response {
    if let Err(error) = operator(&state, &headers).await {
        return auth_error(error);
    }
    let zone = match require_backup(&state).await {
        Ok(zone) => zone,
        Err(error) => return error,
    };
    match crate::backup::wrap_portable_archive(
        zone,
        match state.store() {
            Ok(store) => store,
            Err(error) => return error_response(StatusCode::SERVICE_UNAVAILABLE, error),
        },
        &state.cfg,
        &id,
        &body.password,
    )
    .await
    {
        Ok(path) => {
            let cleanup = path.parent().map(std::path::Path::to_path_buf);
            stream_file(path, &format!("jmodguard-{id}-portable.jmb"), cleanup).await
        }
        Err(error) => error_response(StatusCode::BAD_REQUEST, error),
    }
}

#[derive(Deserialize)]
struct RestoreBody {
    password: Option<String>,
    acknowledge_historical: Option<bool>,
}

async fn api_backup_restore(
    State(state): State<Arc<Conductor>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<RestoreBody>,
) -> Response {
    let operator = match operator(&state, &headers).await {
        Ok(op) => op,
        Err(error) => return auth_error(error),
    };
    if let Err(error) = require_backup(&state).await {
        return error;
    }
    if state.cfg.restore_target.is_none() {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            anyhow::anyhow!("JMODGUARD_CONDUCTOR_RESTORE_TARGET is required for restore"),
        );
    }
    match enqueue_restore(&state, &operator, &id, body).await {
        Ok(job) => Json(serde_json::json!({ "id": job, "state": "queued" })).into_response(),
        Err(error) => error_response(StatusCode::BAD_REQUEST, error),
    }
}

fn restore_password_for_enqueue(password: Option<String>) -> anyhow::Result<Option<String>> {
    match password {
        None => Ok(None),
        Some(value) => {
            let trimmed = value.trim();
            anyhow::ensure!(!trimmed.is_empty(), "export password must not be empty");
            Ok(Some(trimmed.to_string()))
        }
    }
}

async fn enqueue_restore(
    state: &Conductor,
    operator: &Operator,
    id: &str,
    body: RestoreBody,
) -> anyhow::Result<String> {
    let job = random_token();
    let password = restore_password_for_enqueue(body.password)?;
    let password_required = password.is_some();
    state.remember_restore_password(&job, password)?;
    let inserted = match state
        .store()?
        .insert_backup_job_with_id_if_idle(
            &job,
            "backup_restore",
            &serde_json::json!({
                "id": id,
                "password_required": password_required,
                "acknowledge_historical": body.acknowledge_historical.unwrap_or(false),
            }),
            1,
        )
        .await
    {
        Ok(inserted) => inserted,
        Err(error) => {
            state.forget_restore_password(&job);
            return Err(error);
        }
    };
    if !inserted {
        state.forget_restore_password(&job);
        anyhow::bail!("a backup or restore job is already running");
    }
    state
        .store()?
        .audit(&operator.operator_id, "backup_restore", id)
        .await?;
    Ok(job)
}

async fn api_backup_upload(
    State(state): State<Arc<Conductor>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if let Err(error) = operator(&state, &headers).await {
        return auth_error(error);
    }
    let zone = match require_backup(&state).await {
        Ok(zone) => zone,
        Err(error) => return error,
    };
    let settings = match state.settings().await {
        Ok(settings) => settings,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error),
    };
    let (id, file) = match zone.begin_upload() {
        Ok(upload) => upload,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error),
    };
    let mut file = tokio::fs::File::from_std(file);
    let mut stream = body.into_data_stream();
    let mut bytes = 0_u64;
    let mut prefix = Vec::with_capacity(crate::backup::MAGIC.len());
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                zone.remove_quarantine(&id);
                return error_response(StatusCode::BAD_REQUEST, error.into());
            }
        };
        bytes = match bytes.checked_add(chunk.len() as u64) {
            Some(bytes) if bytes <= settings.backup_max_archive_bytes => bytes,
            _ => {
                zone.remove_quarantine(&id);
                return error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    anyhow::anyhow!("upload exceeds configured byte budget"),
                );
            }
        };
        if prefix.len() < crate::backup::MAGIC.len() {
            let needed = crate::backup::MAGIC.len() - prefix.len();
            prefix.extend_from_slice(&chunk[..chunk.len().min(needed)]);
        }
        if let Err(error) = file.write_all(&chunk).await {
            zone.remove_quarantine(&id);
            return error_response(StatusCode::BAD_REQUEST, error.into());
        }
    }
    if prefix != crate::backup::MAGIC {
        zone.remove_quarantine(&id);
        return error_response(
            StatusCode::BAD_REQUEST,
            anyhow::anyhow!("unsupported archive format"),
        );
    }
    if let Err(error) = file.sync_all().await {
        zone.remove_quarantine(&id);
        return error_response(StatusCode::BAD_REQUEST, error.into());
    }
    drop(file);
    let store = match state.store() {
        Ok(store) => store,
        Err(error) => {
            zone.remove_quarantine(&id);
            return error_response(StatusCode::SERVICE_UNAVAILABLE, error);
        }
    };
    if let Err(error) = store
        .insert_archive(&crate::store::ArchiveRow {
            id: id.clone(),
            kind: "upload".into(),
            status: "quarantined".into(),
            created_ms: crate::store::now_ms(),
            captured_ms: None,
            scope_json: "{}".into(),
            object_key: Some(format!("quarantine/{id}.jmb")),
            sha256: None,
            bytes: Some(i64::try_from(bytes).unwrap_or(i64::MAX)),
            family: "unknown".into(),
            last_error: None,
        })
        .await
    {
        zone.remove_quarantine(&id);
        return error_response(StatusCode::BAD_REQUEST, error);
    }
    Json(serde_json::json!({ "id": id, "status": "quarantined" })).into_response()
}

async fn stream_file(
    path: std::path::PathBuf,
    filename: &str,
    cleanup: Option<std::path::PathBuf>,
) -> Response {
    match tokio::fs::File::open(&path).await {
        Ok(file) => {
            if let Some(cleanup) = cleanup
                && let Err(error) = tokio::fs::remove_dir_all(cleanup).await
            {
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.into());
            }
            let body = Body::from_stream(ReaderStream::new(file));
            let disposition = format!("attachment; filename=\"{filename}\"");
            (
                StatusCode::OK,
                [
                    (
                        CONTENT_TYPE,
                        HeaderValue::from_static("application/octet-stream"),
                    ),
                    (
                        CONTENT_DISPOSITION,
                        HeaderValue::from_str(&disposition)
                            .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
                    ),
                ],
                body,
            )
                .into_response()
        }
        Err(error) => {
            if let Some(cleanup) = cleanup
                && let Err(cleanup_error) = tokio::fs::remove_dir_all(cleanup).await
            {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    anyhow::anyhow!(
                        "portable archive open failed: {error}; scratch cleanup failed: {cleanup_error}"
                    ),
                );
            }
            error_response(StatusCode::NOT_FOUND, error.into())
        }
    }
}

#[derive(Deserialize)]
struct MovePlanQuery {
    guild_id: String,
    destination_store_id: String,
}

#[derive(Deserialize)]
struct MoveBody {
    guild_id: String,
    destination_store_id: String,
    #[serde(default)]
    preop: String,
    #[serde(default)]
    confirm_shared_impact: bool,
}

async fn api_moves(State(state): State<Arc<Conductor>>, headers: HeaderMap) -> Response {
    if let Err(error) = operator(&state, &headers).await {
        return auth_error(error);
    }
    let catalog = match crate::guild_move::catalog_view(&state.cfg).await {
        Ok(body) if crate::guild_move::moves_available(&state.cfg) => body,
        Ok(_) => serde_json::json!({ "available": false, "stores": [], "placements": [] }),
        Err(error) if !crate::guild_move::moves_available(&state.cfg) => {
            serde_json::json!({ "available": false, "error": format!("{error:#}") })
        }
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error),
    };
    let jobs = match state.store() {
        Ok(store) => store.list_jobs(50).await.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let moves: Vec<_> = jobs
        .into_iter()
        .filter(|job| job.kind == "guild_move")
        .collect();
    let failed_moves = match state.store() {
        Ok(store) => store.failed_guild_move_jobs().await.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    Json(serde_json::json!({
        "available": crate::guild_move::moves_available(&state.cfg),
        "catalog": catalog,
        "jobs": moves,
        "failed_jobs": failed_moves,
    }))
    .into_response()
}

async fn api_move_plan(
    State(state): State<Arc<Conductor>>,
    headers: HeaderMap,
    Query(query): Query<MovePlanQuery>,
) -> Response {
    if let Err(error) = operator(&state, &headers).await {
        return auth_error(error);
    }
    if !crate::guild_move::moves_available(&state.cfg) {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            anyhow::anyhow!("guild moves are not configured"),
        );
    }
    let guild_id: i64 = match query.guild_id.parse() {
        Ok(id) => id,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                anyhow::anyhow!("guild id must be a Discord snowflake"),
            );
        }
    };
    match crate::guild_move::plan(&state.cfg, guild_id, &query.destination_store_id).await {
        Ok(body) => Json(body).into_response(),
        Err(error) => error_response(StatusCode::BAD_REQUEST, error),
    }
}

async fn api_move_start(
    State(state): State<Arc<Conductor>>,
    headers: HeaderMap,
    Json(body): Json<MoveBody>,
) -> Response {
    let operator = match operator(&state, &headers).await {
        Ok(op) => op,
        Err(error) => return auth_error(error),
    };
    let guild_id: i64 = match body.guild_id.parse() {
        Ok(id) => id,
        Err(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                anyhow::anyhow!("guild id must be a Discord snowflake"),
            );
        }
    };
    match crate::guild_move::enqueue(
        &state,
        crate::guild_move::MoveRequest {
            guild_id,
            destination_store_id: body.destination_store_id,
            preop: body.preop,
            confirm_shared_impact: body.confirm_shared_impact,
        },
    )
    .await
    {
        Ok(id) => match state
            .store()
            .map_err(|error| error_response(StatusCode::SERVICE_UNAVAILABLE, error))
        {
            Ok(store) => {
                if let Err(error) = store.audit(&operator.operator_id, "guild_move", &id).await {
                    return error_response(StatusCode::BAD_REQUEST, error);
                }
                Json(serde_json::json!({ "id": id })).into_response()
            }
            Err(error) => error,
        },
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
