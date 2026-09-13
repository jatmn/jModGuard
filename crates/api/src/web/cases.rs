//! Private selected-case and evidence administration for an enrolled guild.
use super::state::ApiState;
use super::{ApiError, ApiResult, AuthCtx, authorize_manage_guild};
use crate::db::guild_data::CaseId;
use crate::tenant::{self, TenantError};
use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use serde::Deserialize;
use std::sync::Arc;
use tower_sessions::Session;

#[derive(Deserialize)]
pub struct CasePage {
    after: Option<String>,
    limit: Option<u32>,
    #[serde(default)]
    removed: bool,
}

#[derive(Deserialize)]
pub struct NoteBody {
    entry_id: String,
    text: String,
}

#[derive(Deserialize)]
pub struct CaseDeleteRolesBody {
    roles: Vec<String>,
    revision: String,
}

async fn scoped(
    app: &Arc<ApiState>,
    session: &Session,
    headers: &HeaderMap,
    guild_id: u64,
) -> ApiResult<(AuthCtx, crate::db::GuildEnrollment)> {
    let auth = authorize_manage_guild(app, session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, headers)?;
    let enrollment = super::authorized_enrollment(&auth, guild_id)?;
    Ok((auth, enrollment))
}

fn can_delete_cases(auth: &AuthCtx, guild_id: u64) -> bool {
    auth.guilds
        .iter()
        .find(|guild| guild.id == guild_id.to_string())
        .is_some_and(|guild| guild.administrator_permission())
}

async fn live(
    app: &ApiState,
    enrollment: crate::db::GuildEnrollment,
) -> ApiResult<(
    crate::db::guild_data::GuildDataStore,
    crate::db::guild_data::GuildDataScope,
)> {
    Ok(app.tenant.live(enrollment).await?)
}

fn parse_case_id(id: &str) -> ApiResult<CaseId> {
    CaseId::try_from(id.to_owned()).map_err(|_| ApiError(anyhow::anyhow!("invalid case id")))
}

pub async fn list_cases(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path(guild_id): Path<u64>,
    Query(page): Query<CasePage>,
) -> ApiResult<Json<serde_json::Value>> {
    let (auth, enrollment) = scoped(&app, &session, &headers, guild_id).await?;
    let (store, scope) = live(&app, enrollment).await?;
    let after = match page.after.as_deref() {
        Some(id) => Some(parse_case_id(id)?),
        None => None,
    };
    let limit = page
        .limit
        .unwrap_or(50)
        .clamp(1, tenant::case_limits().max_page_size);
    let ids = if page.removed {
        if !can_delete_cases(&auth, guild_id) {
            return Err(ApiError(anyhow::anyhow!(
                "case cleanup requires Administrator permission"
            )));
        }
        store
            .removed_case_ids(scope, after, limit, tenant::case_limits())
            .await?
    } else {
        store
            .case_ids(scope, after, limit, tenant::case_limits())
            .await?
    };
    Ok(Json(serde_json::json!({
        "cases": ids.into_iter().map(|id| id.as_str().to_owned()).collect::<Vec<_>>(),
        "can_delete": can_delete_cases(&auth, guild_id),
    })))
}

pub async fn get_case(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path((guild_id, case_id)): Path<(u64, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let (auth, enrollment) = scoped(&app, &session, &headers, guild_id).await?;
    let (store, scope) = live(&app, enrollment).await?;
    let id = parse_case_id(&case_id)?;
    let Some(data) = store
        .case(scope.clone(), id.clone(), tenant::case_limits())
        .await?
    else {
        return Err(ApiError(anyhow::anyhow!("case not found")));
    };
    Ok(Json(serde_json::json!({
        "id": case_id,
        "case": data,
        "can_delete": can_delete_cases(&auth, guild_id),
    })))
}

pub async fn list_history(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path((guild_id, case_id)): Path<(u64, String)>,
    Query(page): Query<CasePage>,
) -> ApiResult<Json<serde_json::Value>> {
    let (_, enrollment) = scoped(&app, &session, &headers, guild_id).await?;
    let (store, scope) = live(&app, enrollment).await?;
    let case = parse_case_id(&case_id)?;
    let after = page.after.and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    let limit = page
        .limit
        .unwrap_or(32)
        .clamp(1, tenant::history_limits().entry.max_page_size);
    let refs = store
        .case_history_ids(
            scope.clone(),
            case.clone(),
            after,
            limit,
            tenant::history_limits(),
        )
        .await?;
    let mut entries = Vec::new();
    for item in refs {
        if let Some(data) = store
            .case_history_entry(
                scope.clone(),
                case.clone(),
                item.id.clone(),
                tenant::history_limits(),
            )
            .await?
        {
            entries.push(serde_json::json!({
                "id": item.id.as_str(),
                "sequence": item.sequence.to_string(),
                "entry": data,
            }));
        }
    }
    Ok(Json(serde_json::json!({ "history": entries })))
}

pub async fn add_note(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path((guild_id, case_id)): Path<(u64, String)>,
    Json(body): Json<NoteBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let (auth, enrollment) = scoped(&app, &session, &headers, guild_id).await?;
    let (store, scope) = live(&app, enrollment).await?;
    let text = body.text.trim();
    if text.is_empty() {
        return Err(ApiError(anyhow::anyhow!("note cannot be empty")));
    }
    let actor = i64::try_from(auth.discord_user_id()).unwrap_or(0);
    let recorded = store
        .append_case_note(
            scope,
            parse_case_id(&case_id)?,
            parse_case_id(&body.entry_id)?,
            (actor > 0).then_some(actor),
            text.to_owned(),
            tenant::history_limits(),
        )
        .await?;
    Ok(Json(
        serde_json::json!({ "result": format!("{recorded:?}") }),
    ))
}

pub async fn delete_case(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path((guild_id, case_id)): Path<(u64, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let (auth, enrollment) = scoped(&app, &session, &headers, guild_id).await?;
    if !can_delete_cases(&auth, guild_id) {
        return Err(ApiError(anyhow::anyhow!(
            "you cannot remove cases in this server (needs Administrator permission or server ownership)"
        )));
    }
    let (store, scope) = live(&app, enrollment).await?;
    let id = parse_case_id(&case_id)?;
    let removed = store.remove_case(scope.clone(), id.clone()).await?;
    // Journal removal is the user-visible delete. Object-backend cleanup is a
    // separate worker step; pending jobs remain retryable. Do not map worker I/O
    // failure onto the administrator-facing delete outcome.
    let cleanup_pending = match app.tenant.cleanup_owned_evidence(&store, &scope).await {
        Ok(()) => false,
        Err(error) => {
            tracing::warn!(
                guild_id,
                case_id = %id.as_str(),
                "case record removed; private evidence cleanup is still pending: {error:#}"
            );
            true
        }
    };
    let mut backup_cleanup_pending = false;
    if (removed || store.case_removed(scope.clone(), id.clone()).await?)
        && let Err(error) = app.tenant.record_case_cleanup(&scope, &id).await
    {
        backup_cleanup_pending = true;
        tracing::warn!(
            guild_id,
            case_id = %id.as_str(),
            "backup cleanup intent not recorded after case removal: {error:#}"
        );
    }
    Ok(Json(serde_json::json!({
        "removed": removed,
        "cleanup_pending": cleanup_pending || backup_cleanup_pending,
        "discord_action_reversed": false,
    })))
}

pub async fn list_evidence(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path((guild_id, case_id)): Path<(u64, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let (_, enrollment) = scoped(&app, &session, &headers, guild_id).await?;
    let (store, scope) = live(&app, enrollment).await?;
    let items = store
        .case_evidence_index(scope, parse_case_id(&case_id)?, tenant::evidence_limits())
        .await?;
    Ok(Json(serde_json::json!({
        "evidence": items.into_iter().map(|item| serde_json::json!({
            "id": item.job.id.as_str(),
            "bytes": item.job.reserved_bytes,
            "sha256": item.sha256,
            "channel_id": item.origin.channel_id.to_string(),
            "message_id": item.origin.message_id.to_string(),
            "attachment_id": item.origin.attachment_id.to_string(),
        })).collect::<Vec<_>>(),
    })))
}

pub async fn get_evidence(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path((guild_id, case_id, evidence_id)): Path<(u64, String, String)>,
) -> ApiResult<Response> {
    let (_, enrollment) = scoped(&app, &session, &headers, guild_id).await?;
    let Some((_, bytes)) = app
        .tenant
        .read_available_evidence(
            enrollment,
            parse_case_id(&case_id)?,
            parse_case_id(&evidence_id)?,
        )
        .await?
    else {
        return Err(ApiError(anyhow::anyhow!("evidence not found")));
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_DISPOSITION, "attachment")
        .header(header::CACHE_CONTROL, "private, no-store")
        .body(Body::from(bytes))
        .map_err(|error| ApiError(anyhow::anyhow!(error)))
}

pub async fn get_case_delete_roles(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path(guild_id): Path<u64>,
) -> ApiResult<Json<serde_json::Value>> {
    let (auth, enrollment) = scoped(&app, &session, &headers, guild_id).await?;
    let (store, scope) = live(&app, enrollment).await?;
    let settings = store.tenant_settings(scope).await?;
    Ok(Json(serde_json::json!({
        "roles": settings.case_delete_roles.into_iter().map(|id| id.to_string()).collect::<Vec<_>>(),
        "revision": settings.revision,
        "can_delete": can_delete_cases(&auth, guild_id),
        "delegated_roles_require_member_scope": true,
    })))
}

pub async fn put_case_delete_roles(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path(guild_id): Path<u64>,
    Json(body): Json<CaseDeleteRolesBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let (auth, enrollment) = scoped(&app, &session, &headers, guild_id).await?;
    if !can_delete_cases(&auth, guild_id) {
        return Err(ApiError(anyhow::anyhow!(
            "you cannot change case-removal roles (needs Administrator permission or server ownership)"
        )));
    }
    let (store, scope) = live(&app, enrollment).await?;
    let mut roles = Vec::new();
    for raw in body.roles {
        let id = raw
            .parse::<i64>()
            .map_err(|_| ApiError(anyhow::anyhow!("invalid role id")))?;
        roles.push(id);
    }
    let revision = store
        .set_case_delete_roles(scope, &body.revision, roles)
        .await
        .map_err(ApiError)?;
    Ok(Json(serde_json::json!({ "revision": revision })))
}

pub async fn case_count(
    app: &ApiState,
    enrollment: crate::db::GuildEnrollment,
) -> ApiResult<Option<i64>> {
    match app.tenant.live(enrollment).await {
        Ok((store, scope)) => Ok(Some(store.case_count(scope, tenant::case_limits()).await?)),
        Err(error) => {
            if matches!(
                error.downcast_ref::<TenantError>(),
                Some(TenantError::Unplaced | TenantError::Offline | TenantError::Membership)
            ) {
                Ok(None)
            } else {
                Err(ApiError(error))
            }
        }
    }
}
