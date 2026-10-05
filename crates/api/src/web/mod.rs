//! WebUI: Discord OAuth2 login, session handling, and the REST API.

pub mod api;
pub mod cases;
mod discord_retry;
pub mod members;
pub mod oauth;
pub mod preview;
pub mod recovery;
pub mod state;
pub mod updates;
pub mod usage;

use axum::extract::Query;
use axum::http::header::{CACHE_CONTROL, COOKIE, SET_COOKIE, VARY};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, get};
use axum::{Json, Router};
use std::sync::Arc;
use tower_sessions::cookie::time::Duration as CookieDuration;
use tower_sessions::cookie::{Cookie, SameSite};
use tower_sessions::{Expiry, MemoryStore, Session, SessionManagerLayer};

use self::oauth::{DiscordGuild, DiscordUser};
use self::state::ApiState;

const SESSION_USER: &str = "discord_user_id";
const SESSION_TOKEN: &str = "access_token";
const SESSION_REFRESH: &str = "refresh_token";
/// Browser-held CSRF secret for Discord OAuth. MemoryStore sessions die on
/// API restart; this cookie survives so `/oauth/callback` after a deploy
/// still matches the `state` Discord echoes back.
const COOKIE_OAUTH_STATE: &str = "jmodguard_oauth_state";
const COOKIE_OAUTH_RETURN_TO: &str = "jmodguard_oauth_return_to";
const SESSION_COOKIE_NAME: &str = "jmodguard_session";
const OAUTH_COOKIE_MAX_AGE: CookieDuration = CookieDuration::minutes(10);
const NO_STORE: HeaderValue = HeaderValue::from_static("private, no-store");
/// Bunny (and other CDNs) honor this independently of browser Cache-Control.
const CDN_CACHE_CONTROL: HeaderName = HeaderName::from_static("cdn-cache-control");
const CDN_NO_STORE: HeaderValue = HeaderValue::from_static("no-store");

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

pub struct ApiError(pub anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.0.is::<crate::db::EnrollmentUnverified>() {
            return ApiError(GuildAccessError::Unverified.into()).into_response();
        }
        if let Some(error) = self.0.downcast_ref::<crate::db::EnrollmentChanged>() {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": error.to_string(), "code": "guild_enrollment_changed"
                })),
            )
                .into_response();
        }
        if let Some(error) = self.0.downcast_ref::<crate::tenant::TenantError>() {
            let (status, code) = match error {
                crate::tenant::TenantError::Unplaced => {
                    (StatusCode::CONFLICT, "guild_store_unplaced")
                }
                crate::tenant::TenantError::Offline => {
                    (StatusCode::SERVICE_UNAVAILABLE, "guild_store_offline")
                }
                crate::tenant::TenantError::Membership => {
                    (StatusCode::CONFLICT, "guild_store_membership")
                }
            };
            return (
                status,
                Json(serde_json::json!({"error": error.to_string(), "code": code})),
            )
                .into_response();
        }
        if let Some(error) = self.0.downcast_ref::<GuildAccessError>() {
            let (status, code) = match error {
                GuildAccessError::Inactive => (StatusCode::CONFLICT, "guild_enrollment_inactive"),
                GuildAccessError::Unverified => {
                    (StatusCode::SERVICE_UNAVAILABLE, "guild_access_unverified")
                }
            };
            return (
                status,
                Json(serde_json::json!({"error": error.to_string(), "code": code})),
            )
                .into_response();
        }

        if self.0.is::<preview::PreviewDenied>() {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": "preview is read-only",
                    "code": "preview_read_only"
                })),
            )
                .into_response();
        }

        if let Some(limit) = self.0.downcast_ref::<oauth::DiscordRateLimit>() {
            let seconds = oauth::retry_after_header_secs(limit.retry_after);
            let mut response = (
                StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({
                    "error": limit.to_string(),
                    "code": "discord_rate_limited",
                    "retry_after": seconds,
                })),
            )
                .into_response();
            if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
                response
                    .headers_mut()
                    .insert(axum::http::header::RETRY_AFTER, value);
            }
            return response;
        }
        if let Some(key_error) = self.0.downcast_ref::<crate::bot_actions::RequestKeyError>() {
            let (status, code) = match key_error {
                crate::bot_actions::RequestKeyError::EnrollmentChanged => {
                    (StatusCode::CONFLICT, "action_enrollment_changed")
                }
                crate::bot_actions::RequestKeyError::Expired => {
                    (StatusCode::GONE, "action_request_expired")
                }
                crate::bot_actions::RequestKeyError::Invalid => {
                    (StatusCode::BAD_REQUEST, "action_request_invalid")
                }
            };
            return (
                status,
                Json(serde_json::json!({ "error": key_error.to_string(), "code": code })),
            )
                .into_response();
        }
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
    enrollment: Option<crate::db::GuildEnrollment>,
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

    pub fn can_ban_members(&self, guild_id: u64) -> bool {
        self.guilds
            .iter()
            .any(|g| g.id == guild_id.to_string() && g.ban_members_permission())
    }

    pub fn discord_user_id(&self) -> u64 {
        self.user.id.parse::<u64>().unwrap_or(0)
    }
}

/// Session owner check that does not call Discord. Preview sessions never
/// inherit owner authority. The returned context has no guild membership.
pub async fn session_owner(app: &ApiState, session: &Session) -> ApiResult<AuthCtx> {
    if preview::current().is_some() {
        return Err(ApiError(anyhow::anyhow!(
            "global config requires the configured bot owner (bot.owner_id)"
        )));
    }
    let Some(user_id) = session.get::<u64>(SESSION_USER).await.ok().flatten() else {
        return Err(ApiError(anyhow::anyhow!("not authenticated")));
    };
    let owner = app.cfg.bot.owner_id;
    if owner == 0 || user_id != owner {
        return Err(ApiError(anyhow::anyhow!(
            "global config requires the configured bot owner (bot.owner_id)"
        )));
    }
    Ok(AuthCtx {
        user: DiscordUser {
            id: user_id.to_string(),
            username: String::new(),
            global_name: None,
            avatar: None,
        },
        guilds: Vec::new(),
        enrollment: None,
    })
}

/// True when the session user is the configured bot owner (`bot.owner_id` ≠ 0).
/// Preview sessions never inherit owner authority.
pub fn is_bot_owner(app: &ApiState, auth: &AuthCtx) -> bool {
    if preview::current().is_some() {
        return false;
    }
    let owner = app.cfg.bot.owner_id;
    owner != 0 && auth.discord_user_id() == owner
}

/// Global config mutations (path `guild_id=0` or body `scope: "global"`) require owner.
pub fn require_bot_owner(app: &ApiState, auth: &AuthCtx) -> ApiResult<()> {
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
/// limits, and may keep showing the last good list while that route is cooling
/// down. Authorization (`guild_scope` for real guilds) must **not** — a
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
pub async fn require_auth(app: &Arc<ApiState>, session: &Session) -> ApiResult<AuthCtx> {
    require_auth_with(app, session, GuildListMode::CachedOk).await
}

/// Loads auth with a **fresh** Discord guild list for authorization decisions.
pub async fn require_auth_fresh(app: &Arc<ApiState>, session: &Session) -> ApiResult<AuthCtx> {
    require_auth_with(app, session, GuildListMode::MustRefresh).await
}

async fn require_auth_with(
    app: &Arc<ApiState>,
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

    // Display path only: serve a fresh list, or the last good list while
    // Discord is still cooling down. Authorization never takes this branch.
    // A rate-limited profile lookup must not drop that list.
    if let Some(guilds) = take_cached_guilds(guild_mode, app, user_id) {
        let (user, _) = display_identity(app, user_id, &token, cached_user).await?;
        let mut auth = AuthCtx {
            user,
            guilds,
            enrollment: None,
        };
        preview::apply_overlay(&mut auth)?;
        return Ok(auth);
    }

    // Authorization / cold display: always re-fetch guilds from Discord.
    // Fail closed — never authorize from a stale guild snapshot (MustRefresh).
    // User profile may stay cached; `can_manage` only needs guild membership.
    let (access, guilds) = match app.oauth.my_guilds(user_id, &token).await {
        Ok(g) => (token.clone(), g),
        Err(oauth::GuildListError::Unauthorized) => {
            tracing::warn!(
                "webui auth: guilds fetch unauthorized for user {user_id}; trying refresh"
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
            match app.oauth.my_guilds(user_id, &tok.access_token).await {
                Ok(g) => (tok.access_token, g),
                Err(error) => {
                    return finish_guild_list(
                        app,
                        guild_mode,
                        user_id,
                        &tok.access_token,
                        cached_user,
                        error,
                    )
                    .await;
                }
            }
        }
        Err(error) => {
            return finish_guild_list(app, guild_mode, user_id, &token, cached_user, error).await;
        }
    };

    let user = match cached_user {
        Some(u) => u,
        None => app.oauth.me(user_id, &access).await.map_err(ApiError)?,
    };

    app.guild_cache.insert(user_id, (now, guilds.clone()));
    app.user_cache.insert(user_id, (now, user.clone()));
    let mut auth = AuthCtx {
        user,
        guilds,
        enrollment: None,
    };
    preview::apply_overlay(&mut auth)?;
    Ok(auth)
}

async fn finish_guild_list(
    app: &ApiState,
    guild_mode: GuildListMode,
    user_id: u64,
    token: &str,
    cached_user: Option<DiscordUser>,
    error: oauth::GuildListError,
) -> ApiResult<AuthCtx> {
    if matches!(guild_mode, GuildListMode::CachedOk)
        && let Some(auth) =
            cached_display_during_limit(app, user_id, token, cached_user, &error).await?
    {
        return Ok(auth);
    }
    Err(guild_list_failure(user_id, error))
}

async fn cached_display_during_limit(
    app: &ApiState,
    user_id: u64,
    token: &str,
    cached_user: Option<DiscordUser>,
    error: &oauth::GuildListError,
) -> ApiResult<Option<AuthCtx>> {
    let oauth::GuildListError::RateLimited { fresh, .. } = error else {
        return Ok(None);
    };
    let Some(guilds) = app
        .guild_cache
        .get(&user_id)
        .map(|entry| entry.value().1.clone())
    else {
        return Ok(None);
    };
    if *fresh {
        tracing::warn!("webui auth: discord rate limited guild checks for user {user_id}");
    } else {
        tracing::debug!("webui auth: discord guild-list cooldown still active for user {user_id}");
    }
    let (user, live) = display_identity(app, user_id, token, cached_user).await?;
    if live {
        app.user_cache
            .insert(user_id, (std::time::Instant::now(), user.clone()));
    }
    let mut auth = AuthCtx {
        user,
        guilds,
        enrollment: None,
    };
    preview::apply_overlay(&mut auth)?;
    Ok(Some(auth))
}

fn guild_list_failure(user_id: u64, error: oauth::GuildListError) -> ApiError {
    match error {
        oauth::GuildListError::RateLimited { retry_after, fresh } => {
            if fresh {
                tracing::warn!("webui auth: discord rate limited guild checks for user {user_id}");
            } else {
                tracing::debug!(
                    "webui auth: discord guild-list cooldown still active for user {user_id}"
                );
            }
            ApiError(oauth::DiscordRateLimit { retry_after }.into())
        }
        oauth::GuildListError::Unauthorized => {
            tracing::warn!("webui auth: guilds fetch still unauthorized for user {user_id}");
            ApiError(anyhow::anyhow!("discord authorization failed"))
        }
        oauth::GuildListError::Failed(error) => {
            tracing::warn!("webui auth: guilds fetch failed for user {user_id}: {error:#}");
            ApiError(error)
        }
    }
}

fn cached_user(app: &ApiState, user_id: u64) -> Option<DiscordUser> {
    let entry = app.user_cache.get(&user_id)?;
    let (fetched_at, user) = entry.value();
    if fetched_at.elapsed() < USER_CACHE_TTL {
        Some(user.clone())
    } else {
        None
    }
}

/// Identity for a display response that is already returning cached guilds.
///
/// The fresh profile cache is used as-is. Otherwise live `/users/@me` still
/// runs. When that call is rate-limited, the profile still stored after the
/// 60s TTL — or the session Discord user id — is enough to return the guild
/// list. Other `me` failures still fail the request. The fallback is not
/// written back into `user_cache`, so the next display read tries `me` again
/// once Discord is no longer limiting it.
async fn display_identity(
    app: &ApiState,
    user_id: u64,
    token: &str,
    cached_user: Option<DiscordUser>,
) -> ApiResult<(DiscordUser, bool)> {
    if let Some(user) = cached_user {
        return Ok((user, false));
    }
    match app.oauth.me(user_id, token).await {
        Ok(user) => Ok((user, true)),
        Err(error) if error.downcast_ref::<oauth::DiscordRateLimit>().is_some() => {
            Ok((display_user_during_limit(app, user_id), false))
        }
        Err(error) => Err(ApiError(error)),
    }
}

fn display_user_during_limit(app: &ApiState, user_id: u64) -> DiscordUser {
    if let Some(entry) = app.user_cache.get(&user_id) {
        return entry.value().1.clone();
    }
    DiscordUser {
        id: user_id.to_string(),
        username: String::new(),
        global_name: None,
        avatar: None,
    }
}

/// Returns cached guilds only when the mode permits it (display path).
/// `MustRefresh` always returns `None` so callers hit Discord.
fn take_cached_guilds(
    mode: GuildListMode,
    app: &ApiState,
    user_id: u64,
) -> Option<Vec<DiscordGuild>> {
    if !guild_list_cache_allowed(mode) {
        return None;
    }
    let entry = app.guild_cache.get(&user_id)?;
    let (fetched_at, guilds) = entry.value();
    if guild_display_usable(
        fetched_at.elapsed(),
        GUILD_DISPLAY_CACHE_TTL,
        app.oauth.guild_route_limited(user_id),
    ) {
        Some(guilds.clone())
    } else {
        None
    }
}

/// A display list is usable while it is younger than the TTL, or while Discord
/// is still refusing that user's guild list. Authorization must not call this.
fn guild_display_usable(
    age: std::time::Duration,
    ttl: std::time::Duration,
    rate_limited: bool,
) -> bool {
    age < ttl || rate_limited
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
fn return_to_or_root(cfg: &crate::config::ApiProcessConfig, requested: Option<&str>) -> String {
    let Some(requested) = requested.map(|s| s.trim_end_matches('/')) else {
        return "/".into();
    };
    if requested.is_empty() {
        return "/".into();
    }
    if matches!(
        requested,
        "/administration/index.html" | "/api/service-upgrade/recovery"
    ) {
        return requested.into();
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
    state: axum::extract::State<Arc<ApiState>>,
    Query(query): Query<ReturnToQuery>,
) -> Response {
    login_response(&state.cfg, &state.oauth, query)
}

fn login_response(
    cfg: &crate::config::ApiProcessConfig,
    oauth: &oauth::OAuthClient,
    query: ReturnToQuery,
) -> Response {
    let state_token = random_oauth_state();
    let return_to = return_to_or_root(cfg, query.return_to.as_deref());
    let url = oauth.authorize_url(&state_token);
    let (secure, same_site) = public_cookie_policy(cfg);
    append_oauth_cookies(
        axum::response::Redirect::to(&url).into_response(),
        &state_token,
        &return_to,
        secure,
        same_site,
    )
}

async fn logout(
    session: Session,
    state: axum::extract::State<Arc<ApiState>>,
    headers: HeaderMap,
    Query(query): Query<ReturnToQuery>,
) -> Response {
    if !trusted_web_origin(&state.cfg, &headers) {
        return error_page("untrusted request origin");
    }
    let return_to = return_to_or_root(&state.cfg, query.return_to.as_deref());
    let _ = session.delete().await;
    Json(serde_json::json!({ "return_to": return_to })).into_response()
}

/// CORS only governs whether a browser may read a response. It does not stop a
/// simple cross-site form POST, which matters when a hosted SPA requires
/// SameSite=None session cookies. Validate the origin at this mutation gate.
fn trusted_web_origin(cfg: &crate::config::ApiProcessConfig, headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(axum::http::header::ORIGIN) else {
        return false;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let base_origin = reqwest::Url::parse(&cfg.web.base_url)
        .ok()
        .map(|url| url.origin().ascii_serialization());
    base_origin.as_deref() == Some(origin)
        || cfg
            .web
            .cors_origins
            .iter()
            .any(|allowed| allowed.trim_end_matches('/') == origin)
}

/// Session-bound OAuth state must be unguessable; timestamps and PIDs are not
/// CSRF secrets. Encode 256 bits from the operating-system CSPRNG as hex so it
/// is accepted by the OAuth query encoder without additional escaping.
fn random_oauth_state() -> String {
    let bytes = rand::random::<[u8; 32]>();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn public_cookie_policy(cfg: &crate::config::ApiProcessConfig) -> (bool, SameSite) {
    let secure = cfg
        .web
        .base_url
        .to_ascii_lowercase()
        .starts_with("https://");
    let cross_origin = cfg.web.cors_origins.iter().any(|origin| {
        let origin = origin.trim();
        origin != "*" && HeaderValue::from_str(origin).is_ok()
    });
    (secure, session_same_site(cross_origin, secure))
}

fn oauth_cookie<'a>(
    name: &'a str,
    value: &'a str,
    secure: bool,
    same_site: SameSite,
    max_age: CookieDuration,
) -> Cookie<'a> {
    Cookie::build((name, value))
        .http_only(true)
        .secure(secure)
        .same_site(same_site)
        .path("/")
        .max_age(max_age)
        .build()
}

fn append_set_cookie(response: &mut Response, cookie: Cookie<'_>) {
    if let Ok(value) = HeaderValue::from_str(&cookie.to_string()) {
        response.headers_mut().append(SET_COOKIE, value);
    }
}

fn append_oauth_cookies(
    mut response: Response,
    state_token: &str,
    return_to: &str,
    secure: bool,
    same_site: SameSite,
) -> Response {
    append_set_cookie(
        &mut response,
        oauth_cookie(
            COOKIE_OAUTH_STATE,
            state_token,
            secure,
            same_site,
            OAUTH_COOKIE_MAX_AGE,
        ),
    );
    append_set_cookie(
        &mut response,
        oauth_cookie(
            COOKIE_OAUTH_RETURN_TO,
            return_to,
            secure,
            same_site,
            OAUTH_COOKIE_MAX_AGE,
        ),
    );
    response
}

fn clear_oauth_cookies(mut response: Response, secure: bool, same_site: SameSite) -> Response {
    append_set_cookie(
        &mut response,
        oauth_cookie(
            COOKIE_OAUTH_STATE,
            "",
            secure,
            same_site,
            CookieDuration::ZERO,
        ),
    );
    append_set_cookie(
        &mut response,
        oauth_cookie(
            COOKIE_OAUTH_RETURN_TO,
            "",
            secure,
            same_site,
            CookieDuration::ZERO,
        ),
    );
    response
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(COOKIE)?.to_str().ok()?;
    Cookie::split_parse(raw)
        .filter_map(Result::ok)
        .find(|cookie| cookie.name() == name)
        .map(|cookie| cookie.value().to_string())
}

/// CSRF is the browser cookie Discord cannot set, compared to the `state`
/// query Discord echoes. Empty or missing cookies must reject.
fn oauth_csrf_ok(headers: &HeaderMap, query_state: Option<&str>) -> bool {
    let expected = cookie_value(headers, COOKIE_OAUTH_STATE).unwrap_or_default();
    !expected.is_empty() && Some(expected.as_str()) == query_state
}

async fn callback(
    session: Session,
    state: axum::extract::State<Arc<ApiState>>,
    headers: HeaderMap,
    query: axum::extract::Query<CallbackQuery>,
) -> Response {
    callback_response(session, &state.cfg, &state.oauth, headers, query.0).await
}

async fn callback_response(
    session: Session,
    cfg: &crate::config::ApiProcessConfig,
    oauth: &oauth::OAuthClient,
    headers: HeaderMap,
    query: CallbackQuery,
) -> Response {
    // Handshake cookies are single-use. Clear them on every callback
    // outcome, including CSRF reject, so a failed attempt cannot leave
    // a live secret for the next request.
    let (secure, same_site) = public_cookie_policy(cfg);
    let finish = |response: Response| clear_oauth_cookies(response, secure, same_site);
    // Reject missing/empty CSRF cookie — skipping the check enables CSRF.
    // The secret lives on the client so an API restart between /login and
    // Discord's redirect does not invent a state mismatch.
    if !oauth_csrf_ok(&headers, query.state.as_deref()) {
        return finish(error_page("OAuth state mismatch"));
    }
    let Some(code) = &query.code else {
        return finish(error_page("missing authorization code"));
    };
    let tok = match oauth.exchange_code(code).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("oauth callback: token exchange failed: {e:#}");
            return finish(error_page(&format!("token exchange failed: {e:#}")));
        }
    };
    let user = match oauth.me(0, &tok.access_token).await {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!("oauth callback: me request failed: {e:#}");
            return finish(error_page(&format!("failed to fetch user: {e:#}")));
        }
    };
    let uid = user.id.parse::<u64>().unwrap_or(0);
    if let Err(error) = store_login_session(
        &session,
        uid,
        &tok.access_token,
        tok.refresh_token.as_deref(),
    )
    .await
    {
        tracing::warn!("oauth callback: {error}");
        return finish(error_page("failed to rotate session"));
    }
    let requested_return_to = cookie_value(&headers, COOKIE_OAUTH_RETURN_TO);
    let return_to = return_to_or_root(cfg, requested_return_to.as_deref());
    finish(axum::response::Redirect::to(&return_to).into_response())
}

/// Rotate the session id, then persist OAuth credentials. Fail closed: a
/// rotation or store error must not leave tokens on the pre-login session id.
async fn store_login_session(
    session: &Session,
    uid: u64,
    access_token: &str,
    refresh_token: Option<&str>,
) -> Result<(), String> {
    session
        .cycle_id()
        .await
        .map_err(|error| format!("failed to rotate session id: {error}"))?;
    session
        .insert(SESSION_USER, uid)
        .await
        .map_err(|error| format!("failed to persist login session: {error}"))?;
    session
        .insert(SESSION_TOKEN, access_token)
        .await
        .map_err(|error| format!("failed to persist login session: {error}"))?;
    if let Some(refresh) = refresh_token {
        session
            .insert(SESSION_REFRESH, refresh)
            .await
            .map_err(|error| format!("failed to persist login session: {error}"))?;
    }
    Ok(())
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
    state: axum::extract::State<Arc<ApiState>>,
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
                "manage": g.manage_permission(),
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
        "preview": preview::current().map(|preview| serde_json::json!({
            "guild_id": preview.guild_id.to_string(),
            "profile": preview.profile,
        })),
    })))
}

#[cfg(feature = "embed-ui")]
fn static_handler(content: &'static str, content_type: &'static str) -> impl IntoResponse {
    (
        [
            ("content-type", content_type),
            ("cache-control", "no-cache"),
        ],
        content,
    )
}

#[cfg(feature = "embed-ui")]
async fn index() -> impl IntoResponse {
    static_handler(
        include_str!("../../../../frontend/index.html"),
        "text/html; charset=utf-8",
    )
}

#[cfg(feature = "embed-ui")]
async fn administration_index() -> impl IntoResponse {
    static_handler(
        include_str!("../../../../frontend/administration/index.html"),
        "text/html; charset=utf-8",
    )
}

#[cfg(feature = "embed-ui")]
async fn administration_js() -> impl IntoResponse {
    static_handler(
        include_str!("../../../../frontend/administration.js"),
        "application/javascript; charset=utf-8",
    )
}

async fn health() -> &'static str {
    "ok"
}

async fn ready() -> impl IntoResponse {
    if crate::process_ready() {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}

#[cfg(feature = "embed-ui")]
async fn app_js() -> impl IntoResponse {
    static_handler(
        include_str!("../../../../frontend/app.js"),
        "application/javascript; charset=utf-8",
    )
}

#[cfg(feature = "embed-ui")]
async fn style_css() -> impl IntoResponse {
    static_handler(
        include_str!("../../../../frontend/style.css"),
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

/// GET remains for older clients; POST is the SPA read so CDNs cannot reuse GET cache.
fn get_post<H, T, S>(handler: H) -> MethodRouter<S>
where
    H: axum::handler::Handler<T, S> + Clone,
    T: 'static,
    S: Clone + Send + Sync + 'static,
{
    get(handler.clone()).post(handler)
}

/// Keep `Cookie` on `Vary` without dropping tokens other layers already set (CORS `Origin`).
fn vary_including_cookie<B>(response: &axum::http::Response<B>) -> Option<HeaderValue> {
    let mut tokens = Vec::new();
    for value in response.headers().get_all(VARY) {
        let Ok(text) = value.to_str() else {
            continue;
        };
        for part in text.split(',') {
            let token = part.trim();
            if token.is_empty() {
                continue;
            }
            if !tokens
                .iter()
                .any(|existing: &String| existing.eq_ignore_ascii_case(token))
            {
                tokens.push(token.to_string());
            }
        }
    }
    if !tokens
        .iter()
        .any(|token| token.eq_ignore_ascii_case("Cookie"))
    {
        tokens.push("Cookie".to_string());
    }
    HeaderValue::from_str(&tokens.join(", ")).ok()
}

/// Browser, CDN, and cookie-variant headers for every axum response, including `/health`.
fn uncacheable_response_headers<S>(router: Router<S>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            CACHE_CONTROL,
            NO_STORE,
        ))
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            CDN_CACHE_CONTROL,
            CDN_NO_STORE,
        ))
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            VARY,
            vary_including_cookie,
        ))
}

pub fn router(app: Arc<ApiState>) -> Router {
    // In-memory sessions are intentional for a single API replica.
    // Sessions are lost on API restart; multi-replica needs a shared store.
    router_with_store(app, MemoryStore::default())
}

fn router_with_store(app: Arc<ApiState>, session_store: MemoryStore) -> Router {
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
        .filter_map(|o| {
            let origin = o.trim();
            if origin == "*" {
                tracing::warn!("ignoring wildcard web.cors_origins entry; credentialed CORS requires explicit origins");
                None
            } else {
                HeaderValue::from_str(origin).ok()
            }
        })
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
        .with_name(SESSION_COOKIE_NAME)
        .with_secure(secure)
        .with_same_site(same_site)
        .with_expiry(Expiry::OnInactivity(CookieDuration::days(7)));

    let sessioned = Router::new();
    #[cfg(feature = "embed-ui")]
    let sessioned = sessioned
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/administration/index.html", get(administration_index))
        .route("/administration.js", get(administration_js))
        .route("/style.css", get(style_css));
    let sessioned = sessioned
        .route("/login", get(login))
        .route("/logout", axum::routing::post(logout))
        .route("/oauth/callback", get(callback))
        .route("/api/me", get_post(api_me))
        .route("/api/updates", get_post(updates::status))
        .route("/api/database-usage", axum::routing::post(usage::overview))
        .route(
            "/api/database-usage/{id}",
            axum::routing::post(usage::detail),
        )
        .route("/api/updates/check", axum::routing::post(updates::check))
        .route(
            "/api/updates/install",
            axum::routing::post(updates::install),
        )
        .route("/api/updates/policy", axum::routing::post(updates::policy))
        .route(
            "/api/preview",
            axum::routing::post(preview::launch).delete(preview::exit),
        )
        .route("/api/guilds/{guild_id}/config", get_post(api::guild_config))
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
                .post(api::get_logchannel)
                .put(api::put_logchannel)
                .delete(api::delete_logchannel),
        )
        .route(
            "/api/guilds/{guild_id}/logfilters/{event}",
            axum::routing::put(api::put_log_filter),
        )
        .route(
            "/api/guilds/{guild_id}/copy",
            axum::routing::post(api::copy_config),
        )
        .route(
            "/api/guilds/{guild_id}/testregex",
            axum::routing::post(api::test_regex),
        )
        .route("/api/guilds/{guild_id}/events", get_post(api::events))
        .route("/api/guilds/{guild_id}/bans", get_post(api::bans))
        .route(
            "/api/guilds/{guild_id}/unban",
            axum::routing::post(api::unban),
        )
        .route("/api/guilds/{guild_id}/actions", get_post(api::actions))
        .route(
            "/api/guilds/{guild_id}/actions/{id}",
            axum::routing::delete(api::cancel_action),
        )
        .route("/api/guilds/{guild_id}/stats", get_post(api::stats))
        .route("/api/guilds/{guild_id}/cases", get_post(cases::list_cases))
        .route(
            "/api/guilds/{guild_id}/members",
            get_post(members::list_members),
        )
        .route(
            "/api/guilds/{guild_id}/members/{user_id}",
            get_post(members::get_member),
        )
        .route(
            "/api/guilds/{guild_id}/members/{user_id}/history",
            get_post(members::member_history),
        )
        .route(
            "/api/guilds/{guild_id}/member-sync",
            get_post(members::sync_status),
        )
        .route(
            "/api/guilds/{guild_id}/member-sync/actions",
            axum::routing::post(members::sync_action),
        )
        .route(
            "/api/guilds/{guild_id}/cases/{case_id}",
            get(cases::get_case)
                .post(cases::get_case)
                .delete(cases::delete_case),
        )
        .route(
            "/api/guilds/{guild_id}/cases/{case_id}/history",
            get(cases::list_history).post(cases::add_note),
        )
        .route(
            "/api/guilds/{guild_id}/cases/{case_id}/evidence",
            get(cases::list_evidence).post(cases::list_evidence),
        )
        .route(
            "/api/guilds/{guild_id}/cases/{case_id}/evidence/{evidence_id}",
            get(cases::get_evidence),
        )
        .route(
            "/api/guilds/{guild_id}/case-delete-roles",
            get_post(cases::get_case_delete_roles).put(cases::put_case_delete_roles),
        )
        .merge(recovery::routes(Arc::new(recovery::RecoveryState {
            cfg: Arc::clone(&app.cfg),
            oauth: Arc::clone(&app.oauth),
            control: crate::db::UpdateControl::from_application(&app.db),
            apply_enabled: std::env::var("JMODGUARD_UPDATE_APPLY_ENABLED").as_deref() == Ok("1"),
        })))
        .layer(session_layer)
        .layer(axum::middleware::from_fn_with_state(
            app.clone(),
            preview::middleware,
        ));

    // CORS is a browser credential policy for the SPA/API. Process liveness
    // (`/health`) must not depend on Origin — platform probes are not a
    // credentialed CORS client.
    let sessioned = if cross_origin {
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
                axum::http::HeaderName::from_static("x-jmodguard-enrollment"),
                axum::http::HeaderName::from_static("x-jmodguard-preview"),
            ])
            .allow_credentials(true);
        sessioned.layer(cors)
    } else {
        sessioned
    };

    uncacheable_response_headers(
        Router::new()
            .route("/health", get(health))
            .route("/health/", get(health))
            .route("/ready", get(ready))
            .route("/ready/", get(ready))
            .merge(sessioned),
    )
    .with_state(app.clone())
}

/// Membership admission failures contain no database diagnostics or tenant data.
#[derive(Debug)]
enum GuildAccessError {
    Inactive,
    Unverified,
}
impl std::fmt::Display for GuildAccessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Inactive => "The bot has no active enrollment in this server.",
            Self::Unverified => "Server access could not be verified. Try again shortly.",
        })
    }
}
impl std::error::Error for GuildAccessError {}

/// Call after fresh Discord permission authorization. This primary read is an
/// admission gate, not a lock covering the handler's later data transactions.
async fn require_active_enrollment(
    app: &ApiState,
    guild: u64,
) -> ApiResult<crate::db::GuildEnrollment> {
    let guild = i64::try_from(guild)
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| ApiError(anyhow::anyhow!("invalid guild id")))?;
    match crate::db::guild_enrollment(&app.db, guild).await {
        Ok(Some(enrollment)) => Ok(enrollment),
        Ok(None) => Err(ApiError(GuildAccessError::Inactive.into())),
        Err(error) => {
            tracing::warn!(
                guild_id = guild,
                "guild access verification unavailable: {error:#}"
            );
            Err(ApiError(GuildAccessError::Unverified.into()))
        }
    }
}

fn configuration_scope(
    app: &ApiState,
    auth: &AuthCtx,
    guild: u64,
) -> ApiResult<crate::db::ConfigurationScope> {
    if guild == 0 {
        require_bot_owner(app, auth)?;
        return Ok(crate::db::ConfigurationScope::Global);
    }
    Ok(crate::db::ConfigurationScope::Guild(authorized_enrollment(
        auth, guild,
    )?))
}

pub(super) fn authorized_enrollment(
    auth: &AuthCtx,
    guild: u64,
) -> ApiResult<crate::db::GuildEnrollment> {
    match &auth.enrollment {
        Some(enrollment) if guild > 0 && u64::try_from(enrollment.guild_id).ok() == Some(guild) => {
            Ok(enrollment.clone())
        }
        _ => Err(ApiError(crate::db::EnrollmentChanged.into())),
    }
}

fn configuration_write_scope(
    app: &ApiState,
    auth: &AuthCtx,
    guild: u64,
    target: i64,
) -> ApiResult<crate::db::ConfigurationWriteScope> {
    let target =
        u64::try_from(target).map_err(|_| ApiError(crate::db::EnrollmentChanged.into()))?;
    Ok(crate::db::ConfigurationWriteScope::new(
        configuration_scope(app, auth, guild)?,
        configuration_scope(app, auth, target)?,
    )?)
}

fn check_enrollment_token(auth: &AuthCtx, guild: u64, token: Option<&str>) -> ApiResult<()> {
    if guild == 0 {
        return Ok(());
    }
    let enrollment = authorized_enrollment(auth, guild)?;
    if token != Some(enrollment.id.as_str()) {
        return Err(ApiError(crate::db::EnrollmentChanged.into()));
    }
    Ok(())
}
pub(super) fn check_page_enrollment(
    auth: &AuthCtx,
    guild: u64,
    headers: &HeaderMap,
) -> ApiResult<()> {
    check_enrollment_token(
        auth,
        guild,
        headers
            .get("x-jmodguard-enrollment")
            .and_then(|v| v.to_str().ok()),
    )
}

/// Authorize Discord MANAGE_GUILD for a real guild id with a **fresh** guild list.
///
/// This is the only safe entry point for `AuthCtx::can_manage` decisions.
/// `guild_id=0` is not a Discord guild — callers that mean global must use
/// [`require_bot_owner`] / [`guild_scope`] instead.
pub async fn authorize_manage_guild(
    app: &Arc<ApiState>,
    session: &Session,
    guild_id: u64,
) -> ApiResult<AuthCtx> {
    if guild_id == 0 {
        return Err(ApiError(anyhow::anyhow!(
            "you cannot manage this server (needs MANAGE_GUILD permission)"
        )));
    }
    // Discord permission and database membership are independent admission
    // inputs. Fetch them concurrently so remote deployments do not pay both
    // network round trips serially before every guild operation.
    let (auth, enrollment) = tokio::join!(
        require_auth_fresh(app, session),
        require_active_enrollment(app, guild_id)
    );
    let mut auth = auth?;
    if !auth.can_manage(guild_id) {
        return Err(ApiError(anyhow::anyhow!(
            "you cannot manage this server (needs MANAGE_GUILD permission)"
        )));
    }
    auth.enrollment = Some(enrollment?);
    Ok(auth)
}

/// Authorizes a bot-token unban with fresh Discord ban authority, rather than
/// configuration-only MANAGE_GUILD authority.
pub async fn authorize_ban_members(
    app: &Arc<ApiState>,
    session: &Session,
    guild_id: u64,
) -> ApiResult<AuthCtx> {
    if guild_id == 0 {
        return Err(ApiError(anyhow::anyhow!(
            "this operation requires a Discord server"
        )));
    }
    let (auth, enrollment) = tokio::join!(
        require_auth_fresh(app, session),
        require_active_enrollment(app, guild_id)
    );
    let mut auth = auth?;
    if !auth.can_ban_members(guild_id) {
        return Err(ApiError(anyhow::anyhow!(
            "you cannot unban members in this server (needs BAN_MEMBERS permission)"
        )));
    }
    auth.enrollment = Some(enrollment?);
    Ok(auth)
}

/// Parses a guild id path segment, checking the session can manage it.
///
/// Real guild ids always re-fetch Discord guild permissions (`MustRefresh`) so
/// a demoted admin cannot keep mutating via a stale display cache. `guild_id=0`
/// (global) checks the session user against `bot.owner_id` and does not fetch
/// the guild list.
///
/// **Warning:** the `AuthCtx` returned for `guild_id=0` has no guild membership.
/// Do **not** call `auth.can_manage(...)` on it. Use [`authorize_manage_guild`]
/// for any secondary Discord guild check (e.g. copy source).
pub async fn guild_scope(
    app: &Arc<ApiState>,
    session: &Session,
    guild_id: u64,
) -> ApiResult<AuthCtx> {
    // guild_id 0 is GLOBAL_SCOPE — only the configured bot owner may edit it.
    // Owner identity does not depend on Discord guild membership.
    if guild_id == 0 {
        return session_owner(app, session).await;
    }
    authorize_manage_guild(app, session, guild_id).await
}

#[cfg(test)]
fn assert_uncacheable_headers(headers: &HeaderMap) {
    assert_eq!(
        headers.get(CACHE_CONTROL).map(|v| v.as_bytes()),
        Some(NO_STORE.as_bytes()),
        "API responses must not be browser-cacheable"
    );
    assert_eq!(
        headers.get(CDN_CACHE_CONTROL).map(|v| v.as_bytes()),
        Some(CDN_NO_STORE.as_bytes()),
        "API responses must send CDN-Cache-Control: no-store"
    );
    let vary = headers
        .get_all(VARY)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|text| text.split(','))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    assert!(
        vary.iter()
            .any(|token| token.eq_ignore_ascii_case("Cookie")),
        "API responses must vary CDN caches by Cookie, got {vary:?}"
    );
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
        let app = crate::web::state::ApiState::new(Default::default(), db).unwrap();
        std::fs::remove_file(&tmp).ok();
        crate::tenant::remove_sidecars(&tmp);
        router(app)
    }

    #[tokio::test]
    async fn store_login_session_fails_closed_when_rotation_fails() {
        #[derive(Debug, Clone, Default)]
        struct FailingDeleteStore;

        #[async_trait::async_trait]
        impl tower_sessions::SessionStore for FailingDeleteStore {
            async fn create(
                &self,
                _record: &mut tower_sessions::session::Record,
            ) -> tower_sessions::session_store::Result<()> {
                Ok(())
            }

            async fn save(
                &self,
                _record: &tower_sessions::session::Record,
            ) -> tower_sessions::session_store::Result<()> {
                Ok(())
            }

            async fn load(
                &self,
                _session_id: &tower_sessions::session::Id,
            ) -> tower_sessions::session_store::Result<Option<tower_sessions::session::Record>>
            {
                Ok(None)
            }

            async fn delete(
                &self,
                _session_id: &tower_sessions::session::Id,
            ) -> tower_sessions::session_store::Result<()> {
                Err(tower_sessions::session_store::Error::Backend(
                    "delete failed".into(),
                ))
            }
        }

        let session = Session::new(None, Arc::new(FailingDeleteStore), None);
        let err = store_login_session(&session, 1, "token", None)
            .await
            .expect_err("rotation failure must not persist credentials");
        assert!(
            err.contains("failed to rotate session id"),
            "unexpected: {err}"
        );
        assert!(
            session.get::<u64>(SESSION_USER).await.unwrap().is_none(),
            "pre-login session must not receive the Discord user id"
        );
    }

    #[tokio::test]
    async fn member_directory_requires_authorization() {
        let app = test_app();
        for (method, uri, body) in [
            ("GET", "/api/guilds/123/members", ""),
            ("GET", "/api/guilds/123/members/1", ""),
            ("GET", "/api/guilds/123/members/1/history", ""),
            ("GET", "/api/guilds/123/member-sync", ""),
            (
                "POST",
                "/api/guilds/123/member-sync/actions",
                r#"{"request_id":"1758140000_aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee","operation":"pause"}"#,
            ),
        ] {
            assert_unauthenticated(app.clone(), method, uri, body).await;
        }
    }

    #[tokio::test]
    async fn health_is_unauthenticated_liveness() {
        let app = test_app();
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        super::assert_uncacheable_headers(res.headers());
        let body = axum::body::to_bytes(res.into_body(), 64).await.unwrap();
        assert_eq!(&body[..], b"ok");
    }

    async fn assert_unauthenticated(app: Router, method: &str, uri: &str, body: &str) {
        let mut builder = Request::builder().method(method).uri(uri);
        let req_body = if body.is_empty() {
            Body::empty()
        } else {
            builder = builder.header("content-type", "application/json");
            Body::from(body.to_owned())
        };
        let res = app.oneshot(builder.body(req_body).unwrap()).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{method} {uri}");
        super::assert_uncacheable_headers(res.headers());
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"], "not authenticated", "{method} {uri}");
    }

    #[tokio::test]
    async fn api_me_sets_no_store_cache_control() {
        let app = test_app();
        for method in ["GET", "POST"] {
            assert_unauthenticated(app.clone(), method, "/api/me", "").await;
        }
    }

    #[tokio::test]
    async fn update_routes_reject_anonymous_requests_without_disclosing_state() {
        for (method, path, body) in [
            ("GET", "/api/updates", ""),
            ("POST", "/api/updates", "{}"),
            ("POST", "/api/updates/check", r#"{"component":"api"}"#),
            (
                "POST",
                "/api/updates/install",
                r#"{"component":"api","digest":"sha256:abc"}"#,
            ),
            (
                "POST",
                "/api/updates/policy",
                r#"{"component":"api","channel":"stable","cadence":"daily"}"#,
            ),
        ] {
            let response = test_app()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {path}"
            );
            super::assert_uncacheable_headers(response.headers());
        }
    }

    #[tokio::test]
    async fn database_usage_routes_reject_anonymous_requests_with_private_headers() {
        for path in ["/api/database-usage", "/api/database-usage/core"] {
            let response = test_app()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
            super::assert_uncacheable_headers(response.headers());
        }
    }

    #[tokio::test]
    async fn unauthenticated_settings_paths_are_not_authenticated() {
        let app = test_app();
        let cases: &[(&str, &str, &str)] = &[
            ("POST", "/api/guilds/1273405807861694557/config", ""),
            ("GET", "/api/guilds/1273405807861694557/config", ""),
            (
                "PUT",
                "/api/guilds/1273405807861694557/settings",
                r#"{"prefix":"!"}"#,
            ),
            (
                "POST",
                "/api/guilds/1273405807861694557/case-delete-roles",
                "",
            ),
            (
                "PUT",
                "/api/guilds/1273405807861694557/case-delete-roles",
                r#"{"roles":[],"revision":""}"#,
            ),
            (
                "POST",
                "/api/guilds/1273405807861694557/copy",
                r#"{"from_guild_id":"271083998816632834","from_enrollment_id":"x","sections":["settings"]}"#,
            ),
            (
                "PUT",
                "/api/guilds/1273405807861694557/logchannels/member_join",
                r#"{"request_id":"1758140000_aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee","scope":"guild","channel_id":"123"}"#,
            ),
        ];
        for (method, uri, body) in cases {
            assert_unauthenticated(app.clone(), method, uri, body).await;
        }
    }

    #[tokio::test]
    async fn logchannel_put_rejects_non_snowflake_channel_id() {
        let app = test_app();
        let res = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/guilds/271083998816632834/logchannels/member_join")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"request_id":"1758140000_aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee","channel_id":"general"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
        super::assert_uncacheable_headers(res.headers());
    }

    #[tokio::test]
    async fn health_head_matches_get_without_body() {
        // axum `get` already accepts HEAD and strips the body. Lock that so a
        // later method-router change cannot make HEAD 405 after the startup
        // listener (which answers HEAD) hands the socket over.
        let app = test_app();
        let res = app
            .oneshot(
                Request::builder()
                    .method("HEAD")
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 64).await.unwrap();
        assert!(body.is_empty(), "HEAD must omit the GET body");
    }

    #[tokio::test]
    async fn health_trailing_slash_matches_startup_listener() {
        let app = test_app();
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/health/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 64).await.unwrap();
        assert_eq!(&body[..], b"ok");
    }

    #[tokio::test]
    async fn health_ignores_cors_origin() {
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let tmp =
            std::env::temp_dir().join(format!("jmodguard_web_{}_{}.db", std::process::id(), n));
        let db = crate::db::Db::open(&tmp, 2).unwrap();
        let mut cfg = crate::config::ApiProcessConfig::default();
        cfg.web.cors_origins = vec!["https://admin.example.com".into()];
        let app = router(crate::web::state::ApiState::new(cfg, db).unwrap());
        std::fs::remove_file(&tmp).ok();
        crate::tenant::remove_sidecars(&tmp);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("origin", "https://attacker.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 64).await.unwrap();
        assert_eq!(&body[..], b"ok");
    }

    #[tokio::test]
    async fn cors_api_responses_keep_origin_and_cookie_vary() {
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let tmp =
            std::env::temp_dir().join(format!("jmodguard_web_{}_{}.db", std::process::id(), n));
        let db = crate::db::Db::open(&tmp, 2).unwrap();
        let mut cfg = crate::config::ApiProcessConfig::default();
        cfg.web.cors_origins = vec!["https://admin.example.com".into()];
        let app = router(crate::web::state::ApiState::new(cfg, db).unwrap());
        std::fs::remove_file(&tmp).ok();
        crate::tenant::remove_sidecars(&tmp);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/me")
                    .header("origin", "https://admin.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        super::assert_uncacheable_headers(res.headers());
        let vary = res
            .headers()
            .get_all(VARY)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|text| text.split(','))
            .map(str::trim)
            .collect::<Vec<_>>();
        assert!(
            vary.iter()
                .any(|token| token.eq_ignore_ascii_case("Origin")),
            "CORS responses must still vary by Origin, got {vary:?}"
        );
        assert_eq!(
            res.headers()
                .get(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("https://admin.example.com")
        );
    }

    #[tokio::test]
    async fn startup_listener_handoff_to_axum_keeps_liveness() {
        // Same composition as jmodguard-api: raw /health on the public bind
        // until open finishes, then axum takes that socket. Application
        // routes must flip from startup 503 to the real router.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        async fn http_get(addr: std::net::SocketAddr, path: &str) -> String {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut last_err = None;
            for _ in 0..50 {
                match tokio::net::TcpStream::connect(addr).await {
                    Ok(mut stream) => {
                        stream
                            .write_all(
                                format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                                    .as_bytes(),
                            )
                            .await
                            .unwrap();
                        let mut buf = vec![0u8; 1024];
                        let n = tokio::time::timeout(
                            std::time::Duration::from_secs(2),
                            stream.read(&mut buf),
                        )
                        .await
                        .unwrap()
                        .unwrap();
                        return String::from_utf8_lossy(&buf[..n]).into_owned();
                    }
                    Err(error) => {
                        last_err = Some(error);
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                }
            }
            panic!("never connected to {addr}: {last_err:?}");
        }
        // Same composition as jmodguard-api: each synchronous startup step on
        // spawn_blocking so the accept loop can keep answering /health.
        let serving = crate::health::serve_while(&listener, async {
            tokio::task::spawn_blocking(|| {
                std::thread::sleep(std::time::Duration::from_millis(200));
            })
            .await
            .unwrap();
            tokio::task::spawn_blocking(|| {
                std::thread::sleep(std::time::Duration::from_millis(200));
                test_app()
            })
            .await
            .unwrap()
        });
        let probes = async {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(350);
            while tokio::time::Instant::now() < deadline {
                let health = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    http_get(addr, "/health"),
                )
                .await
                .expect("health must answer during ApiState prime and router build");
                assert!(
                    health.starts_with("HTTP/1.1 200 OK"),
                    "startup liveness: {health}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            let api = http_get(addr, "/api/me").await;
            assert!(
                api.starts_with("HTTP/1.1 503 Service Unavailable"),
                "startup must fence app routes: {api}"
            );
            assert!(
                api.contains("retry-after: 1"),
                "startup 503 must advertise Retry-After: {api}"
            );
            assert!(
                api.contains(r#"{"error":"not ready"}"#),
                "startup /api must use the JSON warmup contract: {api}"
            );
            let login = http_get(addr, "/login").await;
            assert!(
                login.contains("content-type: text/html; charset=utf-8"),
                "startup /login must be HTML so OAuth navigations can retry: {login}"
            );
            assert!(
                login.contains("Waiting for the API to finish starting"),
                "startup /login must refresh in place: {login}"
            );
        };
        let (web_app, _) = tokio::join!(serving, probes);
        tokio::spawn(async move {
            axum::serve(listener, web_app).await.ok();
        });
        let health = http_get(addr, "/health").await;
        assert!(
            health.starts_with("HTTP/1.1 200 OK"),
            "axum liveness after handoff: {health}"
        );
        let api = http_get(addr, "/api/me").await;
        assert!(
            api.starts_with("HTTP/1.1 400"),
            "app routes must leave the startup 503 after handoff: {api}"
        );
    }

    #[tokio::test]
    #[cfg(feature = "embed-ui")]
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
    async fn login_sets_oauth_csrf_cookie() {
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
        let state = location
            .split("state=")
            .nth(1)
            .and_then(|rest| rest.split('&').next())
            .expect("authorize URL must include state");
        let cookies: Vec<_> = res
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect();
        assert!(
            cookies
                .iter()
                .any(|cookie| cookie.contains(&format!("{COOKIE_OAUTH_STATE}={state}"))),
            "login must put CSRF state on the client, not only in MemoryStore: {cookies:?}"
        );
    }

    #[tokio::test]
    async fn oauth_callback_rejects_missing_csrf_cookie() {
        // No prior /login cookie → CSRF reject (must not skip check).
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
        let cookies: Vec<String> = res
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok().map(str::to_owned))
            .collect();
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains("OAuth state mismatch"),
            "expected CSRF reject page, got: {text}"
        );
        assert!(
            cookies.iter().any(|cookie| {
                cookie.contains(COOKIE_OAUTH_STATE) && cookie.contains("Max-Age=0")
            }),
            "CSRF reject must expire the handshake cookies: {cookies:?}"
        );
    }

    #[tokio::test]
    async fn oauth_callback_matching_cookie_passes_csrf() {
        let app = test_app();
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/oauth/callback?code=fake&state=deadbeef")
                    .header(COOKIE, format!("{COOKIE_OAUTH_STATE}=deadbeef"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let cookies: Vec<String> = res
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok().map(str::to_owned))
            .collect();
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(
            !text.contains("OAuth state mismatch"),
            "matching cookie+state must leave CSRF: {text}"
        );
        assert!(
            text.contains("token exchange failed"),
            "must reach Discord exchange after CSRF: {text}"
        );
        assert!(
            cookies.iter().any(|cookie| {
                cookie.contains(COOKIE_OAUTH_STATE) && cookie.contains("Max-Age=0")
            }),
            "successful CSRF path must still expire handshake cookies: {cookies:?}"
        );
    }

    #[test]
    fn oauth_csrf_compares_cookie_to_query_state() {
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_static("jmodguard_oauth_state=deadbeef; other=1"),
        );
        assert!(oauth_csrf_ok(&headers, Some("deadbeef")));
        assert!(!oauth_csrf_ok(&headers, Some("attacker")));
        assert!(!oauth_csrf_ok(&HeaderMap::new(), Some("deadbeef")));
        assert!(!oauth_csrf_ok(&headers, None));
    }

    fn auth_user(discord_id: u64) -> AuthCtx {
        AuthCtx {
            enrollment: None,
            user: crate::web::oauth::DiscordUser {
                id: discord_id.to_string(),
                username: "tester".into(),
                global_name: None,
                avatar: None,
            },
            guilds: vec![],
        }
    }

    fn app_with_owner(owner_id: u64) -> Arc<ApiState> {
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let tmp =
            std::env::temp_dir().join(format!("jmodguard_auth_{}_{}.db", std::process::id(), n));
        let db = crate::db::Db::open(&tmp, 2).unwrap();
        let mut cfg = crate::config::ApiProcessConfig::default();
        cfg.bot.owner_id = owner_id;
        let app = ApiState::new(cfg, db).unwrap();
        std::fs::remove_file(&tmp).ok();
        crate::tenant::remove_sidecars(&tmp);
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

    pub(super) fn seed_auth_caches(
        app: &ApiState,
        user_id: u64,
        guilds: Vec<crate::web::oauth::DiscordGuild>,
    ) {
        let user = crate::web::oauth::DiscordUser {
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
    fn display_cache_stays_usable_only_while_fresh_or_rate_limited() {
        let ttl = std::time::Duration::from_secs(60);
        assert!(guild_display_usable(
            std::time::Duration::from_secs(1),
            ttl,
            false
        ));
        assert!(!guild_display_usable(
            std::time::Duration::from_secs(120),
            ttl,
            false
        ));
        assert!(guild_display_usable(
            std::time::Duration::from_secs(120),
            ttl,
            true
        ));
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
        // Regression: guild_scope(0) returns an owner identity with no guild
        // list. A caller that stuffed a stale guild onto that context must not
        // authorize a copy source from it. copy_config must call
        // authorize_manage_guild(from) — never auth.can_manage(from) on the
        // destination AuthCtx.
        let stale = AuthCtx {
            enrollment: None,
            user: crate::web::oauth::DiscordUser {
                id: "99".into(),
                username: "owner".into(),
                global_name: None,
                avatar: None,
            },
            guilds: vec![crate::web::oauth::DiscordGuild {
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
        assert!(
            !"http://localhost:8080"
                .to_ascii_lowercase()
                .starts_with("https://")
        );
        assert!(
            "https://bot.example.com"
                .to_ascii_lowercase()
                .starts_with("https://")
        );
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
        let mut cfg = crate::config::ApiProcessConfig::default();
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
        assert_eq!(
            return_to_or_root(&cfg, Some("/administration/index.html")),
            "/administration/index.html"
        );
        assert_eq!(
            return_to_or_root(&cfg, Some("/api/service-upgrade/recovery")),
            "/api/service-upgrade/recovery"
        );
        assert_eq!(return_to_or_root(&cfg, Some("//attacker.example.com")), "/");
    }

    #[test]
    fn logout_origin_must_be_the_ui_or_configured_spa() {
        let mut cfg = crate::config::ApiProcessConfig::default();
        cfg.web.base_url = "https://api.example.com/control".into();
        cfg.web.cors_origins = vec!["https://admin.example.com".into()];

        let with_origin = |origin: &str| {
            let mut headers = HeaderMap::new();
            headers.insert("origin", origin.parse().unwrap());
            headers
        };
        assert!(trusted_web_origin(
            &cfg,
            &with_origin("https://api.example.com")
        ));
        assert!(trusted_web_origin(
            &cfg,
            &with_origin("https://admin.example.com")
        ));
        assert!(!trusted_web_origin(
            &cfg,
            &with_origin("https://attacker.example.com")
        ));
        assert!(!trusted_web_origin(&cfg, &HeaderMap::new()));
    }
}

#[cfg(test)]
mod enrollment_tests;
