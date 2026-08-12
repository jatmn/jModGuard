use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::marker::PhantomData;

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

/// Effective (fully resolved) guild settings used at runtime.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

/// Persisted settings row: only explicitly set fields are stored.
///
/// Missing fields inherit from the next layer (guild override → global →
/// hardcoded / config defaults). This is the root contract for "global
/// defaults apply to all servers until overridden."
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GuildSettingsOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u32>,
}

impl GuildSettingsOverride {
    pub fn is_empty(&self) -> bool {
        self.prefix.is_none() && self.bot_name.is_none() && self.retention_days.is_none()
    }

    /// Apply this override on top of `base` (only `Some` fields replace).
    pub fn merge_into(&self, base: &GuildSettings) -> GuildSettings {
        GuildSettings {
            prefix: self
                .prefix
                .clone()
                .unwrap_or_else(|| base.prefix.clone()),
            bot_name: self
                .bot_name
                .clone()
                .unwrap_or_else(|| base.bot_name.clone()),
            retention_days: self.retention_days.unwrap_or(base.retention_days),
        }
    }

    /// Convert a fully-populated settings blob into an all-fields override
    /// (used when intentionally materializing every field, e.g. tests).
    pub fn from_complete(settings: &GuildSettings) -> Self {
        Self {
            prefix: Some(settings.prefix.clone()),
            bot_name: Some(settings.bot_name.clone()),
            retention_days: Some(settings.retention_days),
        }
    }
}

/// Field-level settings resolution: guild override → global → default.
pub fn resolve_settings(
    guild: &Option<GuildSettingsOverride>,
    global: &Option<GuildSettingsOverride>,
    default: &GuildSettings,
) -> GuildSettings {
    let mut out = default.clone();
    if let Some(gl) = global {
        out = gl.merge_into(&out);
    }
    if let Some(g) = guild {
        out = g.merge_into(&out);
    }
    out
}

/// Per-field settings patch: `None` = leave unchanged, `Some(None)` = clear override
/// (inherit parent), `Some(Some(v))` = set override.
///
/// Wire contract (WebUI/HTTP): omitted field → unchanged; JSON `null` → clear;
/// value → set. Default `Option<Option<T>>` serde collapses `null` into omitted
/// (outer `None`) — use [`de_settings_patch_field`] on HTTP bodies.
pub type SettingsPatchField<T> = Option<Option<T>>;

/// Deserialize a tri-state settings patch field from JSON.
///
/// - absent + `#[serde(default)]` → `None` (unchanged)
/// - `null` → `Some(None)` (clear override / inherit parent)
/// - value → `Some(Some(v))` (set override)
pub fn de_settings_patch_field<'de, D, T>(deserializer: D) -> Result<SettingsPatchField<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct PatchVisitor<T>(PhantomData<T>);

    impl<'de, T: Deserialize<'de>> Visitor<'de> for PatchVisitor<T> {
        type Value = SettingsPatchField<T>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("null (clear override), a value (set), or an absent field (unchanged)")
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Some(None))
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Some(None))
        }

        fn visit_some<D2: Deserializer<'de>>(self, deserializer: D2) -> Result<Self::Value, D2::Error> {
            T::deserialize(deserializer).map(|v| Some(Some(v)))
        }
    }

    deserializer.deserialize_option(PatchVisitor(PhantomData))
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
    #[serde(
        default,
        deserialize_with = "crate::ids::de_u64_vec",
        serialize_with = "crate::ids::ser_u64_vec_str"
    )]
    pub whitelist_roles: Vec<u64>,
    #[serde(
        default,
        deserialize_with = "crate::ids::de_u64_vec",
        serialize_with = "crate::ids::ser_u64_vec_str"
    )]
    pub whitelist_channels: Vec<u64>,
    #[serde(
        default,
        deserialize_with = "crate::ids::de_u64_vec",
        serialize_with = "crate::ids::ser_u64_vec_str"
    )]
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
    #[serde(
        default,
        deserialize_with = "crate::ids::de_u64_vec",
        serialize_with = "crate::ids::ser_u64_vec_str"
    )]
    pub roles: Vec<u64>,
    #[serde(
        default,
        deserialize_with = "crate::ids::de_u64_vec",
        serialize_with = "crate::ids::ser_u64_vec_str"
    )]
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
        name.eq_ignore_ascii_case(&self.name)
            || self.aliases.iter().any(|a| name.eq_ignore_ascii_case(a))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "params", rename_all = "snake_case")]
pub enum Behavior {
    Ban {
        default_reason: String,
        delete_days: u8,
    },
    Unban {
        default_reason: String,
    },
    Kick {
        default_reason: String,
    },
    Timeout {
        default_minutes: u32,
    },
    Warn {
        default_reason: String,
    },
    Warns,
    WarnClear,
    Banlist,
    Purge {
        default_count: u32,
    },
    Say,
    Slowmode {
        default_seconds: u64,
    },
    Setname,
    SetPrefix,
    Help,
    About,
    Stats,
    Testregex,
    Rule {
        action: String,
    },
    Template {
        action: String,
    },
    Logchannel {
        action: String,
    },
    Plan {
        steps: Vec<PlanStep>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlanStep {
    Reply { template: String },
    Dm { template: String },
    Log { event: String, template: String },
    AddRole {
        #[serde(
            deserialize_with = "crate::ids::de_u64",
            serialize_with = "crate::ids::ser_u64_str"
        )]
        role_id: u64,
    },
    RemoveRole {
        #[serde(
            deserialize_with = "crate::ids::de_u64",
            serialize_with = "crate::ids::ser_u64_str"
        )]
        role_id: u64,
    },
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize, Default)]
    struct PatchBody {
        #[serde(default, deserialize_with = "de_settings_patch_field")]
        prefix: SettingsPatchField<String>,
        #[serde(default, deserialize_with = "de_settings_patch_field")]
        retention_days: SettingsPatchField<u32>,
    }

    #[test]
    fn settings_patch_null_clears_absent_leaves_unchanged() {
        let clear: PatchBody = serde_json::from_str(r#"{"prefix":null}"#).unwrap();
        assert_eq!(clear.prefix, Some(None));
        assert_eq!(clear.retention_days, None);

        let omit: PatchBody = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(omit.prefix, None);
        assert_eq!(omit.retention_days, None);

        let set: PatchBody = serde_json::from_str(r#"{"prefix":"!x","retention_days":0}"#).unwrap();
        assert_eq!(set.prefix, Some(Some("!x".into())));
        assert_eq!(set.retention_days, Some(Some(0)));
    }
}
