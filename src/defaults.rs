//! Seeded defaults for rules, commands, and event templates.
//! These are the global (guild_id = 0) fallbacks written on first boot.
//! Every value here is fully editable at runtime via the WebUI / bot commands.

use crate::types::{ActionDef, Behavior, CommandDef, PlanStep, RULE_IDS, RuleDef};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

pub(crate) const OLD_INVITE_DESCRIPTION: &str =
    "Discord invite links (whitelisted servers can be configured in params.allowlist_guilds).";
pub(crate) const INVITE_DESCRIPTION: &str =
    "Discord invite links, except invite codes configured in allowlist_codes.";

/// Shipped rules as this install resolves them where no global row is stored.
/// After the velocity upgrade on an existing install, the velocity default is
/// dry-run, so removing the global row never turns on live enforcement.
pub fn install_default_rules(velocity_dry_run: bool) -> Vec<RuleDef> {
    let mut rules = default_rules();
    if velocity_dry_run
        && let Some(index) = crate::types::RULE_IDS
            .iter()
            .position(|id| *id == "velocity")
    {
        rules[index].dry_run = true;
    }
    rules
}

pub fn default_rules() -> Vec<RuleDef> {
    let mut out = Vec::with_capacity(RULE_IDS.len());
    for id in RULE_IDS {
        out.push(rule(id));
    }
    out
}

pub(crate) fn rule(id: &str) -> RuleDef {
    let (name, description, params, actions) = match id {
        "mention_spam" => (
            "Mention spam",
            "Too many @everyone/@here or role mentions within the configured window.",
            json!({"window_secs": 10, "max_mentions_in_window": 5}),
            vec![action(1, "ban", "Ping spam (@everyone/@here)", 0, 1)],
        ),
        "attachment_flood" => (
            "Exact image spam",
            "Repeated images by one author across channels. Matches identical bytes or a quick cross-channel repost with matching image metadata. Dry-run logs only. Configure an automod log channel.",
            json!({"mode":"exact_image_test", "window_secs": 60, "min_attachments": 2, "min_channels": 3}),
            vec![spam_test_action("Exact image spam test")],
        ),
        "repeat_text" => (
            "Repeated text",
            "The same trimmed text posted by one author across channels. Matching is exact after leading/trailing whitespace only. Dry-run logs only; normal mode deletes the matched messages and bans.",
            json!({"mode":"exact_text", "window_secs": 60, "min_channels": 3, "min_length": 20}),
            vec![spam_test_action("Repeated text test")],
        ),
        "new_member_url" => (
            "New-member URL",
            "A member posts a link or invite within the first minutes after joining.",
            json!({"grace_secs": 120}),
            vec![action(1, "kick", "Posted a link right after joining", 0, 0)],
        ),
        "badword" => (
            "Badwords",
            "Admin-defined regex patterns matched against normalized text (bypass-resistant).",
            json!({}),
            vec![
                action(1, "delete", "Banned word", 0, 0),
                action(2, "warn", "Repeated banned words", 0, 0),
                action(4, "timeout", "Repeated banned words", 60, 0),
            ],
        ),
        "known_spam" => (
            "Known spam",
            "Saved full messages and URL wildcards. First match bans and purges the previous hour; dry-run logs only.",
            json!({}),
            vec![crate::known_spam::action()],
        ),
        "invite" => (
            "Invite links",
            INVITE_DESCRIPTION,
            json!({"allowlist_codes": []}),
            vec![
                action(1, "delete", "Invite link", 0, 0),
                action(2, "warn", "Repeated invite links", 0, 0),
                action(4, "timeout", "Repeated invite links", 60, 0),
            ],
        ),
        "velocity" => (
            "Message velocity",
            "Too many messages in a short window.",
            json!({"window_secs": 10, "max_messages": 8}),
            vec![
                action(1, "delete", "Message velocity", 0, 0),
                action(2, "timeout", "Message velocity", 30, 0),
                action(4, "kick", "Message velocity", 0, 0),
            ],
        ),
        "account_age" => (
            "Account age",
            "Accounts younger than the allowed minimum age (report-only by default).",
            json!({"min_age_days": 3}),
            vec![action(
                1,
                "log",
                "Account younger than the configured minimum age",
                0,
                0,
            )],
        ),
        crate::onboarding_trap::RULE_ID => (
            "Onboarding bot trap",
            "Kick members who select the trap role in Discord Onboarding.",
            json!({"role_id": null, "join_window_minutes": crate::onboarding_trap::DEFAULT_JOIN_WINDOW}),
            vec![action(
                1,
                "kick",
                "[Automod] Kicked for suspected bot (onboarding)",
                0,
                0,
            )],
        ),
        _ => unreachable!(),
    };
    RuleDef {
        name: name.into(),
        description: description.into(),
        enabled: id != crate::onboarding_trap::RULE_ID,
        dry_run: matches!(
            id,
            "attachment_flood" | "repeat_text" | crate::onboarding_trap::RULE_ID
        ),
        params,
        actions,
        whitelist_roles: vec![],
        whitelist_channels: vec![],
        whitelist_users: vec![],
        skip_moderators: id == crate::onboarding_trap::RULE_ID,
        include_channels: vec![],
        log_automod: true,
        open_case: true,
    }
}

fn spam_test_action(reason: &str) -> ActionDef {
    // The editor carries the saved deletion choice between dry-run and enforcement.
    ActionDef {
        purge_last_hour: true,
        ..action(1, "log", reason, 0, 0)
    }
}

fn action(
    at: u32,
    action: &str,
    reason: &str,
    duration_minutes: u32,
    delete_days: u8,
) -> ActionDef {
    ActionDef {
        at,
        action: action.into(),
        duration_minutes,
        delete_days,
        purge_last_hour: false,
        reason: reason.into(),
    }
}

/// Writable non-built-in command for configuration writer and API tests.
pub fn custom_command_fixture(name: &str) -> CommandDef {
    CommandDef {
        name: name.into(),
        aliases: vec![],
        category: "Fixture".into(),
        description: "fixture".into(),
        usage: format!("!{name}"),
        enabled: true,
        permission: "everyone".into(),
        roles: vec![],
        channels: vec![],
        cooldown_secs: 0,
        behavior: Behavior::Say,
        response: String::new(),
        log_event: String::new(),
        builtin_override: false,
    }
}

#[allow(clippy::vec_init_then_push)]
pub fn default_commands() -> Vec<CommandDef> {
    let mut out = Vec::new();
    out.push(cmd(
        "ban",
        "Moderation",
        "Ban a user from the server.",
        "!ban <user> [--dm] [--minutes N] [reason]",
        "mod",
        Behavior::Ban {
            default_reason: "Banned by a moderator".into(),
            delete_days: 0,
        },
        "Banned {target} | {reason}",
        "mod_action",
        vec![],
    ));
    out.push(cmd(
        "softban",
        "Moderation",
        "Ban, delete the last day of messages, then unban a user.",
        "!softban <user> [reason]",
        "mod",
        Behavior::Softban {
            default_reason: "Softbanned by a moderator".into(),
        },
        "Softbanned {target} | {reason}",
        "mod_action",
        vec![],
    ));
    out.push(cmd(
        "unban",
        "Moderation",
        "Unban a user from the server.",
        "!unban <user> [reason]",
        "mod",
        Behavior::Unban {
            default_reason: "Unbanned".into(),
        },
        "Unbanned {target} | {reason}",
        "mod_action",
        vec![],
    ));
    out.push(cmd(
        "kick",
        "Moderation",
        "Kick a user from the server.",
        "!kick <user> [--dm] [reason]",
        "mod",
        Behavior::Kick {
            default_reason: "Kicked by a moderator".into(),
        },
        "Kicked {target} | {reason}",
        "mod_action",
        vec![],
    ));
    out.push(cmd(
        "timeout",
        "Moderation",
        "Timeout a user (0 minutes removes the timeout).",
        "!timeout <user> <minutes> [reason]",
        "mod",
        Behavior::Timeout {
            default_minutes: 60,
        },
        "Timed out {target} for {minutes} minutes | {reason}",
        "mod_action",
        vec![],
    ));
    out.push(cmd(
        "warn",
        "Moderation",
        "Warn a user.",
        "!warn <user> <reason>",
        "mod",
        Behavior::Warn {
            default_reason: "Warning from moderator".into(),
        },
        "Warned {target} | {reason}",
        "mod_action",
        vec![],
    ));
    out.push(cmd(
        "warns",
        "Moderation",
        "List a user's active warnings.",
        "!warns <user>",
        "mod",
        Behavior::Warns,
        "",
        "mod_action",
        vec![],
    ));
    out.push(cmd(
        "warnclear",
        "Moderation",
        "Clear all warnings for a user.",
        "!warnclear <user>",
        "mod",
        Behavior::WarnClear,
        "Cleared all warnings for {target}",
        "mod_action",
        vec![],
    ));
    out.push(cmd(
        "userinfo",
        "Moderation",
        "Show a member's cached profile, warnings, and recent cases.",
        "!userinfo [user]",
        "mod",
        Behavior::UserInfo,
        "",
        "mod_action",
        vec![],
    ));
    out.push(cmd(
        "purge",
        "Moderation",
        "Delete the last N messages in this channel.",
        "!purge [count]",
        "mod",
        Behavior::Purge { default_count: 50 },
        "Deleted {count} messages",
        "mod_action",
        vec![],
    ));
    out.push(cmd(
        "say",
        "Moderation",
        "Post a message as the bot (optional #channel).",
        "!say [#channel] <text>",
        "mod",
        Behavior::Say,
        "",
        "mod_action",
        vec![],
    ));
    out.push(cmd(
        "slowmode",
        "Moderation",
        "Set slowmode on this channel in seconds.",
        "!slowmode <seconds>",
        "mod",
        Behavior::Slowmode { default_seconds: 5 },
        "Slowmode set to {seconds}s",
        "mod_action",
        vec![],
    ));
    out.push(cmd(
        "banlist",
        "Moderation",
        "List recent bans tracked by the bot.",
        "!banlist [limit]",
        "mod",
        Behavior::Banlist,
        "",
        "mod_action",
        vec![],
    ));
    out.push(cmd(
        "voice",
        "Moderation",
        "Report which voice channel a user is in (gateway cache).",
        "!voice <user>",
        "mod",
        Behavior::Voice,
        "",
        "",
        vec![],
    ));
    out.push(cmd(
        "setname",
        "Admin",
        "Queue the bot's nickname in this server (omit / clear / - to reset).",
        "!setname [name|clear|-]",
        "admin",
        Behavior::Setname,
        "Bot nickname queued: {name}",
        "mod_action",
        vec![],
    ));
    out.push(cmd(
        "prefix",
        "Admin",
        "Set this server's command prefix.",
        "!prefix <prefix>",
        "admin",
        Behavior::SetPrefix,
        "Prefix set to {prefix}",
        "mod_action",
        vec![],
    ));
    out.push(cmd(
        "template",
        "Admin",
        "View or edit log event templates: !template <get|set|list|reset> [event] [text]",
        "!template <get|set|list|reset> [event] [text]",
        "admin",
        Behavior::Template {
            action: "list".into(),
        },
        "",
        "mod_action",
        vec![],
    ));
    out.push(cmd(
        "rule",
        "Admin",
        "Automod rule controls: !rule <list|toggle|dryrun> [rule] [on|off]",
        "!rule <list|toggle|dryrun> [rule] [on|off]",
        "admin",
        Behavior::Rule {
            action: "list".into(),
        },
        "",
        "mod_action",
        vec![],
    ));
    out.push(cmd(
        "logchannel",
        "Admin",
        "Route log events to channels: !logchannel <list|set|unset> [event] [#channel]",
        "!logchannel <list|set|unset> [event] [#channel]",
        "admin",
        Behavior::Logchannel {
            action: "list".into(),
        },
        "",
        "mod_action",
        vec![],
    ));
    out.push(cmd(
        "testregex",
        "Admin",
        "Test a regex against a sample (raw and normalized).",
        "!testregex <pattern> <sample>",
        "admin",
        Behavior::Testregex,
        "",
        "mod_action",
        vec![],
    ));
    out.push(cmd(
        "help",
        "Utility",
        "List commands and their descriptions.",
        "!help [command]",
        "everyone",
        Behavior::Help,
        "",
        "",
        vec![],
    ));
    out.push(cmd(
        "about",
        "Utility",
        "About this bot.",
        "!about",
        "everyone",
        Behavior::About,
        "",
        "",
        vec![],
    ));
    out.push(cmd(
        "stats",
        "Utility",
        "This server's moderation counts, release state, and container CPU and memory.",
        "!stats",
        "admin",
        Behavior::Stats,
        "",
        "",
        vec![],
    ));

    for c in &mut out {
        c.aliases = aliases_for(&c.name);
    }
    out
}

/// Built-in commands merged from the running binary but not written to global
/// `commands` rows during rolling upgrades (older binaries cannot deserialize
/// unknown `Behavior` kinds).
pub const BINARY_OVERLAY_COMMAND_NAMES: &[&str] = &["voice", "softban"];

pub fn is_binary_overlay_command_name(name: &str) -> bool {
    BINARY_OVERLAY_COMMAND_NAMES
        .iter()
        .any(|overlay| overlay.eq_ignore_ascii_case(name))
}

/// Behaviors merged from the running binary. Only a server override of the
/// `voice` or `softban` built-in may store them; custom rows may not.
pub fn is_binary_overlay_behavior(behavior: &Behavior) -> bool {
    matches!(behavior, Behavior::Voice | Behavior::Softban { .. })
}

pub fn command_def_is_binary_overlay(cmd: &CommandDef) -> bool {
    is_binary_overlay_command_name(&cmd.name) || is_binary_overlay_behavior(&cmd.behavior)
}

/// The compiled-in commands, built once per process, with ASCII-lowercased
/// keys pointing at the first command in catalog order that owns each label.
struct ShippedCommands {
    commands: Vec<CommandDef>,
    by_name: HashMap<String, usize>,
    by_label: HashMap<String, usize>,
}

fn shipped_commands() -> &'static ShippedCommands {
    static SHIPPED: OnceLock<ShippedCommands> = OnceLock::new();
    SHIPPED.get_or_init(|| {
        let commands = default_commands();
        let mut by_name = HashMap::new();
        let mut by_label = HashMap::new();
        for (index, cmd) in commands.iter().enumerate() {
            by_name
                .entry(cmd.name.to_ascii_lowercase())
                .or_insert(index);
            for label in std::iter::once(&cmd.name).chain(&cmd.aliases) {
                by_label.entry(label.to_ascii_lowercase()).or_insert(index);
            }
        }
        ShippedCommands {
            commands,
            by_name,
            by_label,
        }
    })
}

impl ShippedCommands {
    fn find(&self, index: &HashMap<String, usize>, label: &str) -> Option<&CommandDef> {
        let position = if label.bytes().any(|b| b.is_ascii_uppercase()) {
            index.get(&label.to_ascii_lowercase())
        } else {
            index.get(label)
        }?;
        self.commands.get(*position)
    }
}

/// The shipped command whose primary name matches `name` (ASCII case-insensitive).
fn shipped_command(name: &str) -> Option<&'static CommandDef> {
    let shipped = shipped_commands();
    shipped.find(&shipped.by_name, name)
}

/// Every shipped command name and alias (derived from [`default_commands`]).
pub fn builtin_command_labels() -> Vec<String> {
    let mut labels: Vec<String> = shipped_commands().by_label.keys().cloned().collect();
    labels.sort();
    labels
}

/// Canonical primary command name when `label` matches a shipped name or alias.
pub fn builtin_command_for_label(label: &str) -> Option<&'static str> {
    let shipped = shipped_commands();
    shipped
        .find(&shipped.by_label, label)
        .map(|cmd| cmd.name.as_str())
}

pub fn command_persist_rejected_message(storage_name: &str) -> String {
    format!("built-in command `{storage_name}` cannot be written to the database")
}

/// Primary shipped command name (not an alias).
pub fn is_builtin_primary_command_name(name: &str) -> bool {
    shipped_command(name).is_some()
}

/// Command permission levels, lowest first. Unknown text has no rank.
pub fn command_permission_rank(permission: &str) -> Option<u8> {
    match permission {
        "everyone" => Some(0),
        "mod" => Some(1),
        "admin" => Some(2),
        "owner" => Some(3),
        _ => None,
    }
}

fn same_behavior_kind(a: &Behavior, b: &Behavior) -> bool {
    std::mem::discriminant(a) == std::mem::discriminant(b)
}

/// Shipped built-in for runtime enforcement and API reads, with this server's
/// override applied (#623). Only a row flagged `builtin_override` (set by the
/// writers) is an override; an unflagged row under a built-in name is a copy of
/// a global seed or a custom command named like a later built-in, and is
/// ignored. An override changes settings only: the name, category and behavior
/// kind stay shipped, the permission never drops below the shipped level, and
/// aliases that are another built-in's label are dropped. Global rows are never
/// passed here; they stay shipped seeds.
pub fn effective_builtin_command(
    primary: &str,
    guild_override: Option<&CommandDef>,
) -> Option<CommandDef> {
    let shipped = shipped_command(primary)?;
    let Some(stored) = guild_override.filter(|row| row.builtin_override) else {
        return Some(shipped.clone());
    };
    let mut effective = stored.clone();
    effective.name = shipped.name.clone();
    effective.category = shipped.category.clone();
    if !same_behavior_kind(&stored.behavior, &shipped.behavior) {
        effective.behavior = shipped.behavior.clone();
    }
    let floor = command_permission_rank(&shipped.permission).unwrap_or(0);
    if command_permission_rank(&stored.permission).is_none_or(|rank| rank < floor) {
        effective.permission = shipped.permission.clone();
    }
    let mut seen = HashSet::new();
    effective.aliases.retain(|alias| {
        let alias = alias.to_ascii_lowercase();
        !alias.is_empty()
            && !alias.eq_ignore_ascii_case(&shipped.name)
            && builtin_command_for_label(&alias)
                .is_none_or(|owner| owner.eq_ignore_ascii_case(&shipped.name))
            && seen.insert(alias)
    });
    Some(effective)
}

fn install_shipped_builtin(
    commands: &mut std::collections::HashMap<String, CommandDef>,
    shipped: CommandDef,
) {
    let stale: Vec<String> = commands
        .keys()
        .filter(|key| key.eq_ignore_ascii_case(&shipped.name))
        .cloned()
        .collect();
    for key in stale {
        commands.remove(&key);
    }
    commands.insert(shipped.name.clone(), shipped);
}

/// Removes legacy rows and aliases that compete with built-in labels. Shipped
/// labels and built-in override aliases win over custom aliases; a custom
/// command's own name wins over a built-in override alias.
pub fn resolve_legacy_builtin_command_collisions(
    commands: &mut std::collections::HashMap<String, CommandDef>,
) {
    let shadow_keys: Vec<String> = commands
        .keys()
        .filter(|key| {
            builtin_command_for_label(key).is_some_and(|primary| !primary.eq_ignore_ascii_case(key))
        })
        .cloned()
        .collect();
    for key in shadow_keys {
        commands.remove(&key);
    }
    let custom_names: HashSet<String> = commands
        .keys()
        .filter(|key| !is_builtin_primary_command_name(key))
        .map(|key| key.to_ascii_lowercase())
        .collect();
    // Built-ins claim their override aliases in name order, so two overrides
    // that share one resolve the same way in every process.
    let mut builtin_keys: Vec<String> = commands
        .keys()
        .filter(|key| is_builtin_primary_command_name(key))
        .cloned()
        .collect();
    builtin_keys.sort();
    let mut builtin_aliases = HashSet::new();
    for key in builtin_keys {
        let Some(command) = commands.get_mut(&key) else {
            continue;
        };
        command.aliases.retain(|alias| {
            let alias = alias.to_ascii_lowercase();
            builtin_command_for_label(&alias).is_some()
                || (!custom_names.contains(&alias) && !builtin_aliases.contains(&alias))
        });
        builtin_aliases.extend(
            command
                .aliases
                .iter()
                .map(|alias| alias.to_ascii_lowercase()),
        );
    }
    for (key, command) in commands.iter_mut() {
        if is_builtin_primary_command_name(key) {
            continue;
        }
        command.aliases.retain(|alias| {
            builtin_command_for_label(alias).is_none()
                && !builtin_aliases.contains(&alias.to_ascii_lowercase())
        });
    }
}

/// Effective command map for one guild: guild rows overlay global rows, then
/// built-ins are installed with that guild's overrides
/// (`apply_shipped_builtin_commands`). Pass no guild rows for the global scope.
pub fn effective_command_registry(
    global_commands: &std::collections::HashMap<String, CommandDef>,
    guild_commands: &std::collections::HashMap<String, CommandDef>,
) -> std::collections::HashMap<String, CommandDef> {
    let mut commands = global_commands.clone();
    for (name, def) in guild_commands {
        commands.insert(name.clone(), def.clone());
    }
    apply_shipped_builtin_commands(&mut commands, guild_commands);
    commands
}

/// Replaces built-in primaries with the shipped command plus the guild's
/// override from `guild_commands` (see [`effective_builtin_command`]). Global
/// rows for built-ins are ignored.
pub fn apply_shipped_builtin_commands(
    commands: &mut std::collections::HashMap<String, CommandDef>,
    guild_commands: &std::collections::HashMap<String, CommandDef>,
) {
    for shipped in &shipped_commands().commands {
        let stored = builtin_command_override(guild_commands, &shipped.name);
        if let Some(effective) = effective_builtin_command(&shipped.name, stored) {
            install_shipped_builtin(commands, effective);
        }
    }
    resolve_legacy_builtin_command_collisions(commands);
}

/// Select an eligible override before resolving its settings. Prefer the
/// canonical writer key; older case variants remain readable in stable order.
/// An unmarked seed copy must never hide a marked override.
pub fn builtin_command_override<'a>(
    commands: &'a std::collections::HashMap<String, CommandDef>,
    primary: &str,
) -> Option<&'a CommandDef> {
    commands
        .get(primary)
        .filter(|row| row.builtin_override)
        .or_else(|| {
            commands
                .iter()
                .filter(|(key, row)| row.builtin_override && key.eq_ignore_ascii_case(primary))
                .min_by_key(|(key, _)| *key)
                .map(|(_, row)| row)
        })
}

/// The row a writer stores: a built-in primary is marked as this server's
/// override, any other row is not.
pub fn command_row_for_storage(storage_name: &str, cmd: &CommandDef) -> CommandDef {
    let mut row = cmd.clone();
    row.builtin_override = is_builtin_primary_command_name(storage_name);
    if row.builtin_override {
        row.name = storage_name.to_ascii_lowercase();
    }
    row
}

/// Why a command row must not be written, or `None` when it may be.
/// `guild_id` is the target scope (0 = global). A built-in primary may be
/// written only as a guild override that keeps its name and behavior kind,
/// never lowers its permission, takes no other built-in's label, and stores
/// only settings the bot can run (one-word aliases, nonzero role IDs, and
/// delete-days, timeout and slowmode values within Discord's limits). Other
/// rows must not claim a built-in name or alias, or a binary-overlay behavior.
pub fn command_persist_error(
    storage_name: &str,
    cmd: &CommandDef,
    guild_id: i64,
) -> Option<String> {
    if let Some(shipped) = shipped_command(storage_name) {
        let name = &shipped.name;
        if guild_id == 0 {
            return Some(format!(
                "built-in command `{name}` can only be changed for one server"
            ));
        }
        if !cmd.name.eq_ignore_ascii_case(name) {
            return Some(format!("built-in command `{name}` cannot be renamed"));
        }
        if !same_behavior_kind(&cmd.behavior, &shipped.behavior) {
            return Some(format!(
                "built-in command `{name}` keeps its behavior; only its settings can change"
            ));
        }
        let floor = command_permission_rank(&shipped.permission).unwrap_or(0);
        if command_permission_rank(&cmd.permission).is_none_or(|rank| rank < floor) {
            return Some(format!(
                "built-in command `{name}` needs at least `{}` permission",
                shipped.permission
            ));
        }
        for alias in &cmd.aliases {
            // Prefix input splits on whitespace, so such an alias could never run.
            if alias.is_empty() {
                return Some("aliases cannot be empty".into());
            }
            if alias.chars().any(char::is_whitespace) {
                return Some(format!("alias `{alias}` must be one word with no spaces"));
            }
            if let Some(owner) = builtin_command_for_label(alias)
                && !owner.eq_ignore_ascii_case(name)
            {
                return Some(format!(
                    "alias `{alias}` belongs to built-in command `{owner}`"
                ));
            }
        }
        // The bot builds a Discord role ID from each entry; zero is not one.
        if cmd.roles.contains(&0) {
            return Some(format!(
                "built-in command `{name}` cannot be restricted to role ID 0"
            ));
        }
        // Discord's limits, as the editor offers them; the bot clamps purge counts.
        let out_of_range =
            match &cmd.behavior {
                Behavior::Ban { delete_days, .. } => {
                    (*delete_days > 7).then_some("delete days must be 0 to 7")
                }
                Behavior::Timeout { default_minutes } => (*default_minutes > 40320)
                    .then_some("default timeout must be 0 to 40320 minutes"),
                Behavior::Slowmode { default_seconds } => (*default_seconds > 21600)
                    .then_some("default slowmode must be 0 to 21600 seconds"),
                _ => None,
            };
        if let Some(limit) = out_of_range {
            return Some(format!("built-in command `{name}`: {limit}"));
        }
        return None;
    }
    let rejected = is_binary_overlay_command_name(&cmd.name)
        || is_binary_overlay_behavior(&cmd.behavior)
        || builtin_command_for_label(storage_name).is_some()
        || builtin_command_for_label(&cmd.name).is_some()
        || cmd
            .aliases
            .iter()
            .any(|alias| builtin_command_for_label(alias).is_some());
    rejected.then(|| command_persist_rejected_message(storage_name))
}

/// Global seed rows: every shipped default except [`BINARY_OVERLAY_COMMAND_NAMES`].
pub fn default_global_seed_commands() -> Vec<CommandDef> {
    default_commands()
        .into_iter()
        .filter(|cmd| !BINARY_OVERLAY_COMMAND_NAMES.contains(&cmd.name.as_str()))
        .collect()
}

/// Inserts shipped binary-overlay commands, replacing any legacy stored row (including
/// guild-persisted `voice` rows from older builds). Stored access controls are ignored.
pub fn merge_binary_overlay_commands(commands: &mut std::collections::HashMap<String, CommandDef>) {
    for shipped in default_commands() {
        if !BINARY_OVERLAY_COMMAND_NAMES.contains(&shipped.name.as_str()) {
            continue;
        }
        install_shipped_builtin(commands, shipped);
    }
}

fn aliases_for(name: &str) -> Vec<String> {
    match name {
        "ban" => vec!["yeet".into(), "b".into()],
        "kick" => vec!["k".into()],
        "timeout" => vec!["mute".into(), "t".into()],
        "unban" => vec!["ub".into()],
        "warn" => vec!["w".into()],
        "purge" => vec!["clear".into(), "p".into()],
        "say" => vec!["echo".into()],
        "banlist" => vec!["bans".into()],
        "help" => vec!["commands".into(), "h".into()],
        "stats" => vec!["status".into()],
        _ => vec![],
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd(
    name: &str,
    category: &str,
    description: &str,
    usage: &str,
    permission: &str,
    behavior: Behavior,
    response: &str,
    log_event: &str,
    _roles: Vec<u64>,
) -> CommandDef {
    CommandDef {
        name: name.into(),
        aliases: vec![],
        category: category.into(),
        description: description.into(),
        usage: usage.into(),
        enabled: true,
        permission: permission.into(),
        roles: vec![],
        channels: vec![],
        cooldown_secs: 0,
        behavior,
        response: response.into(),
        log_event: log_event.into(),
        builtin_override: false,
    }
}

/// Default event templates (rendered into the configured log channels).
pub fn default_templates() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "member_join",
            "Member joined\n\n**User:** {user} · {mention} (`{user_id}`)\n**Account created:** `{account_created}`\n**Account age:** `{account_age}`\n**Joined at:** `{time}`",
        ),
        (
            "member_leave",
            "Member left\n\n**User:** {user} · {mention} (`{user_id}`)\n**User ID:** `{user_id}`\n**Time in server:** `{duration}`\n**Joined at:** `{joined}`\n**Left at:** `{time}`",
        ),
        (
            "member_ban",
            "Member banned\n\n**User:** {user} · {mention} (`{user_id}`)\n**Moderator:** {actor}\n**Reason:** {reason}\n**Time:** `{time}`",
        ),
        (
            "member_unban",
            "Member unbanned\n\n**User:** {user} · {mention} (`{user_id}`)\n**Moderator:** {actor}\n**Reason:** {reason}\n**Time:** `{time}`",
        ),
        (
            "member_kick",
            "Member kicked\n\n**User:** {user} · {mention} (`{user_id}`)\n**Moderator:** {actor}\n**Reason:** {reason}\n**Time:** `{time}`",
        ),
        (
            "message_edit",
            "Message edited\n\n**Channel:** {channel}\n**Message:** {message_url}\n**Author:** {mention}\n**Time:** `{time}`\n**Before (last observed):** {before}\n**After:** {after}\n**Attachments:** {attachments}\n**Coverage:** {coverage}",
        ),
        (
            "message_delete",
            "Message deleted\n\n**Channel:** {channel}\n**Message:** {message_url}\n**Author:** {mention}\n**Time:** `{time}`\n**Last observed text:** {content}\n**Attachments:** {attachments}\n**Coverage:** {coverage}",
        ),
        (
            "message_bulk_delete",
            "Messages deleted\n\n**Channel:** {channel}\n**Unique count:** `{count}`\n**Retained:** `{retained}`\n**Known missing:** `{unavailable}`\n**Storage unknown:** `{unknown}`\n**Filtered:** `{filtered}`\n**Transcript omitted:** `{omitted}`\n**Time:** `{time}`\n**Details:** {details}",
        ),
        (
            "automod",
            "Automod\n\n**Rule:** {rule}\n**User:** {mention}\n**Action:** {action}\n**Reason:** {reason}\n**Channel:** {channel}\n**Time:** `{time}`\n**Evidence:** {content}",
        ),
        (
            "mod_action",
            "Moderation action\n\n**Action:** {action}\n**User:** {user} · {mention}\n**Moderator:** {actor}\n**Reason:** {reason}\n**Time:** `{time}`",
        ),
        (
            "timeout_add",
            "Timeout added\n\n**User:** {user} · {mention} (`{user_id}`)\n**Moderator:** {actor}\n**Duration:** `{duration}`\n**Reason:** {reason}\n**Time:** `{time}`",
        ),
        (
            "timeout_remove",
            "Timeout removed\n\n**User:** {user} · {mention} (`{user_id}`)\n**Moderator:** {actor}\n**Time:** `{time}`",
        ),
        (
            "plan_executed",
            "Custom command executed\n\n**Command:** `{command}`\n**Run by:** {actor}\n**Time:** `{time}`\n**Details:** {content}",
        ),
        (
            "system",
            "System notice\n\n**Time:** `{time}`\n**Details:** {content}",
        ),
    ]
}

/// `!userinfo` / `/userinfo` reply body (rendered and clipped to 2000 UTF-16 units).
pub const USER_INFO_TEMPLATE: &str = "**User info**\n**ID:** `{user_id}`\n**Username:** {username}\n**Global name:** {global_name}\n**Nick:** {nick}\n**Account:** {account_created} ({account_age})\n**Joined:** {joined_at}\n**Roles:** {roles}\n**Voice:** {voice}\n**Warnings:** {warns}\n**Cases:**\n{cases}{clipped}";

/// Acknowledge a button on a moderation card posted by an older version.
pub const RETIRED_IMAGE_REVIEW: &str =
    "This review has expired. Use the current automod settings or moderation commands.";

/// DM body when a moderator uses kick/ban with `--dm` or slash `dm`.
pub const MOD_REASON_DM_TEMPLATE: &str = "A moderator has requested your removal from **{guild}**.\nAction: {action}\nReason: {reason}\n— {actor}";

/// DM when Discord already shows the user removed but a moderator requested `--dm`.
pub const MOD_REASON_DM_ALREADY_REMOVED_TEMPLATE: &str =
    "You are already removed from **{guild}**.\nAction: {action}\nReason: {reason}\n— {actor}";

/// Slash option label for the optional kick/ban `dm` flag.
pub const SLASH_MOD_REASON_DM_OPTION_TEMPLATE: &str = "DM the reason before applying the action";

/// Follow-up when the preliminary reason DM was sent but kick/ban failed.
pub const MOD_REASON_DM_MODERATION_FAILED_TEMPLATE: &str =
    "Update: your removal from **{guild}** did not complete; you may still be in the server.";

/// Shipped kick/ban confirmation clause when a reason DM was requested.
pub const MOD_REASON_DM_CLAUSE_TEMPLATE: &str = " | {dm_status}";
pub const MOD_REASON_DM_STATUS_SENT: &str = "Reason DM sent.";
pub const MOD_REASON_DM_STATUS_NOT_SENT: &str = "Reason DM not sent.";
pub const MOD_REASON_DM_STATUS_UNCERTAIN: &str = "Reason DM delivery uncertain.";

/// User-facing error when kick/ban targets the bot itself.
pub const CANNOT_MODERATE_BOT_TEMPLATE: &str = "Cannot moderate the bot with `{command}`.";

/// Code defaults for [`crate::types::ACTIVITY_LOG_EVENTS`].
///
/// These stay out of [`default_templates`] so startup does not insert new
/// global template rows. Delivery falls back to this wording until an operator
/// saves an override.
pub fn activity_log_templates() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "member_role_update",
            "Member roles updated\n\n**User:** {user} · {mention} (`{user_id}`)\n**Added:** {added}\n**Removed:** {removed}\n**Time:** `{time}`",
        ),
        (
            "role_change",
            "Role changed\n\n**Action:** {action}\n**Role:** {role} (`{role_id}`)\n**Name:** {role_name}\n**Detail:** {detail}\n**Time:** `{time}`",
        ),
        (
            "voice_join",
            "Voice joined\n\n**User:** {user} · {mention} (`{user_id}`)\n**Channel:** {channel}\n**Time:** `{time}`",
        ),
        (
            "voice_leave",
            "Voice left\n\n**User:** {user} · {mention} (`{user_id}`)\n**Channel:** {channel}\n**Time:** `{time}`",
        ),
        (
            "voice_move",
            "Voice moved\n\n**User:** {user} · {mention} (`{user_id}`)\n**From:** {before}\n**To:** {channel}\n**Time:** `{time}`",
        ),
        (
            "nickname_change",
            "Nickname changed\n\n**User:** {user} · {mention} (`{user_id}`)\n**Before:** {before}\n**After:** {after}\n**Time:** `{time}`",
        ),
        (
            "username_change",
            "Username changed\n\n**User:** {user} · {mention} (`{user_id}`)\n**Username:** `{before}` → `{after}`\n**Display name:** `{before_global}` → `{after_global}`\n**Time:** `{time}`",
        ),
        (
            "thread_create",
            "Thread available\n\n**Notice:** {notice}\n**Thread:** {thread} (`{thread_id}`)\n**Name:** {thread_name}\n**Parent:** {channel}\n**Owner:** {mention}\n**Time:** `{time}`",
        ),
        (
            "thread_update",
            "Thread updated\n\n**Thread:** {thread} (`{thread_id}`)\n**Name:** {thread_name}\n**Before name:** {before}\n**Parent:** {channel}\n**Archived:** {archived}\n**Locked:** {locked}\n**Auto-archive:** {auto_archive}\n**Invitable:** {invitable}\n**Time:** `{time}`",
        ),
        (
            "thread_delete",
            "Thread deleted\n\n**Thread:** {thread} (`{thread_id}`)\n**Name:** {thread_name}\n**Parent:** {channel}\n**Time:** `{time}`",
        ),
        (
            "automod_delete",
            "Automod message deleted\n\n**Channel:** {channel}\n**Rule:** {rule} (`{rule_id}`)\n**Reason:** {reason}\n**Message:** {message_url}\n**Author:** {mention}\n**Time:** `{time}`\n**Last observed text:** {content}\n**Attachments:** {attachments}\n**Coverage:** {coverage}",
        ),
        (
            "member_rejoin",
            "Member rejoined with prior cases\n\n**User:** {user} · {mention} (`{user_id}`)\n**Cases:** {cases}\n**Notice:** {notice}",
        ),
    ]
}

/// Shipped templates plus activity-log defaults. Startup seeding stays on
/// [`default_templates`] alone.
pub fn rendered_log_templates() -> Vec<(&'static str, &'static str)> {
    let mut templates = default_templates();
    templates.extend(activity_log_templates());
    templates
}

/// Exact shipped strings before embed presentation. Match bytes, never custom wording.
pub(crate) fn pre_embed_log_templates() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "member_join",
            "**:inbox_tray: {user} joined the server**\n\n**User:** {mention} (`{user_id}`)\n**Account created:** `{account_created}`\n**Account age:** `{account_age}`\n**Joined at:** `{time}`",
        ),
        (
            "member_leave",
            "**:outbox_tray: {user} left the server**\n\n**User ID:** `{user_id}`\n**Time in server:** `{duration}`\n**Joined at:** `{joined}`\n**Left at:** `{time}`",
        ),
        (
            "member_ban",
            "**:hammer: {user} banned**\n\n**Moderator:** {actor}\n**Reason:** {reason}\n**Time:** `{time}`",
        ),
        (
            "member_unban",
            "**:unlock: {user} unbanned**\n\n**Moderator:** {actor}\n**Reason:** {reason}\n**Time:** `{time}`",
        ),
        (
            "member_kick",
            "**:boot: {user} kicked**\n\n**Moderator:** {actor}\n**Reason:** {reason}\n**Time:** `{time}`",
        ),
        (
            "message_edit",
            "**:pencil2: Message updated in {channel}**\n\n**Message:** {message_url}\n**Author:** {mention}\n**Time:** `{time}`\n**Before (last observed):** {before}\n**After:** {after}\n**Attachments:** {attachments}\n**Coverage:** {coverage}",
        ),
        (
            "message_delete",
            "**:wastebasket: Message deleted in {channel}**\n\n**Message:** {message_url}\n**Author:** {mention}\n**Time:** `{time}`\n**Last observed text:** {content}\n**Attachments:** {attachments}\n**Coverage:** {coverage}",
        ),
        (
            "message_bulk_delete",
            "**:wastebasket: Messages deleted in {channel}**\n\n**Unique count:** `{count}`\n**Retained:** `{retained}`\n**Known missing:** `{unavailable}`\n**Storage unknown:** `{unknown}`\n**Filtered:** `{filtered}`\n**Transcript omitted:** `{omitted}`\n**Time:** `{time}`\n**Details:** {details}",
        ),
        (
            "automod",
            "**:shield: Automod: {rule}**\n\n**User:** {mention}\n**Action:** {action}\n**Reason:** {reason}\n**Channel:** {channel}\n**Time:** `{time}`\n**Evidence:** {content}",
        ),
        (
            "mod_action",
            "**:judge: {action}**\n\n**User:** {mention}\n**Moderator:** {actor}\n**Reason:** {reason}\n**Time:** `{time}`",
        ),
        (
            "timeout_add",
            "**:timer: {user} timed out**\n\n**Moderator:** {actor}\n**Duration:** `{duration}`\n**Reason:** {reason}\n**Time:** `{time}`",
        ),
        (
            "timeout_remove",
            "**:alarm_clock: {user} timeout removed**\n\n**Moderator:** {actor}\n**Time:** `{time}`",
        ),
        (
            "plan_executed",
            "**:robot: Custom command executed**\n\n**Command:** `{command}`\n**Run by:** {actor}\n**Time:** `{time}`\n**Details:** {content}",
        ),
        (
            "system",
            "**:gear: System notice**\n\n**Time:** `{time}`\n{content}",
        ),
        (
            "member_role_update",
            "**:label: {user} roles updated**\n\n**User:** {mention} (`{user_id}`)\n**Added:** {added}\n**Removed:** {removed}\n**Time:** `{time}`",
        ),
        (
            "role_change",
            "**:label: Role {action}**\n\n**Role:** {role} (`{role_id}`)\n**Name:** {role_name}\n**Detail:** {detail}\n**Time:** `{time}`",
        ),
        (
            "voice_join",
            "**:loud_sound: {user} joined voice**\n\n**User:** {mention} (`{user_id}`)\n**Channel:** {channel}\n**Time:** `{time}`",
        ),
        (
            "voice_leave",
            "**:mute: {user} left voice**\n\n**User:** {mention} (`{user_id}`)\n**Channel:** {channel}\n**Time:** `{time}`",
        ),
        (
            "voice_move",
            "**:arrow_right: {user} moved voice channels**\n\n**User:** {mention} (`{user_id}`)\n**From:** {before}\n**To:** {channel}\n**Time:** `{time}`",
        ),
        (
            "nickname_change",
            "**:label: {user} nickname changed**\n\n**User:** {mention} (`{user_id}`)\n**Before:** {before}\n**After:** {after}\n**Time:** `{time}`",
        ),
        (
            "username_change",
            "**:label: {user} username changed**\n\n**User:** {mention} (`{user_id}`)\n**Username:** `{before}` → `{after}`\n**Display name:** `{before_global}` → `{after_global}`\n**Time:** `{time}`",
        ),
        (
            "thread_create",
            "**:thread: Thread {notice}**\n\n**Thread:** {thread} (`{thread_id}`)\n**Name:** {thread_name}\n**Parent:** {channel}\n**Owner:** {mention}\n**Time:** `{time}`",
        ),
        (
            "thread_update",
            "**:thread: Thread updated**\n\n**Thread:** {thread} (`{thread_id}`)\n**Name:** {thread_name}\n**Before name:** {before}\n**Parent:** {channel}\n**Archived:** {archived}\n**Locked:** {locked}\n**Auto-archive:** {auto_archive}\n**Invitable:** {invitable}\n**Time:** `{time}`",
        ),
        (
            "thread_delete",
            "**:thread: Thread deleted**\n\n**Thread:** {thread} (`{thread_id}`)\n**Name:** {thread_name}\n**Parent:** {channel}\n**Time:** `{time}`",
        ),
        (
            "automod_delete",
            "**:wastebasket: Automod deleted message in {channel}**\n\n**Rule:** {rule} (`{rule_id}`)\n**Message:** {message_url}\n**Author:** {mention}\n**Time:** `{time}`\n**Last observed text:** {content}\n**Attachments:** {attachments}\n**Coverage:** {coverage}",
        ),
        (
            "member_rejoin",
            "**:arrows_counterclockwise: {user} rejoined with prior moderation**\n\n**User:** {mention} (`{user_id}`)\n{cases}\n{notice}",
        ),
    ]
}

/// Exact shipped delete template before attachment details were added.
pub(crate) const PRE_ATTACHMENT_DELETE_TEMPLATE: &str = "**:wastebasket: Message deleted in {channel}**\n\n**Message:** {message_url}\n**Author:** {mention}\n**Time:** `{time}`\n**Text:** {content}";

/// Byte-for-byte defaults shipped before routine retained snapshots.
pub(crate) const PRE_RETAINED_MESSAGE_TEMPLATES: [(&str, &str); 3] = [
    (
        "message_edit",
        "**:pencil2: Message updated in {channel}**\n\n**Message:** {message_url}\n**Author:** {mention}\n**Time:** `{time}`\n**Attachments:** `{attachments}`\n**Current text:** {after}",
    ),
    (
        "message_delete",
        "**:wastebasket: Message deleted in {channel}**\n\n**Message:** {message_url}\n**Author:** {mention}\n**Time:** `{time}`\n**Text:** {content}\n**Attachments:** {attachments}",
    ),
    (
        "message_bulk_delete",
        "**:wastebasket: Messages deleted in {channel}**\n\n**Count:** `{count}`\n**Time:** `{time}`",
    ),
];

/// Exact default on the first snapshot branch; custom templates are untouched.
pub(crate) const PRE_BULK_STATUS_TEMPLATE: &str = "**:wastebasket: Messages deleted in {channel}**\n\n**Count:** `{count}`\n**Retained:** `{retained}`\n**Unavailable:** `{unavailable}`\n**Filtered:** `{filtered}`\n**Time:** `{time}`\n**Details:** {details}";

// Exact defaults from before the readable-log upgrade; used only for one-time migration.
pub(crate) fn legacy_log_templates() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "member_join",
            "**:inbox_tray: {user} joined**\nUser: {mention} (`{user_id}`)\nAccount created: {account_created} ({account_age})\nJoined at: {time}",
        ),
        (
            "member_leave",
            "**:outbox_tray: {user} left**\nUser: {mention} (`{user_id}`)\nWas in server: {duration}\nLeft at: {time}",
        ),
        (
            "member_ban",
            "**:hammer: {user} banned**\nModerator: {actor}\nReason: {reason}",
        ),
        (
            "member_unban",
            "**:unlock: {user} unbanned**\nModerator: {actor}\nReason: {reason}",
        ),
        (
            "member_kick",
            "**:boot: {user} kicked**\nModerator: {actor}\nReason: {reason}",
        ),
        (
            "message_edit",
            "**:pencil2: Message updated in {channel}**\nMessage: {message_url}\nAuthor: {mention}\nCurrent text: {after}\nAttachments: {attachments}",
        ),
        (
            "message_delete",
            "**:wastebasket: Message deleted in {channel}**\nMessage: {message_url}\nAuthor: {mention}\nText: {content}",
        ),
        (
            "message_bulk_delete",
            "**:wastebasket: {count} messages deleted in {channel}**",
        ),
        (
            "automod",
            "**:shield: Automod: {rule}**\nUser: {mention}\nAction: {action}\nReason: {reason}\nEvidence: {content}\nChannel: {channel}",
        ),
        (
            "mod_action",
            "**:judge: {action}**\nUser: {mention}\nModerator: {actor}\nReason: {reason}",
        ),
        (
            "timeout_add",
            "**:timer: {user} timed out**\nModerator: {actor}\nDuration: {duration}\nReason: {reason}",
        ),
        (
            "timeout_remove",
            "**:alarm_clock: {user} timeout removed**\nModerator: {actor}",
        ),
        (
            "plan_executed",
            "**:robot: Custom command `{command}` executed by {actor}**\n{content}",
        ),
        ("system", "**:gear: {content}"),
    ]
}

/// A sample custom command plan, used to demonstrate the step editor.
#[allow(dead_code)]
pub fn sample_plan_command() -> CommandDef {
    CommandDef {
        name: "welcome".into(),
        aliases: vec![],
        category: "Custom".into(),
        description: "Greet the user and ping a staff role.".into(),
        usage: "!welcome".into(),
        enabled: false,
        permission: "everyone".into(),
        roles: vec![],
        channels: vec![],
        cooldown_secs: 30,
        behavior: Behavior::Plan {
            steps: vec![
                PlanStep::Reply {
                    template: "Hi {user}, welcome to **{guild}**!".into(),
                },
                PlanStep::React {
                    emoji: "👋".into()
                },
                PlanStep::Log {
                    event: "plan_executed".into(),
                    template: "`{actor}` ran `{command}`".into(),
                },
            ],
        },
        response: String::new(),
        log_event: String::new(),
        builtin_override: false,
    }
}

pub const BAN_DURATION_ERROR: &str = "Ban duration must be a positive whole number of minutes.";

#[cfg(test)]
mod rolling_upgrade_tests {
    use super::*;

    #[test]
    fn softban_is_available_without_persisting_a_new_behavior_kind() {
        let mut commands = std::collections::HashMap::new();
        merge_binary_overlay_commands(&mut commands);
        assert!(matches!(
            commands["softban"].behavior,
            Behavior::Softban { .. }
        ));
        assert!(command_def_is_binary_overlay(&commands["softban"]));
        assert!(
            !default_global_seed_commands()
                .iter()
                .any(|cmd| cmd.name == "softban")
        );
    }

    #[test]
    fn builtin_override_applies_settings_but_keeps_shipped_identity_and_access_floor() {
        let shipped = default_commands()
            .into_iter()
            .find(|cmd| cmd.name == "ban")
            .expect("ban");
        let stored = CommandDef {
            name: "Ban".into(),
            category: "Custom".into(),
            enabled: false,
            permission: "admin".into(),
            roles: vec![555],
            channels: vec![777],
            cooldown_secs: 42,
            description: "server text".into(),
            aliases: vec![
                "Hammer".into(),
                "k".into(),
                "yeet".into(),
                "hammer".into(),
                "ban".into(),
            ],
            behavior: Behavior::Ban {
                default_reason: "server reason".into(),
                delete_days: 1,
            },
            response: "server reply".into(),
            log_event: "plan_executed".into(),
            builtin_override: true,
            ..shipped.clone()
        };
        // Without the writer's flag the row is a seed copy and changes nothing.
        let unflagged = CommandDef {
            builtin_override: false,
            ..stored.clone()
        };
        let ignored = effective_builtin_command("ban", Some(&unflagged)).expect("ban");
        assert_eq!(ignored.description, shipped.description);
        assert!(ignored.enabled);
        let merged = effective_builtin_command("ban", Some(&stored)).expect("ban");
        assert_eq!(merged.name, "ban");
        assert_eq!(merged.category, shipped.category);
        assert_eq!(merged.description, "server text");
        // Another built-in's label ("k"), the own name and duplicates are dropped.
        assert_eq!(
            merged.aliases,
            vec!["Hammer".to_string(), "yeet".to_string()]
        );
        assert!(matches!(
            &merged.behavior,
            Behavior::Ban { default_reason, delete_days: 1 } if default_reason == "server reason"
        ));
        assert!(!merged.enabled);
        assert_eq!(merged.permission, "admin");
        assert_eq!(
            (merged.roles.clone(), merged.channels.clone()),
            (vec![555], vec![777])
        );
        assert_eq!(merged.cooldown_secs, 42);
        assert_eq!(
            (merged.response.as_str(), merged.log_event.as_str()),
            ("server reply", "plan_executed")
        );

        // A row that would widen access or swap the behavior falls back to shipped values.
        let widened = CommandDef {
            permission: "everyone".into(),
            behavior: Behavior::Say,
            ..stored.clone()
        };
        let merged = effective_builtin_command("ban", Some(&widened)).expect("ban");
        assert_eq!(merged.permission, shipped.permission);
        assert!(matches!(merged.behavior, Behavior::Ban { .. }));
        let unknown = CommandDef {
            permission: "moderator".into(),
            ..stored.clone()
        };
        assert_eq!(
            effective_builtin_command("ban", Some(&unknown))
                .unwrap()
                .permission,
            "mod"
        );

        // Guild rows override; global rows (shipped seeds) never do.
        let none = std::collections::HashMap::new();
        let mut global_only = std::collections::HashMap::from([("ban".into(), stored.clone())]);
        apply_shipped_builtin_commands(&mut global_only, &none);
        assert!(global_only["ban"].enabled);
        assert_eq!(global_only["ban"].permission, shipped.permission);
        let guild = std::collections::HashMap::from([("Ban".to_string(), stored.clone())]);
        let registry = effective_command_registry(&none, &guild);
        assert!(!registry.contains_key("Ban"));
        assert!(!registry["ban"].enabled);
        assert_eq!(registry["ban"].cooldown_secs, 42);
    }

    #[test]
    fn builtin_override_aliases_resolve_against_custom_commands() {
        let custom = |name: &str, aliases: &[&str]| CommandDef {
            name: name.into(),
            aliases: aliases.iter().map(|a| a.to_string()).collect(),
            category: "Custom".into(),
            description: "x".into(),
            usage: String::new(),
            enabled: true,
            permission: "everyone".into(),
            roles: vec![],
            channels: vec![],
            cooldown_secs: 0,
            behavior: Behavior::Say,
            response: String::new(),
            log_event: String::new(),
            builtin_override: false,
        };
        let shipped = default_commands()
            .into_iter()
            .find(|cmd| cmd.name == "ban")
            .unwrap();
        let ban = CommandDef {
            aliases: vec!["hammer".into(), "greet".into(), "x".into()],
            builtin_override: true,
            ..shipped
        };
        let warn = CommandDef {
            aliases: vec!["x".into(), "caution".into()],
            builtin_override: true,
            ..default_commands()
                .into_iter()
                .find(|cmd| cmd.name == "warn")
                .unwrap()
        };
        let guild = std::collections::HashMap::from([
            ("ban".to_string(), ban),
            ("warn".to_string(), warn),
            ("greet".to_string(), custom("greet", &[])),
            ("other".to_string(), custom("other", &["hammer", "mine"])),
        ]);
        let registry = effective_command_registry(&std::collections::HashMap::new(), &guild);
        // A custom command's own name beats a built-in override alias, and two
        // overrides sharing an alias resolve by built-in name ("ban" before "warn")...
        assert_eq!(
            registry["ban"].aliases,
            vec!["hammer".to_string(), "x".to_string()]
        );
        assert_eq!(registry["warn"].aliases, vec!["caution".to_string()]);
        assert!(registry.contains_key("greet"));
        // ...and a built-in override alias beats a custom alias.
        assert_eq!(registry["other"].aliases, vec!["mine".to_string()]);
    }

    #[test]
    fn resolve_legacy_builtin_command_collisions_strips_reserved_aliases() {
        let mut commands = std::collections::HashMap::from([
            (
                "custom".into(),
                CommandDef {
                    name: "custom".into(),
                    aliases: vec!["status".into(), "mine".into()],
                    category: String::new(),
                    description: "x".into(),
                    usage: "!custom".into(),
                    enabled: true,
                    permission: "everyone".into(),
                    roles: vec![],
                    channels: vec![],
                    cooldown_secs: 0,
                    behavior: Behavior::Say,
                    response: String::new(),
                    log_event: String::new(),
                    builtin_override: false,
                },
            ),
            (
                "yeet".into(),
                CommandDef {
                    name: "yeet".into(),
                    aliases: vec![],
                    category: String::new(),
                    description: "shadow".into(),
                    usage: "!yeet".into(),
                    enabled: true,
                    permission: "everyone".into(),
                    roles: vec![],
                    channels: vec![],
                    cooldown_secs: 0,
                    behavior: Behavior::Help,
                    response: String::new(),
                    log_event: String::new(),
                    builtin_override: false,
                },
            ),
        ]);
        resolve_legacy_builtin_command_collisions(&mut commands);
        assert!(!commands.contains_key("yeet"));
        assert!(!commands["custom"].matches("status"));
        assert!(commands["custom"].matches("mine"));
    }

    #[test]
    fn merge_binary_overlay_commands_replaces_stale_voice_rows() {
        let voice = default_commands()
            .into_iter()
            .find(|cmd| cmd.name == "voice")
            .expect("voice command");
        let mut commands = std::collections::HashMap::from([(
            "voice".into(),
            CommandDef {
                name: "voice".into(),
                description: "legacy guild row".into(),
                behavior: Behavior::Say,
                enabled: false,
                aliases: vec!["vc".into()],
                category: "Custom".into(),
                usage: "!voice".into(),
                permission: "owner".into(),
                roles: vec![],
                channels: vec![],
                cooldown_secs: 99,
                response: "stale".into(),
                log_event: "stale".into(),
                builtin_override: false,
            },
        )]);
        assert!(
            commands.contains_key("voice"),
            "legacy row must exist before merge"
        );
        merge_binary_overlay_commands(&mut commands);
        assert_eq!(commands["voice"].description, voice.description);
        assert_eq!(commands["voice"].enabled, voice.enabled);
        assert_eq!(commands["voice"].permission, voice.permission);
        assert_eq!(commands["voice"].roles, voice.roles);
        assert_eq!(commands["voice"].channels, voice.channels);
        assert_eq!(commands["voice"].cooldown_secs, voice.cooldown_secs);
        assert_eq!(commands["voice"].aliases, voice.aliases);
        assert!(matches!(commands["voice"].behavior, Behavior::Voice));
    }

    #[test]
    fn voice_command_is_binary_overlay_not_global_seed() {
        assert!(
            default_commands().iter().any(|cmd| cmd.name == "voice"),
            "voice must ship in the binary default list"
        );
        assert!(
            !default_global_seed_commands()
                .iter()
                .any(|cmd| cmd.name == "voice"),
            "voice must not be written to shared global command rows"
        );
    }

    #[test]
    fn voice_behavior_is_binary_overlay_even_when_renamed() {
        let voice = default_commands()
            .into_iter()
            .find(|cmd| cmd.name == "voice")
            .expect("voice command");
        let renamed = CommandDef {
            name: "vc".into(),
            ..voice
        };
        assert!(command_def_is_binary_overlay(&renamed));
    }

    #[test]
    fn voice_alias_cannot_replace_builtin_command() {
        let hijack = CommandDef {
            name: "customvc".into(),
            aliases: vec!["voice".into()],
            category: "Moderation".into(),
            description: "steal voice".into(),
            usage: "!customvc".into(),
            enabled: true,
            permission: "mod".into(),
            roles: vec![],
            channels: vec![],
            cooldown_secs: 0,
            behavior: Behavior::Help,
            response: String::new(),
            log_event: String::new(),
            builtin_override: false,
        };
        assert!(command_persist_error("customvc", &hijack, 10).is_some());
    }

    #[test]
    fn builtin_command_labels_cover_names_and_aliases() {
        let labels = builtin_command_labels();
        assert!(labels.iter().any(|l| l == "ban"));
        assert!(labels.iter().any(|l| l == "yeet"));
        assert!(labels.iter().any(|l| l == "kick"));
        assert!(labels.iter().any(|l| l == "k"));
        assert!(labels.iter().any(|l| l == "voice"));
    }

    #[test]
    fn shipped_label_index_matches_catalog_scan() {
        let catalog = default_commands();
        let owner = |label: &str| {
            catalog
                .iter()
                .find(|cmd| {
                    cmd.name.eq_ignore_ascii_case(label)
                        || cmd
                            .aliases
                            .iter()
                            .any(|alias| alias.eq_ignore_ascii_case(label))
                })
                .map(|cmd| cmd.name.as_str())
        };
        let primary = |name: &str| {
            catalog
                .iter()
                .any(|cmd| cmd.name.eq_ignore_ascii_case(name))
        };
        // The Kelvin sign lowercases to `k` only under Unicode rules.
        let mut labels: Vec<String> = ["", "nope", "\u{212A}", "BAN ", "b\u{0}"]
            .map(String::from)
            .into();
        for cmd in &catalog {
            for label in std::iter::once(&cmd.name).chain(&cmd.aliases) {
                let mut mixed = label.clone();
                mixed[..1].make_ascii_uppercase();
                labels.extend([
                    label.clone(),
                    label.to_ascii_uppercase(),
                    mixed,
                    format!("{label}x"),
                ]);
            }
        }
        for label in &labels {
            assert_eq!(builtin_command_for_label(label), owner(label), "{label:?}");
            assert_eq!(
                is_builtin_primary_command_name(label),
                primary(label),
                "{label:?}"
            );
        }
        assert_eq!(builtin_command_for_label("\u{212A}"), None);
        assert_eq!(builtin_command_for_label("YEET"), Some("ban"));
        assert!(!is_builtin_primary_command_name("yeet"));
        for cmd in &catalog {
            let effective = effective_builtin_command(&cmd.name.to_ascii_uppercase(), None)
                .expect("shipped command resolves by name");
            assert_eq!(
                serde_json::to_value(&effective).unwrap(),
                serde_json::to_value(cmd).unwrap()
            );
        }
    }

    #[test]
    fn custom_command_cannot_claim_builtin_name_or_alias() {
        let kick_hijack = CommandDef {
            name: "kick".into(),
            aliases: vec![],
            category: "Moderation".into(),
            description: "steal kick".into(),
            usage: "!plugin".into(),
            enabled: true,
            permission: "mod".into(),
            roles: vec![],
            channels: vec![],
            cooldown_secs: 0,
            behavior: Behavior::Help,
            response: String::new(),
            log_event: String::new(),
            builtin_override: false,
        };
        assert!(command_persist_error("myplugin", &kick_hijack, 10).is_some());

        let alias_hijack = CommandDef {
            name: "myplugin".into(),
            aliases: vec!["K".into()],
            category: "Moderation".into(),
            description: "steal kick alias".into(),
            usage: "!myplugin".into(),
            enabled: true,
            permission: "mod".into(),
            roles: vec![],
            channels: vec![],
            cooldown_secs: 0,
            behavior: Behavior::Help,
            response: String::new(),
            log_event: String::new(),
            builtin_override: false,
        };
        assert!(command_persist_error("myplugin", &alias_hijack, 10).is_some());
    }

    #[test]
    fn builtin_rows_persist_only_as_guild_overrides_within_their_floor() {
        let shipped = |name: &str| {
            default_commands()
                .into_iter()
                .find(|cmd| cmd.name == name)
                .unwrap()
        };
        let stats = shipped("stats");
        assert_eq!(
            command_persist_error("stats", &stats, 0).as_deref(),
            Some("built-in command `stats` can only be changed for one server")
        );
        assert_eq!(command_persist_error("stats", &stats, 10), None);
        let edited = CommandDef {
            enabled: false,
            permission: "owner".into(),
            roles: vec![1],
            channels: vec![2],
            cooldown_secs: 5,
            aliases: vec!["numbers".into()],
            description: "Counts".into(),
            ..stats.clone()
        };
        assert_eq!(command_persist_error("stats", &edited, 10), None);
        for (bad, message) in [
            (
                CommandDef {
                    name: "numbers".into(),
                    ..stats.clone()
                },
                "built-in command `stats` cannot be renamed",
            ),
            (
                CommandDef {
                    behavior: Behavior::Help,
                    ..stats.clone()
                },
                "built-in command `stats` keeps its behavior; only its settings can change",
            ),
            (
                CommandDef {
                    permission: "mod".into(),
                    ..stats.clone()
                },
                "built-in command `stats` needs at least `admin` permission",
            ),
            (
                CommandDef {
                    permission: "moderators".into(),
                    ..stats.clone()
                },
                "built-in command `stats` needs at least `admin` permission",
            ),
            (
                CommandDef {
                    aliases: vec!["B".into()],
                    ..stats.clone()
                },
                "alias `B` belongs to built-in command `ban`",
            ),
        ] {
            assert_eq!(
                command_persist_error("stats", &bad, 10).as_deref(),
                Some(message)
            );
        }
        // Settings the bot cannot run as saved are refused: a zero role ID panics
        // in the role check, a spaced alias never matches the split input, and
        // out-of-range numbers are truncated or rejected by Discord.
        let behavior_of = |name: &str, behavior: Behavior| CommandDef {
            behavior,
            ..shipped(name)
        };
        for (storage, bad, message) in [
            (
                "stats",
                CommandDef {
                    roles: vec![0],
                    ..stats.clone()
                },
                "built-in command `stats` cannot be restricted to role ID 0",
            ),
            (
                "stats",
                CommandDef {
                    roles: vec![0, 555],
                    ..stats.clone()
                },
                "built-in command `stats` cannot be restricted to role ID 0",
            ),
            (
                "stats",
                CommandDef {
                    aliases: vec!["count me".into()],
                    ..stats.clone()
                },
                "alias `count me` must be one word with no spaces",
            ),
            (
                "stats",
                CommandDef {
                    aliases: vec![String::new()],
                    ..stats.clone()
                },
                "aliases cannot be empty",
            ),
            (
                "ban",
                behavior_of(
                    "ban",
                    Behavior::Ban {
                        default_reason: "r".into(),
                        delete_days: 8,
                    },
                ),
                "built-in command `ban`: delete days must be 0 to 7",
            ),
            (
                "timeout",
                behavior_of(
                    "timeout",
                    Behavior::Timeout {
                        default_minutes: 40321,
                    },
                ),
                "built-in command `timeout`: default timeout must be 0 to 40320 minutes",
            ),
            (
                "slowmode",
                behavior_of(
                    "slowmode",
                    Behavior::Slowmode {
                        default_seconds: 65536,
                    },
                ),
                "built-in command `slowmode`: default slowmode must be 0 to 21600 seconds",
            ),
        ] {
            assert_eq!(
                command_persist_error(storage, &bad, 10).as_deref(),
                Some(message)
            );
        }
        // The limits themselves and real role IDs still save.
        for (storage, ok) in [
            (
                "ban",
                behavior_of(
                    "ban",
                    Behavior::Ban {
                        default_reason: "r".into(),
                        delete_days: 7,
                    },
                ),
            ),
            (
                "timeout",
                behavior_of(
                    "timeout",
                    Behavior::Timeout {
                        default_minutes: 40320,
                    },
                ),
            ),
            (
                "slowmode",
                behavior_of(
                    "slowmode",
                    Behavior::Slowmode {
                        default_seconds: 21600,
                    },
                ),
            ),
            (
                "stats",
                CommandDef {
                    roles: vec![555],
                    aliases: vec!["Numbers".into()],
                    ..stats.clone()
                },
            ),
        ] {
            assert_eq!(command_persist_error(storage, &ok, 10), None, "{storage}");
        }
        // Binary-overlay built-ins take guild overrides too: any binary that can
        // open core schema 0024+ knows the softban and voice behavior kinds.
        for name in ["voice", "softban"] {
            assert_eq!(
                command_persist_error(name, &shipped(name), 10),
                None,
                "{name}"
            );
            assert!(
                command_persist_error(name, &shipped(name), 0).is_some(),
                "{name}"
            );
        }
        // A custom row still cannot carry an overlay behavior under another name.
        let renamed = CommandDef {
            name: "vc".into(),
            aliases: vec![],
            ..shipped("voice")
        };
        assert!(command_persist_error("vc", &renamed, 10).is_some());
    }
}

#[cfg(test)]
mod rule_tests {
    use super::rule;
    use serde_json::json;

    #[test]
    fn built_in_detection_defaults_use_matcher_keys() {
        for (id, expected) in [
            (
                "mention_spam",
                json!({"window_secs": 10, "max_mentions_in_window": 5}),
            ),
            ("new_member_url", json!({"grace_secs": 120})),
            ("badword", json!({})),
            ("invite", json!({"allowlist_codes": []})),
            ("velocity", json!({"window_secs": 10, "max_messages": 8})),
            ("account_age", json!({"min_age_days": 3})),
        ] {
            assert_eq!(rule(id).params, expected, "{id}");
        }
    }
}
