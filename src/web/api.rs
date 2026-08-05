//! REST API handlers for the WebUI. Every guild-scoped endpoint verifies the
//! session can manage that guild. Writes invalidate the config cache so the
//! bot picks changes up immediately.

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;
use tower_sessions::Session;

use super::{guild_scope, ApiError, ApiResult};
use crate::db;
use crate::state::AppState;
use crate::types::{CommandDef, RuleDef, GLOBAL_SCOPE};

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

    let settings = db::get_settings(&app.db, guild).await?;
    let global_settings = db::get_settings(&app.db, GLOBAL_SCOPE).await?;

    // rules: effective + scope info
    let g_rules = db::list_rules(&app.db, guild).await?;
    let gl_rules = db::list_rules(&app.db, GLOBAL_SCOPE).await?;
    let default_rules = crate::defaults::default_rules();
    let mut rules = Vec::new();
    for (i, rid) in crate::types::RULE_IDS.iter().enumerate() {
        let def = default_rules[i].clone();
        let guild_row = g_rules.iter().find(|(k, _)| k == rid).map(|(_, v)| v.clone());
        let global_row = gl_rules.iter().find(|(k, _)| k == rid).map(|(_, v)| v.clone());
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

    let badwords = db::list_badwords(&app.db, guild).await?;

    // commands: union of guild + global rows, effective resolution
    let g_cmds = db::list_commands(&app.db, guild).await?;
    let gl_cmds = db::list_commands(&app.db, GLOBAL_SCOPE).await?;
    let defaults = crate::defaults::default_commands();
    let mut commands: Vec<serde_json::Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (name, def) in &gl_cmds {
        let guild_row = g_cmds.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone());
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
            let global_row = gl_cmds.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone());
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
        let guild_row = g_tpl.iter().find(|(k, _)| k == event).map(|(_, v)| v.clone());
        let global_row = gl_tpl.iter().find(|(k, _)| k == event).map(|(_, v)| v.clone());
        templates.push(serde_json::json!({
            "event": event,
            "effective": db::resolve(&guild_row, &global_row, &def.to_string()),
            "guild_override": guild_row,
            "global": global_row.unwrap_or_else(|| def.to_string()),
        }));
    }

    let g_lc = db::list_log_channels(&app.db, guild).await?;
    let gl_lc = db::list_log_channels(&app.db, GLOBAL_SCOPE).await?;
    let mut log_channels = Vec::new();
    for event in crate::types::EVENT_TYPES {
        let guild_row = g_lc.iter().find(|(k, _)| k == event).map(|(_, v)| *v);
        let global_row = gl_lc.iter().find(|(k, _)| k == event).map(|(_, v)| *v);
        log_channels.push(serde_json::json!({
            "event": event,
            "effective": db::resolve(&guild_row, &global_row, &0),
            "guild_override": guild_row,
            "global": global_row,
        }));
    }

    Ok(Json(serde_json::json!({
        "settings": settings,
        "global_settings": global_settings,
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

#[derive(Deserialize)]
pub struct SettingsBody {
    prefix: Option<String>,
    bot_name: Option<String>,
    retention_days: Option<u32>,
}

pub async fn update_settings(
    State(app): State<Arc<AppState>>,
    session: Session,
    Path(guild_id): Path<u64>,
    Json(body): Json<SettingsBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let mut settings = db::get_settings(&app.db, guild_id as i64).await?;
    if let Some(p) = body.prefix {
        settings.prefix = p;
    }
    if let Some(n) = body.bot_name {
        settings.bot_name = n;
    }
    if let Some(r) = body.retention_days {
        settings.retention_days = r;
    }
    db::upsert_settings(&app.db, guild_id as i64, &settings).await?;
    app.invalidate_configs();
    Ok(Json(serde_json::json!({ "ok": true, "settings": settings })))
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
    let cfg = app.configs.get(&app.db, guild_id).await.map_err(ApiError::from)?;
    Ok(Json(serde_json::json!({
        "effective": cfg.rules.get(&rule_id),
        "guild_override": db::get_rule(&app.db, guild_id as i64, &rule_id).await?,
        "global": db::get_rule(&app.db, GLOBAL_SCOPE, &rule_id).await?,
    })))
}

pub async fn put_rule(
    State(app): State<Arc<AppState>>,
    session: Session,
    Path((guild_id, rule_id)): Path<(u64, String)>,
    Json(body): Json<RuleBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = guild_scope(&app, &session, guild_id).await?;
    if !crate::types::RULE_IDS.contains(&rule_id.as_str()) {
        return Err(ApiError(anyhow::anyhow!("unknown rule `{rule_id}`")));
    }
    let scope = scope_from(&body.scope, guild_id)?;
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
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let scope = scope_from(&body.scope, guild_id)?;
    db::delete_rule(&app.db, scope, &rule_id).await?;
    app.invalidate_configs();
    Ok(Json(serde_json::json!({ "ok": true })))
}

fn scope_from(scope: &Option<String>, guild_id: u64) -> ApiResult<i64> {
    match scope.as_deref() {
        Some("global") => Ok(GLOBAL_SCOPE),
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
    Ok(Json(serde_json::json!(words)))
}

pub async fn add_badword(
    State(app): State<Arc<AppState>>,
    session: Session,
    Path(guild_id): Path<u64>,
    Json(body): Json<BadwordBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let scope = scope_from(&body.scope, guild_id)?;
    let id = db::add_badword(&app.db, scope, &body.pattern, body.note.as_deref()).await?;
    app.invalidate_configs();
    Ok(Json(serde_json::json!({ "ok": true, "id": id })))
}

pub async fn delete_badword(
    State(app): State<Arc<AppState>>,
    session: Session,
    Path((guild_id, word_id)): Path<(u64, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = guild_scope(&app, &session, guild_id).await?;
    db::delete_badword(&app.db, word_id).await?;
    app.invalidate_configs();
    Ok(Json(serde_json::json!({ "ok": true })))
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
    let cfg = app.configs.get(&app.db, guild_id).await.map_err(ApiError::from)?;
    Ok(Json(serde_json::json!({
        "effective": cfg.commands.get(&name.to_lowercase()),
        "guild_override": db::get_command_row(&app.db, guild_id as i64, &name).await?,
        "global": db::get_command_row(&app.db, GLOBAL_SCOPE, &name).await?,
    })))
}

pub async fn put_command(
    State(app): State<Arc<AppState>>,
    session: Session,
    Path((guild_id, name)): Path<(u64, String)>,
    Json(body): Json<CommandBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let scope = scope_from(&body.scope, guild_id)?;
    let name = name.to_lowercase();
    let mut data = body.data;
    data.name = name.clone();
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
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let scope = scope_from(&body.scope, guild_id)?;
    let name = body.data.name.to_lowercase();
    if name.is_empty() || name.chars().count() > 32 {
        return Err(ApiError(anyhow::anyhow!("invalid command name")));
    }
    let mut data = body.data;
    data.name = name.clone();
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
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let scope = scope_from(&body.scope, guild_id)?;
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
    let cfg = app.configs.get(&app.db, guild_id).await.map_err(ApiError::from)?;
    Ok(Json(serde_json::json!({
        "effective": cfg.templates.get(&event),
        "guild_override": db::get_template_row(&app.db, guild_id as i64, &event).await?,
        "global": db::get_template_row(&app.db, GLOBAL_SCOPE, &event).await?,
    })))
}

pub async fn put_template(
    State(app): State<Arc<AppState>>,
    session: Session,
    Path((guild_id, event)): Path<(u64, String)>,
    Json(body): Json<TemplateBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let scope = scope_from(&body.scope, guild_id)?;
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
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let scope = scope_from(&body.scope, guild_id)?;
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
    channel_id: i64,
}

pub async fn list_logchannels(
    State(app): State<Arc<AppState>>,
    session: Session,
    Path(guild_id): Path<u64>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let g = db::list_log_channels(&app.db, guild_id as i64).await?;
    let gl = db::list_log_channels(&app.db, GLOBAL_SCOPE).await?;
    Ok(Json(serde_json::json!({ "guild": g, "global": gl })))
}

pub async fn get_logchannel(
    State(app): State<Arc<AppState>>,
    session: Session,
    Path((guild_id, event)): Path<(u64, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let cfg = app.configs.get(&app.db, guild_id).await.map_err(ApiError::from)?;
    Ok(Json(serde_json::json!({
        "effective": cfg.log_channels.get(&event).map(|c| c.get()),
    })))
}

pub async fn put_logchannel(
    State(app): State<Arc<AppState>>,
    session: Session,
    Path((guild_id, event)): Path<(u64, String)>,
    Json(body): Json<LogChannelBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let scope = scope_from(&body.scope, guild_id)?;
    if !crate::types::EVENT_TYPES.contains(&event.as_str()) {
        return Err(ApiError(anyhow::anyhow!("unknown event type `{event}`")));
    }
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
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let scope = scope_from(&body.scope, guild_id)?;
    db::delete_log_channel(&app.db, scope, &event).await?;
    app.invalidate_configs();
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// copy from another server
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CopyBody {
    from_guild_id: u64,
    sections: Vec<String>,
}

pub async fn copy_config(
    State(app): State<Arc<AppState>>,
    session: Session,
    Path(guild_id): Path<u64>,
    Json(body): Json<CopyBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    if !auth.can_manage(body.from_guild_id) {
        return Err(ApiError(anyhow::anyhow!(
            "you cannot manage the source server"
        )));
    }
    let dst = guild_id as i64;
    let src = body.from_guild_id as i64;
    let mut copied = Vec::new();

    if body.sections.iter().any(|s| s == "settings" || s == "all") {
        let s = db::get_settings(&app.db, src).await?;
        db::upsert_settings(&app.db, dst, &s).await?;
        copied.push("settings");
    }
    if body.sections.iter().any(|s| s == "rules" || s == "all") {
        for (rid, rule) in db::list_rules(&app.db, src).await? {
            db::upsert_rule(&app.db, dst, &rid, &rule).await?;
        }
        copied.push("rules");
    }
    if body.sections.iter().any(|s| s == "badwords" || s == "all") {
        for w in db::list_badwords(&app.db, src).await? {
            if w.guild_id == src {
                db::add_badword(&app.db, dst, &w.pattern, w.note.as_deref()).await?;
            }
        }
        copied.push("badwords");
    }
    if body.sections.iter().any(|s| s == "commands" || s == "all") {
        for (name, cmd) in db::list_commands(&app.db, src).await? {
            db::upsert_command(&app.db, dst, &name, &cmd).await?;
        }
        copied.push("commands");
    }
    if body.sections.iter().any(|s| s == "templates" || s == "all") {
        for (event, tpl) in db::list_templates(&app.db, src).await? {
            db::upsert_template(&app.db, dst, &event, &tpl).await?;
        }
        copied.push("templates");
    }
    if body.sections.iter().any(|s| s == "logchannels" || s == "all") {
        for (event, ch) in db::list_log_channels(&app.db, src).await? {
            db::upsert_log_channel(&app.db, dst, &event, ch).await?;
        }
        copied.push("logchannels");
    }

    app.invalidate_configs();
    Ok(Json(serde_json::json!({ "ok": true, "copied": copied })))
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
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let re = regex::Regex::new(&body.pattern)
        .map_err(|e| ApiError(anyhow::anyhow!("invalid regex: {e}")))?;
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
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let q = query.q.unwrap_or_default();
    let limit = query.limit.unwrap_or(50);
    let rows = db::search_members(&app.db, guild_id as i64, &q, limit).await?;
    Ok(Json(serde_json::json!(rows)))
}

#[derive(Deserialize)]
pub struct EventsQuery {
    event: Option<String>,
    user: Option<u64>,
    limit: Option<i64>,
}

pub async fn events(
    State(app): State<Arc<AppState>>,
    session: Session,
    Path(guild_id): Path<u64>,
    Query(query): Query<EventsQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = guild_scope(&app, &session, guild_id).await?;
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
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let rows = db::list_bans(&app.db, guild_id as i64, query.limit.unwrap_or(100)).await?;
    Ok(Json(serde_json::json!(rows)))
}

#[derive(Deserialize)]
pub struct UnbanBody {
    user_id: u64,
    reason: Option<String>,
}

pub async fn unban(
    State(app): State<Arc<AppState>>,
    session: Session,
    Path(guild_id): Path<u64>,
    Json(body): Json<UnbanBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = guild_scope(&app, &session, guild_id).await?;
    let reason = body.reason.unwrap_or_else(|| "unbanned via WebUI".into());

    let token = format!("Bot {}", app.cfg.discord.token);
    let http = serenity::http::Http::new(&token);
    http.remove_ban(
        serenity::model::id::GuildId::new(guild_id),
        serenity::model::id::UserId::new(body.user_id),
        None,
    )
    .await
    .map_err(|e| ApiError(anyhow::anyhow!("discord unban failed: {e}")))?;

    db::mark_unbanned(&app.db, guild_id as i64, body.user_id as i64).await?;
    let actor_id = auth.user.id.parse::<u64>().ok().map(|x| x as i64);
    let _ = db::insert_event(
        &app.db,
        guild_id as i64,
        "member_unban",
        Some(body.user_id as i64),
        actor_id,
        &serde_json::json!({"reason": reason, "source": "webui"}),
    )
    .await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn stats(
    State(app): State<Arc<AppState>>,
    session: Session,
    Path(guild_id): Path<u64>,
) -> ApiResult<Json<serde_json::Value>> {
    let _auth = guild_scope(&app, &session, guild_id).await?;
    let s = db::stats(&app.db).await?;
    Ok(Json(serde_json::json!(s)))
}
