//! Owner-only read-only permission preview. Preview never inherits owner write authority.
use anyhow::anyhow;
use axum::Json;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower_sessions::Session;

use super::oauth::PermissionsField;
use super::state::ApiState;
use super::{ApiError, ApiResult, AuthCtx, authorize_manage_guild, is_bot_owner, require_auth};

pub const PREVIEW_HEADER: &str = "x-jmodguard-preview";
const PREVIEW_TTL: Duration = Duration::from_secs(3600);

tokio::task_local! {
    static PREVIEW: Option<PreviewGrant>;
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PreviewProfile {
    Administrator,
    Moderator,
    Member,
}

impl PreviewProfile {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "administrator" => Some(Self::Administrator),
            "moderator" => Some(Self::Moderator),
            "member" => Some(Self::Member),
            _ => None,
        }
    }

    fn permissions(self) -> u64 {
        const ADMINISTRATOR: u64 = 1 << 3;
        const BAN_MEMBERS: u64 = 1 << 2;
        const MANAGE_GUILD: u64 = 1 << 5;
        match self {
            Self::Administrator => ADMINISTRATOR | BAN_MEMBERS | MANAGE_GUILD,
            Self::Moderator => BAN_MEMBERS | MANAGE_GUILD,
            Self::Member => 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PreviewGrant {
    pub token: String,
    pub actor_id: u64,
    pub guild_id: u64,
    pub profile: PreviewProfile,
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
    let profile = PreviewProfile::parse(body.profile.as_deref().unwrap_or("administrator"))
        .ok_or_else(|| ApiError(anyhow!("unsupported preview profile")))?;
    prune_expired(&app);
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
            profile,
            expires: Instant::now() + PREVIEW_TTL,
        },
    );
    schedule_expiration(&app, token.clone());
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
    let grant = presented
        .as_ref()
        .and_then(|token| active_grant(&app, token));
    if presented.is_some() && grant.is_none() {
        return (
            StatusCode::GONE,
            Json(serde_json::json!({
                "error": "preview expired",
                "code": "preview_expired",
            })),
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

fn active_grant(app: &ApiState, token: &str) -> Option<PreviewGrant> {
    let grant = app.preview.get(token).map(|entry| entry.clone())?;
    if grant.expires > Instant::now() {
        return Some(grant);
    }
    app.preview.remove(token);
    None
}

fn prune_expired(app: &ApiState) {
    let now = Instant::now();
    app.preview.retain(|_, grant| grant.expires > now);
}

fn schedule_expiration(app: &Arc<ApiState>, token: String) {
    let app = Arc::downgrade(app);
    tokio::spawn(async move {
        tokio::time::sleep(PREVIEW_TTL).await;
        let Some(app) = app.upgrade() else {
            return;
        };
        let expired = app
            .preview
            .get(&token)
            .is_some_and(|grant| grant.expires <= Instant::now());
        if expired {
            app.preview.remove(&token);
        }
    });
}

fn is_preview_write(request: &Request<axum::body::Body>) -> bool {
    match request.method() {
        &axum::http::Method::POST => !is_preview_post_read(request.uri().path()),
        &axum::http::Method::PUT | &axum::http::Method::PATCH | &axum::http::Method::DELETE => true,
        _ => false,
    }
}

/// SPA JSON reads use POST so a CDN cannot reuse GET. Those reads stay visible
/// in a preview tab. Note creation, unban, and other POST mutations do not.
fn is_preview_post_read(path: &str) -> bool {
    if path == "/api/me" {
        return true;
    }
    let mut parts = path.split('/').filter(|part| !part.is_empty());
    if parts.next() != Some("api") || parts.next() != Some("guilds") {
        return false;
    }
    let Some(guild) = parts.next() else {
        return false;
    };
    if guild == "0" || !guild.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (
            Some(
                "config"
                    | "case-delete-roles"
                    | "actions"
                    | "events"
                    | "bans"
                    | "stats"
                    | "cases"
                    | "members"
                    | "member-sync"
            ),
            None,
            None,
            None
        ) | (Some("cases"), Some(_), None, None)
            | (Some("cases"), Some(_), Some("evidence"), None)
            | (Some("members"), Some(_), None, None)
            | (Some("members"), Some(_), Some("history"), None)
            | (Some("logchannels"), Some(_), None, None)
    )
}

pub fn apply_overlay(auth: &mut AuthCtx) -> Result<(), ApiError> {
    let Some(preview) = current() else {
        return Ok(());
    };
    if auth.discord_user_id() != preview.actor_id {
        return Err(ApiError(anyhow::Error::new(super::NotAuthenticated)));
    }
    let selected = preview.guild_id.to_string();
    let mut guild = auth
        .guilds
        .iter()
        .find(|guild| guild.id == selected && guild.manage_permission())
        .cloned()
        .ok_or_else(|| {
            ApiError(anyhow!(
                "you cannot manage this server (needs MANAGE_GUILD permission)"
            ))
        })?;
    // Route admission above is based on the live Discord guild. The synthetic
    // mask below controls only what this read-only preview may observe.
    guild.owner = false;
    guild.permissions = Some(PermissionsField::Number(preview.profile.permissions()));
    auth.guilds = vec![guild];
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_preview_post_read;

    #[test]
    fn preview_allows_spa_post_reads_and_rejects_post_mutations() {
        for path in [
            "/api/me",
            "/api/guilds/10/config",
            "/api/guilds/10/case-delete-roles",
            "/api/guilds/10/actions",
            "/api/guilds/10/events",
            "/api/guilds/10/bans",
            "/api/guilds/10/stats",
            "/api/guilds/10/cases",
            "/api/guilds/10/cases/abc",
            "/api/guilds/10/cases/abc/evidence",
            "/api/guilds/10/members",
            "/api/guilds/10/members/123",
            "/api/guilds/10/members/123/history",
            "/api/guilds/10/member-sync",
            "/api/guilds/10/logchannels/message_edit",
        ] {
            assert!(is_preview_post_read(path), "{path}");
        }
        for path in [
            "/api/preview",
            "/api/guilds/0/config",
            "/api/guilds/10/unban",
            "/api/guilds/10/badwords",
            "/api/guilds/10/commands",
            "/api/guilds/10/copy",
            "/api/guilds/10/cases/abc/history",
            "/api/guilds/10/cases/abc/evidence/file",
            "/api/guilds/10/actions/job",
            "/api/guilds/10/member-sync/actions",
        ] {
            assert!(!is_preview_post_read(path), "{path}");
        }
    }
}
