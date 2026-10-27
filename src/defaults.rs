//! Seeded defaults for rules, commands, and event templates.
//! These are the global (guild_id = 0) fallbacks written on first boot.
//! Every value here is fully editable at runtime via the WebUI / bot commands.

use crate::types::{ActionDef, Behavior, CommandDef, PlanStep, RULE_IDS, RuleDef};
use serde_json::json;
use std::collections::HashSet;
use std::sync::OnceLock;

pub fn default_rules() -> Vec<RuleDef> {
    let mut out = Vec::with_capacity(RULE_IDS.len());
    for id in RULE_IDS {
        out.push(rule(id));
    }
    out
}

fn rule(id: &str) -> RuleDef {
    let (name, description, params, actions) = match id {
        "mention_spam" => (
            "Mention spam",
            "Messages pinging @everyone/@here or flooding role/channel mentions.",
            json!({"window_secs": 10, "max_everyone_mentions": 0, "max_mentions_in_window": 5}),
            vec![action(1, "ban", "Ping spam (@everyone/@here)", 0, 1)],
        ),
        "attachment_flood" => (
            "Exact image spam",
            "The same distinct image files posted by one author across channels. Dry-run logs only; normal mode deletes the matched messages and bans. Extra images are allowed. Configure an automod log channel.",
            json!({"mode":"exact_image_test", "window_secs": 60, "min_attachments": 2, "min_channels": 3}),
            vec![action(1, "log", "Exact image spam test", 0, 0)],
        ),
        "repeat_text" => (
            "Repeated text",
            "The same trimmed text posted by one author across channels. Matching is exact after leading/trailing whitespace only. Dry-run logs only; normal mode deletes the matched messages and bans.",
            json!({"mode":"exact_text", "window_secs": 60, "min_channels": 3, "min_length": 20}),
            vec![action(1, "log", "Repeated text test", 0, 0)],
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
        "invite" => (
            "Invite links",
            "Discord invite links (whitelisted servers can be configured in params.allowlist_guilds).",
            json!({"allowlist_guilds": []}),
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
        _ => unreachable!(),
    };
    RuleDef {
        name: name.into(),
        description: description.into(),
        enabled: true,
        dry_run: id == "attachment_flood" || id == "repeat_text",
        params,
        actions,
        whitelist_roles: vec![],
        whitelist_channels: vec![],
        whitelist_users: vec![],
        skip_moderators: false,
        include_channels: vec![],
        log_automod: true,
        open_case: true,
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
        reason: reason.into(),
    }
}

#[allow(clippy::vec_init_then_push)]
pub fn default_commands() -> Vec<CommandDef> {
    let mut out = Vec::new();
    out.push(cmd(
        "ban",
        "Moderation",
        "Ban a user from the server.",
        "!ban <user> [reason]",
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
        "!kick <user> [reason]",
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
pub const BINARY_OVERLAY_COMMAND_NAMES: &[&str] = &["voice"];

pub fn is_binary_overlay_command_name(name: &str) -> bool {
    BINARY_OVERLAY_COMMAND_NAMES
        .iter()
        .any(|overlay| overlay.eq_ignore_ascii_case(name))
}

/// Behaviors merged from the running binary and never written to shared `commands` rows.
pub fn is_binary_overlay_behavior(behavior: &Behavior) -> bool {
    matches!(behavior, Behavior::Voice)
}

pub fn command_def_is_binary_overlay(cmd: &CommandDef) -> bool {
    is_binary_overlay_command_name(&cmd.name) || is_binary_overlay_behavior(&cmd.behavior)
}

fn builtin_command_labels_lower() -> &'static HashSet<String> {
    static LABELS: OnceLock<HashSet<String>> = OnceLock::new();
    LABELS.get_or_init(|| {
        let mut labels = HashSet::new();
        for cmd in default_commands() {
            labels.insert(cmd.name.to_ascii_lowercase());
            for alias in &cmd.aliases {
                labels.insert(alias.to_ascii_lowercase());
            }
        }
        labels
    })
}

/// Every shipped command name and alias (derived from [`default_commands`]).
pub fn builtin_command_labels() -> Vec<String> {
    let mut labels: Vec<String> = builtin_command_labels_lower().iter().cloned().collect();
    labels.sort();
    labels
}

/// Canonical primary command name when `label` matches a shipped name or alias.
pub fn builtin_command_for_label(label: &str) -> Option<String> {
    for cmd in default_commands() {
        if cmd.name.eq_ignore_ascii_case(label) {
            return Some(cmd.name.clone());
        }
        for alias in &cmd.aliases {
            if alias.eq_ignore_ascii_case(label) {
                return Some(cmd.name.clone());
            }
        }
    }
    None
}

fn shipped_builtin_command(primary: &str) -> Option<CommandDef> {
    default_commands()
        .into_iter()
        .find(|cmd| cmd.name.eq_ignore_ascii_case(primary))
}

pub fn command_persist_rejected_message(storage_name: &str) -> String {
    format!(
        "command `{storage_name}` conflicts with a built-in command and cannot be written to the database"
    )
}

/// True when a command row must not be written to shared storage (rolling upgrades).
pub fn rejects_binary_overlay_command_persist(storage_name: &str, cmd: &CommandDef) -> bool {
    if is_binary_overlay_command_name(storage_name)
        || is_binary_overlay_command_name(&cmd.name)
        || is_binary_overlay_behavior(&cmd.behavior)
        || cmd
            .aliases
            .iter()
            .any(|alias| is_binary_overlay_command_name(alias))
    {
        return true;
    }

    let storage_primary = builtin_command_for_label(storage_name);
    let def_primary = builtin_command_for_label(&cmd.name);

    if storage_primary.is_none() {
        if def_primary.is_some() {
            return true;
        }
        return cmd
            .aliases
            .iter()
            .any(|alias| builtin_command_for_label(alias).is_some());
    }

    let primary = storage_primary.expect("label is reserved");
    if !primary.eq_ignore_ascii_case(storage_name) {
        return true;
    }
    if let Some(shipped) = shipped_builtin_command(&primary) {
        let shipped_behavior = serde_json::to_value(&shipped.behavior).ok();
        let cmd_behavior = serde_json::to_value(&cmd.behavior).ok();
        if shipped_behavior != cmd_behavior {
            return true;
        }
    }
    match def_primary {
        Some(owner) if owner.eq_ignore_ascii_case(&primary) => {}
        _ if cmd.name.eq_ignore_ascii_case(&primary) => {}
        _ => return true,
    }
    for alias in &cmd.aliases {
        match builtin_command_for_label(alias) {
            None => {}
            Some(owner) if owner.eq_ignore_ascii_case(&primary) => {}
            Some(_) => return true,
        }
    }
    false
}

/// Global seed rows: every shipped default except [`BINARY_OVERLAY_COMMAND_NAMES`].
pub fn default_global_seed_commands() -> Vec<CommandDef> {
    default_commands()
        .into_iter()
        .filter(|cmd| !BINARY_OVERLAY_COMMAND_NAMES.contains(&cmd.name.as_str()))
        .collect()
}

pub fn merge_binary_overlay_commands(commands: &mut std::collections::HashMap<String, CommandDef>) {
    for cmd in default_commands() {
        if BINARY_OVERLAY_COMMAND_NAMES.contains(&cmd.name.as_str())
            && !commands.contains_key(&cmd.name)
        {
            commands.insert(cmd.name.clone(), cmd);
        }
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
    }
}

/// Default event templates (rendered into the configured log channels).
pub fn default_templates() -> Vec<(&'static str, &'static str)> {
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
    ]
}

/// Appended to an attachment-burst review after a moderator uses a button.
pub const ATTACHMENT_BURST_DECISION: &str = "{body}\n\n**Review:** {decision} by {actor}";
pub const ATTACHMENT_BURST_REASON: &str = "Same attachment in {channels} channels within {window}";
pub const ATTACHMENT_BURST_TIMEOUT_ACTION: &str = "timeout {duration}";
pub const ATTACHMENT_BURST_TIMEOUT_MISSED: &str = "timeout not applied; awaiting review";
pub const ATTACHMENT_BURST_TIMEOUT_UNCERTAIN: &str = "timeout outcome uncertain; awaiting review";
pub const ATTACHMENT_BURST_DENIED: &str = "That review action is not available to you.";
pub const ATTACHMENT_BURST_HANDLED: &str = "That review was already handled.";
pub const ATTACHMENT_BURST_FAILED: &str = "could not apply that action";
pub const ATTACHMENT_BURST_RELEASED: &str = "timeout removed";
pub const ATTACHMENT_BURST_BANNED: &str = "banned";
pub const ATTACHMENT_BURST_REVIEW_REASON: &str = "attachment burst review";

/// Code defaults for [`crate::types::ACTIVITY_LOG_EVENTS`].
///
/// These stay out of [`default_templates`] so startup does not insert new
/// global template rows. Delivery falls back to this wording until an operator
/// saves an override.
pub fn activity_log_templates() -> Vec<(&'static str, &'static str)> {
    vec![
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
    ]
}

/// Shipped templates plus activity-log defaults. Startup seeding stays on
/// [`default_templates`] alone.
pub fn rendered_log_templates() -> Vec<(&'static str, &'static str)> {
    let mut templates = default_templates();
    templates.extend(activity_log_templates());
    templates
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
    }
}

#[cfg(test)]
mod rolling_upgrade_tests {
    use super::*;

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
        };
        assert!(rejects_binary_overlay_command_persist("customvc", &hijack));
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
        };
        assert!(rejects_binary_overlay_command_persist(
            "myplugin",
            &kick_hijack
        ));

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
        };
        assert!(rejects_binary_overlay_command_persist(
            "myplugin",
            &alias_hijack
        ));
    }

    #[test]
    fn builtin_row_rejects_behavior_replacement() {
        let shipped = default_commands()
            .into_iter()
            .find(|cmd| cmd.name == "stats")
            .expect("stats");
        let hijack = CommandDef {
            behavior: Behavior::Help,
            ..shipped
        };
        assert!(rejects_binary_overlay_command_persist("stats", &hijack));
    }
}
