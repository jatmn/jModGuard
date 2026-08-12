//! Event routing: renders a template for an event type and posts it to the
//! configured guild-scoped log channel, plus audit-log recording.

use serenity::client::Context;
use serenity::model::id::GuildId;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::state::AppState;
use crate::{db, templates};

/// Resolves the log channel for an event type from the cached config.
fn channel_for(
    cfg: &crate::state::CachedGuildConfig,
    event: &str,
) -> Option<serenity::model::id::ChannelId> {
    cfg.log_channels.get(event).copied()
}

/// Emits an event: writes to the audit log table and posts the rendered
/// template to the configured log channel (if any). Never panics on errors;
/// failures are traced only.
pub async fn emit(
    app: &Arc<AppState>,
    ctx: &Context,
    guild_id: GuildId,
    event: &str,
    mut vars: BTreeMap<String, String>,
) {
    vars.insert("time".into(), chrono::Utc::now().to_rfc3339());
    vars.insert("guild".into(), guild_id.get().to_string());

    let cfg = match app.configs.get(&app.db, guild_id.get()).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("emit {event}: config load failed: {e:#}");
            return;
        }
    };

    let Some(channel_id) = channel_for(&cfg, event) else {
        return;
    };

    let template: String = cfg
        .templates
        .get(event)
        .cloned()
        .unwrap_or_else(|| default_template(event).to_string());

    let rendered = templates::render(&template, &vars);
    if let Err(e) = channel_id.say(&ctx.http, &rendered).await {
        tracing::warn!("emit {event} to {channel_id}: {e}");
    }
}

/// Emits a mod-action event with actor context (used by command behaviors).
#[allow(clippy::too_many_arguments)]
pub async fn emit_mod_action(
    app: &Arc<AppState>,
    ctx: &Context,
    guild_id: GuildId,
    action: &str,
    target_id: u64,
    target_name: &str,
    target_display: &str,
    actor_id: u64,
    actor_name: &str,
    reason: &str,
) {
    let mut vars = templates::user_vars(target_id, target_name, target_display);
    vars.insert("actor".into(), format!("{} (<@{}>)", actor_name, actor_id));
    vars.insert("action".into(), action.to_string());
    vars.insert("reason".into(), reason.to_string());
    emit(app, ctx, guild_id, "mod_action", vars).await;

    let _ = db::insert_event(
        &app.db,
        guild_id.get() as i64,
        "mod_action",
        Some(target_id as i64),
        Some(actor_id as i64),
        &serde_json::json!({"action": action, "reason": reason}),
    )
    .await;
}

/// Emits a member event (join/leave/ban/unban/kick) with full member context.
pub async fn emit_member_event(
    app: &Arc<AppState>,
    ctx: &Context,
    guild_id: GuildId,
    event: &str,
    user: &serenity::model::user::User,
    extra: BTreeMap<String, String>,
) {
    let account_created = user.created_at();
    let account_age_days =
        (chrono::Utc::now().timestamp() - account_created.unix_timestamp()).max(0) as u64 / 86400;

    let mut vars = templates::user_vars(
        user.id.get(),
        &user.name,
        user.global_name.as_deref().unwrap_or(&user.name),
    );
    vars.insert("account_created".into(), account_created.to_string());
    vars.insert("account_age".into(), format!("{account_age_days} days"));
    vars.extend(extra);
    emit(app, ctx, guild_id, event, vars).await;
}

fn default_template(event: &str) -> &'static str {
    crate::defaults::default_templates()
        .into_iter()
        .find(|(e, _)| *e == event)
        .map(|(_, t)| t)
        .unwrap_or("**{event}**\n{content}")
}
