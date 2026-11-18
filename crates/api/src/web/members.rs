//! Captured-enrollment member directory reads and administrator sync controls.
use super::api;
use super::list_controls::{ListQuery, SortDirection, decode_cursor, encode_cursor};
use super::state::ApiState;
use super::{ApiError, ApiResult, AuthCtx, GuildAccess, authorize_guild};
use crate::bot_actions::{Action, MemberSyncOperation};
use crate::db;
use crate::db::guild_data::{
    GuildDataScope, MemberListAnchor, MemberProfile, MemberQuery, MemberSort, MemberStatusFilter,
    MemberSyncStatus,
};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tower_sessions::Session;

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MemberSortParam {
    Name,
    UserId,
    Joined,
    Status,
    // Keep the previous sort values usable by an already-open dashboard.
    JoinedNewest,
    JoinedOldest,
}

#[derive(Deserialize)]
pub struct MembersQuery {
    #[serde(flatten)]
    list: ListQuery<MemberSortParam, MemberStatusFilter>,
    joined_from: Option<String>,
    joined_to: Option<String>,
    limit: Option<u32>,
}

#[derive(Serialize)]
pub struct MembersResponse {
    members: Vec<MemberProfile>,
    next_cursor: Option<String>,
}

impl MembersQuery {
    fn validate(mut self) -> ApiResult<(MemberQuery, SortDirection)> {
        self.list.from = self.list.from.or(self.joined_from);
        self.list.to = self.list.to.or(self.joined_to);
        let default_dir = match self.list.sort {
            Some(
                MemberSortParam::JoinedOldest
                | MemberSortParam::Name
                | MemberSortParam::UserId
                | MemberSortParam::Status,
            ) => SortDirection::Asc,
            _ => SortDirection::Desc,
        };
        let controls = self.list.validate(MemberSortParam::Joined, default_dir)?;
        let sort = match controls.sort {
            MemberSortParam::Name => MemberSort::Name,
            MemberSortParam::UserId => MemberSort::UserId,
            MemberSortParam::Joined
            | MemberSortParam::JoinedNewest
            | MemberSortParam::JoinedOldest => MemberSort::Joined,
            MemberSortParam::Status => MemberSort::Status,
        };
        if controls
            .q
            .as_ref()
            .is_some_and(|q| q.len() > 128 || q.chars().count() > 120)
        {
            return Err(ApiError(anyhow::anyhow!("search is too long")));
        }
        if controls.subject.is_some() || controls.actor.is_some() {
            return Err(ApiError(anyhow::anyhow!("use member search for user IDs")));
        }
        let after = controls
            .after
            .as_deref()
            .map(|raw| decode_cursor::<_, MemberListAnchor>(raw, "members", sort, controls.dir))
            .transpose()?;
        if after.as_ref().is_some_and(|anchor| !anchor.matches(sort)) {
            return Err(ApiError(anyhow::anyhow!("invalid list cursor")));
        }
        Ok((
            MemberQuery {
                q: controls.q,
                status: controls.outcome.unwrap_or(MemberStatusFilter::Current),
                joined_from_ms: controls.from_ms,
                joined_to_ms: controls.until_ms.map(|until| until - 1),
                sort,
                descending: controls.dir == SortDirection::Desc,
                after,
                limit: self.limit.unwrap_or(50).clamp(1, 100),
            },
            controls.dir,
        ))
    }
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
    need: GuildAccess,
) -> ApiResult<(AuthCtx, db::GuildEnrollment)> {
    let auth = authorize_guild(app, session, guild_id, need).await?;
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

pub async fn list_members(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path(guild_id): Path<u64>,
    Query(query): Query<MembersQuery>,
) -> ApiResult<Json<MembersResponse>> {
    let (_, enrollment) =
        scoped(&app, &session, &headers, guild_id, GuildAccess::Dashboard).await?;
    let (query, dir) = query.validate()?;
    let sort = query.sort;
    let (store, scope) = app.tenant.live(enrollment).await?;
    let page = store.list_members(scope, query).await?;
    let next_cursor = page
        .next_cursor
        .map(|anchor| encode_cursor("members", sort, dir, anchor))
        .transpose()?;
    Ok(Json(MembersResponse {
        members: page.members,
        next_cursor,
    }))
}

pub async fn get_member(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path((guild_id, user_id)): Path<(u64, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let (_, enrollment) =
        scoped(&app, &session, &headers, guild_id, GuildAccess::Dashboard).await?;
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
    let (_, enrollment) =
        scoped(&app, &session, &headers, guild_id, GuildAccess::Dashboard).await?;
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
    let (_, enrollment) =
        scoped(&app, &session, &headers, guild_id, GuildAccess::Dashboard).await?;
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
    let (auth, enrollment) =
        scoped(&app, &session, &headers, guild_id, GuildAccess::Manage).await?;
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
