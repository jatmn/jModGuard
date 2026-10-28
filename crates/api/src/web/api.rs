//! REST API handlers for the WebUI. Every guild-scoped endpoint verifies the
//! session can manage that guild. Writes bump durable `config_version` in the
//! DB and invalidate this process's cache; the bot (and API) poll
//! `config_version` to pick up cross-process changes within `config_poll_secs`.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use std::sync::Arc;
use tower_sessions::Session;

use super::state::ApiState;
use super::{ApiError, ApiResult, AuthCtx, guild_scope, is_bot_owner, require_bot_owner};
use crate::bot_actions::Action;
use crate::db;
use crate::types::{CommandDef, GLOBAL_SCOPE, RuleDef};

fn dashboard_actor(auth: &AuthCtx) -> i64 {
    i64::try_from(auth.discord_user_id()).unwrap_or(0)
}

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
    for (id, row) in &snapshot.guild_rules {
        if !jmodguard::custom_rules::is_custom_id(id) {
            continue;
        }
        if jmodguard::custom_rules::validate_custom_rule_shape(guild_id as i64, id, row).is_err() {
            continue;
        }
        rules.push(serde_json::json!({
            "rule_id": id,
            "name": row.name,
            "description": row.description,
            "effective": row,
            "guild_override": row,
            "global": serde_json::Value::Null,
        }));
    }

    // Runtime enforcement combines global and guild badwords. Return that
    // same set so global rows created in this UI remain inspectable and
    // removable through it.
    let badwords: Vec<_> = snapshot.badwords.iter().map(|w| w.with_status()).collect();

    // commands: raw scope rows stay on guild_override/global. `effective` matches
    // runtime (shipped built-ins and their shipped access, reserved-label collisions).
    let commands = command_config_rows(&snapshot.global_commands, &snapshot.guild_commands);

    let mut templates = Vec::new();
    for (event, def) in crate::defaults::rendered_log_templates() {
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
    for event in crate::types::EVENT_TYPES
        .into_iter()
        .chain(crate::types::ACTIVITY_LOG_EVENTS)
    {
        let guild_row = snapshot.guild_log_channels.get(event).copied();
        let effective = guild_row.unwrap_or(0);
        // Strings so JS never coerces snowflakes through IEEE-754 Number.
        log_channels.push(serde_json::json!({
            "event": event,
            "effective": effective.to_string(),
            "guild_override": guild_row.map(|c| c.to_string()),
            "global": null,
            "ignore": snapshot.guild_log_filters.get(event).cloned().unwrap_or_default(),
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
        "known_bad_images": snapshot.known_bad_images,
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
    let nickname_reconcile_patch = nickname_queued.then(|| {
        serde_json::json!({
            "state": "pending",
            "attempt_count": 0,
            "next_attempt_at": 0,
            "last_error_code": null,
            "last_error": null,
            "last_error_at": null,
        })
    });
    let prefix_present = body.prefix.is_some();
    let bot_name_present = body.bot_name.is_some();
    let prefix_patch = body.prefix.clone();
    let bot_name_patch = body.bot_name.clone();
    let (unchanged, settings_body) = app
        .commit_json_actor(
            dashboard_actor(&auth),
            db::ControlOp::PatchSettings {
                scope: super::configuration_scope(&app, &auth, guild_id)?.to_wire(),
                prefix: db::FieldPatch::from_settings(body.prefix),
                bot_name: db::FieldPatch::from_settings(body.bot_name),
                reconcile: body.sync_nickname,
            },
        )
        .await?;
    let settings: crate::types::GuildSettings = serde_json::from_value(settings_body)?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "unchanged": unchanged,
        "settings": settings_wire(&settings),
        "nickname_applied": null,
        "nickname_queued": nickname_queued,
        "nickname_error": null,
        "config_patch": {
            "kind": "settings",
            "scope": if guild_id == 0 { "global" } else { "guild" },
            "settings": settings_wire(&settings),
            "prefix_present": prefix_present,
            "prefix": prefix_patch,
            "bot_name_present": bot_name_present,
            "bot_name": bot_name_patch,
            "sync_nickname": body.sync_nickname,
            "nickname_reconcile": nickname_reconcile_patch,
        }
    })))
}

// ---------------------------------------------------------------------------
// rules
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct RuleBody {
    scope: Option<String>,
    data: RuleDef,
    #[serde(default)]
    clear_guild_override: bool,
}

pub async fn get_rule(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: axum::http::HeaderMap,
    Path((guild_id, rule_id)): Path<(u64, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let scope = super::configuration_scope(&app, &auth, guild_id)?;
    if !jmodguard::custom_rules::known_rule_id(&rule_id) {
        return Err(ApiError(anyhow::anyhow!("unknown rule `{rule_id}`")));
    }
    if jmodguard::custom_rules::is_custom_id(&rule_id) {
        let layers = db::get_enrolled_rule(&app.db, scope, &rule_id).await?;
        let Some(effective) = layers.guild else {
            return Err(ApiError(anyhow::anyhow!("unknown rule `{rule_id}`")));
        };
        return Ok(Json(serde_json::json!({
            "effective": effective,
            "guild_override": effective,
            "global": null,
        })));
    }
    let idx = crate::types::RULE_IDS
        .iter()
        .position(|r| *r == rule_id.as_str())
        .unwrap();
    let def = crate::defaults::default_rules()[idx].clone();
    let layers = db::get_enrolled_rule(&app.db, scope, &rule_id).await?;
    let (guild_override, global) = (layers.guild, layers.global);
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
    if !jmodguard::custom_rules::known_rule_id(&rule_id) {
        return Err(ApiError(anyhow::anyhow!("unknown rule `{rule_id}`")));
    }
    let scope = scope_from(&app, &auth, &body.scope, guild_id)?;
    if jmodguard::custom_rules::is_custom_id(&rule_id) {
        if scope == GLOBAL_SCOPE {
            return Err(ApiError(anyhow::anyhow!(
                "custom rules cannot use global scope"
            )));
        }
        jmodguard::custom_rules::validate_custom_rule_shape(scope, &rule_id, &body.data)
            .map_err(ApiError)?;
    }
    if body.clear_guild_override && (scope != GLOBAL_SCOPE || guild_id == 0) {
        return Err(ApiError(anyhow::anyhow!(
            "override clearing requires a global rule save from a server"
        )));
    }
    let write = super::configuration_write_scope(&app, &auth, guild_id, scope)?;
    let (request, target) = write.to_wires();
    app.commit_json_actor(
        dashboard_actor(&auth),
        db::ControlOp::UpsertRule {
            request,
            target,
            id: rule_id.clone(),
            rule: body.data.clone(),
            clear_guild_override: body.clear_guild_override,
        },
    )
    .await?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "config_patch": {
            "kind": "rule",
            "id": rule_id,
            "scope": if scope == GLOBAL_SCOPE { "global" } else { "guild" },
            "value": body.data,
            "cleared_guild_override": body.clear_guild_override,
        }
    })))
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
    if !jmodguard::custom_rules::known_rule_id(&rule_id) {
        return Err(ApiError(anyhow::anyhow!("unknown rule `{rule_id}`")));
    }
    let scope = scope_from(&app, &auth, &body.scope, guild_id)?;
    if jmodguard::custom_rules::is_custom_id(&rule_id) && scope == GLOBAL_SCOPE {
        return Err(ApiError(anyhow::anyhow!(
            "custom rules cannot use global scope"
        )));
    }
    let write = super::configuration_write_scope(&app, &auth, guild_id, scope)?;
    let (request, target) = write.to_wires();
    app.commit_json_actor(
        dashboard_actor(&auth),
        db::ControlOp::DeleteRule {
            request,
            target,
            id: rule_id.clone(),
        },
    )
    .await?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "config_patch": {
            "kind": "rule",
            "id": rule_id,
            "scope": if scope == GLOBAL_SCOPE { "global" } else { "guild" },
            "value": null,
        }
    })))
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
    headers: axum::http::HeaderMap,
    Path(guild_id): Path<u64>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let scope = super::configuration_scope(&app, &auth, guild_id)?;
    let words = db::list_enrolled_badwords(&app.db, scope).await?;
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
    let write = super::configuration_write_scope(&app, &auth, guild_id, scope)?;
    let (request, target) = write.to_wires();
    let (_, id_body) = app
        .commit_json_actor(
            dashboard_actor(&auth),
            db::ControlOp::AddBadword {
                request,
                target,
                pattern: body.pattern.clone(),
                note: body.note.clone(),
            },
        )
        .await?;
    let id: i64 = serde_json::from_value(id_body)?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "id": id,
        "config_patch": {
            "kind": "badword",
            "operation": "add",
            "value": {
                "id": id,
                "guild_id": scope.to_string(),
                "pattern": body.pattern,
                "note": body.note,
                "valid": true,
            }
        }
    })))
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
    let write = super::configuration_write_scope(&app, &auth, guild_id, word.guild_id)?;
    let (request, target) = write.to_wires();
    match app
        .commit_json_actor(
            dashboard_actor(&auth),
            db::ControlOp::DeleteBadword {
                request,
                target,
                id: word_id,
            },
        )
        .await
    {
        Ok((_, _)) => Ok(Json(serde_json::json!({
            "ok": true,
            "config_patch": {
                "kind": "badword",
                "operation": "delete",
                "id": word_id,
            }
        }))),
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
    headers: axum::http::HeaderMap,
    Path(guild_id): Path<u64>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let scope = super::configuration_scope(&app, &auth, guild_id)?;
    let layers = db::list_enrolled_commands(&app.db, scope).await?;
    let (g, gl) = (layers.guild, layers.global);
    let commands = command_config_rows(&pairs_to_map(&gl), &pairs_to_map(&g));
    Ok(Json(
        serde_json::json!({ "guild": g, "global": gl, "commands": commands }),
    ))
}

fn pairs_to_map(rows: &[(String, CommandDef)]) -> std::collections::HashMap<String, CommandDef> {
    rows.iter().cloned().collect()
}

fn command_row_by_name<'a>(
    rows: &'a std::collections::HashMap<String, CommandDef>,
    name: &str,
) -> Option<&'a CommandDef> {
    rows.iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, command)| command)
}

/// Config-bundle command entries. Raw stored rows stay on `guild_override` and
/// `global`. `effective` is the runtime registry for that name.
fn command_config_rows(
    global_commands: &std::collections::HashMap<String, CommandDef>,
    guild_commands: &std::collections::HashMap<String, CommandDef>,
) -> Vec<serde_json::Value> {
    let effective = crate::defaults::effective_command_registry(global_commands, guild_commands);
    let mut names: Vec<String> = effective.keys().cloned().collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let def = &effective[&name];
            let guild_row = command_row_by_name(guild_commands, &name).cloned();
            let global_row = command_row_by_name(global_commands, &name).cloned();
            let scope = if global_row.is_some() {
                "global"
            } else if guild_row.is_some() {
                "guild"
            } else {
                "default"
            };
            serde_json::json!({
                "name": name,
                "scope": scope,
                "effective": def,
                "guild_override": guild_row,
                "global": global_row,
            })
        })
        .collect()
}

pub async fn get_command(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: axum::http::HeaderMap,
    Path((guild_id, name)): Path<(u64, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let scope = super::configuration_scope(&app, &auth, guild_id)?;
    let name = name.to_lowercase();
    let canonical = crate::defaults::builtin_command_for_label(&name).unwrap_or(name);
    let layers = db::get_enrolled_command(&app.db, scope, &canonical).await?;
    let (guild_override, global) = (layers.guild, layers.global);
    let mut guild_map = std::collections::HashMap::new();
    let mut global_map = std::collections::HashMap::new();
    if let Some(row) = guild_override.clone() {
        guild_map.insert(canonical.clone(), row);
    }
    if let Some(row) = global.clone() {
        global_map.insert(canonical.clone(), row);
    }
    let registry = crate::defaults::effective_command_registry(&global_map, &guild_map);
    let effective = registry.get(&canonical).cloned();
    Ok(Json(serde_json::json!({
        "effective": effective,
        "guild_override": guild_override,
        "global": global,
    })))
}

fn validate_command_descriptor(storage_name: &str, cmd: &CommandDef) -> ApiResult<()> {
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
    if crate::defaults::rejects_binary_overlay_command_persist(storage_name, cmd) {
        return Err(ApiError(anyhow::anyhow!(
            crate::defaults::command_persist_rejected_message(storage_name)
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
    validate_command_descriptor(&name, &data)?;
    let write = super::configuration_write_scope(&app, &auth, guild_id, scope)?;
    let (request, target) = write.to_wires();
    app.commit_json_actor(
        dashboard_actor(&auth),
        db::ControlOp::UpsertCommand {
            request,
            target,
            name: name.clone(),
            command: data.clone(),
        },
    )
    .await?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "config_patch": {
            "kind": "command",
            "name": name,
            "scope": if scope == GLOBAL_SCOPE { "global" } else { "guild" },
            "value": data,
        }
    })))
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
    validate_command_descriptor(&name, &data)?;
    let write = super::configuration_write_scope(&app, &auth, guild_id, scope)?;
    let (request, target) = write.to_wires();
    app.commit_json_actor(
        dashboard_actor(&auth),
        db::ControlOp::UpsertCommand {
            request,
            target,
            name: name.clone(),
            command: data.clone(),
        },
    )
    .await?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "name": name,
        "config_patch": {
            "kind": "command",
            "name": name,
            "scope": if scope == GLOBAL_SCOPE { "global" } else { "guild" },
            "value": data,
        }
    })))
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
    let write = super::configuration_write_scope(&app, &auth, guild_id, scope)?;
    let (request, target) = write.to_wires();
    app.commit_json_actor(
        dashboard_actor(&auth),
        db::ControlOp::DeleteCommand {
            request,
            target,
            name: name.to_lowercase(),
        },
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
    headers: axum::http::HeaderMap,
    Path(guild_id): Path<u64>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let scope = super::configuration_scope(&app, &auth, guild_id)?;
    let layers = db::list_enrolled_templates(&app.db, scope).await?;
    let (g, gl) = (layers.guild, layers.global);
    Ok(Json(serde_json::json!({ "guild": g, "global": gl })))
}

pub async fn get_template(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: axum::http::HeaderMap,
    Path((guild_id, event)): Path<(u64, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let scope = super::configuration_scope(&app, &auth, guild_id)?;
    let layers = db::get_enrolled_template(&app.db, scope, &event).await?;
    let (guild_override, global) = (layers.guild, layers.global);
    let def = crate::defaults::rendered_log_templates()
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
    if !crate::types::is_log_event(&event) {
        return Err(ApiError(anyhow::anyhow!("unknown event type `{event}`")));
    }
    let write = super::configuration_write_scope(&app, &auth, guild_id, scope)?;
    let (request, target) = write.to_wires();
    app.commit_json_actor(
        dashboard_actor(&auth),
        db::ControlOp::UpsertTemplate {
            request,
            target,
            event: event.clone(),
            template: body.template.clone(),
        },
    )
    .await?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "config_patch": {
            "kind": "template",
            "event": event,
            "scope": if scope == GLOBAL_SCOPE { "global" } else { "guild" },
            "value": body.template,
        }
    })))
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
    let write = super::configuration_write_scope(&app, &auth, guild_id, scope)?;
    let (request, target) = write.to_wires();
    app.commit_json_actor(
        dashboard_actor(&auth),
        db::ControlOp::DeleteTemplate {
            request,
            target,
            event: event.clone(),
        },
    )
    .await?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "config_patch": {
            "kind": "template",
            "event": event,
            "scope": if scope == GLOBAL_SCOPE { "global" } else { "guild" },
            "value": null,
        }
    })))
}

// ---------------------------------------------------------------------------
// log channels
// ---------------------------------------------------------------------------

pub async fn put_log_filter(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: axum::http::HeaderMap,
    Path((guild_id, event)): Path<(u64, String)>,
    Json(filter): Json<db::LogFilter>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = real_guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let scope = super::configuration_write_scope(&app, &auth, guild_id, guild_id as i64)?;
    let (request, target) = scope.to_wires();
    let (_, body) = app
        .commit_json_actor(
            dashboard_actor(&auth),
            db::ControlOp::PutLogFilter {
                request,
                target,
                event: event.clone(),
                filter,
            },
        )
        .await?;
    let filter: db::LogFilter = serde_json::from_value(body)?;
    Ok(Json(serde_json::json!({ "ignore": filter })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogChannelBody {
    request_id: String,
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
    headers: axum::http::HeaderMap,
    Path(guild_id): Path<u64>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let scope = super::configuration_scope(&app, &auth, guild_id)?;
    let g = db::list_enrolled_log_channels(&app.db, scope).await?;
    Ok(Json(serde_json::json!({
        "guild": log_channels_wire(g),
        "global": [],
    })))
}

pub async fn get_logchannel(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: axum::http::HeaderMap,
    Path((guild_id, event)): Path<(u64, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let scope = super::configuration_scope(&app, &auth, guild_id)?;
    let guild_override = db::list_enrolled_log_channels(&app.db, scope)
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
    headers: axum::http::HeaderMap,
    Path((guild_id, event)): Path<(u64, String)>,
    Json(body): Json<LogChannelBody>,
) -> ApiResult<(StatusCode, Json<db::BotAction>)> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let scope = scope_from(&app, &auth, &body.scope, guild_id)?;
    if scope == GLOBAL_SCOPE {
        return Err(ApiError(anyhow::anyhow!(
            "log channels must be configured per guild"
        )));
    }
    if !crate::types::is_log_event(&event) {
        return Err(ApiError(anyhow::anyhow!("unknown event type `{event}`")));
    }
    enqueue_action(
        &app,
        guild_id,
        &auth,
        &body.request_id,
        Action::SetLogChannel {
            event,
            channel_id: Some(body.channel_id),
        },
    )
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClearLogChannelBody {
    scope: Option<String>,
    request_id: String,
}

pub async fn delete_logchannel(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: axum::http::HeaderMap,
    Path((guild_id, event)): Path<(u64, String)>,
    Json(body): Json<ClearLogChannelBody>,
) -> ApiResult<(StatusCode, Json<db::BotAction>)> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let scope = scope_from(&app, &auth, &body.scope, guild_id)?;
    if scope == GLOBAL_SCOPE {
        return Err(ApiError(anyhow::anyhow!(
            "log channels must be configured per guild"
        )));
    }
    enqueue_action(
        &app,
        guild_id,
        &auth,
        &body.request_id,
        Action::SetLogChannel {
            event,
            channel_id: None,
        },
    )
    .await
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
    // One permission snapshot checks both servers; global destinations still
    // require the independent session-owner gate.
    let auth = super::require_auth_fresh(&app, &session).await?;
    let dest = if guild_id == 0 {
        super::session_owner(&app, &session).await?
    } else {
        super::manage_guild_from_auth(&app, auth.clone(), guild_id).await?
    };
    super::check_page_enrollment(&dest, guild_id, &headers)?;
    let source = super::manage_guild_from_auth(&app, auth, body.from_guild_id)
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
    let (_, copied) = app
        .commit_json_actor(
            dashboard_actor(&dest),
            db::ControlOp::CopyGuildConfig {
                source: super::configuration_scope(&app, &source, body.from_guild_id)?.to_wire(),
                destination: super::configuration_scope(&app, &dest, guild_id)?.to_wire(),
                sections: body.sections,
            },
        )
        .await?;
    let result: db::CopyAck = serde_json::from_value(copied)?;

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
    headers: axum::http::HeaderMap,
    Path(guild_id): Path<u64>,
    Query(query): Query<EventsQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = real_guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let enrollment = super::authorized_enrollment(&auth, guild_id)?;
    let rows = db::query_enrolled_events(
        &app.db,
        enrollment,
        query.event.as_deref(),
        query
            .user
            .map(i64::try_from)
            .transpose()
            .map_err(|_| ApiError(anyhow::anyhow!("invalid user id")))?,
        query.limit.unwrap_or(50),
    )
    .await?;
    Ok(Json(serde_json::json!(rows)))
}

pub async fn bans(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: axum::http::HeaderMap,
    Path(guild_id): Path<u64>,
    Query(query): Query<SearchQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = real_guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let enrollment = super::authorized_enrollment(&auth, guild_id)?;
    let rows = db::list_enrolled_bans(&app.db, enrollment, query.limit.unwrap_or(100)).await?;
    Ok(Json(serde_json::json!(rows)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnbanBody {
    request_id: String,
    #[serde(deserialize_with = "crate::ids::de_u64")]
    user_id: u64,
    reason: Option<String>,
}

pub async fn unban(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: axum::http::HeaderMap,
    Path(guild_id): Path<u64>,
    Json(body): Json<UnbanBody>,
) -> ApiResult<(StatusCode, Json<db::BotAction>)> {
    let auth = super::authorize_ban_members(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let reason = body.reason.unwrap_or_else(|| "unbanned via WebUI".into());

    let user_id =
        i64::try_from(body.user_id).map_err(|_| ApiError(anyhow::anyhow!("invalid user id")))?;
    enqueue_action(
        &app,
        guild_id,
        &auth,
        &body.request_id,
        Action::Unban { user_id, reason },
    )
    .await
}

pub async fn stats(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: axum::http::HeaderMap,
    Path(guild_id): Path<u64>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = real_guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let enrollment = super::authorized_enrollment(&auth, guild_id)?;
    // Guild-scoped only — never expose cross-guild aggregates to a guild admin.
    let s = db::enrolled_guild_stats(&app.db, enrollment.clone()).await?;
    let mut value = serde_json::json!(s);
    if let Some(cases) = super::cases::case_count(&app, enrollment).await? {
        value["cases"] = serde_json::json!(cases);
    }
    Ok(Json(value))
}

pub(super) async fn enqueue_action(
    app: &Arc<ApiState>,
    guild: u64,
    auth: &AuthCtx,
    request: &str,
    action: Action,
) -> ApiResult<(StatusCode, Json<db::BotAction>)> {
    action.validate()?;
    let enrollment = super::authorized_enrollment(auth, guild)?;
    let actor = i64::try_from(auth.discord_user_id())
        .map_err(|_| ApiError(anyhow::anyhow!("invalid actor id")))?;
    let (_, job_body) = app
        .commit_json(db::ControlOp::EnqueueBotAction {
            enrollment,
            actor,
            request: request.to_owned(),
            action,
            now: chrono::Utc::now().timestamp(),
            limits: app.cfg.actions.clone(),
        })
        .await
        .map_err(|error| {
            if error.is::<db::EnrollmentChanged>() {
                return ApiError(crate::bot_actions::RequestKeyError::EnrollmentChanged.into());
            }
            if error.is::<db::EnrollmentUnverified>()
                || error.is::<crate::bot_actions::RequestKeyError>()
            {
                return ApiError(error);
            }
            tracing::warn!("bot action admission failed: {error:#}");
            ApiError(anyhow::anyhow!(
                "Could not queue action. Check pending actions and retry using the same request ID."
            ))
        })?;
    let job: db::BotAction = serde_json::from_value(job_body)?;
    Ok((StatusCode::ACCEPTED, Json(job)))
}

fn can_access_action(auth: &AuthCtx, guild: u64, action: &Action) -> bool {
    match action {
        Action::Unban { .. } => auth.can_ban_members(guild),
        Action::SetLogChannel { .. } => auth.can_manage(guild),
        Action::MemberSync { .. } => auth.can_manage(guild),
    }
}

pub async fn actions(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: axum::http::HeaderMap,
    Path(guild): Path<u64>,
) -> ApiResult<Json<Vec<db::BotAction>>> {
    let mut auth = super::require_auth_fresh(&app, &session).await?;
    if guild == 0 || (!auth.can_manage(guild) && !auth.can_ban_members(guild)) {
        return Err(ApiError(anyhow::anyhow!("actions not available")));
    }
    let enrollment = super::require_active_enrollment(&app, guild).await?;
    auth.enrollment = Some(enrollment.clone());
    super::check_page_enrollment(&auth, guild, &headers)?;
    let mut jobs = db::recent_enrolled_bot_actions(&app.db, &enrollment).await?;
    jobs.retain(|j| can_access_action(&auth, guild, &j.action));
    Ok(Json(jobs))
}

pub async fn cancel_action(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: axum::http::HeaderMap,
    Path((guild, id)): Path<(u64, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let mut auth = super::require_auth_fresh(&app, &session).await?;
    if guild == 0 || (!auth.can_manage(guild) && !auth.can_ban_members(guild)) {
        return Err(ApiError(anyhow::anyhow!("action not available")));
    }
    let enrollment = super::require_active_enrollment(&app, guild).await?;
    auth.enrollment = Some(enrollment.clone());
    super::check_page_enrollment(&auth, guild, &headers)?;
    let job = db::enrolled_bot_action(&app.db, &enrollment, &id)
        .await?
        .ok_or_else(|| ApiError(anyhow::anyhow!("action not available")))?;
    if !can_access_action(&auth, guild, &job.action) {
        return Err(ApiError(anyhow::anyhow!("action not available")));
    }
    let (_, cancelled) = app
        .commit_json(db::ControlOp::CancelBotAction {
            enrollment,
            id: id.clone(),
        })
        .await?;
    if !serde_json::from_value::<bool>(cancelled)? {
        return Err(ApiError(anyhow::anyhow!(
            "only unstarted work can be cancelled; log-channel work can also be cleared while running or unknown"
        )));
    }
    Ok(Json(serde_json::json!({"ok":true})))
}

#[cfg(test)]
mod tests {
    use super::{SettingsBody, log_channels_wire};

    #[test]
    fn log_channels_wire_stringifies_snowflake_channel_ids() {
        let snow = 9_007_199_254_740_993i64;
        let out = log_channels_wire(vec![("message_delete".into(), snow)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["event"], "message_delete");
        assert_eq!(out[0]["channel_id"], "9007199254740993");
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnownBadImageBody {
    hash: String,
    note: Option<String>,
}

pub async fn add_known_bad_image(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: axum::http::HeaderMap,
    Path(guild_id): Path<u64>,
    Json(body): Json<KnownBadImageBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let scope = super::configuration_scope(&app, &auth, guild_id)?;
    let hash = db::canonical_image_hash(&body.hash)?;
    let note = db::safe_image_note(body.note)?;
    let (_, result) = app
        .commit_json_actor(
            dashboard_actor(&auth),
            db::ControlOp::AddKnownBadImage {
                scope: scope.to_wire(),
                hash,
                note,
            },
        )
        .await?;
    Ok(Json(
        serde_json::json!({"ok": true, "changed": result["changed"]}),
    ))
}

pub async fn remove_known_bad_image(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: axum::http::HeaderMap,
    Path((guild_id, hash)): Path<(u64, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    super::check_page_enrollment(&auth, guild_id, &headers)?;
    let scope = super::configuration_scope(&app, &auth, guild_id)?;
    let hash = db::canonical_image_hash(&hash)?;
    let (_, result) = app
        .commit_json_actor(
            dashboard_actor(&auth),
            db::ControlOp::RemoveKnownBadImage {
                scope: scope.to_wire(),
                hash,
            },
        )
        .await?;
    Ok(Json(
        serde_json::json!({"ok": true, "changed": result["changed"]}),
    ))
}
