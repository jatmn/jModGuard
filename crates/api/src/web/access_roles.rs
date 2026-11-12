//! Per-guild Discord role -> Web UI access level mappings. Only Manage Server
//! (or the guild owner) may read or change them; mapped roles never can.
use super::state::ApiState;
use super::{ApiError, ApiResult, authorize_manage_guild};
use crate::db::guild_data::{AccessLevel, AccessRole};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use serde::Deserialize;
use std::sync::Arc;
use tower_sessions::Session;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessRoleWire {
    role_id: String,
    level: AccessLevel,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessRolesBody {
    roles: Vec<AccessRoleWire>,
    revision: String,
}

fn roles_wire(roles: Vec<AccessRole>) -> Vec<serde_json::Value> {
    roles
        .into_iter()
        .map(|role| serde_json::json!({ "role_id": role.role_id.to_string(), "level": role.level }))
        .collect()
}

pub async fn get_access_roles(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path(guild_id): Path<u64>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = authorize_manage_guild(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let enrollment = super::authorized_enrollment(&auth, guild_id)?;
    let (store, scope) = app.tenant.live(enrollment).await?;
    let settings = store.tenant_settings(scope).await?;
    Ok(Json(serde_json::json!({
        "roles": roles_wire(settings.access_roles),
        "revision": settings.revision,
        "ttl_secs": super::MEMBER_ROLES_TTL.as_secs(),
    })))
}

pub async fn put_access_roles(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path(guild_id): Path<u64>,
    Json(body): Json<AccessRolesBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = authorize_manage_guild(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let enrollment = super::authorized_enrollment(&auth, guild_id)?;
    if body.roles.len() > 32 {
        return Err(ApiError(anyhow::anyhow!(
            "at most 32 access roles are allowed"
        )));
    }
    let mut roles = Vec::new();
    for role in body.roles {
        let role_id = role
            .role_id
            .parse::<i64>()
            .ok()
            .filter(|id| *id > 0 && u64::try_from(*id).ok() != Some(guild_id))
            .ok_or_else(|| ApiError(anyhow::anyhow!("invalid role id")))?;
        roles.push(AccessRole {
            role_id,
            level: role.level,
        });
    }
    let scope = crate::db::ConfigurationScope::Guild(enrollment.clone()).to_wire();
    let actor = i64::try_from(auth.discord_user_id()).unwrap_or(0);
    let _ = app.tenant.live(enrollment).await?;
    let (unchanged, revision_body) = app
        .commit_json_actor(
            actor,
            crate::db::ControlOp::SetAccessRoles {
                scope,
                revision: body.revision,
                roles: roles.clone(),
            },
        )
        .await
        .map_err(ApiError::from)?;
    let revision: String = serde_json::from_value(revision_body).map_err(ApiError::from)?;
    let roles = roles_wire(roles);
    if !unchanged {
        tracing::info!(
            guild_id,
            actor,
            roles = %serde_json::json!(roles),
            "web ui access roles changed"
        );
    }
    Ok(Json(
        serde_json::json!({ "revision": revision, "roles": roles }),
    ))
}
