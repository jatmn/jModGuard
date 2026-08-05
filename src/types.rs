use serde::{Deserialize, Serialize};

/// Global scope marker: config rows with this guild_id apply to all servers.
pub const GLOBAL_SCOPE: i64 = 0;

/// All automod rule identifiers (fixed set; per-rule behavior is configurable).
pub const RULE_IDS: [&str; 7] = [
    "mention_spam",
    "attachment_flood",
    "new_member_url",
    "badword",
    "invite",
    "velocity",
    "account_age",
];

/// Log event types emitted to per-event log channels.
pub const EVENT_TYPES: [&str; 14] = [
    "member_join",
    "member_leave",
    "member_ban",
    "member_unban",
    "member_kick",
    "message_edit",
    "message_delete",
    "message_bulk_delete",
    "automod",
    "mod_action",
    "timeout_add",
    "timeout_remove",
    "plan_executed",
    "system",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GuildSettings {
    pub prefix: String,
    pub bot_name: String,
    pub retention_days: u32,
}

impl Default for GuildSettings {
    fn default() -> Self {
        Self {
            prefix: "!".into(),
            bot_name: String::new(),
            retention_days: 7,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleDef {
    pub name: String,
    pub description: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub params: serde_json::Value,
    #[serde(default)]
    pub actions: Vec<ActionDef>,
    #[serde(default)]
    pub whitelist_roles: Vec<u64>,
    #[serde(default)]
    pub whitelist_channels: Vec<u64>,
    #[serde(default)]
    pub whitelist_users: Vec<u64>,
}

impl RuleDef {
    pub fn action_for_strike(&self, strike: u32) -> Option<&ActionDef> {
        self.actions
            .iter()
            .filter(|a| a.at <= strike)
            .max_by_key(|a| a.at)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionDef {
    /// Strike count at which this action fires. Higher `at` = more severe.
    #[serde(default)]
    pub at: u32,
    /// log | delete | warn | timeout | kick | ban
    #[serde(default)]
    pub action: String,
    /// Timeout length in minutes (timeout actions).
    #[serde(default)]
    pub duration_minutes: u32,
    /// Message deletion window in days for bans (0-7).
    #[serde(default)]
    pub delete_days: u8,
    /// Reason template; supports the same {variables} as event templates.
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandDef {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub usage: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// everyone | mod | admin | owner
    #[serde(default = "default_perm")]
    pub permission: String,
    #[serde(default)]
    pub roles: Vec<u64>,
    #[serde(default)]
    pub channels: Vec<u64>,
    #[serde(default)]
    pub cooldown_secs: u64,
    pub behavior: Behavior,
    /// In-chat reply template, rendered with command variables.
    #[serde(default)]
    pub response: String,
    /// Log event type emitted after a successful run ("" = none).
    #[serde(default)]
    pub log_event: String,
}

impl CommandDef {
    pub fn matches(&self, name: &str) -> bool {
        name.eq_ignore_ascii_case(&self.name) || self.aliases.iter().any(|a| name.eq_ignore_ascii_case(a))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "params", rename_all = "snake_case")]
pub enum Behavior {
    Ban { default_reason: String, delete_days: u8 },
    Unban { default_reason: String },
    Kick { default_reason: String },
    Timeout { default_minutes: u32 },
    Warn { default_reason: String },
    Warns,
    WarnClear,
    Banlist,
    Purge { default_count: u32 },
    Say,
    Slowmode { default_seconds: u64 },
    Setname,
    SetPrefix,
    Help,
    About,
    Stats,
    Testregex,
    Rule { action: String },
    Template { action: String },
    Logchannel { action: String },
    Plan { steps: Vec<PlanStep> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlanStep {
    Reply { template: String },
    Dm { template: String },
    Log { event: String, template: String },
    AddRole { role_id: u64 },
    RemoveRole { role_id: u64 },
    Timeout { minutes: u32 },
    Kick,
    Ban { delete_days: u8 },
    DeleteCommandMessage,
    React { emoji: String },
    Wait { secs: u64 },
}

fn default_true() -> bool {
    true
}

fn default_perm() -> String {
    "everyone".into()
}
