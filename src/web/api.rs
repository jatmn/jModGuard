//! REST API handlers for the WebUI. Every guild-scoped endpoint verifies the
//! session can manage that guild. Writes bump durable `config_version` in the
//! DB and invalidate this process's cache; the bot (and API) poll
//! `config_version` to pick up cross-process changes within `config_poll_secs`.

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;
use std::sync::Arc;
use tower_sessions::Session;

use super::{
    ApiError, ApiResult, AuthCtx, authorize_manage_guild, guild_scope, is_bot_owner,
    require_bot_owner,
};
use crate::db;
use crate::state::AppState;
use crate::types::{CommandDef, GLOBAL_SCOPE, RuleDef};

/// Authorizes an endpoint that operates on a real Discord guild. Global
/// configuration (`guild_id = 0`) is intentionally handled by configuration
/// routes, but it must never reach Discord REST or guild-local audit tables.
async fn real_guild_scope(
    app: &Arc<AppState>,
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

pub async fn guild_config(
    State(app): State<Arc<AppState>>,
    session: Session,
    Path(guild_id): Path<u64>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let guild = guild_id as i64;

    let settings_bundle = db::get_settings_web_bundle(&app.db, guild).await?;

    // rules: effective + scope info
    let g_rules = db::list_rules(&app.db, guild).await?;
    let gl_rules = db::list_rules(&app.db, GLOBAL_SCOPE).await?;
    let default_rules = crate::defaults::default_rules();
    let mut rules = Vec::new();
    for (i, rid) in crate::types::RULE_IDS.iter().enumerate() {
        let def = default_rules[i].clone();
        let guild_row = g_rules
            .iter()
            .find(|(k, _)| k == rid)
            .map(|(_, v)| v.clone());
        let global_row = gl_rules
            .iter()
            .find(|(k, _)| k == rid)
            .map(|(_, v)| v.clone());
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
    let badwords = db::list_badwords(&app.db, guild).await?;
    let badwords: Vec<_> = badwords.iter().map(|w| w.with_status()).collect();

    // commands: union of guild + global rows, effective resolution
    let g_cmds = db::list_commands(&app.db, guild).await?;
    let gl_cmds = db::list_commands(&app.db, GLOBAL_SCOPE).await?;
    let defaults = crate::defaults::default_commands();
    let mut commands: Vec<serde_json::Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (name, def) in &gl_cmds {
        let guild_row = g_cmds
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
    for (name, def) in &g_cmds {
        if seen.insert(name.to_lowercase()) {
            let global_row = gl_cmds
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

    let g_tpl = db::list_templates(&app.db, guild).await?;
    let gl_tpl = db::list_templates(&app.db, GLOBAL_SCOPE).await?;
    let mut templates = Vec::new();
    for (event, def) in crate::defaults::default_templates() {
        let guild_row = g_tpl
            .iter()
            .find(|(k, _)| k == event)
            .map(|(_, v)| v.clone());
        let global_row = gl_tpl
            .iter()
            .find(|(k, _)| k == event)
            .map(|(_, v)| v.clone());
        templates.push(serde_json::json!({
            "event": event,
            "effective": db::resolve(&guild_row, &global_row, &def.to_string()),
            "guild_override": guild_row,
            "global": global_row.unwrap_or_else(|| def.to_string()),
        }));
    }

    let g_lc = db::list_log_channels(&app.db, guild).await?;
    let mut log_channels = Vec::new();
    for event in crate::types::EVENT_TYPES {
        let guild_row = g_lc.iter().find(|(k, _)| k == event).map(|(_, v)| *v);
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
        "settings": settings_bundle.settings,
        "settings_override": settings_bundle.settings_override,
        "global_settings": settings_bundle.global_settings,
        "global_settings_override": settings_bundle.global_settings_override,
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
pub struct SettingsBody {
    #[serde(default, deserialize_with = "crate::types::de_settings_patch_field")]
    prefix: crate::types::SettingsPatchField<String>,
    #[serde(default, deserialize_with = "crate::types::de_settings_patch_field")]
    bot_name: crate::types::SettingsPatchField<String>,
    #[serde(default, deserialize_with = "crate::types::de_settings_patch_field")]
    retention_days: crate::types::SettingsPatchField<u32>,
    /// Explicit repair action for Discord↔SQLite nickname drift. Kept separate
    /// from ordinary saves so a stale form never re-applies a nickname.
    #[serde(default)]
    sync_nickname: bool,
}

pub async fn update_settings(
    State(app): State<Arc<AppState>>,
    session: Session,
    Path(guild_id): Path<u64>,
    Json(body): Json<SettingsBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = guild_scope(&app, &session, guild_id).await?;
    if guild_id == 0 && body.bot_name.is_some() {
        return Err(ApiError(anyhow::anyhow!(
            "global bot_name is not supported; set the Discord nickname per server"
        )));
    }

    // Apply Discord nickname before persisting (same ordering as !setname).
    // Persisting first left durable bot_name that skipped retry on Discord failure.
    // If SQLite fails after Discord succeeds, restore the previous nick so the
    // two stores do not permanently diverge.
    //
    // Track the nick Discord currently has after the pre-patch step so a
    // post-commit re-sync can close parent-read→patch TOCTOU on inherit
    // (copy_config already does this).
    let _nickname_guard = if body.bot_name.is_some() && guild_id != 0 {
        Some(app.lock_nickname_update().await)
    } else {
        None
    };
    let mut nickname_applied: Option<bool> = None;
    let mut previous_nick: Option<String> = None;
    let mut discord_nick: Option<String> = None;
    if let Some(ref bot_name_patch) = body.bot_name
        && guild_id != 0
    {
        db::request_nickname_reconcile(&app.db, guild_id as i64).await?;
        let current = db::get_settings(&app.db, guild_id as i64).await?;
        previous_nick = Some(current.bot_name.clone());
        discord_nick = Some(current.bot_name.clone());
        let target_nick = match bot_name_patch {
            None => {
                db::get_parent_settings(&app.db, guild_id as i64)
                    .await?
                    .bot_name
            }
            Some(n) => n.clone(),
        };
        if current.bot_name != target_nick || body.sync_nickname {
            match apply_bot_nickname(&app, guild_id, &target_nick).await {
                Ok(()) => {
                    nickname_applied = Some(true);
                    discord_nick = Some(target_nick);
                }
                Err(e) => {
                    tracing::warn!(
                        "failed to apply Discord nickname before WebUI settings save \
                         (guild={guild_id}): {e:#}"
                    );
                    return Err(ApiError(anyhow::anyhow!(
                        "failed to update Discord nickname: {e:#}"
                    )));
                }
            }
        }
    }

    // Persist only the fields present in the body as a parent-relative
    // override. Values equal to the parent layer are stripped so global
    // inheritance keeps working.
    let patched_bot_name = body.bot_name.is_some();
    let settings = match db::patch_settings(
        &app.db,
        guild_id as i64,
        body.prefix,
        body.bot_name,
        body.retention_days,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            if !db::is_database_commit_indeterminate(&e)
                && nickname_applied == Some(true)
                && let Some(ref prev) = previous_nick
                && let Err(re) = apply_bot_nickname(&app, guild_id, prev).await
            {
                tracing::error!(
                    "failed to restore Discord nickname after settings save error \
                     (guild={guild_id}): {re:#} (save error: {e:#})"
                );
            }
            return Err(ApiError(e));
        }
    };
    app.invalidate_configs();

    // Post-commit re-sync: committed effective bot_name is authoritative.
    // Inherit (null) re-reads parent inside patch_settings and can diverge from
    // the pre-patch Discord apply when global nick changes concurrently.
    let mut nickname_error: Option<String> = None;
    if patched_bot_name && guild_id != 0 {
        let want = settings.bot_name.as_str();
        let have = discord_nick.as_deref().unwrap_or("");
        if have != want {
            match apply_bot_nickname(&app, guild_id, want).await {
                Ok(()) => {
                    let prev = previous_nick.as_deref().unwrap_or("");
                    nickname_applied = Some(want != prev);
                }
                Err(e) => {
                    tracing::error!(
                        "failed to re-sync Discord nickname after settings save \
                         (guild={guild_id}, want={want:?}): {e:#}"
                    );
                    nickname_error = Some(format!(
                        "settings saved but Discord nickname sync failed: {e:#}"
                    ));
                }
            }
        }
    }

    Ok(Json(serde_json::json!({
        "ok": true,
        "settings": settings,
        "nickname_applied": nickname_applied,
        "nickname_error": nickname_error,
    })))
}

/// Sets (or clears, when empty) the bot's Discord nickname for `guild_id` via REST.
async fn apply_bot_nickname(app: &AppState, guild_id: u64, bot_name: &str) -> anyhow::Result<()> {
    let nick = bot_name.to_string();
    crate::discord::with_retries("Discord nickname", guild_id, || {
        let nick = nick.clone();
        async move {
            let me = app.bot_user_id().await?;
            use serenity::builder::EditMember;
            serenity::model::id::GuildId::new(guild_id)
                .edit_member(app.discord_http(), me, EditMember::new().nickname(nick))
                .await?;
            Ok(())
        }
    })
    .await
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
    State(app): State<Arc<AppState>>,
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
    State(app): State<Arc<AppState>>,
    session: Session,
    Path((guild_id, rule_id)): Path<(u64, String)>,
    Json(body): Json<RuleBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    if !crate::types::RULE_IDS.contains(&rule_id.as_str()) {
        return Err(ApiError(anyhow::anyhow!("unknown rule `{rule_id}`")));
    }
    let scope = scope_from(&app, &auth, &body.scope, guild_id)?;
    db::upsert_rule(&app.db, scope, &rule_id, &body.data).await?;
    app.invalidate_configs();
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct ScopeBody {
    scope: Option<String>,
}

pub async fn delete_rule(
    State(app): State<Arc<AppState>>,
    session: Session,
    Path((guild_id, rule_id)): Path<(u64, String)>,
    Json(body): Json<ScopeBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    let scope = scope_from(&app, &auth, &body.scope, guild_id)?;
    db::delete_rule(&app.db, scope, &rule_id).await?;
    app.invalidate_configs();
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub(crate) fn scope_from(
    app: &AppState,
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
    State(app): State<Arc<AppState>>,
    session: Session,
    Path(guild_id): Path<u64>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let words = db::list_badwords(&app.db, guild_id as i64).await?;
    let words: Vec<_> = words.iter().map(|w| w.with_status()).collect();
    Ok(Json(serde_json::json!(words)))
}

pub async fn add_badword(
    State(app): State<Arc<AppState>>,
    session: Session,
    Path(guild_id): Path<u64>,
    Json(body): Json<BadwordBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    let scope = scope_from(&app, &auth, &body.scope, guild_id)?;
    let id = db::add_badword(&app.db, scope, &body.pattern, body.note.as_deref()).await?;
    app.invalidate_configs();
    Ok(Json(serde_json::json!({ "ok": true, "id": id })))
}

pub async fn delete_badword(
    State(app): State<Arc<AppState>>,
    session: Session,
    Path((guild_id, word_id)): Path<(u64, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    // Uniform "not found" for missing / wrong-guild / non-owner global — never
    // distinguish existence across scopes (error-oracle / IDOR probe).
    let not_found = || ApiError(anyhow::anyhow!("badword not found"));
    let Some(word) = db::get_badword(&app.db, word_id).await? else {
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
    match db::delete_badword(&app.db, word_id, word.guild_id).await {
        Ok(()) => {
            app.invalidate_configs();
            Ok(Json(serde_json::json!({ "ok": true })))
        }
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
    State(app): State<Arc<AppState>>,
    session: Session,
    Path(guild_id): Path<u64>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let g = db::list_commands(&app.db, guild_id as i64).await?;
    let gl = db::list_commands(&app.db, GLOBAL_SCOPE).await?;
    Ok(Json(serde_json::json!({ "guild": g, "global": gl })))
}

pub async fn get_command(
    State(app): State<Arc<AppState>>,
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
    State(app): State<Arc<AppState>>,
    session: Session,
    Path((guild_id, name)): Path<(u64, String)>,
    Json(body): Json<CommandBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    let scope = scope_from(&app, &auth, &body.scope, guild_id)?;
    let name = name.to_lowercase();
    let mut data = body.data;
    data.name = name.clone();
    validate_command_descriptor(&data)?;
    db::upsert_command(&app.db, scope, &name, &data).await?;
    app.invalidate_configs();
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn create_command(
    State(app): State<Arc<AppState>>,
    session: Session,
    Path(guild_id): Path<u64>,
    Json(body): Json<CommandBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    let scope = scope_from(&app, &auth, &body.scope, guild_id)?;
    let name = body.data.name.to_lowercase();
    let mut data = body.data;
    data.name = name.clone();
    validate_command_descriptor(&data)?;
    db::upsert_command(&app.db, scope, &name, &data).await?;
    app.invalidate_configs();
    Ok(Json(serde_json::json!({ "ok": true, "name": name })))
}

pub async fn delete_command(
    State(app): State<Arc<AppState>>,
    session: Session,
    Path((guild_id, name)): Path<(u64, String)>,
    Json(body): Json<ScopeBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    let scope = scope_from(&app, &auth, &body.scope, guild_id)?;
    db::delete_command(&app.db, scope, &name.to_lowercase()).await?;
    app.invalidate_configs();
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
    State(app): State<Arc<AppState>>,
    session: Session,
    Path(guild_id): Path<u64>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let g = db::list_templates(&app.db, guild_id as i64).await?;
    let gl = db::list_templates(&app.db, GLOBAL_SCOPE).await?;
    Ok(Json(serde_json::json!({ "guild": g, "global": gl })))
}

pub async fn get_template(
    State(app): State<Arc<AppState>>,
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
    State(app): State<Arc<AppState>>,
    session: Session,
    Path((guild_id, event)): Path<(u64, String)>,
    Json(body): Json<TemplateBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    let scope = scope_from(&app, &auth, &body.scope, guild_id)?;
    if !crate::types::EVENT_TYPES.contains(&event.as_str()) {
        return Err(ApiError(anyhow::anyhow!("unknown event type `{event}`")));
    }
    db::upsert_template(&app.db, scope, &event, &body.template).await?;
    app.invalidate_configs();
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn delete_template(
    State(app): State<Arc<AppState>>,
    session: Session,
    Path((guild_id, event)): Path<(u64, String)>,
    Json(body): Json<ScopeBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    let scope = scope_from(&app, &auth, &body.scope, guild_id)?;
    db::delete_template(&app.db, scope, &event).await?;
    app.invalidate_configs();
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
    State(app): State<Arc<AppState>>,
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
    State(app): State<Arc<AppState>>,
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
    State(app): State<Arc<AppState>>,
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
    app.invalidate_configs();
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn delete_logchannel(
    State(app): State<Arc<AppState>>,
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
    app.invalidate_configs();
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// copy from another server
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CopyBody {
    #[serde(deserialize_with = "crate::ids::de_u64")]
    from_guild_id: u64,
    sections: Vec<String>,
}

pub async fn copy_config(
    State(app): State<Arc<AppState>>,
    session: Session,
    Path(guild_id): Path<u64>,
    Json(body): Json<CopyBody>,
) -> ApiResult<Json<serde_json::Value>> {
    // Destination: owner gate when guild_id=0, else fresh Manage Guild.
    let _dest = guild_scope(&app, &session, guild_id).await?;
    // Source is a *second* Discord guild authorization. Never reuse `_dest`'s
    // AuthCtx: when destination is global, guild_scope returns CachedOk and
    // can_manage(from) would trust a stale ≤60s guild list (owner demoted on
    // source could still copy into global). Always refresh for source.
    if authorize_manage_guild(&app, &session, body.from_guild_id)
        .await
        .is_err()
    {
        return Err(ApiError(anyhow::anyhow!(
            "you cannot manage the source server"
        )));
    }

    let wants_settings = body.sections.iter().any(|s| s == "settings" || s == "all");

    // Refuse known hard-fail cases *before* Discord nickname mutation.
    // Without this, Discord-then-SQLite left nick updated when badwords /
    // logchannels-only refused inside the copy transaction.
    // copy_guild_config also re-runs preflight so a concurrent source change
    // between this check and the write cannot skip validation.
    db::preflight_copy_guild_config(
        &app.db,
        body.from_guild_id as i64,
        guild_id as i64,
        body.sections.clone(),
    )
    .await
    .map_err(ApiError)?;

    // Keep the external Discord mutation and the settings transaction ordered
    // with every other nickname writer in this process.
    let _nickname_guard = if wants_settings && guild_id != 0 {
        Some(app.lock_nickname_update().await)
    } else {
        None
    };

    // Apply Discord nickname *before* SQLite (same contract as update_settings
    // / !setname). Doing it after left copied settings committed when Discord
    // failed, so operators saw an error while the DB already changed.
    // If SQLite copy still fails after Discord, restore the previous nick.
    // After a successful copy, re-read committed settings and re-sync Discord
    // so a stale preview (source changed mid-copy) cannot leave nick drift.
    let mut nickname_applied: Option<bool> = None;
    let mut nickname_error: Option<String> = None;
    let mut previous_nick: Option<String> = None;
    let mut preview_nick: Option<String> = None;
    if wants_settings && guild_id != 0 {
        db::request_nickname_reconcile(&app.db, guild_id as i64).await?;
        let preview =
            db::preview_settings_after_copy(&app.db, body.from_guild_id as i64, guild_id as i64)
                .await?;
        let current = db::get_settings(&app.db, guild_id as i64).await?;
        previous_nick = Some(current.bot_name.clone());
        preview_nick = Some(preview.bot_name.clone());
        if preview.bot_name != current.bot_name {
            apply_bot_nickname(&app, guild_id, &preview.bot_name)
                .await
                .map_err(|e| {
                    tracing::warn!(
                        "failed to apply Discord nickname before settings copy \
                         (guild={guild_id}): {e:#}"
                    );
                    ApiError(anyhow::anyhow!("failed to update Discord nickname: {e:#}"))
                })?;
            nickname_applied = Some(true);
        }
    }

    let result = match db::copy_guild_config(
        &app.db,
        body.from_guild_id as i64,
        guild_id as i64,
        body.sections,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            if !db::is_database_commit_indeterminate(&e)
                && nickname_applied == Some(true)
                && let Some(ref prev) = previous_nick
                && let Err(re) = apply_bot_nickname(&app, guild_id, prev).await
            {
                tracing::error!(
                    "failed to restore Discord nickname after copy error \
                     (guild={guild_id}): {re:#} (copy error: {e:#})"
                );
            }
            return Err(ApiError(e));
        }
    };
    app.invalidate_configs();

    // Close preview→copy TOCTOU: committed bot_name is the source of truth.
    // SQLite already committed — surface Discord re-sync failure in the body
    // (not a hard 5xx) so operators see the partial outcome instead of a silent
    // success while Discord lags.
    if wants_settings && guild_id != 0 {
        let after = app.configs.get(&app.db, guild_id).await?.settings.clone();
        let applied = preview_nick.as_deref().unwrap_or("");
        if after.bot_name != applied {
            match apply_bot_nickname(&app, guild_id, &after.bot_name).await {
                Ok(()) => {
                    let prev = previous_nick.as_deref().unwrap_or("");
                    nickname_applied = Some(after.bot_name != prev);
                }
                Err(e) => {
                    tracing::error!(
                        "failed to re-sync Discord nickname after settings copy \
                         (guild={guild_id}, want={:?}): {e:#}",
                        after.bot_name
                    );
                    nickname_error = Some(format!(
                        "settings copied but Discord nickname sync failed: {e:#}"
                    ));
                }
            }
        }
    }

    Ok(Json(serde_json::json!({
        "ok": true,
        "copied": result.copied,
        "skipped_badwords": result.skipped_badwords,
        "skipped_logchannels": result.skipped_logchannels,
        "nickname_applied": nickname_applied,
        "nickname_error": nickname_error,
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
    State(app): State<Arc<AppState>>,
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
    q: Option<String>,
    limit: Option<i64>,
}

pub async fn members(
    State(app): State<Arc<AppState>>,
    session: Session,
    Path(guild_id): Path<u64>,
    Query(query): Query<SearchQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = real_guild_scope(&app, &session, guild_id).await?;
    let q = query.q.unwrap_or_default();
    let limit = query.limit.unwrap_or(50);
    let rows = db::search_members(&app.db, guild_id as i64, &q, limit).await?;
    Ok(Json(serde_json::json!(rows)))
}

#[derive(Deserialize)]
pub struct EventsQuery {
    event: Option<String>,
    #[serde(default, deserialize_with = "crate::ids::de_opt_u64")]
    user: Option<u64>,
    limit: Option<i64>,
}

pub async fn events(
    State(app): State<Arc<AppState>>,
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
    State(app): State<Arc<AppState>>,
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
    State(app): State<Arc<AppState>>,
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
    State(app): State<Arc<AppState>>,
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
    fn settings_body_null_means_clear_override() {
        let body: SettingsBody =
            serde_json::from_str(r#"{"prefix":null,"bot_name":null,"retention_days":null}"#)
                .unwrap();
        assert_eq!(body.prefix, Some(None));
        assert_eq!(body.bot_name, Some(None));
        assert_eq!(body.retention_days, Some(None));

        let omit: SettingsBody = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(omit.prefix, None);
        assert_eq!(omit.bot_name, None);
        assert_eq!(omit.retention_days, None);

        let set: SettingsBody =
            serde_json::from_str(r#"{"prefix":"!x","bot_name":"Bot","retention_days":0}"#).unwrap();
        assert_eq!(set.prefix, Some(Some("!x".into())));
        assert_eq!(set.bot_name, Some(Some("Bot".into())));
        assert_eq!(set.retention_days, Some(Some(0)));
    }
}
