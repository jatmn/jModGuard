//! Data-driven command engine: commands live in the database (per-guild with
//! global fallback), dispatch through a shared pipeline (permission check,
//! cooldown, behavior execution), and mirror to Discord slash commands.

pub mod behaviors;
pub mod plans;

use anyhow::Result;
use serenity::builder::{Builder, CreateCommand, CreateCommandOption, EditInteractionResponse};
use serenity::client::Context;
use serenity::model::application::{CommandDataOptionValue, CommandOptionType, Interaction};
use serenity::model::prelude::{ChannelId, GuildId, Permissions, Role, RoleId, UserId};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::state::{AppState, CachedGuildConfig};
use crate::templates;
use crate::types::CommandDef;
use crate::{db, logging};

use std::collections::HashMap as StdHashMap;

// ---------------------------------------------------------------------------
// args
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Args {
    parts: Vec<String>,
    pos: usize,
}

impl Args {
    pub fn new(raw: &str) -> Self {
        Self {
            parts: raw.split_whitespace().map(|s| s.to_string()).collect(),
            pos: 0,
        }
    }

    pub fn from_parts(parts: Vec<String>) -> Self {
        Self { parts, pos: 0 }
    }

    pub fn next_arg(&mut self) -> Option<String> {
        let v = self.parts.get(self.pos).cloned();
        if v.is_some() {
            self.pos += 1;
        }
        v
    }

    pub fn peek(&self) -> Option<&str> {
        self.parts.get(self.pos).map(|s| s.as_str())
    }

    pub fn rest(&mut self) -> String {
        let rest: Vec<String> = self.parts[self.pos.min(self.parts.len())..].to_vec();
        self.pos = self.parts.len();
        rest.join(" ")
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.parts.len()
    }
}

// ---------------------------------------------------------------------------
// permission model
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum PermLevel {
    Everyone,
    Mod,
    Admin,
    Owner,
}

impl PermLevel {
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "owner" => Self::Owner,
            "admin" => Self::Admin,
            "mod" => Self::Mod,
            _ => Self::Everyone,
        }
    }
}

/// Computes the caller's effective permission level for a guild.
pub fn perm_level(
    app: &AppState,
    guild_roles: &StdHashMap<serenity::model::id::RoleId, Role>,
    member: &serenity::model::guild::Member,
) -> PermLevel {
    if member.user.id.get() == app.cfg.bot.owner_id {
        return PermLevel::Owner;
    }
    let perms = guild_permissions(guild_roles, member);
    if perms.contains(Permissions::ADMINISTRATOR) || perms.contains(Permissions::MANAGE_GUILD) {
        return PermLevel::Admin;
    }
    let mod_perms = Permissions::from_bits_truncate(
        Permissions::KICK_MEMBERS.bits()
            | Permissions::BAN_MEMBERS.bits()
            | Permissions::MANAGE_MESSAGES.bits()
            | Permissions::MODERATE_MEMBERS.bits(),
    );
    if perms.intersects(mod_perms) {
        return PermLevel::Mod;
    }
    PermLevel::Everyone
}

/// Guild-level permissions from roles (owner bypass included).
pub fn guild_permissions(
    guild_roles: &StdHashMap<serenity::model::id::RoleId, Role>,
    member: &serenity::model::guild::Member,
) -> Permissions {
    let mut bits = 0u64;
    for role_id in &member.roles {
        if let Some(role) = guild_roles.get(role_id) {
            bits |= role.permissions.bits();
        }
    }
    Permissions::from_bits_truncate(bits)
}

/// Resolves (guild roles, caller member) for command enforcement, preferring
/// the cache and falling back to an HTTP guild fetch. Never holds a cache
/// reference across `.await`.
async fn guild_snapshot(
    ctx: &Context,
    guild_id: GuildId,
    member_from_msg: Option<serenity::model::guild::Member>,
    actor: UserId,
) -> Option<(StdHashMap<RoleId, Role>, serenity::model::guild::Member)> {
    if let Some(guild) = ctx.cache.guild(guild_id) {
        let member = member_from_msg.or_else(|| guild.members.get(&actor).cloned());
        return member.map(|m| (guild.roles.clone(), m));
    }
    tracing::warn!("guild {guild_id} not in cache; falling back to HTTP fetch");
    match ctx.http.get_guild(guild_id).await {
        Ok(guild) => {
            // REST guild payload carries roles but no member map.
            let member = member_from_msg;
            member.map(|m| (guild.roles.clone(), m))
        }
        Err(e) => {
            tracing::warn!("HTTP guild fetch for {guild_id} failed: {e}");
            None
        }
    }
}

fn check_enforcement(
    app: &AppState,
    _cfg: &CachedGuildConfig,
    cmd: &CommandDef,
    guild_roles: &StdHashMap<serenity::model::id::RoleId, Role>,
    member: &serenity::model::guild::Member,
    channel_id: serenity::model::id::ChannelId,
) -> Result<(), String> {
    if !cmd.enabled {
        return Err("that command is disabled".into());
    }
    let level = perm_level(app, guild_roles, member);
    if level < PermLevel::parse(&cmd.permission) {
        return Err("you don't have permission to use that command".into());
    }
    if !cmd.roles.is_empty()
        && !cmd
            .roles
            .iter()
            .any(|r| member.roles.contains(&RoleId::new(*r)))
    {
        return Err("this command is restricted to specific roles".into());
    }
    if !cmd.channels.is_empty() && !cmd.channels.contains(&channel_id.get()) {
        return Err("this command is restricted to specific channels".into());
    }
    Ok(())
}

/// Caller context passed to behavior executors.
pub struct CmdEnv<'a> {
    pub app: &'a Arc<AppState>,
    pub ctx: &'a Context,
    pub cmd: &'a CommandDef,
    pub guild_id: GuildId,
    pub channel_id: serenity::model::id::ChannelId,
    pub actor: serenity::model::id::UserId,
    pub actor_member: serenity::model::guild::Member,
    pub args: Args,
    /// Source message (prefix commands) — None for slash commands.
    pub source_message: Option<serenity::model::id::MessageId>,
}

pub struct BehaviorOutcome {
    pub response: Option<String>,
    pub vars: BTreeMap<String, String>,
}

// ---------------------------------------------------------------------------
// prefix dispatch
// ---------------------------------------------------------------------------

pub async fn handle_prefix(
    app: &Arc<AppState>,
    ctx: &Context,
    cfg: Arc<CachedGuildConfig>,
    msg: &serenity::model::channel::Message,
) -> Result<bool> {
    if msg.author.bot || msg.author.id.get() == ctx.cache.current_user().id.get() {
        return Ok(false);
    }
    let Some(guild_id) = msg.guild_id else {
        return Ok(false);
    };
    let prefix = &cfg.settings.prefix;
    tracing::debug!(
        "prefix check: prefix={prefix:?}, content={:?}, starts={}",
        msg.content,
        prefix.is_empty() || msg.content.starts_with(prefix.as_str())
    );
    if prefix.is_empty() || !msg.content.starts_with(prefix.as_str()) {
        return Ok(false);
    }
    let body = &msg.content[prefix.len()..];
    let mut it = body.splitn(2, char::is_whitespace);
    let Some(name) = it.next() else {
        return Ok(false);
    };
    if name.is_empty() {
        return Ok(false);
    }
    let raw_args = it.next().unwrap_or("");

    let Some(cmd) = cfg.find_command(name) else {
        tracing::debug!(
            "no command matched prefix input `{name}` in guild {guild_id} (prefix `{prefix}`)"
        );
        // Give the user feedback instead of silence: reply only to short
        // inputs so stray characters can't trigger spam.
        if raw_args.is_empty() && body.chars().count() <= 32 {
            let reply = format!("Unknown command `{name}`. Use `{prefix}help` to list commands.");
            if let Err(e) = msg.channel_id.say(&ctx.http, reply).await {
                tracing::warn!("failed to send unknown-command reply: {e}");
            }
        }
        return Ok(false);
    };
    let cmd = cmd.clone();

    // Resolve the caller's member data and the guild's roles. The cache is
    // preferred; if the guild is not cached (e.g. still flagged unavailable),
    // fall back to an HTTP guild fetch so commands still work.
    let member_from_msg = msg
        .member
        .clone()
        .map(|m| serenity::model::guild::Member::from(*m));
    let Some((guild_roles, member)) =
        guild_snapshot(ctx, guild_id, member_from_msg, msg.author.id).await
    else {
        tracing::warn!(
            "could not resolve member data for {} in {guild_id}",
            msg.author.id
        );
        return Ok(false);
    };

    if let Err(reason) = check_enforcement(app, &cfg, &cmd, &guild_roles, &member, msg.channel_id) {
        let _ = msg
            .channel_id
            .say(&ctx.http, format!("{} — {reason}", member.display_name()))
            .await;
        return Ok(true);
    }

    if !cooldown_check(app, guild_id, msg.author.id, &cmd) {
        let _ = msg
            .channel_id
            .say(&ctx.http, "that command is on cooldown")
            .await;
        return Ok(true);
    }

    let env = CmdEnv {
        app,
        ctx,
        cmd: &cmd,
        guild_id,
        channel_id: msg.channel_id,
        actor: msg.author.id,
        actor_member: member,
        args: Args::new(raw_args),
        source_message: Some(msg.id),
    };
    tracing::info!(
        "command `{}` invoked by {} in guild {} (channel {})",
        cmd.name,
        msg.author.id,
        guild_id,
        msg.channel_id
    );
    let outcome = match run_command(env).await {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!("command `{}` failed: {e:#}", cmd.name);
            let reply = format!("error: {e:#}");
            if let Err(say_err) = msg.channel_id.say(&ctx.http, reply).await {
                tracing::warn!("failed to send error reply: {say_err}");
            }
            return Ok(true);
        }
    };
    send_outcome(
        app,
        ctx,
        &cmd,
        guild_id,
        msg.channel_id,
        msg.author.id,
        outcome,
    )
    .await;
    Ok(true)
}

// ---------------------------------------------------------------------------
// slash commands
// ---------------------------------------------------------------------------

/// Registers guild slash commands from the command registry.
pub async fn sync_slash_commands(app: &Arc<AppState>, ctx: &Context, guild_id: GuildId) -> bool {
    let target = match db::get_slash_registry_version(&app.db).await {
        Ok(version) => version.max(app.acknowledged_slash_registry_version()),
        Err(error) => {
            tracing::warn!("slash sync: registry version load failed for {guild_id}: {error:#}");
            return false;
        }
    };
    sync_slash_commands_versioned_http(app, &ctx.http, guild_id, target).await
}

/// Registry-poll variant: command rows and the version that identifies them
/// are read from one database snapshot. A lagging replica is retried instead
/// of allowing Discord to accept stale rows under a newer acknowledged version.
pub async fn sync_slash_commands_versioned_http(
    app: &Arc<AppState>,
    http: impl AsRef<serenity::http::Http>,
    guild_id: GuildId,
    target_version: u64,
) -> bool {
    let (loaded_version, commands) =
        match db::load_slash_commands_with_version(&app.db, guild_id.get() as i64).await {
            Ok(loaded) => loaded,
            Err(error) => {
                tracing::warn!(
                    "slash sync: versioned registry load failed for {guild_id}: {error:#}"
                );
                return false;
            }
        };
    if loaded_version != target_version {
        tracing::warn!(
            "slash sync: replica returned registry version {loaded_version} for {guild_id}, expected {target_version}; will retry"
        );
        return false;
    }
    set_slash_commands(http, guild_id, &commands).await
}

async fn set_slash_commands(
    http: impl AsRef<serenity::http::Http>,
    guild_id: GuildId,
    registry: &std::collections::HashMap<String, CommandDef>,
) -> bool {
    let mut commands: Vec<CreateCommand> = Vec::new();
    for cmd in registry.values() {
        if !cmd.enabled {
            continue;
        }
        // Legacy rows predate WebUI validation. Keep their previous behavior
        // rather than silently dropping commands when rebuilding Discord's
        // whole guild command set after an upgrade.
        let name = sanitize_slash_name(&cmd.name);
        let description = slash_description(&cmd.description);
        let mut builder = CreateCommand::new(name).description(description);
        for opt in behaviors::slash_spec(&cmd.behavior) {
            let mut o = CreateCommandOption::new(opt.kind, opt.name, opt.description)
                .required(opt.required);
            if opt.kind == CommandOptionType::Integer {
                o = o.min_int_value(0);
            }
            builder = builder.add_option(o);
        }
        commands.push(builder);
    }
    match tokio::time::timeout(
        crate::discord::DISCORD_REST_TIMEOUT,
        guild_id.set_commands(http.as_ref(), commands),
    )
    .await
    {
        Ok(Ok(_)) => true,
        Ok(Err(e)) => {
            tracing::warn!("slash sync failed for {guild_id}: {e}");
            false
        }
        Err(_) => {
            tracing::warn!("slash sync timed out for {guild_id}");
            false
        }
    }
}

fn sanitize_slash_name(name: &str) -> String {
    let out: String = name
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(32)
        .collect();
    if out.is_empty() {
        "command".to_string()
    } else {
        out
    }
}

fn slash_description(description: &str) -> String {
    if description.is_empty() {
        return "Custom command".to_string();
    }
    let count = description.chars().count();
    if count <= 100 {
        return description.to_string();
    }
    let mut out: String = description.chars().take(99).collect();
    out.push('…');
    out
}

/// Owned data extracted from a `CommandInteraction` up front, because the
/// interaction itself is `!Send` (it borrows the cache) and must be dropped
/// before any `.await` in the dispatched future.
struct SlashCtx {
    id: serenity::model::id::InteractionId,
    token: String,
    channel_id: ChannelId,
    guild_id: GuildId,
    actor: UserId,
    member: Option<serenity::model::guild::Member>,
    name: String,
    options: Vec<(String, String)>,
}

pub async fn handle_interaction(app: &Arc<AppState>, ctx: &Context, interaction: Interaction) {
    let Interaction::Command(interaction) = interaction else {
        return;
    };

    // Extract everything owned, then drop the interaction immediately.
    let slash = SlashCtx {
        id: interaction.id,
        token: interaction.token.clone(),
        channel_id: interaction.channel_id,
        guild_id: interaction.guild_id.unwrap_or_default(),
        actor: interaction.user.id,
        member: interaction.member.clone().map(|m| *m),
        name: interaction.data.name.clone(),
        options: interaction
            .data
            .options
            .iter()
            .map(|o| (o.name.clone(), option_value_string(Some(o.value.clone()))))
            .collect(),
    };
    drop(interaction);

    if slash.guild_id.get() == 0 {
        let _ = serenity::builder::CreateInteractionResponse::Message(
            serenity::builder::CreateInteractionResponseMessage::new()
                .content("this command only works inside a server")
                .ephemeral(true),
        )
        .execute(&ctx.http, (slash.id, slash.token.as_str()))
        .await;
        return;
    }
    let guild_id = slash.guild_id;

    // Defer so slow behaviors (purge, bans) don't hit the 3s window.
    let deferred = serenity::builder::CreateInteractionResponse::Defer(
        serenity::builder::CreateInteractionResponseMessage::new(),
    )
    .execute(&ctx.http, (slash.id, slash.token.as_str()))
    .await
    .is_ok();

    let cfg = match app.configs.get(&app.db, guild_id.get()).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("interaction: config load failed: {e:#}");
            respond(&slash, ctx, deferred, "internal error").await;
            return;
        }
    };

    let Some(cmd) = cfg.find_command(&slash.name) else {
        tracing::debug!(
            "no slash command matched `{}` in guild {guild_id}",
            slash.name
        );
        respond(&slash, ctx, deferred, "unknown command").await;
        return;
    };
    let cmd = cmd.clone();

    tracing::info!(
        "slash command `{}` invoked by {} in guild {}",
        cmd.name,
        slash.actor,
        guild_id
    );

    let actor_id = slash.actor;
    // Resolve roles + member via cache, falling back to HTTP (see
    // guild_snapshot). The interaction itself is dropped already.
    let snapshot = guild_snapshot(ctx, guild_id, slash.member.clone(), actor_id).await;
    let Some((guild_roles, member)) = snapshot else {
        respond(
            &slash,
            ctx,
            deferred,
            "guild not cached / member data unavailable",
        )
        .await;
        return;
    };

    if let Err(reason) = check_enforcement(app, &cfg, &cmd, &guild_roles, &member, slash.channel_id)
    {
        respond(&slash, ctx, deferred, &reason.to_string()).await;
        return;
    }
    if !cooldown_check(app, guild_id, actor_id, &cmd) {
        respond(&slash, ctx, deferred, "that command is on cooldown").await;
        return;
    }

    let args = Args::from_parts(options_to_args(&cmd, &slash.options));
    let env = CmdEnv {
        app,
        ctx,
        cmd: &cmd,
        guild_id,
        channel_id: slash.channel_id,
        actor: actor_id,
        actor_member: member,
        args,
        source_message: None,
    };
    match run_command(env).await {
        Ok(outcome) => {
            let text = render_outcome(&cmd, &outcome);
            let ephemeral = outcome.response.is_none() && cmd.response.is_empty();
            if deferred {
                let _ = EditInteractionResponse::new()
                    .content(text)
                    .execute(&ctx.http, slash.token.as_str())
                    .await;
            } else {
                let _ = serenity::builder::CreateInteractionResponse::Message(
                    serenity::builder::CreateInteractionResponseMessage::new()
                        .content(text)
                        .ephemeral(ephemeral),
                )
                .execute(&ctx.http, (slash.id, slash.token.as_str()))
                .await;
            }
            log_outcome(app, ctx, &cmd, guild_id, actor_id, outcome).await;
        }
        Err(e) => {
            let text = format!("error: {e:#}");
            respond(&slash, ctx, deferred, &text).await;
        }
    }
}

fn option_value_string(value: Option<CommandDataOptionValue>) -> String {
    match value {
        Some(CommandDataOptionValue::User(id)) => id.get().to_string(),
        Some(CommandDataOptionValue::Channel(id)) => id.get().to_string(),
        Some(CommandDataOptionValue::Role(id)) => id.get().to_string(),
        Some(CommandDataOptionValue::String(s)) => s,
        Some(CommandDataOptionValue::Integer(i)) => i.to_string(),
        Some(CommandDataOptionValue::Boolean(b)) => b.to_string(),
        Some(CommandDataOptionValue::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn options_to_args(cmd: &CommandDef, options: &[(String, String)]) -> Vec<String> {
    let mut out = Vec::new();
    for spec in behaviors::slash_spec(&cmd.behavior) {
        let s = options
            .iter()
            .find(|(n, _)| n == spec.name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        if !s.is_empty() {
            out.push(s);
        }
    }
    out
}

async fn respond(slash: &SlashCtx, ctx: &Context, deferred: bool, text: &str) {
    if deferred {
        let _ = EditInteractionResponse::new()
            .content(text)
            .execute(&ctx.http, slash.token.as_str())
            .await;
    } else {
        let _ = serenity::builder::CreateInteractionResponse::Message(
            serenity::builder::CreateInteractionResponseMessage::new()
                .content(text)
                .ephemeral(true),
        )
        .execute(&ctx.http, (slash.id, slash.token.as_str()))
        .await;
    }
}

// ---------------------------------------------------------------------------
// shared pipeline
// ---------------------------------------------------------------------------

async fn run_command(env: CmdEnv<'_>) -> Result<BehaviorOutcome> {
    behaviors::run(&env).await
}

fn render_outcome(cmd: &CommandDef, outcome: &BehaviorOutcome) -> String {
    if !cmd.response.is_empty() {
        let mut vars = outcome.vars.clone();
        vars.insert(
            "response".into(),
            outcome.response.clone().unwrap_or_default(),
        );
        templates::render(&cmd.response, &vars)
    } else if let Some(r) = &outcome.response {
        r.clone()
    } else {
        "done".to_string()
    }
}

async fn send_outcome(
    app: &Arc<AppState>,
    ctx: &Context,
    cmd: &CommandDef,
    guild_id: GuildId,
    channel_id: serenity::model::id::ChannelId,
    actor_id: serenity::model::id::UserId,
    outcome: BehaviorOutcome,
) {
    let text = render_outcome(cmd, &outcome);
    if !text.is_empty()
        && let Err(e) = channel_id.say(&ctx.http, text).await
    {
        tracing::warn!(
            "failed to send response for `{}` in {channel_id} (missing Send Messages permission?): {e}",
            cmd.name
        );
    }
    log_outcome(app, ctx, cmd, guild_id, actor_id, outcome).await;
}

async fn log_outcome(
    app: &Arc<AppState>,
    ctx: &Context,
    cmd: &CommandDef,
    guild_id: GuildId,
    actor_id: serenity::model::id::UserId,
    outcome: BehaviorOutcome,
) {
    if cmd.log_event.is_empty() {
        return;
    }
    let mut vars = outcome.vars.clone();
    vars.insert("command".into(), cmd.name.clone());
    let actor_name = ctx
        .cache
        .user(actor_id)
        .map(|u| u.name.clone())
        .unwrap_or_else(|| actor_id.to_string());
    vars.insert("actor".into(), format!("{actor_name} (<@{actor_id}>)"));
    logging::emit(app, ctx, guild_id, &cmd.log_event, vars).await;
}

fn cooldown_check(
    app: &AppState,
    guild_id: GuildId,
    user: serenity::model::id::UserId,
    cmd: &CommandDef,
) -> bool {
    if cmd.cooldown_secs == 0 {
        return true;
    }
    let key = (guild_id.get(), user.get(), cmd.name.clone());
    let now = Instant::now();
    if let Some(entry) = app.cooldowns.get(&key)
        && now.duration_since(*entry.value()) < Duration::from_secs(cmd.cooldown_secs)
    {
        return false;
    }
    app.cooldowns.insert(key, now);
    true
}

/// Resolves a user from a mention (`<@123>` / `<@!123>`), raw ID, or member
/// name/nick search.
pub async fn resolve_user(
    ctx: &Context,
    guild_id: GuildId,
    token: &str,
) -> Option<serenity::model::id::UserId> {
    let trimmed = token.trim();
    if let Some(inner) = trimmed
        .strip_prefix("<@")
        .and_then(|t| t.strip_suffix('>'))
        .map(|t| t.trim_start_matches('!'))
        && let Ok(id) = inner.parse::<u64>()
    {
        return Some(id.into());
    }
    if let Ok(id) = trimmed.parse::<u64>() {
        return Some(id.into());
    }
    let guild = ctx.cache.guild(guild_id)?;
    let lower = trimmed.to_lowercase();
    guild
        .members
        .iter()
        .find(|(_, m)| {
            m.user.name.to_lowercase() == lower
                || m.nick
                    .as_deref()
                    .map(|n| n.to_lowercase() == lower)
                    .unwrap_or(false)
                || m.user
                    .global_name
                    .as_deref()
                    .map(|g| g.to_lowercase() == lower)
                    .unwrap_or(false)
        })
        .map(|(_, m)| m.user.id)
}
