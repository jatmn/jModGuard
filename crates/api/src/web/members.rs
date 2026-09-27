//! Captured-enrollment member directory reads and administrator sync controls.
use super::api;
use super::state::ApiState;
use super::{ApiError, ApiResult, AuthCtx, authorize_manage_guild};
use crate::bot_actions::{Action, MemberSyncOperation};
use crate::db;
use crate::db::guild_data::{
    GuildDataScope, MemberQuery, MemberSort, MemberStatusFilter, MemberSyncStatus,
};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{NaiveDate, TimeZone, Utc};
use serde::Deserialize;
use std::sync::Arc;
use tower_sessions::Session;

#[derive(Deserialize)]
pub struct MembersQuery {
    q: Option<String>,
    status: Option<String>,
    joined_from: Option<String>,
    joined_to: Option<String>,
    sort: Option<String>,
    after: Option<String>,
    limit: Option<u32>,
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    after: Option<String>,
    limit: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncActionBody {
    request_id: String,
    operation: MemberSyncOperation,
}

async fn scoped(
    app: &Arc<ApiState>,
    session: &Session,
    headers: &HeaderMap,
    guild_id: u64,
) -> ApiResult<(AuthCtx, db::GuildEnrollment)> {
    let auth = authorize_manage_guild(app, session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, headers)?;
    let enrollment = super::authorized_enrollment(&auth, guild_id)?;
    Ok((auth, enrollment))
}

fn member_sync_invalid_operation() -> Response {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({
            "error": "this member import control is not available in the current import state",
            "code": "member_sync_invalid_operation"
        })),
    )
        .into_response()
}

/// Match WebUI enablement and `member_sync_action` store guards before queueing work.
fn validate_member_sync_operation(
    status: &MemberSyncStatus,
    operation: MemberSyncOperation,
) -> Option<Response> {
    let allowed = match operation {
        MemberSyncOperation::Pause => {
            matches!(status.status.as_str(), "queued" | "running" | "waiting")
        }
        MemberSyncOperation::Resume => status.status == "paused",
        MemberSyncOperation::Retry => status.allows_member_sync_retry(),
        MemberSyncOperation::Reconcile => {
            status.initial_completed_at.is_some() && status.status == "complete"
        }
    };
    if allowed {
        None
    } else {
        Some(member_sync_invalid_operation())
    }
}

async fn member_sync_bot_id(
    store: &db::guild_data::GuildDataStore,
    scope: GuildDataScope,
) -> ApiResult<i64> {
    if let Some(bot_id) = store.persisted_member_sync_bot_id(scope).await? {
        return Ok(bot_id);
    }
    // Pre-seed status does not compare against OAuth client id; any positive id is ignored until
    // the bot writes guild_member_seed.
    Ok(1)
}

fn parse_user_id(raw: &str) -> ApiResult<i64> {
    raw.parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| ApiError(anyhow::anyhow!("invalid user id")))
}

fn date_ms(raw: Option<String>, end: bool) -> ApiResult<Option<i64>> {
    raw.map(|value| {
        let date = NaiveDate::parse_from_str(&value, "%Y-%m-%d")
            .map_err(|_| ApiError(anyhow::anyhow!("invalid joined date")))?;
        let time = if end {
            date.and_hms_milli_opt(23, 59, 59, 999)
        } else {
            date.and_hms_milli_opt(0, 0, 0, 0)
        }
        .ok_or_else(|| ApiError(anyhow::anyhow!("invalid joined date")))?;
        Ok(Utc.from_utc_datetime(&time).timestamp_millis())
    })
    .transpose()
}

pub async fn list_members(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path(guild_id): Path<u64>,
    Query(query): Query<MembersQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let (_, enrollment) = scoped(&app, &session, &headers, guild_id).await?;
    let (store, scope) = app.tenant.live(enrollment).await?;
    let q = query.q.map(|value| value.trim().to_owned());
    if q.as_ref()
        .is_some_and(|value| value.chars().count() > 120 || value.len() > 128)
    {
        return Err(ApiError(anyhow::anyhow!("search is too long")));
    }
    if query.after.as_ref().is_some_and(|value| value.len() > 64) {
        return Err(ApiError(anyhow::anyhow!("cursor is too long")));
    }
    let status = match query.status.as_deref().unwrap_or("current") {
        "current" => MemberStatusFilter::Current,
        "banned" => MemberStatusFilter::Banned,
        "previously_banned" => MemberStatusFilter::PreviouslyBanned,
        "all" => MemberStatusFilter::All,
        _ => return Err(ApiError(anyhow::anyhow!("invalid member status"))),
    };
    let sort = match query.sort.as_deref().unwrap_or("joined_newest") {
        "joined_newest" => MemberSort::JoinedNewest,
        "joined_oldest" => MemberSort::JoinedOldest,
        _ => return Err(ApiError(anyhow::anyhow!("invalid member sort"))),
    };
    let page = store
        .list_members(
            scope,
            MemberQuery {
                q,
                status,
                joined_from_ms: date_ms(query.joined_from, false)?,
                joined_to_ms: date_ms(query.joined_to, true)?,
                sort,
                after: query.after,
                limit: query.limit.unwrap_or(50).clamp(1, 100),
            },
        )
        .await?;
    Ok(Json(serde_json::to_value(page)?))
}

pub async fn get_member(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path((guild_id, user_id)): Path<(u64, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let (_, enrollment) = scoped(&app, &session, &headers, guild_id).await?;
    let (store, scope) = app.tenant.live(enrollment).await?;
    let member = store
        .get_member(scope, parse_user_id(&user_id)?)
        .await?
        .ok_or_else(|| ApiError(anyhow::anyhow!("member not found")))?;
    Ok(Json(serde_json::to_value(member)?))
}

pub async fn member_history(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path((guild_id, user_id)): Path<(u64, String)>,
    Query(query): Query<HistoryQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let (_, enrollment) = scoped(&app, &session, &headers, guild_id).await?;
    if query.after.as_ref().is_some_and(|value| value.len() > 128) {
        return Err(ApiError(anyhow::anyhow!("cursor is too long")));
    }
    let (store, scope) = app.tenant.live(enrollment).await?;
    let page = store
        .member_history(
            scope,
            parse_user_id(&user_id)?,
            query.after,
            query.limit.unwrap_or(50).clamp(1, 100),
        )
        .await?;
    Ok(Json(serde_json::to_value(page)?))
}

pub async fn sync_status(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path(guild_id): Path<u64>,
) -> ApiResult<Json<serde_json::Value>> {
    let (_, enrollment) = scoped(&app, &session, &headers, guild_id).await?;
    let (store, scope) = app.tenant.live(enrollment).await?;
    let bot_id = member_sync_bot_id(&store, scope.clone()).await?;
    let status = store
        .member_sync_status(scope, bot_id, Utc::now().timestamp_millis())
        .await?;
    Ok(Json(serde_json::to_value(status)?))
}

pub async fn sync_action(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path(guild_id): Path<u64>,
    Json(body): Json<SyncActionBody>,
) -> ApiResult<Response> {
    let (auth, enrollment) = scoped(&app, &session, &headers, guild_id).await?;
    let (store, scope) = app.tenant.live(enrollment).await?;
    let bot_id = member_sync_bot_id(&store, scope.clone()).await?;
    let status = store
        .member_sync_status(scope, bot_id, Utc::now().timestamp_millis())
        .await?;
    if status.run_id.is_none() {
        return Ok((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "member import job is unavailable; wait for bot initialization or repair saved metadata",
                "code": "member_sync_missing_job"
            })),
        )
            .into_response());
    }
    if let Some(response) = validate_member_sync_operation(&status, body.operation) {
        return Ok(response);
    }
    Ok(api::enqueue_action(
        &app,
        guild_id,
        &auth,
        &body.request_id,
        Action::MemberSync {
            operation: body.operation,
        },
    )
    .await?
    .into_response())
}
