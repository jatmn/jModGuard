//! The one shared emoji map for Discord log reports.
//!
//! Standard Unicode emoji only, never custom server emotes, so every guild can
//! render them. The bot applies them when it builds a Discord embed from a
//! shipped default: one emoji before the title and one before each field label.
//! Stored templates, the WebUI editor, its live preview and its API chrome stay
//! emoji-free. Custom templates are unchanged unless they opt in with `{emoji}`.

use std::collections::BTreeMap;

/// Template type to title emoji. Every log event has exactly one entry.
pub const EVENTS: &[(&str, &str)] = &[
    ("member_join", "📥"),
    ("member_leave", "📤"),
    ("member_rejoin", "📋"),
    ("member_ban", "🔨"),
    ("member_unban", "✅"),
    ("member_kick", "👢"),
    ("message_edit", "✏️"),
    ("message_delete", "🗑️"),
    ("message_bulk_delete", "🗑️"),
    ("automod", "🛡️"),
    ("automod_delete", "🛡️"),
    ("mod_action", "⚖️"),
    ("timeout_add", "⏱️"),
    ("timeout_remove", "⏱️"),
    ("plan_executed", "⌨️"),
    ("system", "⚙️"),
    ("member_role_update", "🏷️"),
    ("role_change", "🏷️"),
    ("voice_join", "🔊"),
    ("voice_leave", "🔇"),
    ("voice_move", "🔀"),
    ("nickname_change", "📛"),
    ("username_change", "📛"),
    ("thread_create", "🧵"),
    ("thread_update", "🧵"),
    ("thread_delete", "🧵"),
];

/// `mod_action` reports pick a more specific title emoji from `{action}`.
pub const ACTIONS: &[(&str, &str)] = &[
    ("ban", "🔨"),
    ("softban", "🔨"),
    ("unban", "✅"),
    ("kick", "👢"),
    ("timeout", "⏱️"),
    ("warn", "⚠️"),
    ("warnclear", "⚠️"),
    ("purge", "🗑️"),
    ("slowmode", "🐢"),
];

/// Status reports. Reserved for the `!status` embed; its rendering is not changed here.
pub const STATUS: &str = "📊";

/// Field label to emoji, for every label in the shipped log templates.
pub const LABELS: &[(&str, &str)] = &[
    ("User", "👤"),
    ("Author", "👤"),
    ("Owner", "👤"),
    ("User ID", "🆔"),
    ("Moderator", "🧑‍⚖️"),
    ("Run by", "🧑‍⚖️"),
    ("Reason", "📝"),
    ("Time", "⏰"),
    ("Joined at", "⏰"),
    ("Left at", "⏰"),
    ("Account created", "📅"),
    ("Duration", "⌛"),
    ("Time in server", "⌛"),
    ("Account age", "⌛"),
    ("Channel", "📍"),
    ("Parent", "📍"),
    ("From", "📍"),
    ("To", "📍"),
    ("Cases", "📋"),
    ("Details", "ℹ️"),
    ("Detail", "ℹ️"),
    ("Notice", "📢"),
    ("Message", "🔗"),
    ("Before (last observed)", "⏪"),
    ("Before", "⏪"),
    ("Before name", "⏪"),
    ("After", "⏩"),
    ("Attachments", "📎"),
    ("Coverage", "🔍"),
    ("Last observed text", "💬"),
    ("Evidence", "🧾"),
    ("Unique count", "🔢"),
    ("Retained", "💾"),
    ("Known missing", "❓"),
    ("Storage unknown", "❔"),
    ("Filtered", "🧹"),
    ("Transcript omitted", "✂️"),
    ("Rule", "📏"),
    ("Action", "⚡"),
    ("Command", "⌨️"),
    ("Added", "➕"),
    ("Removed", "➖"),
    ("Role", "🏷️"),
    ("Name", "🔤"),
    ("Username", "🔤"),
    ("Display name", "🪪"),
    ("Thread", "🧵"),
    ("Archived", "🗄️"),
    ("Locked", "🔒"),
    ("Auto-archive", "⏲️"),
    ("Invitable", "✉️"),
];

fn lookup(table: &[(&str, &'static str)], key: &str) -> Option<&'static str> {
    table.iter().find(|(name, _)| *name == key).map(|(_, e)| *e)
}

/// Every emoji in the map.
pub fn all() -> impl Iterator<Item = &'static str> {
    EVENTS
        .iter()
        .chain(ACTIONS)
        .chain(LABELS)
        .map(|(_, emoji)| *emoji)
        .chain([STATUS])
}

/// Title emoji for one report. `None` for an unknown template type.
pub fn title(event: &str, vars: &BTreeMap<String, String>) -> Option<&'static str> {
    if event == "mod_action"
        && let Some(emoji) = vars
            .get("action")
            .and_then(|action| lookup(ACTIONS, action.trim().to_ascii_lowercase().as_str()))
    {
        return Some(emoji);
    }
    lookup(EVENTS, event)
}

/// Emoji for a shipped field label, if mapped.
pub fn label(name: &str) -> Option<&'static str> {
    lookup(LABELS, name)
}

/// Prefix `text` with `emoji` once; text already starting with a map emoji is unchanged.
pub fn decorate(emoji: Option<&str>, text: &str) -> String {
    match emoji {
        Some(emoji) if !all().any(|known| text.starts_with(known)) => format!("{emoji} {text}"),
        _ => text.to_string(),
    }
}

/// Supply `{emoji}` for templates that opt in. Unknown types leave it unset, so
/// the token stays visible like any other unknown variable. Caller values win.
pub fn with_var(event: &str, vars: &BTreeMap<String, String>) -> Option<BTreeMap<String, String>> {
    let emoji = title(event, vars)?;
    if vars.contains_key("emoji") {
        return None;
    }
    let mut vars = vars.clone();
    vars.insert("emoji".into(), emoji.into());
    Some(vars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_log_event_has_one_title_emoji() {
        let mut events = crate::types::EVENT_TYPES
            .into_iter()
            .chain(crate::types::ACTIVITY_LOG_EVENTS)
            .collect::<Vec<_>>();
        events.sort_unstable();
        let mut mapped = EVENTS.iter().map(|(event, _)| *event).collect::<Vec<_>>();
        mapped.sort_unstable();
        assert_eq!(mapped, events, "add new template types to emoji::EVENTS");
        assert_eq!(title("not_an_event", &BTreeMap::new()), None);
    }

    #[test]
    fn mod_action_uses_its_action_emoji() {
        for (action, emoji) in [
            ("ban", "🔨"),
            (" Warn ", "⚠️"),
            ("kick", "👢"),
            ("timeout", "⏱️"),
            ("unban", "✅"),
            ("custom", "⚖️"),
        ] {
            let vars = BTreeMap::from([("action".into(), action.into())]);
            assert_eq!(title("mod_action", &vars), Some(emoji), "{action}");
        }
        let warn = BTreeMap::from([("action".into(), "warn".into())]);
        assert_eq!(title("automod", &warn), Some("🛡️"));
    }

    #[test]
    fn map_is_standard_unicode_emoji_only() {
        for emoji in all() {
            assert!(!emoji.contains("<:") && !emoji.contains("<a:"), "{emoji}");
            assert!(
                !emoji.is_empty() && !emoji.chars().any(|c| c.is_ascii()),
                "{emoji}"
            );
        }
        // Sequences keep their presentation selector and joiner.
        assert_eq!(
            label("Moderator"),
            Some("\u{1F9D1}\u{200D}\u{2696}\u{FE0F}")
        );
        assert_eq!(label("Details"), Some("\u{2139}\u{FE0F}"));
        assert_eq!(lookup(EVENTS, "timeout_add"), Some("\u{23F1}\u{FE0F}"));
    }

    #[test]
    fn decorate_is_idempotent() {
        let once = decorate(Some("🔨"), "Member banned");
        assert_eq!(once, "🔨 Member banned");
        assert_eq!(decorate(Some("🔨"), &once), once);
        assert_eq!(decorate(Some("👤"), "🧑‍⚖️ Moderator"), "🧑‍⚖️ Moderator");
        assert_eq!(decorate(None, "Plain"), "Plain");
    }

    #[test]
    fn emoji_variable_is_opt_in_and_never_overrides() {
        let vars = BTreeMap::from([("action".into(), "warn".into())]);
        assert_eq!(with_var("mod_action", &vars).unwrap()["emoji"], "⚠️");
        assert!(with_var("unknown", &vars).is_none());
        let own = BTreeMap::from([("emoji".into(), "x".into())]);
        assert!(with_var("system", &own).is_none());
    }
}
