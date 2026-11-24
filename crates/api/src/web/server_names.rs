//! Captured-enrollment read of the bot's role and channel names for the rule
//! editor pickers (#621). The API never asks Discord.
use super::state::ApiState;
use super::{ApiResult, GuildAccess, authorize_guild};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use std::sync::Arc;
use tower_sessions::Session;

pub async fn server_names(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Path(guild_id): Path<u64>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = authorize_guild(&app, &session, guild_id, GuildAccess::Settings).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let enrollment = super::authorized_enrollment(&auth, guild_id)?;
    let (store, scope) = app.tenant.live(enrollment).await?;
    let mut names = serde_json::to_value(store.server_names(scope).await?)?;
    names["media_preset_channel_kinds"] =
        serde_json::json!(jmodguard::custom_rules::MEDIA_PRESET_CHANNEL_KINDS);
    Ok(Json(names))
}
