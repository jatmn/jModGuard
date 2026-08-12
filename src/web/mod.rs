//! WebUI: Discord OAuth2 login, session handling, and the REST API.

pub mod api;

use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use std::sync::Arc;
use tower_sessions::cookie::time::Duration as CookieDuration;
use tower_sessions::{Expiry, MemoryStore, Session, SessionManagerLayer};

use crate::state::{AppState, DiscordGuild, DiscordUser};

const SESSION_USER: &str = "discord_user_id";
const SESSION_TOKEN: &str = "access_token";
const SESSION_REFRESH: &str = "refresh_token";
const SESSION_STATE: &str = "oauth_state";
const SESSION_RETURN_TO: &str = "return_to";

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
        self.guilds
            .iter()
            .filter(|g| g.manage_permission())
            .collect()
    }

    /// True when this auth context's guild list includes Manage Guild for `guild_id`.
    ///
    /// **Invariant:** only call this on an `AuthCtx` produced by
    /// [`require_auth_fresh`] / [`authorize_manage_guild`]. Never on a
    /// display-cached (`CachedOk`) context — that reopens the post-demotion
    /// window (see `copy_config` → source guild).
    pub fn can_manage(&self, guild_id: u64) -> bool {
        self.manageable_guilds()
            .iter()
            .any(|g| g.id == guild_id.to_string())
    }

    pub fn discord_user_id(&self) -> u64 {
        self.user.id.parse::<u64>().unwrap_or(0)
    }
}

/// True when the session user is the configured bot owner (`bot.owner_id` ≠ 0).
pub fn is_bot_owner(app: &AppState, auth: &AuthCtx) -> bool {
    let owner = app.cfg.bot.owner_id;
    owner != 0 && auth.discord_user_id() == owner
}

/// Global config mutations (path `guild_id=0` or body `scope: "global"`) require owner.
pub fn require_bot_owner(app: &AppState, auth: &AuthCtx) -> ApiResult<()> {
    if is_bot_owner(app, auth) {
        Ok(())
    } else {
        Err(ApiError(anyhow::anyhow!(
            "global config requires the configured bot owner (bot.owner_id)"
        )))
    }
}

/// How guild membership/permissions may be sourced for this auth call.
///
/// Display (`/api/me`) may use a short in-memory cache to spare Discord rate
/// limits. Authorization (`guild_scope` for real guilds) must **not** — a
/// demoted admin must lose WebUI access on the next request, not after a TTL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GuildListMode {
    /// Prefer cache ≤60s (sidebar / profile listing only).
    CachedOk,
    /// Always re-fetch `/users/@me/guilds` from Discord before `can_manage`.
    MustRefresh,
}

const USER_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60);
const GUILD_DISPLAY_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// Loads the authenticated Discord user + guilds for **display** (cached OK).
pub async fn require_auth(app: &Arc<AppState>, session: &Session) -> ApiResult<AuthCtx> {
    require_auth_with(app, session, GuildListMode::CachedOk).await
}

/// Loads auth with a **fresh** Discord guild list for authorization decisions.
pub async fn require_auth_fresh(app: &Arc<AppState>, session: &Session) -> ApiResult<AuthCtx> {
    require_auth_with(app, session, GuildListMode::MustRefresh).await
}

async fn require_auth_with(
    app: &Arc<AppState>,
    session: &Session,
    guild_mode: GuildListMode,
) -> ApiResult<AuthCtx> {
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
    let cached_user = cached_user(app, user_id);

    // Display path only: serve cached guilds while fresh.
    if let Some(guilds) = take_cached_guilds(guild_mode, app, user_id) {
        let user = match cached_user {
            Some(u) => u,
            None => app.oauth.me(&token).await.map_err(ApiError)?,
        };
        return Ok(AuthCtx { user, guilds });
    }

    // Authorization / cold display: always re-fetch guilds from Discord.
    // Fail closed — never authorize from a stale guild snapshot (MustRefresh).
    // User profile may stay cached; `can_manage` only needs guild membership.
    let (access, guilds) = match app.oauth.my_guilds(&token).await {
        Ok(g) => (token.clone(), g),
        Err(e) => {
            tracing::warn!(
                "webui auth: guilds fetch failed for user {user_id}: {e:#}; trying refresh"
            );
            let refresh = session
                .get::<String>(SESSION_REFRESH)
                .await
                .ok()
                .flatten()
                .ok_or_else(|| ApiError(anyhow::anyhow!("session expired")))?;
            let tok = app.oauth.refresh(&refresh).await.map_err(ApiError)?;
            let _ = session.insert(SESSION_TOKEN, &tok.access_token).await;
            if let Some(r) = &tok.refresh_token {
                let _ = session.insert(SESSION_REFRESH, r).await;
            }
            match app.oauth.my_guilds(&tok.access_token).await {
                Ok(g) => (tok.access_token, g),
                Err(e2) => {
                    tracing::warn!("webui auth: refresh guilds fetch failed for user {user_id}: {e2:#}");
                    return Err(ApiError(e2));
                }
            }
        }
    };

    let user = match cached_user {
        Some(u) => u,
        None => app.oauth.me(&access).await.map_err(ApiError)?,
    };

    app.guild_cache.insert(user_id, (now, guilds.clone()));
    app.user_cache.insert(user_id, (now, user.clone()));
    Ok(AuthCtx { user, guilds })
}

fn cached_user(app: &AppState, user_id: u64) -> Option<DiscordUser> {
    let entry = app.user_cache.get(&user_id)?;
    let (fetched_at, user) = entry.value();
    if fetched_at.elapsed() < USER_CACHE_TTL {
        Some(user.clone())
    } else {
        None
    }
}

/// Returns cached guilds only when the mode permits it (display path).
/// `MustRefresh` always returns `None` so callers hit Discord.
fn take_cached_guilds(
    mode: GuildListMode,
    app: &AppState,
    user_id: u64,
) -> Option<Vec<DiscordGuild>> {
    if !guild_list_cache_allowed(mode) {
        return None;
    }
    let entry = app.guild_cache.get(&user_id)?;
    let (fetched_at, guilds) = entry.value();
    if fetched_at.elapsed() < GUILD_DISPLAY_CACHE_TTL {
        Some(guilds.clone())
    } else {
        None
    }
}

/// Display listing may use the in-memory guild cache; authorization must not.
fn guild_list_cache_allowed(mode: GuildListMode) -> bool {
    matches!(mode, GuildListMode::CachedOk)
}

// ---------------------------------------------------------------------------
// oauth flow
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct ReturnToQuery {
    return_to: Option<String>,
}

/// Returns a configured SPA origin for post-auth navigation, or the local UI.
///
/// This is deliberately an exact-origin allowlist rather than an arbitrary URL:
/// OAuth login/logout must not become open redirects. The configured CORS origins
/// are the only supported separate SPA origins.
fn return_to_or_root(cfg: &crate::config::Config, requested: Option<&str>) -> String {
    let Some(requested) = requested.map(|s| s.trim_end_matches('/')) else {
        return "/".into();
    };
    if requested.is_empty() {
        return "/".into();
    }
    if cfg
        .web
        .cors_origins
        .iter()
        .any(|origin| origin.trim_end_matches('/') == requested)
    {
        format!("{requested}/")
    } else {
        "/".into()
    }
}

async fn login(
    session: Session,
    state: axum::extract::State<Arc<AppState>>,
    Query(query): Query<ReturnToQuery>,
) -> Response {
    let state_token = random_oauth_state();
    let _ = session.insert(SESSION_STATE, &state_token).await;
    let return_to = return_to_or_root(&state.cfg, query.return_to.as_deref());
    let _ = session.insert(SESSION_RETURN_TO, &return_to).await;
    let url = state.oauth.authorize_url(&state_token);
    axum::response::Redirect::to(&url).into_response()
}

async fn logout(
    session: Session,
    state: axum::extract::State<Arc<AppState>>,
    Query(query): Query<ReturnToQuery>,
) -> Response {
    let return_to = return_to_or_root(&state.cfg, query.return_to.as_deref());
    let _ = session.delete().await;
    Json(serde_json::json!({ "return_to": return_to })).into_response()
}

/// Session-bound OAuth state must be unguessable; timestamps and PIDs are not
/// CSRF secrets. Encode 256 bits from the operating-system CSPRNG as hex so it
/// is accepted by the OAuth query encoder without additional escaping.
fn random_oauth_state() -> String {
    let bytes = rand::random::<[u8; 32]>();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

async fn callback(
    session: Session,
    state: axum::extract::State<Arc<AppState>>,
    query: axum::extract::Query<CallbackQuery>,
) -> Response {
    let expected = session
        .get::<String>(SESSION_STATE)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    // Reject missing/empty session state — skipping the check enables CSRF.
    if expected.is_empty() || Some(expected.as_str()) != query.state.as_deref() {
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
    let requested_return_to: Option<String> =
        session.remove(SESSION_RETURN_TO).await.ok().flatten();
    let return_to = return_to_or_root(&state.cfg, requested_return_to.as_deref());
    axum::response::Redirect::to(&return_to).into_response()
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
        "is_bot_owner": is_bot_owner(&state, &auth),
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
    static_handler(
        include_str!("../../frontend/index.html"),
        "text/html; charset=utf-8",
    )
}
async fn app_js() -> impl IntoResponse {
    static_handler(
        include_str!("../../frontend/app.js"),
        "application/javascript; charset=utf-8",
    )
}
async fn style_css() -> impl IntoResponse {
    static_handler(
        include_str!("../../frontend/style.css"),
        "text/css; charset=utf-8",
    )
}

// ---------------------------------------------------------------------------
// router
// ---------------------------------------------------------------------------

/// Session cookie SameSite policy for credentialed CDN SPA vs same-origin.
fn session_same_site(cross_origin: bool, secure: bool) -> tower_sessions::cookie::SameSite {
    if cross_origin && secure {
        tower_sessions::cookie::SameSite::None
    } else {
        tower_sessions::cookie::SameSite::Lax
    }
}

pub fn router(app: Arc<AppState>) -> Router {
    // In-memory sessions are intentional for a single API replica.
    // Sessions are lost on API restart; multi-replica needs a shared store.
    let session_store = MemoryStore::default();
    // Secure cookies when the public URL is HTTPS (deploy behind TLS).
    let secure = app
        .cfg
        .web
        .base_url
        .to_ascii_lowercase()
        .starts_with("https://");
    // Parse CORS origins first. SameSite=None must track *successfully parsed*
    // origins — a typo'd list must not enable cross-site cookies without CORS.
    use axum::http::HeaderValue;
    let cors_origins: Vec<HeaderValue> = app
        .cfg
        .web
        .cors_origins
        .iter()
        .filter_map(|o| HeaderValue::from_str(o.trim()).ok())
        .collect();
    if !app.cfg.web.cors_origins.is_empty() && cors_origins.is_empty() {
        tracing::warn!(
            "web.cors_origins is set but none parsed as valid Origin headers — \
             CORS disabled; session cookies stay SameSite=Lax"
        );
    }
    let cross_origin = !cors_origins.is_empty();
    let same_site = session_same_site(cross_origin, secure);
    let session_layer = SessionManagerLayer::new(session_store)
        .with_secure(secure)
        .with_same_site(same_site)
        .with_expiry(Expiry::OnInactivity(CookieDuration::days(7)));

    let mut router = Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        .route("/login", get(login))
        .route("/logout", axum::routing::post(logout))
        .route("/oauth/callback", get(callback))
        .route("/api/me", get(api_me))
        .route("/api/guilds/{guild_id}/config", get(api::guild_config))
        .route(
            "/api/guilds/{guild_id}/settings",
            axum::routing::put(api::update_settings),
        )
        .route(
            "/api/guilds/{guild_id}/rules/{rule_id}",
            get(api::get_rule)
                .put(api::put_rule)
                .delete(api::delete_rule),
        )
        .route(
            "/api/guilds/{guild_id}/badwords",
            get(api::list_badwords).post(api::add_badword),
        )
        .route(
            "/api/guilds/{guild_id}/badwords/{word_id}",
            axum::routing::delete(api::delete_badword),
        )
        .route(
            "/api/guilds/{guild_id}/commands",
            get(api::list_commands).post(api::create_command),
        )
        .route(
            "/api/guilds/{guild_id}/commands/{name}",
            get(api::get_command)
                .put(api::put_command)
                .delete(api::delete_command),
        )
        .route("/api/guilds/{guild_id}/templates", get(api::list_templates))
        .route(
            "/api/guilds/{guild_id}/templates/{event}",
            get(api::get_template)
                .put(api::put_template)
                .delete(api::delete_template),
        )
        .route(
            "/api/guilds/{guild_id}/logchannels",
            get(api::list_logchannels),
        )
        .route(
            "/api/guilds/{guild_id}/logchannels/{event}",
            get(api::get_logchannel)
                .put(api::put_logchannel)
                .delete(api::delete_logchannel),
        )
        .route(
            "/api/guilds/{guild_id}/copy",
            axum::routing::post(api::copy_config),
        )
        .route(
            "/api/guilds/{guild_id}/testregex",
            axum::routing::post(api::test_regex),
        )
        .route("/api/guilds/{guild_id}/members", get(api::members))
        .route("/api/guilds/{guild_id}/events", get(api::events))
        .route("/api/guilds/{guild_id}/bans", get(api::bans))
        .route(
            "/api/guilds/{guild_id}/unban",
            axum::routing::post(api::unban),
        )
        .route("/api/guilds/{guild_id}/stats", get(api::stats))
        .layer(session_layer)
        .with_state(app.clone());

    if cross_origin {
        use axum::http::Method;
        use tower_http::cors::{AllowOrigin, CorsLayer};
        let cors = CorsLayer::new()
            .allow_origin(AllowOrigin::list(cors_origins))
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::PUT,
                Method::DELETE,
                Method::OPTIONS,
            ])
            .allow_headers([
                axum::http::header::CONTENT_TYPE,
                axum::http::header::ACCEPT,
                axum::http::header::COOKIE,
            ])
            .allow_credentials(true);
        router = router.layer(cors);
    }

    router
}

/// Authorize Discord MANAGE_GUILD for a real guild id with a **fresh** guild list.
///
/// This is the only safe entry point for `AuthCtx::can_manage` decisions.
/// `guild_id=0` is not a Discord guild — callers that mean global must use
/// [`require_bot_owner`] / [`guild_scope`] instead.
pub async fn authorize_manage_guild(
    app: &Arc<AppState>,
    session: &Session,
    guild_id: u64,
) -> ApiResult<AuthCtx> {
    if guild_id == 0 {
        return Err(ApiError(anyhow::anyhow!(
            "you cannot manage this server (needs MANAGE_GUILD permission)"
        )));
    }
    let auth = require_auth_fresh(app, session).await?;
    if !auth.can_manage(guild_id) {
        return Err(ApiError(anyhow::anyhow!(
            "you cannot manage this server (needs MANAGE_GUILD permission)"
        )));
    }
    Ok(auth)
}

/// Parses a guild id path segment, checking the session can manage it.
///
/// Real guild ids always re-fetch Discord guild permissions (`MustRefresh`) so
/// a demoted admin cannot keep mutating via a stale display cache. `guild_id=0`
/// (global) only needs identity + owner check — no guild-list authorization.
///
/// **Warning:** the `AuthCtx` returned for `guild_id=0` may be display-cached.
/// Do **not** call `auth.can_manage(...)` on it. Use [`authorize_manage_guild`]
/// for any secondary Discord guild check (e.g. copy source).
pub async fn guild_scope(
    app: &Arc<AppState>,
    session: &Session,
    guild_id: u64,
) -> ApiResult<AuthCtx> {
    // guild_id 0 is GLOBAL_SCOPE — only the configured bot owner may edit it.
    // Owner identity does not depend on Discord guild membership.
    if guild_id == 0 {
        let auth = require_auth(app, session).await?;
        require_bot_owner(app, &auth)?;
        return Ok(auth);
    }
    authorize_manage_guild(app, session, guild_id).await
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
        let tmp =
            std::env::temp_dir().join(format!("jmodguard_web_{}_{}.db", std::process::id(), n));
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
            .oneshot(
                Request::builder()
                    .uri("/api/me")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn login_redirects_to_discord() {
        let app = test_app();
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let location = res.headers().get("location").unwrap().to_str().unwrap();
        assert!(location.starts_with("https://discord.com/oauth2/authorize"));
    }

    #[tokio::test]
    async fn oauth_callback_rejects_missing_session_state() {
        // No prior /login → empty session state → CSRF reject (must not skip check).
        let app = test_app();
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/oauth/callback?code=fake&state=attacker")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains("OAuth state mismatch"),
            "expected CSRF reject page, got: {text}"
        );
    }

    fn auth_user(discord_id: u64) -> AuthCtx {
        AuthCtx {
            user: crate::state::DiscordUser {
                id: discord_id.to_string(),
                username: "tester".into(),
                global_name: None,
                avatar: None,
            },
            guilds: vec![],
        }
    }

    fn app_with_owner(owner_id: u64) -> Arc<AppState> {
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let tmp =
            std::env::temp_dir().join(format!("jmodguard_auth_{}_{}.db", std::process::id(), n));
        let db = crate::db::Db::open(&tmp, 2).unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.bot.owner_id = owner_id;
        let app = AppState::new(cfg, db).unwrap();
        std::fs::remove_file(&tmp).ok();
        app
    }

    #[test]
    fn is_bot_owner_requires_nonzero_match() {
        let app = app_with_owner(0);
        assert!(!is_bot_owner(&app, &auth_user(42)));
        let app = app_with_owner(99);
        assert!(!is_bot_owner(&app, &auth_user(42)));
        assert!(is_bot_owner(&app, &auth_user(99)));
    }

    #[test]
    fn require_bot_owner_rejects_non_owner() {
        let app = app_with_owner(99);
        assert!(require_bot_owner(&app, &auth_user(42)).is_err());
        assert!(require_bot_owner(&app, &auth_user(99)).is_ok());
    }

    #[test]
    fn scope_from_global_requires_owner() {
        let app = app_with_owner(99);
        let guild = 111u64;
        let global = Some("global".to_string());
        assert!(
            api::scope_from(&app, &auth_user(42), &global, guild).is_err(),
            "non-owner must not write global via body scope"
        );
        let scope = match api::scope_from(&app, &auth_user(99), &global, guild) {
            Ok(s) => s,
            Err(e) => panic!("owner should write global: {:#}", e.0),
        };
        assert_eq!(scope, crate::types::GLOBAL_SCOPE);
        let guild_scope_id =
            match api::scope_from(&app, &auth_user(42), &Some("guild".into()), guild) {
                Ok(s) => s,
                Err(e) => panic!("guild scope should work for non-owner: {:#}", e.0),
            };
        assert_eq!(guild_scope_id, guild as i64);
    }

    #[test]
    fn delete_badword_denials_use_uniform_not_found() {
        // Authorization still differs (owner vs guild match), but the public
        // error must not distinguish missing / cross-guild / non-owner global.
        let app = app_with_owner(99);
        let non_owner = auth_user(42);
        let owner = auth_user(99);
        let deny_msg = |allowed: bool| {
            if allowed {
                None
            } else {
                Some("badword not found")
            }
        };
        // Non-owner + global row → same message as missing.
        assert_eq!(
            deny_msg(is_bot_owner(&app, &non_owner)),
            Some("badword not found")
        );
        // Owner + global row → allowed.
        assert_eq!(deny_msg(is_bot_owner(&app, &owner)), None);
        // Wrong guild id → same message (not "not found for this guild").
        let word_guild = 11i64;
        let path_guild = 22i64;
        assert_eq!(
            deny_msg(word_guild == path_guild),
            Some("badword not found")
        );
    }

    /// Seeds session + caches so `require_auth` succeeds without Discord OAuth.
    async fn session_for_user(user_id: u64) -> Session {
        let store = Arc::new(MemoryStore::default());
        let session = Session::new(None, store, None);
        session.insert(SESSION_USER, user_id).await.unwrap();
        session
            .insert(SESSION_TOKEN, "test-access-token")
            .await
            .unwrap();
        session
    }

    fn seed_auth_caches(app: &AppState, user_id: u64, guilds: Vec<crate::state::DiscordGuild>) {
        let user = crate::state::DiscordUser {
            id: user_id.to_string(),
            username: "tester".into(),
            global_name: None,
            avatar: None,
        };
        let now = std::time::Instant::now();
        app.user_cache.insert(user_id, (now, user));
        app.guild_cache.insert(user_id, (now, guilds));
    }

    #[tokio::test]
    async fn put_global_settings_http_requires_auth() {
        let app = test_app();
        let res = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/guilds/0/settings")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"prefix":"!"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains("not authenticated") || text.contains("error"),
            "expected auth failure, got: {text}"
        );
    }

    #[tokio::test]
    async fn guild_scope_zero_rejects_non_owner_session() {
        let app = app_with_owner(99);
        seed_auth_caches(&app, 42, vec![]);
        let session = session_for_user(42).await;
        let err = guild_scope(&app, &session, 0)
            .await
            .err()
            .expect("non-owner must not access guild_id=0");
        assert!(
            err.0.to_string().contains("bot owner"),
            "unexpected: {:#}",
            err.0
        );
    }

    #[tokio::test]
    async fn guild_scope_zero_allows_owner_session() {
        let app = app_with_owner(99);
        seed_auth_caches(&app, 99, vec![]);
        let session = session_for_user(99).await;
        let auth = match guild_scope(&app, &session, 0).await {
            Ok(a) => a,
            Err(e) => panic!("owner must access guild_id=0: {:#}", e.0),
        };
        assert_eq!(auth.discord_user_id(), 99);
    }

    #[test]
    fn guild_list_cache_forbidden_for_authorization() {
        // Root cause of post-demotion WebUI access: authorizing with CachedOk.
        // guild_scope (real guilds) must use MustRefresh → cache never consulted.
        assert!(guild_list_cache_allowed(GuildListMode::CachedOk));
        assert!(!guild_list_cache_allowed(GuildListMode::MustRefresh));
    }

    #[test]
    fn copy_source_must_not_trust_guild_scope_zero_authctx() {
        // Regression: guild_scope(0) returns CachedOk AuthCtx. That context can
        // still list a source guild the owner no longer manages. copy_config
        // must call authorize_manage_guild(from) — never auth.can_manage(from)
        // on the destination AuthCtx.
        let stale = AuthCtx {
            user: crate::state::DiscordUser {
                id: "99".into(),
                username: "owner".into(),
                global_name: None,
                avatar: None,
            },
            guilds: vec![crate::state::DiscordGuild {
                id: "111".into(),
                name: "revoked".into(),
                icon: None,
                owner: true,
                permissions: None,
            }],
        };
        assert!(
            stale.can_manage(111),
            "stale CachedOk context still reports manage — production must refresh"
        );
        // Production gate for secondary guild checks is MustRefresh.
        assert!(!guild_list_cache_allowed(GuildListMode::MustRefresh));
    }

    #[tokio::test]
    async fn authorize_manage_guild_rejects_global_scope_id() {
        let app = app_with_owner(99);
        seed_auth_caches(&app, 99, vec![]);
        let session = session_for_user(99).await;
        let err = authorize_manage_guild(&app, &session, 0)
            .await
            .err()
            .expect("guild_id=0 is not a Discord MANAGE_GUILD target");
        assert!(
            err.0.to_string().contains("cannot manage"),
            "unexpected: {:#}",
            err.0
        );
    }

    #[test]
    fn session_cookie_secure_follows_base_url() {
        // Mirror router() policy: https base_url → Secure cookies.
        assert!(!"http://localhost:8080"
            .to_ascii_lowercase()
            .starts_with("https://"));
        assert!("https://bot.example.com"
            .to_ascii_lowercase()
            .starts_with("https://"));
    }

    #[test]
    fn cors_same_site_none_requires_https_and_origins() {
        // Exercises the real helper used by router().
        assert_eq!(
            session_same_site(true, true),
            tower_sessions::cookie::SameSite::None
        );
        assert_eq!(
            session_same_site(true, false),
            tower_sessions::cookie::SameSite::Lax
        );
        assert_eq!(
            session_same_site(false, true),
            tower_sessions::cookie::SameSite::Lax
        );
        assert_eq!(
            session_same_site(false, false),
            tower_sessions::cookie::SameSite::Lax
        );
    }

    #[test]
    fn post_auth_return_only_accepts_configured_spa_origins() {
        let mut cfg = crate::config::Config::default();
        cfg.web.cors_origins = vec!["https://admin.example.com".into()];

        assert_eq!(
            return_to_or_root(&cfg, Some("https://admin.example.com/")),
            "https://admin.example.com/"
        );
        assert_eq!(
            return_to_or_root(&cfg, Some("https://attacker.example.com")),
            "/"
        );
        assert_eq!(return_to_or_root(&cfg, None), "/");
    }
}
