//! Private selected-case and evidence administration for an enrolled guild.
use super::list_controls::{
    ListQuery, SortDirection, check_cursor_context, cursor_context, decode_cursor, encode_cursor,
    within_list_deadline,
};
use super::state::ApiState;
use super::{ApiError, ApiResult, AuthCtx, GuildAccess, authorize_guild};
use crate::db::guild_data::{
    CaseAction, CaseEvidence, CaseFilter, CaseId, CaseListPosition, CaseListQuery, CaseOutcome,
    CaseSort,
};
use crate::tenant::{self, TenantError};
use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tower_sessions::Session;

const CASE_LIST: &str = "cases";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CaseCursor {
    context: String,
    position: CaseListPosition,
    legacy_id_order: bool,
}

#[derive(Deserialize)]
pub struct CasePage {
    limit: Option<u32>,
    #[serde(default)]
    include_total: bool,
    #[serde(default)]
    removed: bool,
    action: Option<CaseAction>,
    #[serde(flatten)]
    list: ListQuery<CaseSort, CaseOutcome>,
}

#[derive(Deserialize)]
pub struct CaseDetailPage {
    after: Option<String>,
    limit: Option<u32>,
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
    need: GuildAccess,
) -> ApiResult<(AuthCtx, crate::db::GuildEnrollment)> {
    let auth = authorize_guild(app, session, guild_id, need).await?;
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

/// Display names the member directory already recorded for these users, keyed by
/// string ID. Names are presentation only: a directory that is unavailable or has
/// not seen a user leaves that user out instead of failing the case read.
pub(super) async fn directory_names(
    store: &crate::db::guild_data::GuildDataStore,
    scope: crate::db::guild_data::GuildDataScope,
    ids: impl IntoIterator<Item = i64>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut ids: Vec<i64> = ids.into_iter().filter(|id| *id > 0).collect();
    ids.sort_unstable();
    ids.dedup();
    ids.truncate(crate::db::guild_data::DIRECTORY_PROFILE_LOOKUP_LIMIT);
    let profiles = store.member_profiles(scope, ids).await.unwrap_or_default();
    let present = |name: &Option<String>| name.clone().filter(|name| !name.is_empty());
    profiles
        .into_iter()
        .filter_map(|p| {
            // The Members display order: nickname, then global name, then username.
            let display_name = present(&p.nickname)
                .or_else(|| present(&p.global_name))
                .or_else(|| present(&p.username))?;
            let entry = serde_json::json!({
                "display_name": display_name,
                "username": present(&p.username),
            });
            Some((p.user_id, entry))
        })
        .collect()
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
    within_list_deadline(
        "Cases",
        list_cases_inner(&app, &session, &headers, guild_id, page),
    )
    .await
}

async fn list_cases_inner(
    app: &Arc<ApiState>,
    session: &Session,
    headers: &HeaderMap,
    guild_id: u64,
    page: CasePage,
) -> ApiResult<Json<serde_json::Value>> {
    let (auth, enrollment) =
        scoped(app, session, headers, guild_id, GuildAccess::Dashboard).await?;
    let mut limits = tenant::case_limits();
    if !page.removed {
        limits.max_page_size = crate::db::DASHBOARD_LIST_PAGE_LIMIT;
    }
    let limit = page.limit.unwrap_or(50).clamp(1, limits.max_page_size);
    let explicit_sort = page.list.sort.is_some() || page.list.dir.is_some();
    let list = page.list.validate(CaseSort::Date, SortDirection::Desc)?;
    let filter = CaseFilter {
        action: page.action,
        outcome: list.outcome,
        subject_id: list.subject,
        actor_id: list.actor,
    };
    let can_delete = can_delete_cases(&auth, guild_id);
    let (store, scope) = live(app, enrollment).await?;
    let mut summaries = std::collections::BTreeMap::new();
    let (ids, next_cursor, total) = if page.removed {
        if filter != CaseFilter::default()
            || list.q.is_some()
            || list.from_ms.is_some()
            || list.until_ms.is_some()
            || explicit_sort
        {
            return Err(ApiError(anyhow::anyhow!(
                "removed records cannot be filtered or sorted"
            )));
        }
        if !can_delete {
            return Err(ApiError(anyhow::anyhow!(
                "case cleanup requires Administrator permission"
            )));
        }
        let after = list.after.as_deref().map(parse_case_id).transpose()?;
        let (ids, next) = store
            .removed_case_page(scope.clone(), after, limit, limits)
            .await?;
        (ids, next.map(String::from), None)
    } else {
        let context = cursor_context((
            guild_id,
            &filter.action,
            &filter.outcome,
            filter.subject_id,
            filter.actor_id,
            list.q.as_deref().map(str::to_ascii_lowercase),
            list.from_ms,
            list.until_ms,
        ))?;
        let after = list
            .after
            .as_deref()
            .map(|token| {
                // The old endpoint used bare case IDs and always paged in ascending ID order.
                if !explicit_sort && let Ok(id) = parse_case_id(token) {
                    return Ok((CaseListPosition::from_case_id(id), true));
                }
                let cursor: CaseCursor = decode_cursor(token, CASE_LIST, list.sort, list.dir)?;
                check_cursor_context(&cursor.context, &context)?;
                if cursor.legacy_id_order && explicit_sort {
                    return Err(ApiError(anyhow::anyhow!(
                        "list cursor does not match the requested sort"
                    )));
                }
                Ok((cursor.position, cursor.legacy_id_order))
            })
            .transpose()?;
        // Start unsorted requests in legacy ID order. Existing opaque cursors
        // keep the order that was used to produce them.
        let legacy_id_order = after.as_ref().map_or(!explicit_sort, |(_, legacy)| *legacy);
        let page = store
            .list_cases(
                scope.clone(),
                CaseListQuery {
                    filter,
                    search: list.q,
                    from_ms: list.from_ms,
                    until_ms: list.until_ms,
                    sort: if legacy_id_order {
                        CaseSort::Case
                    } else {
                        list.sort
                    },
                    descending: !legacy_id_order && list.dir == SortDirection::Desc,
                    after: after.map(|(position, _)| position),
                    include_total: page.include_total,
                },
                limit,
                limits,
            )
            .await?;
        let next = page
            .next
            .map(|position| {
                encode_cursor(
                    CASE_LIST,
                    list.sort,
                    list.dir,
                    CaseCursor {
                        context,
                        position,
                        legacy_id_order,
                    },
                )
            })
            .transpose()?;
        let ids = page
            .entries
            .into_iter()
            .map(|(id, summary)| {
                summaries.insert(id.as_str().to_owned(), summary);
                id
            })
            .collect();
        (ids, next, page.total)
    };
    let people = summaries
        .values()
        .flat_map(|summary| [Some(summary.subject_id), summary.actor_id])
        .flatten()
        .collect::<Vec<_>>();
    let names = directory_names(&store, scope, people).await;
    Ok(Json(serde_json::json!({
        "cases": ids.into_iter().map(|id| id.as_str().to_owned()).collect::<Vec<_>>(),
        "summaries": summaries,
        "names": names,
        "can_delete": can_delete,
        "next_cursor": next_cursor,
        "total": total,
    })))
}

pub async fn get_case(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path((guild_id, case_id)): Path<(u64, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let (auth, enrollment) =
        scoped(&app, &session, &headers, guild_id, GuildAccess::Dashboard).await?;
    let (store, scope) = live(&app, enrollment).await?;
    let id = parse_case_id(&case_id)?;
    let Some(data) = store
        .case(scope.clone(), id.clone(), tenant::case_limits())
        .await?
    else {
        return Err(ApiError(anyhow::anyhow!("case not found")));
    };
    let authors = match &data.evidence {
        CaseEvidence::Text(items) => items.iter().map(|item| item.author_id).collect(),
        CaseEvidence::Unavailable => Vec::new(),
    };
    let people = [Some(data.subject_id), data.actor_id]
        .into_iter()
        .flatten()
        .chain(authors);
    let names = directory_names(&store, scope, people).await;
    Ok(Json(serde_json::json!({
        "id": case_id,
        "case": data,
        "names": names,
        "can_delete": can_delete_cases(&auth, guild_id),
    })))
}

pub async fn list_history(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path((guild_id, case_id)): Path<(u64, String)>,
    Query(page): Query<CaseDetailPage>,
) -> ApiResult<Json<serde_json::Value>> {
    let (_, enrollment) =
        scoped(&app, &session, &headers, guild_id, GuildAccess::Dashboard).await?;
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
    let mut actors = Vec::new();
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
            actors.extend(data.actor_id);
            entries.push(serde_json::json!({
                "id": item.id.as_str(),
                "sequence": item.sequence.to_string(),
                "entry": data,
            }));
        }
    }
    let names = directory_names(&store, scope, actors).await;
    Ok(Json(
        serde_json::json!({ "history": entries, "names": names }),
    ))
}

pub async fn add_note(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path((guild_id, case_id)): Path<(u64, String)>,
    Json(body): Json<NoteBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let (auth, enrollment) =
        scoped(&app, &session, &headers, guild_id, GuildAccess::Manage).await?;
    let scope = crate::db::ConfigurationScope::Guild(enrollment.clone()).to_wire();
    let _ = live(&app, enrollment).await?;
    let text = body.text.trim();
    if text.is_empty() {
        return Err(ApiError(anyhow::anyhow!("note cannot be empty")));
    }
    let actor = i64::try_from(auth.discord_user_id()).unwrap_or(0);
    let (_, recorded_body) = app
        .commit_json_actor(
            actor,
            crate::db::ControlOp::AppendCaseNote {
                scope,
                case_id: case_id.clone(),
                entry_id: body.entry_id.clone(),
                actor_id: (actor > 0).then_some(actor),
                text: text.to_owned(),
            },
        )
        .await?;
    let recorded: String = serde_json::from_value(recorded_body)?;
    Ok(Json(serde_json::json!({ "result": recorded })))
}

pub async fn delete_case(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path((guild_id, case_id)): Path<(u64, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let (auth, enrollment) =
        scoped(&app, &session, &headers, guild_id, GuildAccess::Manage).await?;
    if !can_delete_cases(&auth, guild_id) {
        return Err(ApiError(anyhow::anyhow!(
            "you cannot remove cases in this server (needs Administrator permission or server ownership)"
        )));
    }
    let scope = crate::db::ConfigurationScope::Guild(enrollment.clone()).to_wire();
    let actor = i64::try_from(auth.discord_user_id()).unwrap_or(0);
    let _ = live(&app, enrollment).await?;
    let (_, body) = app
        .commit_json_actor(actor, crate::db::ControlOp::RemoveCase { scope, case_id })
        .await?;
    Ok(Json(body))
}

pub async fn list_evidence(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path((guild_id, case_id)): Path<(u64, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let (_, enrollment) =
        scoped(&app, &session, &headers, guild_id, GuildAccess::Dashboard).await?;
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
    let (_, enrollment) =
        scoped(&app, &session, &headers, guild_id, GuildAccess::Dashboard).await?;
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
        .body(Body::from(bytes))
        .map_err(|error| ApiError(anyhow::anyhow!(error)))
}

pub async fn get_case_delete_roles(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path(guild_id): Path<u64>,
) -> ApiResult<Json<serde_json::Value>> {
    let (auth, enrollment) =
        scoped(&app, &session, &headers, guild_id, GuildAccess::Dashboard).await?;
    let (store, scope) = live(&app, enrollment).await?;
    let settings = store.tenant_settings(scope).await?;
    let can_delete = can_delete_cases(&auth, guild_id);
    let roles: Vec<_> = if can_delete {
        settings
            .case_delete_roles
            .into_iter()
            .map(|id| id.to_string())
            .collect()
    } else {
        Vec::new()
    };
    Ok(Json(serde_json::json!({
        "roles": roles,
        "revision": settings.revision,
        "can_delete": can_delete,
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
    let (auth, enrollment) =
        scoped(&app, &session, &headers, guild_id, GuildAccess::Manage).await?;
    if !can_delete_cases(&auth, guild_id) {
        return Err(ApiError(anyhow::anyhow!(
            "you cannot change case-removal roles (needs Administrator permission or server ownership)"
        )));
    }
    let scope = crate::db::ConfigurationScope::Guild(enrollment.clone()).to_wire();
    let actor = i64::try_from(auth.discord_user_id()).unwrap_or(0);
    let _ = live(&app, enrollment).await?;
    let mut roles = Vec::new();
    for raw in body.roles {
        let id = raw
            .parse::<i64>()
            .map_err(|_| ApiError(anyhow::anyhow!("invalid role id")))?;
        roles.push(id);
    }
    let (_, revision_body) = app
        .commit_json_actor(
            actor,
            crate::db::ControlOp::SetCaseDeleteRoles {
                scope,
                revision: body.revision,
                roles,
            },
        )
        .await
        .map_err(ApiError::from)?;
    let revision: String = serde_json::from_value(revision_body).map_err(ApiError::from)?;
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
