//! WebUI: Discord OAuth2 login, session handling, and the REST API.

pub mod api;

use anyhow::Result;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use std::sync::Arc;
use tower_sessions::cookie::time::Duration as CookieDuration;
use tower_sessions::{Expiry, MemoryStore, Session, SessionManagerLayer};

use crate::db;
use crate::state::{AppState, DiscordGuild, DiscordUser};

const SESSION_USER: &str = "discord_user_id";
const SESSION_TOKEN: &str = "access_token";
const SESSION_REFRESH: &str = "refresh_token";
const SESSION_STATE: &str = "oauth_state";

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

pub struct ApiError(pub anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({ "error": format!("{:#}", self.0) });
        (StatusCode::BAD_REQUEST, Json(body)).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}

pub type ApiResult<T> = std::result::Result<T, ApiError>;

// ---------------------------------------------------------------------------
// session helpers
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AuthCtx {
    pub user: DiscordUser,
    pub guilds: Vec<DiscordGuild>,
}

impl AuthCtx {
    pub fn manageable_guilds(&self) -> Vec<&DiscordGuild> {
        self.guilds.iter().filter(|g| g.manage_permission()).collect()
    }

    pub fn can_manage(&self, guild_id: u64) -> bool {
        self.manageable_guilds().iter().any(|g| g.id == guild_id.to_string())
    }
}

/// Loads the authenticated Discord user + guilds, refreshing the token if
/// needed. Guild/user data is cached in-memory for 60s to limit API calls.
pub async fn require_auth(app: &Arc<AppState>, session: &Session) -> ApiResult<AuthCtx> {
    let Some(user_id) = session.get::<u64>(SESSION_USER).await.ok().flatten() else {
        return Err(ApiError(anyhow::anyhow!("not authenticated")));
    };
    let token = session
        .get::<String>(SESSION_TOKEN)
        .await
        .ok()
        .flatten()
        .ok_or_else(|| ApiError(anyhow::anyhow!("session expired")))?;

    let now = std::time::Instant::now();
    if let Some(cached) = app.guild_cache.get(&user_id) {
        let (fetched_at, guilds) = cached.value();
        if fetched_at.elapsed() < std::time::Duration::from_secs(60) {
            let user = match app.user_cache.get(&user_id) {
                Some(u) => {
                    let (t, u) = u.value();
                    if t.elapsed() < std::time::Duration::from_secs(60) {
                        Some(u.clone())
                    } else {
                        None
                    }
                }
                None => None,
            };
            let user = match user {
                Some(u) => u,
                None => app.oauth.me(&token).await.map_err(ApiError)?,
            };
            return Ok(AuthCtx { user, guilds: guilds.clone() });
        }
    }

    let (user, guilds) = match fetch_user_and_guilds(app, &token).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("webui auth: token fetch failed for user {user_id}: {e:#}; trying refresh");
            // try once with a refreshed token
            let refresh = session
                .get::<String>(SESSION_REFRESH)
                .await
                .ok()
                .flatten()
                .ok_or_else(|| ApiError(anyhow::anyhow!("session expired")))?;
            let tok = app
                .oauth
                .refresh(&refresh)
                .await
                .map_err(ApiError)?;
            let _ = session.insert(SESSION_TOKEN, &tok.access_token).await;
            if let Some(r) = &tok.refresh_token {
                let _ = session.insert(SESSION_REFRESH, r).await;
            }
            match fetch_user_and_guilds(app, &tok.access_token).await {
                Ok(v) => v,
                Err(e2) => {
                    tracing::warn!("webui auth: refresh fetch failed for user {user_id}: {e2:#}");
                    return Err(ApiError(e2));
                }
            }
        }
    };

    app.guild_cache.insert(user_id, (now, guilds.clone()));
    app.user_cache.insert(user_id, (now, user.clone()));
    Ok(AuthCtx { user, guilds })
}

async fn fetch_user_and_guilds(
    app: &Arc<AppState>,
    token: &str,
) -> Result<(DiscordUser, Vec<DiscordGuild>)> {
    let user = app.oauth.me(token).await?;
    let guilds = app.oauth.my_guilds(token).await?;
    Ok((user, guilds))
}

// ---------------------------------------------------------------------------
// oauth flow
// ---------------------------------------------------------------------------

async fn login(session: Session, state: axum::extract::State<Arc<AppState>>) -> Response {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let state_token = format!("{nanos:x}-{}", std::process::id());
    let _ = session.insert(SESSION_STATE, &state_token).await;
    let url = state.oauth.authorize_url(&state_token);
    axum::response::Redirect::to(&url).into_response()
}

async fn logout(session: Session) -> Response {
    let _ = session.delete().await;
    axum::response::Redirect::to("/").into_response()
}

async fn callback(
    session: Session,
    state: axum::extract::State<Arc<AppState>>,
    query: axum::extract::Query<CallbackQuery>,
) -> Response {
    let expected = session.get::<String>(SESSION_STATE).await.ok().flatten().unwrap_or_default();
    if !expected.is_empty() && Some(expected.as_str()) != query.state.as_deref() {
        return error_page("OAuth state mismatch");
    }
    let Some(code) = &query.code else {
        return error_page("missing authorization code");
    };
    let tok = match state.oauth.exchange_code(code).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("oauth callback: token exchange failed: {e:#}");
            return error_page(&format!("token exchange failed: {e:#}"));
        }
    };
    let user = match state.oauth.me(&tok.access_token).await {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!("oauth callback: me request failed: {e:#}");
            return error_page(&format!("failed to fetch user: {e:#}"));
        }
    };
    let uid = user.id.parse::<u64>().unwrap_or(0);
    let _ = session.insert(SESSION_USER, uid).await;
    let _ = session.insert(SESSION_TOKEN, &tok.access_token).await;
    if let Some(r) = &tok.refresh_token {
        let _ = session.insert(SESSION_REFRESH, r).await;
    }
    let _: Option<String> = session.remove(SESSION_STATE).await.ok().flatten();
    axum::response::Redirect::to("/").into_response()
}

#[derive(serde::Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    #[allow(dead_code)]
    error: Option<String>,
}

fn error_page(msg: &str) -> Response {
    let body = format!(
        "<html><body style='font-family:monospace;padding:40px'><h2>jModGuard login error</h2><p>{}</p><p><a href='/'>Go back</a></p></body></html>",
        msg.replace('<', "&lt;").replace('>', "&gt;")
    );
    (StatusCode::BAD_REQUEST, body).into_response()
}

// ---------------------------------------------------------------------------
// api/me + static
// ---------------------------------------------------------------------------

async fn api_me(
    state: axum::extract::State<Arc<AppState>>,
    session: Session,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = require_auth(&state, &session).await?;
    let guilds: Vec<serde_json::Value> = auth
        .guilds
        .iter()
        .filter(|g| g.manage_permission())
        .map(|g| {
            serde_json::json!({
                "id": g.id,
                "name": g.name,
                "icon": g.icon,
                "manage": true,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({
        "user": {
            "id": auth.user.id,
            "username": auth.user.username,
            "global_name": auth.user.global_name,
            "avatar_url": auth.user.avatar_url(),
        },
        "guilds": guilds,
    })))
}

fn static_handler(content: &'static str, content_type: &'static str) -> impl IntoResponse {
    (
        [
            ("content-type", content_type),
            ("cache-control", "no-cache"),
        ],
        content,
    )
}

async fn index() -> impl IntoResponse {
    static_handler(include_str!("static/index.html"), "text/html; charset=utf-8")
}
async fn app_js() -> impl IntoResponse {
    static_handler(include_str!("static/app.js"), "application/javascript; charset=utf-8")
}
async fn style_css() -> impl IntoResponse {
    static_handler(include_str!("static/style.css"), "text/css; charset=utf-8")
}

// ---------------------------------------------------------------------------
// router
// ---------------------------------------------------------------------------

pub fn router(app: Arc<AppState>) -> Router {
    let session_store = MemoryStore::default();
    let session_layer = SessionManagerLayer::new(session_store)
        .with_secure(false)
        .with_expiry(Expiry::OnInactivity(CookieDuration::days(7)));

    Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        .route("/login", get(login))
        .route("/logout", get(logout))
        .route("/oauth/callback", get(callback))
        .route("/api/me", get(api_me))
        .route("/api/guilds/{guild_id}/config", get(api::guild_config))
        .route("/api/guilds/{guild_id}/settings", axum::routing::put(api::update_settings))
        .route("/api/guilds/{guild_id}/rules/{rule_id}", get(api::get_rule).put(api::put_rule).delete(api::delete_rule))
        .route("/api/guilds/{guild_id}/badwords", get(api::list_badwords).post(api::add_badword))
        .route("/api/guilds/{guild_id}/badwords/{word_id}", axum::routing::delete(api::delete_badword))
        .route("/api/guilds/{guild_id}/commands", get(api::list_commands).post(api::create_command))
        .route("/api/guilds/{guild_id}/commands/{name}", get(api::get_command).put(api::put_command).delete(api::delete_command))
        .route("/api/guilds/{guild_id}/templates", get(api::list_templates))
        .route("/api/guilds/{guild_id}/templates/{event}", get(api::get_template).put(api::put_template).delete(api::delete_template))
        .route("/api/guilds/{guild_id}/logchannels", get(api::list_logchannels))
        .route("/api/guilds/{guild_id}/logchannels/{event}", get(api::get_logchannel).put(api::put_logchannel).delete(api::delete_logchannel))
        .route("/api/guilds/{guild_id}/copy", axum::routing::post(api::copy_config))
        .route("/api/guilds/{guild_id}/testregex", axum::routing::post(api::test_regex))
        .route("/api/guilds/{guild_id}/members", get(api::members))
        .route("/api/guilds/{guild_id}/events", get(api::events))
        .route("/api/guilds/{guild_id}/bans", get(api::bans))
        .route("/api/guilds/{guild_id}/unban", axum::routing::post(api::unban))
        .route("/api/guilds/{guild_id}/stats", get(api::stats))
        .layer(session_layer)
        .with_state(app)
}

/// Parses a guild id path segment, checking the session can manage it.
pub async fn guild_scope(
    app: &Arc<AppState>,
    session: &Session,
    guild_id: u64,
) -> ApiResult<AuthCtx> {
    let auth = require_auth(app, session).await?;
    if !auth.can_manage(guild_id) {
        return Err(ApiError(anyhow::anyhow!(
            "you cannot manage this server (needs MANAGE_GUILD permission)"
        )));
    }
    let _ = db::ensure_guild(&app.db, guild_id as i64).await;
    Ok(auth)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn test_app() -> Router {
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let tmp = std::env::temp_dir().join(format!(
            "jmodguard_web_{}_{}.db",
            std::process::id(),
            n
        ));
        let db = crate::db::Db::open(&tmp, 2).unwrap();
        let app = crate::state::AppState::new(Default::default(), db).unwrap();
        std::fs::remove_file(&tmp).ok();
        router(app)
    }

    #[tokio::test]
    async fn static_pages_serve() {
        let app = test_app();
        for path in ["/", "/app.js", "/style.css"] {
            let res = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK, "GET {path}");
        }
    }

    #[tokio::test]
    async fn api_me_requires_auth() {
        let app = test_app();
        let res = app
            .oneshot(Request::builder().uri("/api/me").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn login_redirects_to_discord() {
        let app = test_app();
        let res = app
            .oneshot(Request::builder().uri("/login").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let location = res.headers().get("location").unwrap().to_str().unwrap();
        assert!(location.starts_with("https://discord.com/oauth2/authorize"));
    }
}
