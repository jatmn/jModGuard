//! REST API handlers for the WebUI. Every guild-scoped endpoint verifies the
//! session can manage that guild. Writes bump durable `config_version` in the
//! DB and invalidate this process's cache; the bot (and API) poll
//! `config_version` to pick up cross-process changes within `config_poll_secs`.

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;
use std::sync::Arc;
use tower_sessions::Session;

use super::state::ApiState;
use super::{
    ApiError, ApiResult, AuthCtx, authorize_manage_guild, guild_scope, is_bot_owner,
    require_bot_owner,
};
use crate::db;
use crate::types::{CommandDef, GLOBAL_SCOPE, RuleDef};

/// Authorizes an endpoint that operates on a real Discord guild. Global
/// configuration (`guild_id = 0`) is intentionally handled by configuration
/// routes, but it must never reach Discord REST or guild-local audit tables.
async fn real_guild_scope(
    app: &Arc<ApiState>,
    session: &Session,
    guild_id: u64,
) -> ApiResult<AuthCtx> {
    if guild_id == 0 {
        return Err(ApiError(anyhow::anyhow!(
            "this operation requires a Discord server"
        )));
    }
    guild_scope(app, session, guild_id).await
}

// ---------------------------------------------------------------------------
// config bundle
// ---------------------------------------------------------------------------

// The old internal schema still has this field during the staged overhaul.
// It is not a supported chat-history control or part of the API contract.
fn settings_wire(settings: &impl serde::Serialize) -> serde_json::Value {
    let mut value = serde_json::json!(settings);
    if let Some(object) = value.as_object_mut() {
        object.remove("retention_days");
    }
    value
}

pub async fn guild_config(
    State(app): State<Arc<ApiState>>,
    session: Session,
    Path(guild_id): Path<u64>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    let snapshot = db::load_enrolled_web_config_snapshot(
        &app.db,
        super::configuration_scope(&app, &auth, guild_id)?,
    )
    .await?;

    // rules: effective + scope info
    let default_rules = crate::defaults::default_rules();
    let mut rules = Vec::new();
    for (i, rid) in crate::types::RULE_IDS.iter().enumerate() {
        let def = default_rules[i].clone();
        let guild_row = snapshot.guild_rules.get(*rid).cloned();
        let global_row = snapshot.global_rules.get(*rid).cloned();
        let effective = db::resolve(&guild_row, &global_row, &def);
        rules.push(serde_json::json!({
            "rule_id": rid,
            "name": def.name,
            "description": def.description,
            "effective": effective,
            "guild_override": guild_row,
            "global": global_row.unwrap_or(def),
        }));
    }

    // Runtime enforcement combines global and guild badwords. Return that
    // same set so global rows created in this UI remain inspectable and
    // removable through it.
    let badwords: Vec<_> = snapshot.badwords.iter().map(|w| w.with_status()).collect();

    // commands: union of guild + global rows, effective resolution
    let defaults = crate::defaults::default_commands();
    let mut commands: Vec<serde_json::Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (name, def) in &snapshot.global_commands {
        let guild_row = snapshot
            .guild_commands
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone());
        commands.push(serde_json::json!({
            "name": name,
            "scope": "global",
            "effective": db::resolve(&guild_row, &Some(def.clone()), def),
            "guild_override": guild_row,
            "global": def,
        }));
        seen.insert(name.to_lowercase());
    }
    for (name, def) in &snapshot.guild_commands {
        if seen.insert(name.to_lowercase()) {
            let global_row = snapshot
                .global_commands
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone());
            commands.push(serde_json::json!({
                "name": name,
                "scope": "guild",
                "effective": def,
                "guild_override": Some(def),
                "global": global_row,
            }));
        }
    }
    // built-in defaults that have no rows at either scope (rare)
    for def in defaults {
        let name = def.name.to_lowercase();
        if seen.contains(&name) {
            continue;
        }
        commands.push(serde_json::json!({
            "name": name,
            "scope": "default",
            "effective": def,
            "guild_override": null,
            "global": null,
        }));
    }

    let mut templates = Vec::new();
    for (event, def) in crate::defaults::default_templates() {
        let guild_row = snapshot.guild_templates.get(event).cloned();
        let global_row = snapshot.global_templates.get(event).cloned();
        templates.push(serde_json::json!({
            "event": event,
            "effective": db::resolve(&guild_row, &global_row, &def.to_string()),
            "guild_override": guild_row,
            "global": global_row.unwrap_or_else(|| def.to_string()),
        }));
    }

    let mut log_channels = Vec::new();
    for event in crate::types::EVENT_TYPES {
        let guild_row = snapshot.guild_log_channels.get(event).copied();
        let effective = guild_row.unwrap_or(0);
        // Strings so JS never coerces snowflakes through IEEE-754 Number.
        log_channels.push(serde_json::json!({
            "event": event,
            "effective": effective.to_string(),
            "guild_override": guild_row.map(|c| c.to_string()),
            "global": null,
        }));
    }

    Ok(Json(serde_json::json!({
        "enrollment_id": auth.enrollment.as_ref().map(|e| e.id.as_str()),
        "settings": settings_wire(&snapshot.settings.settings),
        "settings_override": settings_wire(&snapshot.settings.settings_override),
        "global_settings": settings_wire(&snapshot.settings.global_settings),
        "global_settings_override": settings_wire(&snapshot.settings.global_settings_override),
        "nickname_reconcile": snapshot.nickname_reconcile,
        "rules": rules,
        "badwords": badwords,
        "commands": commands,
        "templates": templates,
        "log_channels": log_channels,
        "event_types": crate::types::EVENT_TYPES,
        "rule_ids": crate::types::RULE_IDS,
    })))
}

// ---------------------------------------------------------------------------
// settings
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SettingsBody {
    #[serde(default, deserialize_with = "crate::types::de_settings_patch_field")]
    prefix: crate::types::SettingsPatchField<String>,
    #[serde(default, deserialize_with = "crate::types::de_settings_patch_field")]
    bot_name: crate::types::SettingsPatchField<String>,
    /// Explicit repair action for Discord↔SQLite nickname drift. Kept separate
    /// from ordinary saves so a stale form never re-applies a nickname.
    #[serde(default)]
    sync_nickname: bool,
}

pub async fn update_settings(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: axum::http::HeaderMap,
    Path(guild_id): Path<u64>,
    Json(body): Json<SettingsBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    if guild_id == 0 && body.bot_name.is_some() {
        return Err(ApiError(anyhow::anyhow!(
            "global bot_name is not supported; set the Discord nickname per server"
        )));
    }

    // Persist only the fields present in the body as a parent-relative
    // override. Values equal to the parent layer are stripped so global
    // inheritance keeps working.
    let nickname_queued = (body.bot_name.is_some() || body.sync_nickname) && guild_id != 0;
    let settings = db::patch_enrolled_settings(
        &app.db,
        super::configuration_scope(&app, &auth, guild_id)?,
        body.prefix,
        body.bot_name,
        body.sync_nickname,
    )
    .await?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "settings": settings_wire(&settings),
        "nickname_applied": null,
        "nickname_queued": nickname_queued,
        "nickname_error": null,
    })))
}

// ---------------------------------------------------------------------------
// rules
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct RuleBody {
    scope: Option<String>,
    data: RuleDef,
}

pub async fn get_rule(
    State(app): State<Arc<ApiState>>,
    session: Session,
    Path((guild_id, rule_id)): Path<(u64, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = guild_scope(&app, &session, guild_id).await?;
    if !crate::types::RULE_IDS.contains(&rule_id.as_str()) {
        return Err(ApiError(anyhow::anyhow!("unknown rule `{rule_id}`")));
    }
    let idx = crate::types::RULE_IDS
        .iter()
        .position(|r| *r == rule_id.as_str())
        .unwrap();
    let def = crate::defaults::default_rules()[idx].clone();
    let guild_override = db::get_rule(&app.db, guild_id as i64, &rule_id).await?;
    let global = db::get_rule(&app.db, GLOBAL_SCOPE, &rule_id).await?;
    let effective = db::resolve(&guild_override, &global, &def);
    Ok(Json(serde_json::json!({
        "effective": effective,
        "guild_override": guild_override,
        "global": global,
    })))
}

pub async fn put_rule(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: axum::http::HeaderMap,
    Path((guild_id, rule_id)): Path<(u64, String)>,
    Json(body): Json<RuleBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    if !crate::types::RULE_IDS.contains(&rule_id.as_str()) {
        return Err(ApiError(anyhow::anyhow!("unknown rule `{rule_id}`")));
    }
    let scope = scope_from(&app, &auth, &body.scope, guild_id)?;
    db::upsert_enrolled_rule(
        &app.db,
        super::configuration_write_scope(&app, &auth, guild_id, scope)?,
        &rule_id,
        &body.data,
    )
    .await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct ScopeBody {
    scope: Option<String>,
}

pub async fn delete_rule(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: axum::http::HeaderMap,
    Path((guild_id, rule_id)): Path<(u64, String)>,
    Json(body): Json<ScopeBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let scope = scope_from(&app, &auth, &body.scope, guild_id)?;
    db::delete_enrolled_rule(
        &app.db,
        super::configuration_write_scope(&app, &auth, guild_id, scope)?,
        &rule_id,
    )
    .await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub(crate) fn scope_from(
    app: &ApiState,
    auth: &AuthCtx,
    scope: &Option<String>,
    guild_id: u64,
) -> ApiResult<i64> {
    match scope.as_deref() {
        Some("global") => {
            // Same gate as path guild_id=0 — body scope must not bypass owner.
            require_bot_owner(app, auth)?;
            Ok(GLOBAL_SCOPE)
        }
        _ => Ok(guild_id as i64),
    }
}

// ---------------------------------------------------------------------------
// badwords
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct BadwordBody {
    scope: Option<String>,
    pattern: String,
    note: Option<String>,
}

pub async fn list_badwords(
    State(app): State<Arc<ApiState>>,
    session: Session,
    Path(guild_id): Path<u64>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let words = db::list_badwords(&app.db, guild_id as i64).await?;
    let words: Vec<_> = words.iter().map(|w| w.with_status()).collect();
    Ok(Json(serde_json::json!(words)))
}

pub async fn add_badword(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: axum::http::HeaderMap,
    Path(guild_id): Path<u64>,
    Json(body): Json<BadwordBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let scope = scope_from(&app, &auth, &body.scope, guild_id)?;
    let id = db::add_enrolled_badword(
        &app.db,
        super::configuration_write_scope(&app, &auth, guild_id, scope)?,
        &body.pattern,
        body.note.as_deref(),
    )
    .await?;
    Ok(Json(serde_json::json!({ "ok": true, "id": id })))
}

pub async fn delete_badword(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: axum::http::HeaderMap,
    Path((guild_id, word_id)): Path<(u64, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    // Uniform "not found" for missing / wrong-guild / non-owner global — never
    // distinguish existence across scopes (error-oracle / IDOR probe).
    let not_found = || ApiError(anyhow::anyhow!("badword not found"));
    let Some(word) = db::get_badword_primary(&app.db, word_id).await? else {
        return Err(not_found());
    };
    let allowed = if word.guild_id == GLOBAL_SCOPE {
        is_bot_owner(&app, &auth)
    } else {
        word.guild_id == guild_id as i64
    };
    if !allowed {
        return Err(not_found());
    }
    // Post-auth miss (TOCTOU race / already deleted) must stay uniform — do not
    // surface guild-scoped DB wording if the layer ever diverges again.
    match db::delete_enrolled_badword(
        &app.db,
        super::configuration_write_scope(&app, &auth, guild_id, word.guild_id)?,
        word_id,
    )
    .await
    {
        Ok(()) => Ok(Json(serde_json::json!({ "ok": true }))),
        Err(e) if e.to_string().contains("badword not found") => Err(not_found()),
        Err(e) => Err(ApiError(e)),
    }
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CommandBody {
    scope: Option<String>,
    data: CommandDef,
}

pub async fn list_commands(
    State(app): State<Arc<ApiState>>,
    session: Session,
    Path(guild_id): Path<u64>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let g = db::list_commands(&app.db, guild_id as i64).await?;
    let gl = db::list_commands(&app.db, GLOBAL_SCOPE).await?;
    Ok(Json(serde_json::json!({ "guild": g, "global": gl })))
}

pub async fn get_command(
    State(app): State<Arc<ApiState>>,
    session: Session,
    Path((guild_id, name)): Path<(u64, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let name = name.to_lowercase();
    let guild_override = db::get_command_row(&app.db, guild_id as i64, &name).await?;
    let global = db::get_command_row(&app.db, GLOBAL_SCOPE, &name).await?;
    let def = crate::defaults::default_commands()
        .into_iter()
        .find(|c| c.name == name);
    let effective = guild_override.clone().or_else(|| global.clone()).or(def);
    Ok(Json(serde_json::json!({
        "effective": effective,
        "guild_override": guild_override,
        "global": global,
    })))
}

fn validate_command_descriptor(cmd: &CommandDef) -> ApiResult<()> {
    let valid_name = !cmd.name.is_empty()
        && cmd.name.len() <= 32
        && cmd
            .name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_');
    if !valid_name {
        return Err(ApiError(anyhow::anyhow!(
            "command name must be 1-32 lowercase ASCII letters, digits, hyphens, or underscores"
        )));
    }
    let description_len = cmd.description.chars().count();
    if !(1..=100).contains(&description_len) {
        return Err(ApiError(anyhow::anyhow!(
            "command description must be 1-100 characters"
        )));
    }
    Ok(())
}

pub async fn put_command(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: axum::http::HeaderMap,
    Path((guild_id, name)): Path<(u64, String)>,
    Json(body): Json<CommandBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let scope = scope_from(&app, &auth, &body.scope, guild_id)?;
    let name = name.to_lowercase();
    let mut data = body.data;
    data.name = name.clone();
    validate_command_descriptor(&data)?;
    db::upsert_enrolled_command(
        &app.db,
        super::configuration_write_scope(&app, &auth, guild_id, scope)?,
        &name,
        &data,
    )
    .await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn create_command(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: axum::http::HeaderMap,
    Path(guild_id): Path<u64>,
    Json(body): Json<CommandBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let scope = scope_from(&app, &auth, &body.scope, guild_id)?;
    let name = body.data.name.to_lowercase();
    let mut data = body.data;
    data.name = name.clone();
    validate_command_descriptor(&data)?;
    db::upsert_enrolled_command(
        &app.db,
        super::configuration_write_scope(&app, &auth, guild_id, scope)?,
        &name,
        &data,
    )
    .await?;
    Ok(Json(serde_json::json!({ "ok": true, "name": name })))
}

pub async fn delete_command(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: axum::http::HeaderMap,
    Path((guild_id, name)): Path<(u64, String)>,
    Json(body): Json<ScopeBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let scope = scope_from(&app, &auth, &body.scope, guild_id)?;
    db::delete_enrolled_command(
        &app.db,
        super::configuration_write_scope(&app, &auth, guild_id, scope)?,
        &name.to_lowercase(),
    )
    .await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// templates
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct TemplateBody {
    scope: Option<String>,
    template: String,
}

pub async fn list_templates(
    State(app): State<Arc<ApiState>>,
    session: Session,
    Path(guild_id): Path<u64>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let g = db::list_templates(&app.db, guild_id as i64).await?;
    let gl = db::list_templates(&app.db, GLOBAL_SCOPE).await?;
    Ok(Json(serde_json::json!({ "guild": g, "global": gl })))
}

pub async fn get_template(
    State(app): State<Arc<ApiState>>,
    session: Session,
    Path((guild_id, event)): Path<(u64, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let guild_override = db::get_template_row(&app.db, guild_id as i64, &event).await?;
    let global = db::get_template_row(&app.db, GLOBAL_SCOPE, &event).await?;
    let def = crate::defaults::default_templates()
        .into_iter()
        .find(|(e, _)| *e == event)
        .map(|(_, t)| t.to_string());
    let effective = guild_override.clone().or_else(|| global.clone()).or(def);
    Ok(Json(serde_json::json!({
        "effective": effective,
        "guild_override": guild_override,
        "global": global,
    })))
}

pub async fn put_template(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: axum::http::HeaderMap,
    Path((guild_id, event)): Path<(u64, String)>,
    Json(body): Json<TemplateBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let scope = scope_from(&app, &auth, &body.scope, guild_id)?;
    if !crate::types::EVENT_TYPES.contains(&event.as_str()) {
        return Err(ApiError(anyhow::anyhow!("unknown event type `{event}`")));
    }
    db::upsert_enrolled_template(
        &app.db,
        super::configuration_write_scope(&app, &auth, guild_id, scope)?,
        &event,
        &body.template,
    )
    .await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn delete_template(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: axum::http::HeaderMap,
    Path((guild_id, event)): Path<(u64, String)>,
    Json(body): Json<ScopeBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let scope = scope_from(&app, &auth, &body.scope, guild_id)?;
    db::delete_enrolled_template(
        &app.db,
        super::configuration_write_scope(&app, &auth, guild_id, scope)?,
        &event,
    )
    .await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// log channels
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct LogChannelBody {
    scope: Option<String>,
    #[serde(deserialize_with = "crate::ids::de_i64")]
    channel_id: i64,
}

/// Wire-format log channel map: channel snowflakes as decimal strings.
fn log_channels_wire(rows: Vec<(String, i64)>) -> Vec<serde_json::Value> {
    rows.into_iter()
        .map(|(event, channel_id)| {
            serde_json::json!({
                "event": event,
                "channel_id": channel_id.to_string(),
            })
        })
        .collect()
}

pub async fn list_logchannels(
    State(app): State<Arc<ApiState>>,
    session: Session,
    Path(guild_id): Path<u64>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let g = db::list_log_channels(&app.db, guild_id as i64).await?;
    Ok(Json(serde_json::json!({
        "guild": log_channels_wire(g),
        "global": [],
    })))
}

pub async fn get_logchannel(
    State(app): State<Arc<ApiState>>,
    session: Session,
    Path((guild_id, event)): Path<(u64, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let guild_override = db::list_log_channels(&app.db, guild_id as i64)
        .await?
        .into_iter()
        .find(|(e, _)| e == &event)
        .map(|(_, c)| c);
    let effective = guild_override;
    // Match guild_config: snowflakes as strings (never JS Number).
    Ok(Json(serde_json::json!({
        "effective": effective.map(|c| c.to_string()),
        "guild_override": guild_override.map(|c| c.to_string()),
        "global": null,
    })))
}

pub async fn put_logchannel(
    State(app): State<Arc<ApiState>>,
    session: Session,
    Path((guild_id, event)): Path<(u64, String)>,
    Json(body): Json<LogChannelBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    let scope = scope_from(&app, &auth, &body.scope, guild_id)?;
    if scope == GLOBAL_SCOPE {
        return Err(ApiError(anyhow::anyhow!(
            "log channels must be configured per guild"
        )));
    }
    if !crate::types::EVENT_TYPES.contains(&event.as_str()) {
        return Err(ApiError(anyhow::anyhow!("unknown event type `{event}`")));
    }
    crate::discord::validate_log_channel_guild(
        app.discord_http(),
        guild_id,
        serenity::model::id::ChannelId::new(body.channel_id as u64),
    )
    .await
    .map_err(ApiError)?;
    db::upsert_log_channel(&app.db, scope, &event, body.channel_id).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn delete_logchannel(
    State(app): State<Arc<ApiState>>,
    session: Session,
    Path((guild_id, event)): Path<(u64, String)>,
    Json(body): Json<ScopeBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    let scope = scope_from(&app, &auth, &body.scope, guild_id)?;
    if scope == GLOBAL_SCOPE {
        return Err(ApiError(anyhow::anyhow!(
            "log channels must be configured per guild"
        )));
    }
    db::delete_log_channel(&app.db, scope, &event).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// copy from another server
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CopyBody {
    #[serde(deserialize_with = "crate::ids::de_u64")]
    from_guild_id: u64,
    from_enrollment_id: Option<String>,
    sections: Vec<String>,
}

pub async fn copy_config(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: axum::http::HeaderMap,
    Path(guild_id): Path<u64>,
    Json(body): Json<CopyBody>,
) -> ApiResult<Json<serde_json::Value>> {
    // Destination: owner gate when guild_id=0, else fresh Manage Guild.
    let dest = guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&dest, guild_id, &headers)?;
    // Source is a *second* Discord guild authorization. Never reuse `dest`'s
    // AuthCtx: when destination is global, guild_scope returns CachedOk and
    // can_manage(from) would trust a stale ≤60s guild list (owner demoted on
    // source could still copy into global). Always refresh for source.
    let source = authorize_manage_guild(&app, &session, body.from_guild_id)
        .await
        .map_err(|error| {
            if error.0.is::<super::GuildAccessError>() {
                error
            } else {
                ApiError(anyhow::anyhow!("you cannot manage the source server"))
            }
        })?;

    super::check_enrollment_token(
        &source,
        body.from_guild_id,
        body.from_enrollment_id.as_deref(),
    )?;
    let wants_settings = body.sections.iter().any(|s| s == "settings" || s == "all");

    // Settings copy atomically commits the desired nickname and outbox row;
    // the bot applies Discord from that durable state.
    let nickname_queued = wants_settings && guild_id != 0;
    let result = db::copy_enrolled_guild_config(
        &app.db,
        super::configuration_scope(&app, &source, body.from_guild_id)?,
        super::configuration_scope(&app, &dest, guild_id)?,
        body.sections,
    )
    .await?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "copied": result.copied,
        "skipped_badwords": result.skipped_badwords,
        "skipped_logchannels": result.skipped_logchannels,
        "nickname_applied": null,
        "nickname_queued": nickname_queued,
        "nickname_error": null,
    })))
}

// ---------------------------------------------------------------------------
// tools / lookups
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct TestRegexBody {
    pattern: String,
    sample: String,
}

pub async fn test_regex(
    State(app): State<Arc<ApiState>>,
    session: Session,
    Path(guild_id): Path<u64>,
    Json(body): Json<TestRegexBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = real_guild_scope(&app, &session, guild_id).await?;
    if body.sample.len() > crate::normalize::USER_REGEX_MAX_SAMPLE_BYTES {
        return Err(ApiError(anyhow::anyhow!(
            "sample too long (max {} bytes)",
            crate::normalize::USER_REGEX_MAX_SAMPLE_BYTES
        )));
    }
    let re = crate::normalize::compile_user_regex(&body.pattern)
        .map_err(|e| ApiError(anyhow::anyhow!(e)))?;
    let normalized = crate::normalize::normalize_lenient(&body.sample);
    Ok(Json(serde_json::json!({
        "raw_match": re.is_match(&body.sample),
        "normalized_match": re.is_match(&normalized),
        "normalized_sample": normalized,
    })))
}

#[derive(Deserialize)]
pub struct SearchQuery {
    limit: Option<i64>,
}

#[derive(Deserialize)]
pub struct EventsQuery {
    event: Option<String>,
    #[serde(default, deserialize_with = "crate::ids::de_opt_u64")]
    user: Option<u64>,
    limit: Option<i64>,
}

pub async fn events(
    State(app): State<Arc<ApiState>>,
    session: Session,
    Path(guild_id): Path<u64>,
    Query(query): Query<EventsQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = real_guild_scope(&app, &session, guild_id).await?;
    let rows = db::query_events(
        &app.db,
        guild_id as i64,
        query.event.as_deref(),
        query.user.map(|u| u as i64),
        query.limit.unwrap_or(50),
    )
    .await?;
    Ok(Json(serde_json::json!(rows)))
}

pub async fn bans(
    State(app): State<Arc<ApiState>>,
    session: Session,
    Path(guild_id): Path<u64>,
    Query(query): Query<SearchQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = real_guild_scope(&app, &session, guild_id).await?;
    let rows = db::list_bans(&app.db, guild_id as i64, query.limit.unwrap_or(100)).await?;
    Ok(Json(serde_json::json!(rows)))
}

#[derive(Deserialize)]
pub struct UnbanBody {
    #[serde(deserialize_with = "crate::ids::de_u64")]
    user_id: u64,
    reason: Option<String>,
}

pub async fn unban(
    State(app): State<Arc<ApiState>>,
    session: Session,
    Path(guild_id): Path<u64>,
    Json(body): Json<UnbanBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = super::authorize_ban_members(&app, &session, guild_id).await?;
    let reason = body.reason.unwrap_or_else(|| "unbanned via WebUI".into());

    // Discord is the source of truth for ban status. Local `bans` is a mirror
    // (also healed by GUILD_BAN_REMOVE on the bot). Never fail the request after
    // Discord already unbanned — that lied to operators and left confusing drift.
    app.discord_http()
        .remove_ban(
            serenity::model::id::GuildId::new(guild_id),
            serenity::model::id::UserId::new(body.user_id),
            Some(&reason),
        )
        .await
        .map_err(|e| ApiError(anyhow::anyhow!("discord unban failed: {e}")))?;

    if let Err(e) = db::mark_unbanned(&app.db, guild_id as i64, body.user_id as i64).await {
        tracing::warn!(
            "local ban mirror update failed after Discord unban \
             (guild={guild_id} user={}): {e:#}; gateway event may heal",
            body.user_id
        );
    }
    let actor_id = auth.user.id.parse::<u64>().ok().map(|x| x as i64);
    if let Err(e) = db::insert_event(
        &app.db,
        guild_id as i64,
        "member_unban",
        Some(body.user_id as i64),
        actor_id,
        &serde_json::json!({"reason": reason, "source": "webui"}),
    )
    .await
    {
        tracing::warn!("failed to record webui unban event: {e:#}");
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn stats(
    State(app): State<Arc<ApiState>>,
    session: Session,
    Path(guild_id): Path<u64>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = real_guild_scope(&app, &session, guild_id).await?;
    // Guild-scoped only — never expose cross-guild aggregates to a guild admin.
    let s = db::guild_stats(&app.db, guild_id as i64).await?;
    Ok(Json(serde_json::json!(s)))
}

#[cfg(test)]
mod tests {
    use super::{SettingsBody, log_channels_wire};

    #[test]
    fn log_channels_wire_stringifies_snowflake_channel_ids() {
        let snow = 1329331947876479006i64;
        let out = log_channels_wire(vec![("message_delete".into(), snow)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["event"], "message_delete");
        assert_eq!(out[0]["channel_id"], "1329331947876479006");
        assert!(out[0]["channel_id"].is_string());
    }

    #[test]
    fn chat_history_retention_is_not_an_api_setting() {
        for value in [
            serde_json::json!(0),
            serde_json::json!(7),
            serde_json::Value::Null,
        ] {
            assert!(
                serde_json::from_value::<SettingsBody>(serde_json::json!({"retention_days":value}))
                    .is_err()
            );
        }
        let value = super::settings_wire(&crate::types::GuildSettings::default());
        assert!(value.get("retention_days").is_none());
        assert!(value.get("prefix").is_some());
        assert!(super::settings_wire(&None::<crate::types::GuildSettingsOverride>).is_null());
    }

    #[test]
    fn settings_body_null_means_clear_override() {
        let body: SettingsBody =
            serde_json::from_str(r#"{"prefix":null,"bot_name":null}"#).unwrap();
        assert_eq!(body.prefix, Some(None));
        assert_eq!(body.bot_name, Some(None));

        let omit: SettingsBody = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(omit.prefix, None);
        assert_eq!(omit.bot_name, None);

        let set: SettingsBody =
            serde_json::from_str(r#"{"prefix":"!x","bot_name":"Bot"}"#).unwrap();
        assert_eq!(set.prefix, Some(Some("!x".into())));
        assert_eq!(set.bot_name, Some(Some("Bot".into())));
    }
}
