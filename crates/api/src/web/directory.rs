//! Owner-only server directory: every server the bot is or was connected to,
//! who added the bot, and owner-initiated departure. The session-owner gate is
//! the only authorization; no Discord guild list is fetched for it. Directory
//! rows are display facts and never authorize guild access.
use super::state::ApiState;
use super::{session_owner, trusted_web_origin};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use jmodguard::bot_actions::Action;
use jmodguard::db::{self, GuildEnrollment};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tower_sessions::Session;

/// Directory responses stay bounded; a deployment beyond the limit keeps
/// every active server (sorted first) and drops the oldest departed history.
const DIRECTORY_LIMIT: i64 = 1000;

fn error(status: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error": message})))
}

async fn owner(
    app: &Arc<ApiState>,
    session: &Session,
) -> Result<String, (StatusCode, Json<Value>)> {
    match session_owner(app, session).await {
        Ok(auth) => Ok(auth.user.id),
        Err(failure) if failure.0.is::<super::SessionStoreUnavailable>() => Err(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "session store unavailable",
        )),
        Err(failure) if failure.0.is::<super::AuthLost>() => Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"discord authorization failed", "code":"auth_lost"})),
        )),
        Err(failure) if failure.0.is::<super::NotAuthenticated>() => {
            Err(error(StatusCode::UNAUTHORIZED, "sign in required"))
        }
        Err(_) => Err(error(StatusCode::FORBIDDEN, "bot owner required")),
    }
}

fn origin(app: &ApiState, headers: &HeaderMap) -> Result<(), (StatusCode, Json<Value>)> {
    if trusted_web_origin(&app.cfg, headers) {
        Ok(())
    } else {
        Err(error(StatusCode::FORBIDDEN, "untrusted request origin"))
    }
}

fn guild_id(raw: u64) -> Result<i64, (StatusCode, Json<Value>)> {
    i64::try_from(raw)
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| error(StatusCode::BAD_REQUEST, "invalid guild id"))
}

/// Owner-only directory of every server the bot is or was connected to.
/// POST read: a cached GET must never retain a directory snapshot.
pub async fn overview(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    owner(&app, &session).await?;
    origin(&app, &headers)?;
    let servers = db::owner_guild_directory(&app.db, DIRECTORY_LIMIT)
        .await
        .map_err(|failure| {
            tracing::warn!("owner directory read failed: {failure:#}");
            error(StatusCode::SERVICE_UNAVAILABLE, "directory unavailable")
        })?;
    Ok(Json(json!({ "servers": servers })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaveBody {
    request_id: String,
}

/// Owner-only departure request. Admission captures the current active
/// enrollment; the bot re-verifies that membership inside its write
/// transaction before dispatching the leave.
pub async fn leave(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path(guild): Path<u64>,
    Json(body): Json<LeaveBody>,
) -> Result<(StatusCode, Json<db::BotAction>), (StatusCode, Json<Value>)> {
    let owner_id = owner(&app, &session).await?;
    origin(&app, &headers)?;
    let guild = guild_id(guild)?;
    let enrollment = read_active_enrollment(&app, guild).await?;
    let actor = owner_id
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| error(StatusCode::FORBIDDEN, "bot owner required"))?;
    let (_, job_body) = app
        .commit_json(db::ControlOp::EnqueueBotAction {
            enrollment,
            actor,
            request: body.request_id.clone(),
            action: Action::LeaveGuild {},
            now: chrono::Utc::now().timestamp(),
            limits: app.cfg.actions.clone(),
        })
        .await
        .map_err(|failure| {
            if failure.is::<db::EnrollmentChanged>() {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": "Server membership changed. Reload before making a new request.",
                        "code": "action_enrollment_changed",
                    })),
                );
            }
            if let Some(key_error) = failure
                .downcast_ref::<jmodguard::bot_actions::RequestKeyError>()
            {
                let (status, code) = match key_error {
                    jmodguard::bot_actions::RequestKeyError::EnrollmentChanged => {
                        (StatusCode::CONFLICT, "action_enrollment_changed")
                    }
                    jmodguard::bot_actions::RequestKeyError::Expired => {
                        (StatusCode::GONE, "action_request_expired")
                    }
                    jmodguard::bot_actions::RequestKeyError::Invalid => {
                        (StatusCode::BAD_REQUEST, "action_request_invalid")
                    }
                };
                return (
                    status,
                    Json(json!({ "error": key_error.to_string(), "code": code })),
                );
            }
            tracing::warn!("owner leave admission failed: {failure:#}");
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Could not queue action. Check pending actions and retry using the same request ID.",
            )
        })?;
    let job: db::BotAction = serde_json::from_value(job_body)
        .map_err(|_| error(StatusCode::SERVICE_UNAVAILABLE, "invalid queued action"))?;
    Ok((StatusCode::ACCEPTED, Json(job)))
}

/// Owner-only receipt read for a queued departure (POST so no shared cache
/// can retain a terminal state). The read binds to the guild's current active
/// enrollment like every other dashboard queue read; once the bot departs,
/// the receipt is no longer served and the directory shows the departure.
pub async fn receipt(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path((guild, action_id)): Path<(u64, String)>,
) -> Result<Json<db::BotAction>, (StatusCode, Json<Value>)> {
    owner(&app, &session).await?;
    origin(&app, &headers)?;
    let guild = guild_id(guild)?;
    let enrollment = read_active_enrollment(&app, guild).await?;
    let job = db::enrolled_bot_action(&app.db, &enrollment, &action_id)
        .await
        .map_err(|failure| {
            tracing::warn!("owner receipt read failed: {failure:#}");
            error(StatusCode::SERVICE_UNAVAILABLE, "receipt unavailable")
        })?
        // The owner directory surfaces departures only; other queue rows stay
        // on their guild dashboards.
        .filter(|job| matches!(job.action, Action::LeaveGuild {}))
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "action not found"))?;
    Ok(Json(job))
}

async fn read_active_enrollment(
    app: &Arc<ApiState>,
    guild: i64,
) -> Result<GuildEnrollment, (StatusCode, Json<Value>)> {
    match db::guild_enrollment(&app.db, guild).await {
        Ok(Some(enrollment)) => Ok(enrollment),
        Ok(None) => Err(error(
            StatusCode::CONFLICT,
            "The bot has no active enrollment in this server.",
        )),
        Err(failure) => {
            tracing::warn!(guild, "guild access verification unavailable: {failure:#}");
            Err(error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Server access could not be verified. Try again shortly.",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_sessions::MemoryStore;

    fn app_with_owner(owner_id: u64) -> Arc<ApiState> {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let tmp = std::env::temp_dir().join(format!(
            "jmodguard_directory_{}_{}.db",
            std::process::id(),
            n
        ));
        let db = crate::db::Db::open(&tmp, 2).unwrap();
        let mut cfg = crate::config::ApiProcessConfig::default();
        cfg.bot.owner_id = owner_id;
        let app = ApiState::new(cfg, db).unwrap();
        std::fs::remove_file(&tmp).ok();
        crate::tenant::remove_sidecars(&tmp);
        app
    }

    async fn session_for_user(user_id: u64) -> Session {
        let store = Arc::new(MemoryStore::default());
        let session = Session::new(None, store, None);
        session
            .insert(super::super::SESSION_USER, user_id)
            .await
            .unwrap();
        session
            .insert(super::super::SESSION_TOKEN, "test-access-token")
            .await
            .unwrap();
        session
    }

    fn origin_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::ORIGIN,
            "http://localhost:8080".parse().unwrap(),
        );
        headers
    }

    fn key(label: &str) -> String {
        format!("{}_{}", chrono::Utc::now().timestamp(), label)
    }

    async fn overview_body(
        app: &Arc<ApiState>,
        session: &Session,
        headers: &HeaderMap,
    ) -> Result<serde_json::Value, (StatusCode, Json<Value>)> {
        let response = overview(State(Arc::clone(app)), session.clone(), headers.clone()).await;
        response.map(|Json(value)| value)
    }

    #[tokio::test]
    async fn owner_directory_rejects_everyone_but_the_configured_owner() {
        let app = app_with_owner(99);

        // Anonymous: a session without a user id is rejected before any read.
        let store = Arc::new(MemoryStore::default());
        let anonymous = Session::new(None, store, None);
        assert_eq!(
            overview_body(&app, &anonymous, &origin_headers())
                .await
                .unwrap_err()
                .0,
            StatusCode::UNAUTHORIZED
        );

        // Non-owner: rejected even with a valid session.
        let non_owner = session_for_user(42).await;
        let failure = overview_body(&app, &non_owner, &origin_headers())
            .await
            .unwrap_err();
        assert_eq!(failure.0, StatusCode::FORBIDDEN);
        assert_eq!(failure.1.0["error"], "bot owner required");

        // Missing or untrusted origin: rejected for everyone.
        let owner = session_for_user(99).await;
        assert_eq!(
            overview_body(&app, &owner, &HeaderMap::new())
                .await
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
        let mut foreign = HeaderMap::new();
        foreign.insert(
            axum::http::header::ORIGIN,
            "https://evil.test".parse().unwrap(),
        );
        assert_eq!(
            overview_body(&app, &owner, &foreign).await.unwrap_err().0,
            StatusCode::FORBIDDEN
        );

        // Owner with a trusted origin: allowed.
        let body = overview_body(&app, &owner, &origin_headers())
            .await
            .unwrap();
        assert_eq!(body["servers"], json!([]));
    }

    #[tokio::test]
    async fn owner_leave_checks_the_origin_before_any_data_access() {
        let app = app_with_owner(99);
        let owner = session_for_user(99).await;
        let mut foreign = origin_headers();
        foreign.insert(
            axum::http::header::ORIGIN,
            "https://evil.test".parse().unwrap(),
        );
        let failure = leave(
            State(Arc::clone(&app)),
            owner,
            foreign,
            Path(55),
            Json(LeaveBody {
                request_id: key("a"),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(failure.0, StatusCode::FORBIDDEN);
        assert_eq!(failure.1.0["error"], "untrusted request origin");
    }
}
