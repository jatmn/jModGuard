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

use crate::state::{BotState, CachedGuildConfig};
use crate::templates;
use crate::types::{Behavior, CommandDef};
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
    app: &BotState,
    guild_roles: &StdHashMap<serenity::model::id::RoleId, Role>,
    member: &serenity::model::guild::Member,
) -> PermLevel {
    permission_level(
        app.cfg.bot.owner_id,
        guild_roles,
        member.user.id.get(),
        member.roles.iter().map(|role| role.get()),
    )
}

pub fn permission_level(
    owner_id: u64,
    guild_roles: &StdHashMap<RoleId, Role>,
    user_id: u64,
    role_ids: impl IntoIterator<Item = u64>,
) -> PermLevel {
    if user_id == owner_id {
        return PermLevel::Owner;
    }
    let mut bits = 0u64;
    for role_id in role_ids {
        if let Some(role) = guild_roles.get(&RoleId::new(role_id)) {
            bits |= role.permissions.bits();
        }
    }
    let perms = Permissions::from_bits_truncate(bits);
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeratorStatus {
    Moderator,
    Member,
    /// The guild or a required role is not cached, so status cannot be resolved.
    Unknown,
}

/// Guild-cache lookup. A miss is [`ModeratorStatus::Unknown`], not a member.
pub fn cached_moderator_status(
    app: &BotState,
    cache: &serenity::cache::Cache,
    guild_id: GuildId,
    user_id: u64,
    role_ids: &[u64],
) -> ModeratorStatus {
    let Some(guild) = cache.guild(guild_id) else {
        return ModeratorStatus::Unknown;
    };
    if role_ids
        .iter()
        .any(|id| !guild.roles.contains_key(&RoleId::new(*id)))
    {
        return ModeratorStatus::Unknown;
    }
    if permission_level(
        app.cfg.bot.owner_id,
        &guild.roles,
        user_id,
        role_ids.iter().copied(),
    ) >= PermLevel::Mod
    {
        ModeratorStatus::Moderator
    } else {
        ModeratorStatus::Member
    }
}

/// `true` only when the cache shows moderator or higher. Unknown is `false`,
/// so admission still sees the message; kick and ban must consult
/// [`cached_moderator_status`] before punishing the member.
pub fn cached_is_moderator(
    app: &BotState,
    ctx: &Context,
    guild_id: GuildId,
    user_id: u64,
    role_ids: &[u64],
) -> bool {
    cached_moderator_status(app, &ctx.cache, guild_id, user_id, role_ids)
        == ModeratorStatus::Moderator
}

/// Whether this member can use Mention Everyone in the channel.
///
/// `Unknown` means the channel is not in `guild.channels` or `guild.threads`,
/// a thread's parent channel is not cached, or the guild or member needed for
/// that lookup is missing. That is not a deny: the message can still be
/// deleted, but kick and ban are withheld.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MentionPermission {
    Allowed,
    Denied,
    Unknown,
}

/// Moderator bit and mention permission for one gateway message.
///
/// The author id is the message author. `PartialMember::user` is often null,
/// and `Member::from` then fills a default user id that must not be used here.
pub(crate) struct MessageAuthorScope {
    pub is_moderator: bool,
    pub mention: MentionPermission,
}

/// Mention permission for one gateway message.
///
/// A missing guild cache entry or a message with no member is unknown, not a
/// cached deny. The author is not treated as a moderator.
pub(crate) fn author_message_scope(
    owner_id: u64,
    guild: Option<&serenity::model::guild::Guild>,
    channel_id: ChannelId,
    author_id: UserId,
    partial: Option<&serenity::model::guild::PartialMember>,
) -> MessageAuthorScope {
    match (guild, partial) {
        (Some(guild), Some(partial)) => {
            message_author_scope(owner_id, guild, channel_id, author_id, partial)
        }
        _ => MessageAuthorScope {
            is_moderator: false,
            mention: MentionPermission::Unknown,
        },
    }
}

pub(crate) fn message_author_scope(
    owner_id: u64,
    guild: &serenity::model::guild::Guild,
    channel_id: ChannelId,
    author_id: UserId,
    partial: &serenity::model::guild::PartialMember,
) -> MessageAuthorScope {
    let is_moderator = permission_level(
        owner_id,
        &guild.roles,
        author_id.get(),
        partial.roles.iter().map(|role| role.get()),
    ) >= PermLevel::Mod;
    MessageAuthorScope {
        is_moderator,
        mention: mention_permission_in(guild, channel_id, author_id, &partial.roles),
    }
}

/// Mention Everyone in this channel, including overwrites and thread parents.
///
/// Application ownership is not a Discord permission. True only when the
/// channel permission is [`MentionPermission::Allowed`].
pub fn can_mention_everyone_in(
    guild: &serenity::model::guild::Guild,
    channel_id: ChannelId,
    user_id: UserId,
    role_ids: &[RoleId],
) -> bool {
    mention_permission_in(guild, channel_id, user_id, role_ids) == MentionPermission::Allowed
}

pub(crate) fn mention_permission_in(
    guild: &serenity::model::guild::Guild,
    channel_id: ChannelId,
    user_id: UserId,
    role_ids: &[RoleId],
) -> MentionPermission {
    let Some(channel) = channel_for_member_permissions(guild, channel_id) else {
        return MentionPermission::Unknown;
    };
    if !guild.roles.contains_key(&RoleId::new(guild.id.get()))
        || role_ids.iter().any(|id| !guild.roles.contains_key(id))
    {
        return MentionPermission::Unknown;
    }
    let Some(partial) = partial_member(guild.id, role_ids) else {
        return MentionPermission::Unknown;
    };
    let perms = guild.partial_member_permissions_in(channel, user_id, &partial);
    if perms.contains(Permissions::MENTION_EVERYONE) {
        MentionPermission::Allowed
    } else {
        MentionPermission::Denied
    }
}

/// `PartialMember` is `non_exhaustive`, so tests and this helper share a decode.
fn partial_member(
    guild_id: GuildId,
    role_ids: &[RoleId],
) -> Option<serenity::model::guild::PartialMember> {
    let roles: Vec<String> = role_ids.iter().map(|id| id.get().to_string()).collect();
    serde_json::from_value(serde_json::json!({
        "deaf": false,
        "joined_at": null,
        "mute": false,
        "nick": null,
        "roles": roles,
        "pending": false,
        "premium_since": null,
        "guild_id": guild_id.get().to_string(),
        "user": null,
        "permissions": null,
        "unusual_dm_activity_until": null,
        "avatar": null,
        "banner": null,
        "avatar_decoration_data": null,
    }))
    .ok()
}

fn is_thread(kind: serenity::model::channel::ChannelType) -> bool {
    matches!(
        kind,
        serenity::model::channel::ChannelType::PublicThread
            | serenity::model::channel::ChannelType::PrivateThread
            | serenity::model::channel::ChannelType::NewsThread
    )
}

fn channel_for_member_permissions(
    guild: &serenity::model::guild::Guild,
    channel_id: ChannelId,
) -> Option<&serenity::model::channel::GuildChannel> {
    if let Some(channel) = guild.channels.get(&channel_id) {
        return permission_channel(guild, channel);
    }
    let thread = guild
        .threads
        .iter()
        .find(|thread| thread.id == channel_id)?;
    permission_channel(guild, thread)
}

fn permission_channel<'a>(
    guild: &'a serenity::model::guild::Guild,
    channel: &'a serenity::model::channel::GuildChannel,
) -> Option<&'a serenity::model::channel::GuildChannel> {
    if is_thread(channel.kind) {
        let parent_id = channel.parent_id?;
        return guild.channels.get(&parent_id);
    }
    Some(channel)
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
    app: &BotState,
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
    pub app: &'a Arc<BotState>,
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

pub(super) async fn refuse_fenced_discord_mutation(env: &CmdEnv<'_>) -> Result<()> {
    if db::guild_stores::guild_is_write_fenced(&env.app.db, env.guild_id.get() as i64).await? {
        return Err(db::guild_stores::GuildWriteFenced.into());
    }
    Ok(())
}

pub struct BehaviorOutcome {
    pub response: Option<String>,
    pub vars: BTreeMap<String, String>,
    /// True when the behavior's visible effect already landed in the
    /// invocation channel (e.g. `!say` posting there), so a confirmation
    /// reply would be redundant. Prefix dispatch sends nothing; slash
    /// dispatch withdraws the deferred answer (or answers ephemerally when
    /// the defer failed — an interaction must always be answered).
    pub suppress_response: bool,
}

// ---------------------------------------------------------------------------
// prefix dispatch
// ---------------------------------------------------------------------------

pub async fn handle_prefix(
    app: &Arc<BotState>,
    http: &serenity::http::Http,
    cache: &serenity::cache::Cache,
    cfg: Arc<CachedGuildConfig>,
    msg: &serenity::model::channel::Message,
    ctx: Option<&Context>,
) -> Result<bool> {
    if msg.author.bot || msg.author.id.get() == cache.current_user().id.get() {
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
        prefix_unknown_command_reply(
            &app.activation,
            msg.channel_id,
            http,
            name,
            prefix,
            raw_args,
            body,
        )
        .await;
        return Ok(false);
    };
    let cmd = cmd.clone();
    let ctx =
        ctx.ok_or_else(|| anyhow::anyhow!("prefix command dispatch requires gateway context"))?;

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
        let text = format!("{} — {reason}", member.display_name());
        if raced_channel_say(&app.activation, msg.channel_id, http, text).await
            == RacedWrite::Cutoff
        {
            tracing::warn!("enforcement denial reply suppressed after writer cutoff");
        }
        return Ok(true);
    }

    if !cooldown_check(app, guild_id, msg.author.id, &cmd) {
        if raced_channel_say(
            &app.activation,
            msg.channel_id,
            http,
            "that command is on cooldown".into(),
        )
        .await
            == RacedWrite::Cutoff
        {
            tracing::warn!("cooldown reply suppressed after writer cutoff");
        }
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
            send_prefix_command_error(&app.activation, msg.channel_id, http, &cmd.name, &e).await;
            return Ok(true);
        }
    };
    send_outcome(
        app,
        http,
        cache,
        &cmd,
        guild_id,
        msg.channel_id,
        msg.author.id,
        outcome,
    )
    .await;
    Ok(true)
}

async fn prefix_unknown_command_reply(
    activation: &crate::activation::Activation,
    channel_id: serenity::model::id::ChannelId,
    http: &serenity::http::Http,
    name: &str,
    prefix: &str,
    raw_args: &str,
    body: &str,
) {
    if raw_args.is_empty() && body.chars().count() <= 32 {
        let reply = format!("Unknown command `{name}`. Use `{prefix}help` to list commands.");
        if raced_channel_say(activation, channel_id, http, reply).await == RacedWrite::Cutoff {
            tracing::warn!("unknown-command reply suppressed after writer cutoff");
        }
    }
}

// ---------------------------------------------------------------------------
// slash commands
// ---------------------------------------------------------------------------

/// Registers guild slash commands from the command registry.
pub async fn sync_slash_commands(app: &Arc<BotState>, ctx: &Context, guild_id: GuildId) -> bool {
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
    app: &Arc<BotState>,
    http: impl AsRef<serenity::http::Http>,
    guild_id: GuildId,
    target_version: u64,
) -> bool {
    let expected_revision = app.slash_registry_revision();
    if !app.slash_sync_due(
        guild_id.get(),
        target_version,
        &expected_revision,
        std::time::Instant::now(),
    ) {
        return false;
    }
    let (mut loaded_version, mut loaded_revision, mut commands) =
        match db::load_slash_commands_with_version(&app.db, guild_id.get() as i64).await {
            Ok(loaded) => loaded,
            Err(error) => {
                tracing::warn!(
                    "slash sync: versioned registry load failed for {guild_id}: {error:#}"
                );
                return false;
            }
        };
    if loaded_version != target_version || loaded_revision != expected_revision {
        match db::load_slash_commands_with_version_primary(&app.db, guild_id.get() as i64).await {
            Ok((version, revision, primary_commands)) => {
                loaded_version = version;
                loaded_revision = revision;
                commands = primary_commands;
            }
            Err(error) => {
                tracing::warn!(
                    "slash sync: primary registry load failed for {guild_id}: {error:#}"
                );
                return false;
            }
        }
    }
    if loaded_version != target_version || loaded_revision != expected_revision {
        tracing::warn!(
            "slash sync: registry snapshot {loaded_version}/{loaded_revision} for {guild_id} did not match expected {target_version}/{expected_revision}; will retry"
        );
        return false;
    }
    if !app.activation.may_record() {
        return false;
    }
    set_slash_commands(
        app,
        http,
        guild_id,
        &commands,
        target_version,
        &expected_revision,
    )
    .await
}

async fn set_slash_commands(
    app: &BotState,
    http: impl AsRef<serenity::http::Http>,
    guild_id: GuildId,
    registry: &std::collections::HashMap<String, CommandDef>,
    target: u64,
    revision: &str,
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
    tracing::info!(
        guild_id = guild_id.get(),
        target,
        "syncing Discord slash commands"
    );
    let sync = tokio::time::timeout(
        crate::discord::DISCORD_REST_TIMEOUT,
        guild_id.set_commands(http.as_ref(), commands),
    );
    match crate::discord::race_discord_call(&app.activation, sync).await {
        crate::discord::DiscordCall::Cancelled => false,
        crate::discord::DiscordCall::Finished(Ok(Ok(_))) => {
            app.reset_slash_guild_retry(guild_id.get());
            true
        }
        crate::discord::DiscordCall::Finished(Ok(Err(e))) => {
            let class = crate::discord::classify_retry(&e);
            app.slash_sync_failed(
                guild_id.get(),
                target,
                revision,
                class,
                crate::discord::http_error_response(&e),
            );
            if class == crate::discord::RetryClass::Transient {
                tracing::warn!("slash sync deferred for {guild_id}: {e}");
            }
            false
        }
        crate::discord::DiscordCall::Finished(Err(_)) => {
            app.slash_sync_failed(
                guild_id.get(),
                target,
                revision,
                crate::discord::RetryClass::Transient,
                None,
            );
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

pub async fn handle_interaction(app: &Arc<BotState>, ctx: &Context, interaction: Interaction) {
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
            respond(&app.activation, &slash, ctx, deferred, "internal error").await;
            return;
        }
    };

    let Some(cmd) = cfg.find_command(&slash.name) else {
        tracing::debug!(
            "no slash command matched `{}` in guild {guild_id}",
            slash.name
        );
        respond(&app.activation, &slash, ctx, deferred, "unknown command").await;
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
            &app.activation,
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
        respond(&app.activation, &slash, ctx, deferred, &reason.to_string()).await;
        return;
    }
    if !cooldown_check(app, guild_id, actor_id, &cmd) {
        respond(
            &app.activation,
            &slash,
            ctx,
            deferred,
            "that command is on cooldown",
        )
        .await;
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
            if deliver_slash_success(&app.activation, &ctx.http, &slash, deferred, &cmd, &outcome)
                .await
                != RacedWrite::Cutoff
            {
                log_outcome(
                    app,
                    &ctx.http,
                    &ctx.cache,
                    &cmd,
                    guild_id,
                    slash.channel_id,
                    actor_id,
                    outcome,
                )
                .await;
            }
        }
        Err(e) => {
            let text = format!("error: {e:#}");
            respond(&app.activation, &slash, ctx, deferred, &text).await;
        }
    }
}

async fn send_prefix_command_error(
    activation: &crate::activation::Activation,
    channel_id: serenity::model::id::ChannelId,
    http: &serenity::http::Http,
    command_name: &str,
    error: &anyhow::Error,
) {
    let reply = format!("error: {error:#}");
    if raced_channel_say(activation, channel_id, http, reply).await == RacedWrite::Cutoff {
        tracing::warn!(
            "command `{command_name}` error reply suppressed after writer cutoff: {error:#}"
        );
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

async fn respond(
    activation: &crate::activation::Activation,
    slash: &SlashCtx,
    ctx: &Context,
    deferred: bool,
    text: &str,
) {
    let delivered = if deferred {
        raced_slash_edit(activation, &ctx.http, slash.token.as_str(), text).await
    } else {
        raced_slash_create_message(
            activation,
            &ctx.http,
            slash.id,
            slash.token.as_str(),
            text,
            true,
        )
        .await
    };
    if delivered == RacedWrite::Cutoff {
        tracing::warn!(
            "slash response for `{name}` suppressed after writer cutoff",
            name = slash.name
        );
    }
}

/// Outcome of a cutoff-raced Discord write attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RacedWrite {
    /// Writer cutoff won before the call finished.
    Cutoff,
    /// The call finished (success or ordinary Discord error).
    Finished,
}

fn raced_write_chain(first: RacedWrite, second: RacedWrite) -> RacedWrite {
    if first == RacedWrite::Cutoff || second == RacedWrite::Cutoff {
        RacedWrite::Cutoff
    } else {
        RacedWrite::Finished
    }
}

async fn raced_channel_say(
    activation: &crate::activation::Activation,
    channel_id: ChannelId,
    http: &serenity::http::Http,
    text: String,
) -> RacedWrite {
    match crate::discord::race_discord_call(activation, channel_id.say(http, text)).await {
        crate::discord::DiscordCall::Finished(Ok(_)) => RacedWrite::Finished,
        crate::discord::DiscordCall::Finished(Err(error)) => {
            tracing::warn!("failed to send channel message: {error}");
            RacedWrite::Finished
        }
        crate::discord::DiscordCall::Cancelled => RacedWrite::Cutoff,
    }
}

async fn raced_slash_edit(
    activation: &crate::activation::Activation,
    http: &serenity::http::Http,
    token: &str,
    content: &str,
) -> RacedWrite {
    match crate::discord::race_discord_call(
        activation,
        EditInteractionResponse::new()
            .content(content)
            .execute(http, token),
    )
    .await
    {
        crate::discord::DiscordCall::Finished(Ok(_)) => RacedWrite::Finished,
        crate::discord::DiscordCall::Finished(Err(error)) => {
            tracing::warn!("failed to edit slash response: {error}");
            RacedWrite::Finished
        }
        crate::discord::DiscordCall::Cancelled => RacedWrite::Cutoff,
    }
}

async fn raced_slash_create_message(
    activation: &crate::activation::Activation,
    http: &serenity::http::Http,
    id: serenity::model::id::InteractionId,
    token: &str,
    content: &str,
    ephemeral: bool,
) -> RacedWrite {
    match crate::discord::race_discord_call(
        activation,
        serenity::builder::CreateInteractionResponse::Message(
            serenity::builder::CreateInteractionResponseMessage::new()
                .content(content)
                .ephemeral(ephemeral),
        )
        .execute(http, (id, token)),
    )
    .await
    {
        crate::discord::DiscordCall::Finished(Ok(())) => RacedWrite::Finished,
        crate::discord::DiscordCall::Finished(Err(error)) => {
            tracing::warn!("failed to answer slash interaction: {error}");
            RacedWrite::Finished
        }
        crate::discord::DiscordCall::Cancelled => RacedWrite::Cutoff,
    }
}

async fn deliver_slash_success(
    activation: &crate::activation::Activation,
    http: &serenity::http::Http,
    slash: &SlashCtx,
    deferred: bool,
    cmd: &CommandDef,
    outcome: &BehaviorOutcome,
) -> RacedWrite {
    let answer = slash_answer(cmd, outcome);
    let text = render_outcome(cmd, outcome);
    match answer {
        SlashAnswer::Withdraw => {
            if deferred {
                withdraw_deferred_slash_answer(activation, http, slash.token.as_str(), &cmd.name)
                    .await
            } else {
                raced_slash_create_message(
                    activation,
                    http,
                    slash.id,
                    slash.token.as_str(),
                    &text,
                    true,
                )
                .await
            }
        }
        SlashAnswer::Public | SlashAnswer::Ephemeral => {
            if deferred {
                raced_slash_edit(activation, http, slash.token.as_str(), &text).await
            } else {
                raced_slash_create_message(
                    activation,
                    http,
                    slash.id,
                    slash.token.as_str(),
                    &text,
                    answer != SlashAnswer::Public,
                )
                .await
            }
        }
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
        // Keep stored defaults readable by older bots during a rollout or
        // rollback. Only the current process substitutes compact presentation;
        // other custom response text and raw numeric variables stay unchanged.
        let response = match (&cmd.behavior, cmd.response.as_str()) {
            (Behavior::Timeout { .. }, "Timed out {target} for {minutes} minutes | {reason}") => {
                "Timed out {target} for {duration} | {reason}"
            }
            (Behavior::Slowmode { .. }, "Slowmode set to {seconds}s") => {
                "Slowmode set to {duration}"
            }
            _ => &cmd.response,
        };
        templates::render(response, &vars)
    } else if let Some(r) = &outcome.response {
        r.clone()
    } else {
        "done".to_string()
    }
}

/// Text to post in the invocation channel after a prefix command, or None
/// when there is nothing to post. A behavior whose visible effect already
/// landed in the invocation channel (`suppress_response`, e.g. `!say`
/// posting there) gets no confirmation reply — it would be redundant.
fn prefix_response_text(cmd: &CommandDef, outcome: &BehaviorOutcome) -> Option<String> {
    if outcome.suppress_response {
        return None;
    }
    let text = render_outcome(cmd, outcome);
    (!text.is_empty()).then_some(text)
}

/// How the slash dispatcher answers a successful interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlashAnswer {
    /// Public reply: edit the deferred answer, or send a public initial
    /// response when the defer failed.
    Public,
    /// Ephemeral initial response when the defer failed. A deferred answer is
    /// already public (ephemeral is fixed at the initial response), so it is
    /// edited in place like `Public`.
    Ephemeral,
    /// The behavior's visible effect already landed in the invocation channel
    /// (e.g. same-channel `/say`): delete the deferred answer so no redundant
    /// confirmation appears; when the defer failed, answer ephemerally
    /// instead (an interaction must always be answered).
    Withdraw,
}

fn slash_answer(cmd: &CommandDef, outcome: &BehaviorOutcome) -> SlashAnswer {
    if outcome.suppress_response {
        SlashAnswer::Withdraw
    } else if outcome.response.is_none() && cmd.response.is_empty() {
        SlashAnswer::Ephemeral
    } else {
        SlashAnswer::Public
    }
}

/// When delete is rejected, edit the deferred placeholder to this instead of
/// leaving the loading/"done" confirmation visible after same-channel say.
const WITHDRAW_DEFERRED_FALLBACK_CONTENT: &str = "\u{200b}";

/// Remove the public deferred interaction placeholder after a suppressed
/// confirmation. Prefer delete; on failure log and edit to minimal content.
/// `true` when the withdraw finished without cutoff.
async fn withdraw_deferred_slash_answer(
    activation: &crate::activation::Activation,
    http: &serenity::http::Http,
    token: &str,
    command_name: &str,
) -> RacedWrite {
    let delete = match crate::discord::race_discord_call(
        activation,
        http.delete_original_interaction_response(token),
    )
    .await
    {
        crate::discord::DiscordCall::Cancelled => return RacedWrite::Cutoff,
        crate::discord::DiscordCall::Finished(Ok(())) => return RacedWrite::Finished,
        crate::discord::DiscordCall::Finished(Err(error)) => {
            tracing::warn!(
                "failed to delete deferred slash answer for `{command_name}`: {error:#}; editing to minimal content"
            );
            RacedWrite::Finished
        }
    };
    raced_write_chain(
        delete,
        raced_slash_edit(activation, http, token, WITHDRAW_DEFERRED_FALLBACK_CONTENT).await,
    )
}

#[allow(clippy::too_many_arguments)]
async fn send_outcome(
    app: &Arc<BotState>,
    http: &serenity::http::Http,
    cache: &serenity::cache::Cache,
    cmd: &CommandDef,
    guild_id: GuildId,
    channel_id: serenity::model::id::ChannelId,
    actor_id: serenity::model::id::UserId,
    outcome: BehaviorOutcome,
) {
    if let Some(text) = prefix_response_text(cmd, &outcome)
        && raced_channel_say(&app.activation, channel_id, http, text).await == RacedWrite::Cutoff
    {
        return;
    }
    log_outcome(
        app, http, cache, cmd, guild_id, channel_id, actor_id, outcome,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn log_outcome(
    app: &Arc<BotState>,
    http: &serenity::http::Http,
    cache: &serenity::cache::Cache,
    cmd: &CommandDef,
    guild_id: GuildId,
    source_channel_id: ChannelId,
    actor_id: serenity::model::id::UserId,
    outcome: BehaviorOutcome,
) {
    if cmd.log_event.is_empty() {
        return;
    }
    let mut vars = outcome.vars.clone();
    vars.insert("command".into(), cmd.name.clone());
    let actor_name = cache
        .user(actor_id)
        .map(|u| u.name.clone())
        .unwrap_or_else(|| actor_id.to_string());
    vars.insert("actor".into(), format!("{actor_name} (<@{actor_id}>)"));
    vars.insert("actor_id".into(), actor_id.to_string());
    vars.insert("source_channel_id".into(), source_channel_id.to_string());
    logging::emit_to(app, http, guild_id, &cmd.log_event, vars).await;
}

fn cooldown_check(
    app: &BotState,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Behavior;

    #[tokio::test]
    async fn slash_http_failures_are_parked_backed_off_or_stopped() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        for (status, code) in [
            (403, 50013),
            (404, 10004),
            (400, 50035),
            (400, 50001),
            (503, 0),
            (401, 0),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let count = Arc::clone(&calls);
            let server = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    loop {
                        let mut buf = [0; 4096];
                        let n = stream.read(&mut buf).await.unwrap();
                        assert!(n > 0);
                        request.extend_from_slice(&buf[..n]);
                        if request.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    count.fetch_add(1, Ordering::SeqCst);
                    let body = format!(r#"{{"code":{code},"message":"fixture rejection"}}"#);
                    let response = format!(
                        "HTTP/1.1 {status} Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                }
            });
            let path = std::env::temp_dir()
                .join(format!("slash_retry_{:032x}.db", rand::random::<u128>()));
            let db = db::Db::open(&path, 2).unwrap();
            let app = BotState::new(Default::default(), db).unwrap();
            app.activation.mark_lock_held(true);
            let http = serenity::http::HttpBuilder::new("fixture")
                .proxy(format!("http://{address}"))
                .application_id(serenity::model::id::ApplicationId::new(1))
                .ratelimiter_disabled(true)
                .build();
            let target = app.acknowledged_slash_registry_version();
            let guild = GuildId::new(10);
            assert!(!sync_slash_commands_versioned_http(&app, &http, guild, target).await);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert!(!app.slash_sync_due(
                10,
                target,
                &app.slash_registry_revision(),
                std::time::Instant::now() + std::time::Duration::from_secs(5)
            ));
            assert!(!sync_slash_commands_versioned_http(&app, &http, guild, target).await);
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "next poll must not send REST"
            );
            if status == 401 {
                app.reset_slash_guild_retry(10);
                assert!(
                    app.pending_slash_registry_version()
                        .await
                        .unwrap()
                        .is_none()
                );
                assert!(
                    !sync_slash_commands_versioned_http(&app, &http, GuildId::new(20), target)
                        .await
                );
                assert!(!app.slash_sync_due(10, target + 1, "changed", std::time::Instant::now()));
                assert_eq!(calls.load(Ordering::SeqCst), 1);
                assert!(
                    tokio::time::timeout(
                        std::time::Duration::from_secs(2),
                        crate::gateway_budget::wait_for_gateway_identify(&http, &app.db)
                    )
                    .await
                    .unwrap()
                    .is_err()
                );
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    2,
                    "gateway 401 must be surfaced without retrying"
                );
            } else if status == 503 {
                assert!(app.slash_sync_due(
                    10,
                    target,
                    &app.slash_registry_revision(),
                    std::time::Instant::now() + std::time::Duration::from_secs(3601)
                ));
            } else {
                assert!(!app.slash_sync_due(
                    10,
                    target,
                    &app.slash_registry_revision(),
                    std::time::Instant::now() + std::time::Duration::from_secs(7200)
                ));
                assert!(app.slash_sync_due(10, target + 1, "changed", std::time::Instant::now()));
                app.reset_slash_guild_retry(10);
                assert!(app.slash_sync_due(
                    10,
                    target,
                    &app.slash_registry_revision(),
                    std::time::Instant::now()
                ));
            }
            if matches!(status, 401 | 403 | 503) {
                let mut repair = crate::enrollment_repair::EnrollmentRepair::new(
                    Default::default(),
                    std::time::Instant::now(),
                )
                .unwrap();
                let source = crate::enrollment_repair::DiscordMembership {
                    http: &http,
                    bot_user_id: serenity::model::id::UserId::new(1),
                };
                assert!(repair.step(&app.db, &app.tenant, &source).await.is_err());
                if status == 401 {
                    crate::enrollment_repair::tests::assert_auth_stopped_after_deadline(
                        &mut repair,
                    );
                }
                assert!(
                    repair
                        .step(&app.db, &app.tenant, &source)
                        .await
                        .unwrap()
                        .is_none()
                );
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    if status == 401 { 3 } else { 2 },
                    "enrollment errors must not repeat on the next poll"
                );
            }
            server.abort();
            assert!(server.await.unwrap_err().is_cancelled());
            drop(app);
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
            }
            crate::tenant::remove_sidecars(&path);
        }
    }

    fn say_cmd(response: &str) -> CommandDef {
        CommandDef {
            name: "say".into(),
            aliases: vec![],
            category: String::new(),
            description: String::new(),
            usage: String::new(),
            enabled: true,
            permission: "mod".into(),
            roles: vec![],
            channels: vec![],
            cooldown_secs: 0,
            behavior: Behavior::Say,
            response: response.into(),
            log_event: String::new(),
        }
    }

    fn outcome(response: Option<&str>, suppress_response: bool) -> BehaviorOutcome {
        BehaviorOutcome {
            response: response.map(str::to_string),
            vars: BTreeMap::new(),
            suppress_response,
        }
    }

    #[test]
    fn compact_duration_responses_preserve_stored_and_custom_templates() {
        for (name, stored, seconds, compact, legacy) in [
            (
                "timeout",
                "Timed out {target} for {minutes} minutes | {reason}",
                5400,
                "Timed out Member for 1h 30m | test",
                "Timed out Member for 90 minutes | test",
            ),
            (
                "slowmode",
                "Slowmode set to {seconds}s",
                90,
                "Slowmode set to 1m 30s",
                "Slowmode set to 90s",
            ),
        ] {
            let mut command = crate::defaults::default_commands()
                .into_iter()
                .find(|c| c.name == name)
                .unwrap();
            command.response = stored.into();
            // Formatting follows behavior, even if the command is renamed or
            // its permissions change; those settings remain untouched.
            command.name = "renamed".into();
            command.permission = "owner".into();
            let mut result = outcome(None, false);
            result.vars = BTreeMap::from([
                ("target".into(), "Member".into()),
                ("reason".into(), "test".into()),
                ("minutes".into(), "90".into()),
                ("seconds".into(), "90".into()),
                ("duration".into(), templates::format_duration(seconds)),
            ]);
            assert_eq!(
                prefix_response_text(&command, &result).as_deref(),
                Some(compact)
            );
            // Slash replies call the same renderer directly.
            assert_eq!(render_outcome(&command, &result), compact);
            assert_eq!(command.response, stored);
            assert_eq!(command.permission, "owner");
            let mut old_vars = result.vars.clone();
            old_vars.remove("duration");
            assert_eq!(templates::render(&command.response, &old_vars), legacy);

            command.response = "Custom {minutes}m / {seconds}s / {duration}".into();
            assert_eq!(
                render_outcome(&command, &result),
                format!("Custom 90m / 90s / {}", templates::format_duration(seconds))
            );
            command.response = stored.into();
            command.behavior = Behavior::Say;
            assert_eq!(render_outcome(&command, &result), legacy);
        }
    }

    #[test]
    fn prefix_same_channel_say_sends_no_confirmation() {
        // Default command falls back to "done" — suppressed same-channel.
        assert_eq!(
            prefix_response_text(&say_cmd(""), &outcome(None, true)),
            None
        );
        // A custom response template (e.g. "✅ Done.") is also suppressed.
        assert_eq!(
            prefix_response_text(&say_cmd("✅ Done."), &outcome(None, true)),
            None
        );
    }

    #[test]
    fn prefix_cross_channel_say_keeps_confirmation() {
        assert_eq!(
            prefix_response_text(&say_cmd(""), &outcome(None, false)),
            Some("done".to_string())
        );
        assert_eq!(
            prefix_response_text(&say_cmd("✅ Done."), &outcome(None, false)),
            Some("✅ Done.".to_string())
        );
    }

    #[test]
    fn prefix_empty_rendered_response_still_sends_nothing() {
        assert_eq!(
            prefix_response_text(&say_cmd(""), &outcome(Some(""), false)),
            None
        );
    }

    #[test]
    fn mention_everyone_uses_channel_overwrites_not_application_ownership() {
        use serenity::model::channel::{
            ChannelType, GuildChannel, PermissionOverwrite, PermissionOverwriteType,
        };
        use serenity::model::guild::{Guild, Role};
        use serenity::model::id::{ChannelId, GuildId, RoleId, UserId};

        const GUILD: u64 = 100;
        const OWNER: u64 = 1;
        const MEMBER: u64 = 2;
        const APP_OWNER: u64 = 9;
        const ROLE: u64 = 50;
        const CHANNEL: u64 = 7;
        const PARENT: u64 = 8;
        const THREAD: u64 = 70;

        fn role(id: u64, perms: Permissions) -> Role {
            let mut role = Role::default();
            role.id = RoleId::new(id);
            role.permissions = perms;
            role.name = "r".into();
            role
        }
        fn ch(
            id: u64,
            kind: ChannelType,
            parent: Option<u64>,
            overwrites: Vec<PermissionOverwrite>,
        ) -> GuildChannel {
            let mut channel = GuildChannel::default();
            channel.id = ChannelId::new(id);
            channel.guild_id = GuildId::new(GUILD);
            channel.kind = kind;
            channel.parent_id = parent.map(ChannelId::new);
            channel.permission_overwrites = overwrites;
            channel.name = "c".into();
            channel
        }
        let mut guild = Guild::default();
        guild.id = GuildId::new(GUILD);
        guild.owner_id = UserId::new(OWNER);
        guild.name = "g".into();
        guild
            .roles
            .insert(RoleId::new(GUILD), role(GUILD, Permissions::empty()));
        guild
            .roles
            .insert(RoleId::new(ROLE), role(ROLE, Permissions::empty()));
        guild.channels.insert(
            ChannelId::new(CHANNEL),
            ch(
                CHANNEL,
                ChannelType::Text,
                None,
                vec![PermissionOverwrite {
                    allow: Permissions::MENTION_EVERYONE,
                    deny: Permissions::empty(),
                    kind: PermissionOverwriteType::Role(RoleId::new(ROLE)),
                }],
            ),
        );
        assert!(
            super::can_mention_everyone_in(
                &guild,
                ChannelId::new(CHANNEL),
                UserId::new(MEMBER),
                &[RoleId::new(ROLE)],
            ),
            "channel-only allow"
        );
        assert!(
            super::can_mention_everyone_in(
                &guild,
                ChannelId::new(CHANNEL),
                UserId::new(OWNER),
                &[],
            ),
            "guild owner"
        );
        assert!(
            !super::can_mention_everyone_in(
                &guild,
                ChannelId::new(CHANNEL),
                UserId::new(APP_OWNER),
                &[],
            ),
            "application owner is not a Discord permission"
        );

        guild
            .roles
            .get_mut(&RoleId::new(GUILD))
            .unwrap()
            .permissions = Permissions::MENTION_EVERYONE;
        guild.channels.insert(
            ChannelId::new(CHANNEL),
            ch(
                CHANNEL,
                ChannelType::Text,
                None,
                vec![PermissionOverwrite {
                    allow: Permissions::empty(),
                    deny: Permissions::MENTION_EVERYONE,
                    kind: PermissionOverwriteType::Role(RoleId::new(GUILD)),
                }],
            ),
        );
        assert_eq!(
            super::mention_permission_in(&guild, ChannelId::new(CHANNEL), UserId::new(MEMBER), &[],),
            super::MentionPermission::Denied,
            "channel deny beats the default role"
        );

        guild.channels.insert(
            ChannelId::new(CHANNEL),
            ch(CHANNEL, ChannelType::Text, None, vec![]),
        );
        assert!(
            super::can_mention_everyone_in(
                &guild,
                ChannelId::new(CHANNEL),
                UserId::new(MEMBER),
                &[],
            ),
            "default role allow"
        );

        guild
            .roles
            .get_mut(&RoleId::new(GUILD))
            .unwrap()
            .permissions = Permissions::empty();
        guild.channels.insert(
            ChannelId::new(PARENT),
            ch(
                PARENT,
                ChannelType::Text,
                None,
                vec![PermissionOverwrite {
                    allow: Permissions::MENTION_EVERYONE,
                    deny: Permissions::empty(),
                    kind: PermissionOverwriteType::Role(RoleId::new(ROLE)),
                }],
            ),
        );
        guild
            .threads
            .push(ch(THREAD, ChannelType::PublicThread, Some(PARENT), vec![]));
        assert_eq!(
            super::mention_permission_in(
                &guild,
                ChannelId::new(THREAD),
                UserId::new(MEMBER),
                &[RoleId::new(ROLE)],
            ),
            super::MentionPermission::Allowed,
            "thread uses the parent channel"
        );
        assert!(
            everyone_attempt(true, false).is_none(),
            "a member allowed in the parent channel is not punished in the thread"
        );

        guild.threads.clear();
        guild
            .threads
            .push(ch(THREAD, ChannelType::PublicThread, Some(404), vec![]));
        assert_eq!(
            super::mention_permission_in(
                &guild,
                ChannelId::new(THREAD),
                UserId::new(MEMBER),
                &[RoleId::new(ROLE)],
            ),
            super::MentionPermission::Unknown,
            "a thread whose parent is not cached is unknown"
        );
        assert_eq!(
            super::mention_permission_in(
                &guild,
                ChannelId::new(404),
                UserId::new(MEMBER),
                &[RoleId::new(ROLE)],
            ),
            super::MentionPermission::Unknown,
            "a channel missing from the guild cache is unknown"
        );
        guild.roles.remove(&RoleId::new(ROLE));
        assert_eq!(
            super::mention_permission_in(
                &guild,
                ChannelId::new(CHANNEL),
                UserId::new(MEMBER),
                &[RoleId::new(ROLE)]
            ),
            super::MentionPermission::Unknown,
            "missing member role is not a cached deny"
        );
        guild.roles.remove(&RoleId::new(GUILD));
        assert_eq!(
            super::mention_permission_in(&guild, ChannelId::new(CHANNEL), UserId::new(MEMBER), &[]),
            super::MentionPermission::Unknown,
            "missing everyone role is not a cached deny"
        );
    }

    fn everyone_attempt(can_mention: bool, unknown: bool) -> Option<crate::automod::PendingAction> {
        use crate::automod::{AutomodEngine, MsgCtx};
        use crate::state::CachedGuildConfig;
        use crate::types::{ActionDef, GuildSettings, RuleDef};
        use std::collections::HashMap;
        use std::time::Instant;

        let mut rules = HashMap::new();
        rules.insert(
            "custom:0123456789abcdef".to_string(),
            RuleDef {
                name: "custom".into(),
                description: String::new(),
                enabled: true,
                dry_run: false,
                params: serde_json::json!({ "kind": "everyone_attempt" }),
                actions: vec![ActionDef {
                    at: 1,
                    action: "ban".into(),
                    duration_minutes: 0,
                    delete_days: 0,
                    reason: "r".into(),
                }],
                whitelist_roles: vec![],
                whitelist_channels: vec![],
                whitelist_users: vec![],
                skip_moderators: false,
                include_channels: vec![],
                log_automod: true,
                open_case: true,
            },
        );
        let cfg = CachedGuildConfig {
            version: 1,
            durable_version: 1,
            loaded_at: Instant::now(),
            settings: GuildSettings::default(),
            rules,
            commands: HashMap::new(),
            templates: HashMap::new(),
            log_channels: HashMap::new(),
            known_bad_hashes: Default::default(),
            log_filters: HashMap::new(),
            regexes: Vec::new(),
            custom_patterns: HashMap::new(),
        };
        let roles = [];
        let message = MsgCtx {
            guild_id: 100,
            message_id: 1,
            channel_id: 70,
            author_id: 42,
            content: "@everyone",
            attachment_count: 0,
            everyone_mentions: 1,
            here_mentions: 0,
            role_mentions: 0,
            joined_secs_ago: None,
            account_age_days: Some(30),
            member_roles: &roles,
            is_moderator: false,
            can_mention_everyone: can_mention,
            mention_permission_unknown: unknown,
        };
        AutomodEngine::new().evaluate(&cfg, &message)
    }

    #[test]
    fn mention_check_uses_the_message_author_when_the_partial_user_is_null() {
        use serenity::model::channel::{
            ChannelType, GuildChannel, PermissionOverwrite, PermissionOverwriteType,
        };
        use serenity::model::guild::{Guild, Member, PartialMember, Role};
        use serenity::model::id::{ChannelId, GuildId, RoleId, UserId};

        const GUILD: u64 = 100;
        const AUTHOR: u64 = 42;
        const CHANNEL: u64 = 7;

        fn role(id: u64, perms: Permissions) -> Role {
            let mut role = Role::default();
            role.id = RoleId::new(id);
            role.permissions = perms;
            role.name = "r".into();
            role
        }
        fn ch(id: u64, overwrites: Vec<PermissionOverwrite>) -> GuildChannel {
            let mut channel = GuildChannel::default();
            channel.id = ChannelId::new(id);
            channel.guild_id = GuildId::new(GUILD);
            channel.kind = ChannelType::Text;
            channel.permission_overwrites = overwrites;
            channel.name = "c".into();
            channel
        }
        fn gateway_partial() -> PartialMember {
            serde_json::from_value(serde_json::json!({
                "deaf": false,
                "joined_at": null,
                "mute": false,
                "nick": null,
                "roles": [],
                "pending": false,
                "premium_since": null,
                "guild_id": GUILD.to_string(),
                "user": null,
                "permissions": null,
                "unusual_dm_activity_until": null,
                "avatar": null,
                "banner": null,
                "avatar_decoration_data": null,
            }))
            .unwrap()
        }

        let partial = gateway_partial();
        assert!(partial.user.is_none(), "gateway partial has no user");
        assert_ne!(
            Member::from(partial.clone()).user.id,
            UserId::new(AUTHOR),
            "Member::from fills a default user id"
        );

        let mut guild = Guild::default();
        guild.id = GuildId::new(GUILD);
        guild.owner_id = UserId::new(1);
        guild.name = "g".into();
        guild
            .roles
            .insert(RoleId::new(GUILD), role(GUILD, Permissions::empty()));
        guild.channels.insert(
            ChannelId::new(CHANNEL),
            ch(
                CHANNEL,
                vec![PermissionOverwrite {
                    allow: Permissions::MENTION_EVERYONE,
                    deny: Permissions::empty(),
                    kind: PermissionOverwriteType::Member(UserId::new(AUTHOR)),
                }],
            ),
        );
        let allowed = super::message_author_scope(
            0,
            &guild,
            ChannelId::new(CHANNEL),
            UserId::new(AUTHOR),
            &partial,
        );
        assert_eq!(allowed.mention, super::MentionPermission::Allowed);
        assert!(
            everyone_attempt(
                allowed.mention == super::MentionPermission::Allowed,
                allowed.mention == super::MentionPermission::Unknown,
            )
            .is_none(),
            "a channel allow for the message author is not punished"
        );

        guild.channels.insert(
            ChannelId::new(CHANNEL),
            ch(
                CHANNEL,
                vec![PermissionOverwrite {
                    allow: Permissions::empty(),
                    deny: Permissions::MENTION_EVERYONE,
                    kind: PermissionOverwriteType::Member(UserId::new(AUTHOR)),
                }],
            ),
        );
        guild
            .roles
            .get_mut(&RoleId::new(GUILD))
            .unwrap()
            .permissions = Permissions::MENTION_EVERYONE;
        let denied = super::message_author_scope(
            0,
            &guild,
            ChannelId::new(CHANNEL),
            UserId::new(AUTHOR),
            &partial,
        );
        assert_eq!(denied.mention, super::MentionPermission::Denied);
        let pending = everyone_attempt(
            denied.mention == super::MentionPermission::Allowed,
            denied.mention == super::MentionPermission::Unknown,
        )
        .expect("author deny still matches");
        assert!(!pending.mention_permission_unknown);
        assert_eq!(pending.action.action, "ban");
    }

    #[test]
    fn slash_suppressed_say_withdraws_the_answer() {
        // Same-channel say suppresses the confirmation regardless of whether
        // a custom response template (e.g. "✅ Done.") is configured.
        assert_eq!(
            slash_answer(&say_cmd(""), &outcome(None, true)),
            SlashAnswer::Withdraw
        );
        assert_eq!(
            slash_answer(&say_cmd("✅ Done."), &outcome(None, true)),
            SlashAnswer::Withdraw
        );
    }

    #[test]
    fn slash_answer_rule_unchanged_without_suppression() {
        // Default say (no template, no inline reply) keeps an ephemeral answer.
        assert_eq!(
            slash_answer(&say_cmd(""), &outcome(None, false)),
            SlashAnswer::Ephemeral
        );
        // A custom template stays public for cross-channel confirmations.
        assert_eq!(
            slash_answer(&say_cmd("✅ Done."), &outcome(None, false)),
            SlashAnswer::Public
        );
        // An inline behavior reply stays public.
        assert_eq!(
            slash_answer(&say_cmd(""), &outcome(Some("hi"), false)),
            SlashAnswer::Public
        );
    }

    struct StuckDiscordWrite {
        polled: std::sync::Arc<std::sync::atomic::AtomicBool>,
        dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
        started: std::sync::Arc<tokio::sync::Notify>,
    }

    impl StuckDiscordWrite {
        fn new() -> Self {
            Self {
                polled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                dropped: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                started: std::sync::Arc::new(tokio::sync::Notify::new()),
            }
        }
    }

    impl std::future::Future for StuckDiscordWrite {
        type Output = ();

        fn poll(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<()> {
            self.polled.store(true, std::sync::atomic::Ordering::SeqCst);
            self.started.notify_waiters();
            std::task::Poll::Pending
        }
    }

    impl Drop for StuckDiscordWrite {
        fn drop(&mut self) {
            self.dropped
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    async fn cutoff_cancels_pending_discord_write(
        activation: std::sync::Arc<crate::activation::Activation>,
    ) {
        let stuck = StuckDiscordWrite::new();
        let polled = std::sync::Arc::clone(&stuck.polled);
        let dropped = std::sync::Arc::clone(&stuck.dropped);
        let started = std::sync::Arc::clone(&stuck.started);
        let notified = started.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let race_activation = std::sync::Arc::clone(&activation);
        let task = tokio::spawn(async move {
            crate::discord::race_discord_call(&race_activation, stuck).await
        });
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
                panic!("the discord future must be polled while it is still pending");
            }
            _ = &mut notified => {}
        }
        assert!(polled.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!dropped.load(std::sync::atomic::Ordering::SeqCst));
        activation.cut_off();
        let raced = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("cutoff must cancel the discord future")
            .unwrap();
        assert!(matches!(raced, crate::discord::DiscordCall::Cancelled));
        assert!(
            dropped.load(std::sync::atomic::Ordering::SeqCst),
            "dropping the future is the cancellation"
        );
    }

    #[tokio::test]
    async fn respond_suppresses_reply_when_cutoff_wins_during_await() {
        let activation = std::sync::Arc::new(crate::activation::Activation::new());
        activation.mark_lock_held(true);
        cutoff_cancels_pending_discord_write(std::sync::Arc::clone(&activation)).await;
        let http = serenity::http::HttpBuilder::new("fixture")
            .application_id(serenity::model::id::ApplicationId::new(1))
            .ratelimiter_disabled(true)
            .build();
        assert_eq!(
            raced_slash_create_message(
                &activation,
                &http,
                serenity::model::id::InteractionId::new(1),
                "token",
                "error: writer lease is not held",
                true,
            )
            .await,
            RacedWrite::Cutoff
        );
    }

    #[tokio::test]
    async fn send_outcome_suppresses_reply_when_cutoff_wins_during_await() {
        let activation = std::sync::Arc::new(crate::activation::Activation::new());
        activation.mark_lock_held(true);
        cutoff_cancels_pending_discord_write(std::sync::Arc::clone(&activation)).await;
        let http = serenity::http::HttpBuilder::new("fixture")
            .application_id(serenity::model::id::ApplicationId::new(1))
            .ratelimiter_disabled(true)
            .build();
        assert_eq!(
            raced_channel_say(&activation, ChannelId::new(1), &http, "pong".into()).await,
            RacedWrite::Cutoff
        );
    }

    #[tokio::test]
    async fn slash_success_suppresses_reply_when_cutoff_wins_during_await() {
        let activation = std::sync::Arc::new(crate::activation::Activation::new());
        activation.mark_lock_held(true);
        cutoff_cancels_pending_discord_write(std::sync::Arc::clone(&activation)).await;
        let http = serenity::http::HttpBuilder::new("fixture")
            .application_id(serenity::model::id::ApplicationId::new(1))
            .ratelimiter_disabled(true)
            .build();
        assert_eq!(
            raced_slash_edit(&activation, &http, "token", "pong").await,
            RacedWrite::Cutoff,
            "deferred slash success edits must be cancellable at cutoff"
        );
    }

    async fn discord_rejecting_proxy() -> (
        serenity::http::Http,
        Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let server = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut request = Vec::new();
                loop {
                    let mut buf = [0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                    if request.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                counter.fetch_add(1, Ordering::SeqCst);
                let body = r#"{"code":50013,"message":"fixture"}"#;
                let response = format!(
                    "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        let http = serenity::http::HttpBuilder::new("fixture")
            .proxy(format!("http://{address}"))
            .application_id(serenity::model::id::ApplicationId::new(1))
            .ratelimiter_disabled(true)
            .build();
        (http, calls, server)
    }

    fn temp_command_test_db() -> (Arc<BotState>, std::path::PathBuf) {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("cmd_test_{:032x}.db", rand::random::<u128>()));
        let db = db::Db::open(&path, 2).unwrap();
        (BotState::new(Default::default(), db).unwrap(), path)
    }

    fn remove_command_test_db(path: &std::path::Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        crate::tenant::remove_sidecars(path);
    }

    #[tokio::test]
    async fn send_outcome_still_logs_when_confirmation_send_fails() {
        use std::sync::atomic::Ordering;
        let (http, calls, server) = discord_rejecting_proxy().await;
        let (app, path) = temp_command_test_db();
        app.activation.mark_lock_held(true);
        db::upsert_log_channel(&app.db, 1, "mod_action", 99)
            .await
            .unwrap();
        let cache = serenity::cache::Cache::default();
        let http = Arc::new(http);
        let mut cmd = say_cmd("ok");
        cmd.log_event = "mod_action".into();
        send_outcome(
            &app,
            http.as_ref(),
            &cache,
            &cmd,
            GuildId::new(1),
            ChannelId::new(2),
            UserId::new(3),
            outcome(None, false),
        )
        .await;
        assert!(
            calls.load(Ordering::SeqCst) >= 2,
            "a failed confirmation must not skip the configured command log"
        );
        server.abort();
        drop(app);
        remove_command_test_db(&path);
    }

    #[tokio::test]
    async fn slash_success_still_logs_when_confirmation_edit_fails() {
        use std::sync::atomic::Ordering;
        let (http, calls, server) = discord_rejecting_proxy().await;
        let (app, path) = temp_command_test_db();
        app.activation.mark_lock_held(true);
        db::upsert_log_channel(&app.db, 1, "mod_action", 99)
            .await
            .unwrap();
        let cache = serenity::cache::Cache::default();
        let http = Arc::new(http);
        let mut cmd = say_cmd("ok");
        cmd.log_event = "mod_action".into();
        let slash = SlashCtx {
            id: serenity::model::id::InteractionId::new(1),
            token: "token".into(),
            channel_id: ChannelId::new(2),
            guild_id: GuildId::new(1),
            actor: UserId::new(3),
            member: None,
            name: "say".into(),
            options: Vec::new(),
        };
        let outcome = outcome(None, false);
        let write =
            deliver_slash_success(&app.activation, http.as_ref(), &slash, true, &cmd, &outcome)
                .await;
        assert_ne!(write, RacedWrite::Cutoff);
        if write != RacedWrite::Cutoff {
            log_outcome(
                &app,
                http.as_ref(),
                &cache,
                &cmd,
                slash.guild_id,
                slash.channel_id,
                slash.actor,
                outcome,
            )
            .await;
        }
        assert!(
            calls.load(Ordering::SeqCst) >= 2,
            "a failed slash confirmation must not skip the configured command log"
        );
        server.abort();
        drop(app);
        remove_command_test_db(&path);
    }

    #[tokio::test]
    async fn handle_prefix_unknown_command_reply_suppressed_when_cutoff_wins_during_await() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let connected = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&connected);
        let server = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                flag.store(true, Ordering::SeqCst);
                let mut buf = [0u8; 1024];
                while stream.read(&mut buf).await.unwrap_or(0) > 0 {}
            }
        });
        let http = serenity::http::HttpBuilder::new("fixture")
            .proxy(format!("http://{address}"))
            .application_id(serenity::model::id::ApplicationId::new(1))
            .ratelimiter_disabled(true)
            .build();
        let (app, path) = temp_command_test_db();
        app.activation.mark_lock_held(true);
        let http = Arc::new(http);
        let task = tokio::spawn({
            let activation = std::sync::Arc::clone(&app.activation);
            let http = Arc::clone(&http);
            async move {
                prefix_unknown_command_reply(
                    &activation,
                    ChannelId::new(20),
                    http.as_ref(),
                    "nosuch",
                    "!",
                    "",
                    "nosuch",
                )
                .await
            }
        });
        for _ in 0..500 {
            if connected.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(connected.load(Ordering::SeqCst));
        app.activation.cut_off();
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("cutoff must cancel the unknown-command reply without waiting for REST timeout")
            .unwrap();
        server.abort();
        drop(app);
        remove_command_test_db(&path);
    }

    #[tokio::test]
    async fn handle_prefix_unknown_command_uses_raced_channel_say() {
        use crate::state::CachedGuildConfig;
        use crate::types::GuildSettings;
        use serenity::model::channel::Message;
        use serenity::model::user::User;
        use std::collections::HashMap;
        use std::sync::atomic::Ordering;
        use std::time::Instant;

        let (http, calls, server) = discord_rejecting_proxy().await;
        let (app, path) = temp_command_test_db();
        app.activation.mark_lock_held(true);
        let cache = serenity::cache::Cache::default();
        let settings = GuildSettings {
            prefix: "!".into(),
            ..Default::default()
        };
        let cfg = Arc::new(CachedGuildConfig {
            version: 1,
            durable_version: 1,
            loaded_at: Instant::now(),
            settings,
            rules: HashMap::new(),
            commands: HashMap::new(),
            templates: HashMap::new(),
            log_channels: HashMap::new(),
            known_bad_hashes: Default::default(),
            log_filters: HashMap::new(),
            regexes: Vec::new(),
            custom_patterns: HashMap::new(),
        });
        let mut author = User::default();
        author.id = UserId::new(2);
        let mut msg = Message::default();
        msg.content = "!nosuch".into();
        msg.guild_id = Some(GuildId::new(10));
        msg.channel_id = ChannelId::new(20);
        msg.author = author;
        let http = Arc::new(http);
        let result = handle_prefix(&app, http.as_ref(), &cache, cfg, &msg, None)
            .await
            .expect("prefix dispatch should complete");
        assert!(!result);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "unknown-command feedback must attempt exactly one raced channel send"
        );
        server.abort();
        drop(app);
        remove_command_test_db(&path);
    }
}
