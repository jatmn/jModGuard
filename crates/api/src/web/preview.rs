//! Owner-only read-only permission preview. Preview never inherits owner write authority.
use anyhow::anyhow;
use axum::Json;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower_sessions::Session;

use super::state::ApiState;
use super::{ApiError, ApiResult, AuthCtx, authorize_manage_guild, is_bot_owner, require_auth};

pub const PREVIEW_HEADER: &str = "x-jmodguard-preview";
const PREVIEW_TTL: Duration = Duration::from_secs(3600);

tokio::task_local! {
    static PREVIEW: Option<PreviewGrant>;
}

#[derive(Clone, Debug)]
pub struct PreviewGrant {
    pub token: String,
    pub actor_id: u64,
    pub guild_id: u64,
    pub profile: String,
    pub expires: Instant,
}

#[derive(Debug)]
pub struct PreviewDenied;
impl std::fmt::Display for PreviewDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("preview is read-only")
    }
}
impl std::error::Error for PreviewDenied {}

pub fn current() -> Option<PreviewGrant> {
    PREVIEW.try_with(|preview| preview.clone()).ok().flatten()
}

#[derive(Deserialize)]
pub struct LaunchBody {
    pub guild_id: String,
    pub profile: Option<String>,
}

pub async fn launch(
    State(app): State<Arc<ApiState>>,
    session: Session,
    Json(body): Json<LaunchBody>,
) -> ApiResult<Json<serde_json::Value>> {
    if current().is_some() {
        return Err(ApiError(PreviewDenied.into()));
    }
    let auth = require_auth(&app, &session).await?;
    if !is_bot_owner(&app, &auth) {
        return Err(ApiError(anyhow!(
            "preview requires the configured bot owner"
        )));
    }
    let guild_id: u64 = body
        .guild_id
        .parse()
        .map_err(|_| anyhow!("invalid guild id"))?;
    authorize_manage_guild(&app, &session, guild_id).await?;
    let profile = match body.profile.as_deref().unwrap_or("administrator") {
        "administrator" | "moderator" | "member" => {
            body.profile.unwrap_or_else(|| "administrator".into())
        }
        _ => return Err(ApiError(anyhow!("unsupported preview profile"))),
    };
    let token = format!(
        "{:032x}{:032x}",
        rand::random::<u128>(),
        rand::random::<u128>()
    );
    app.preview.insert(
        token.clone(),
        PreviewGrant {
            token: token.clone(),
            actor_id: auth.discord_user_id(),
            guild_id,
            profile: profile.clone(),
            expires: Instant::now() + PREVIEW_TTL,
        },
    );
    Ok(Json(serde_json::json!({
        "token": token,
        "guild_id": guild_id.to_string(),
        "profile": profile,
        "expires_in": PREVIEW_TTL.as_secs(),
    })))
}

pub async fn exit(State(app): State<Arc<ApiState>>) -> ApiResult<Json<serde_json::Value>> {
    if let Some(preview) = current() {
        app.preview.remove(&preview.token);
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn middleware(
    State(app): State<Arc<ApiState>>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let presented = request
        .headers()
        .get(PREVIEW_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let grant = presented.as_ref().and_then(|token| {
        app.preview.get(token).and_then(|entry| {
            let grant = entry.clone();
            grant
                .expires
                .checked_duration_since(Instant::now())
                .map(|_| grant)
        })
    });
    if presented.is_some() && grant.is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "preview expired" })),
        )
            .into_response();
    }
    if grant.is_some() && is_preview_write(&request) && request.uri().path() != "/api/preview" {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "preview is read-only",
                "code": "preview_read_only",
            })),
        )
            .into_response();
    }
    PREVIEW.scope(grant, next.run(request)).await
}

fn is_preview_write(request: &Request<axum::body::Body>) -> bool {
    matches!(
        request.method(),
        &axum::http::Method::POST
            | &axum::http::Method::PUT
            | &axum::http::Method::PATCH
            | &axum::http::Method::DELETE
    )
}

pub fn apply_overlay(auth: &mut AuthCtx) -> Result<(), ApiError> {
    let Some(preview) = current() else {
        return Ok(());
    };
    if auth.discord_user_id() != preview.actor_id {
        return Err(ApiError(anyhow!("not authenticated")));
    }
    let selected = preview.guild_id.to_string();
    auth.guilds.retain(|guild| guild.id == selected);
    if auth.guilds.is_empty() {
        return Err(ApiError(anyhow!(
            "you cannot manage this server (needs MANAGE_GUILD permission)"
        )));
    }
    Ok(())
}
