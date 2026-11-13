//! The one shared emoji map for Discord log reports.
//!
//! Standard Unicode emoji only, never custom server emotes, so every guild can
//! render them. Discord output built from a shipped template gets one emoji
//! before its title and one before each field label: log embeds, command
//! replies, reason DMs, `!userinfo`, `!voice`, `!status` and review notices. Stored
//! templates and WebUI chrome stay emoji-free; the WebUI's Discord preview
//! receives this map from the API. Custom templates are unchanged unless they
//! opt in with `{emoji}`.

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

/// Replies, DMs and notices from shipped text templates, by kind. Built-in
/// command replies use their command name, looked up in [`ACTIONS`] first.
pub const REPLIES: &[(&str, &str)] = &[
    ("banned", "🔨"),
    ("kicked", "👢"),
    ("setname", "📛"),
    ("prefix", "⚙️"),
    ("userinfo", "👤"),
    ("voice", "🔊"),
    ("denied", "⛔"),
    ("handled", "ℹ️"),
    ("failed", "⚠️"),
];

/// Title emoji for the `!status` embed.
pub const STATUS: &str = "📊";

/// Field label to emoji, for shipped templates and the `!status` embed.
pub const LABELS: &[(&str, &str)] = &[
    ("User", "👤"),
    ("ID", "🆔"),
    ("Global name", "🪪"),
    ("Nick", "📛"),
    ("Account", "📅"),
    ("Joined", "⏰"),
    ("Roles", "🏷️"),
    ("Voice", "🔊"),
    ("Warnings", "⚠️"),
    ("Review", "🧑‍⚖️"),
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
    ("Uptime", "⏱️"),
    ("Warns", "⚠️"),
    ("Bans", "🔨"),
    ("Release status", "🚀"),
    ("Container use", "📦"),
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
        .chain(REPLIES)
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

/// Title emoji for a reply, DM or notice kind (a built-in command name or a [`REPLIES`] kind).
pub fn reply(kind: &str) -> Option<&'static str> {
    lookup(ACTIONS, kind).or_else(|| lookup(REPLIES, kind))
}

/// The label that starts a template line: `**Label:**` or plain `Label: `.
pub fn line_label(line: &str) -> Option<&str> {
    let label = match line.strip_prefix("**") {
        Some(rest) => rest.split_once(":**")?.0,
        None => line.split_once(": ")?.0,
    };
    let mut chars = label.chars();
    (chars.next().is_some_and(char::is_alphabetic)
        && chars.all(|c| c.is_alphanumeric() || " #()-".contains(c)))
    .then_some(label)
}

/// Decorate a shipped text template before substitution, so values never gain
/// emoji. The first line is its title (inside a fully bold line, otherwise in
/// front); each later line that starts with a mapped label gets that label's
/// emoji. A first line that starts with a variable is not a title.
pub fn decorate_template(title: Option<&str>, template: &str) -> String {
    let mut lines = template.split('\n');
    let mut out = match lines.next() {
        Some(first) if !first.is_empty() && !first.starts_with('{') => {
            match first
                .strip_prefix("**")
                .and_then(|rest| rest.strip_suffix("**"))
                .filter(|inner| !inner.contains("**"))
            {
                Some(inner) => format!("**{}**", decorate(title, inner)),
                None => decorate(title, first),
            }
        }
        Some(first) => first.to_string(),
        None => String::new(),
    };
    for line in lines {
        out.push('\n');
        match line_label(line).map(|name| (name, label(name))) {
            Some((name, Some(_))) => {
                let bold = line.starts_with("**");
                let start = if bold { 2 } else { 0 };
                out.push_str(&line[..start]);
                out.push_str(&decorate(label(name), &line[start..]));
            }
            _ => out.push_str(line),
        }
    }
    out
}

/// Per-template emoji the WebUI's Discord preview applies, from this map only.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PreviewEmoji {
    /// Title emoji, also the `{emoji}` value for custom templates.
    pub title: Option<&'static str>,
    /// `{action}`-specific title emoji (`mod_action` only).
    pub actions: BTreeMap<&'static str, &'static str>,
    /// Label emoji for the shipped template's fields.
    pub labels: BTreeMap<&'static str, &'static str>,
}

/// Preview data for one log template type and its shipped text.
pub fn preview(event: &str, shipped: &str) -> PreviewEmoji {
    PreviewEmoji {
        title: lookup(EVENTS, event),
        actions: if event == "mod_action" {
            ACTIONS.iter().copied().collect()
        } else {
            BTreeMap::new()
        },
        labels: shipped
            .lines()
            .skip(1)
            .filter_map(|line| {
                let name = line.strip_prefix("**")?.split_once(":** ")?.0;
                LABELS.iter().find(|(known, _)| *known == name).copied()
            })
            .collect(),
    }
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

    /// Count map emoji at the starts of the title and of each label in decorated text.
    fn assert_one_per_title_and_label(template: &str, title: &str) {
        let decorated = decorate_template(Some(title), template);
        let first = decorated.lines().next().unwrap();
        let first = first.strip_prefix("**").unwrap_or(first);
        assert!(first.starts_with(&format!("{title} ")), "{decorated}");
        let mut expected = 1;
        for (plain, line) in template.lines().zip(decorated.lines()).skip(1) {
            if let Some(name) = line_label(plain) {
                let emoji = label(name).unwrap_or_else(|| panic!("map label `{name}`"));
                let start = if plain.starts_with("**") { 2 } else { 0 };
                assert_eq!(&line[start..], format!("{emoji} {}", &plain[start..]));
                expected += 1;
            } else {
                assert_eq!(line, plain, "values and prose stay unchanged");
            }
        }
        let found: usize = decorated
            .lines()
            .map(|line| all().filter(|emoji| line.contains(emoji)).count().min(1))
            .sum();
        assert_eq!(found, expected, "{decorated}");
        assert_eq!(decorate_template(Some(title), &decorated), decorated);
    }

    #[test]
    fn every_shipped_reply_dm_and_notice_template_is_mapped() {
        use crate::defaults as d;
        for command in d::default_commands() {
            if command.response.is_empty() {
                continue;
            }
            let emoji = reply(&command.name).unwrap_or_else(|| panic!("{}", command.name));
            assert_one_per_title_and_label(&command.response, emoji);
        }
        // Reason DMs carry the past-tense action the ban/kick behaviors pass.
        for action in ["banned", "kicked"] {
            assert_one_per_title_and_label(d::MOD_REASON_DM_TEMPLATE, reply(action).unwrap());
            assert_one_per_title_and_label(
                d::MOD_REASON_DM_ALREADY_REMOVED_TEMPLATE,
                reply(action).unwrap(),
            );
        }
        for (template, kind) in [
            (d::MOD_REASON_DM_MODERATION_FAILED_TEMPLATE, "failed"),
            (d::USER_INFO_TEMPLATE, "userinfo"),
            (d::ATTACHMENT_BURST_DENIED, "denied"),
            (d::ATTACHMENT_BURST_HANDLED, "handled"),
            (d::ATTACHMENT_BURST_FAILED, "failed"),
        ] {
            assert_one_per_title_and_label(template, reply(kind).unwrap());
        }
        assert_eq!(
            decorate_template(None, d::ATTACHMENT_BURST_DECISION),
            "{body}\n\n**🧑‍⚖️ Review:** {decision} by {actor}"
        );
        assert_eq!(
            decorate_template(Some("👤"), d::USER_INFO_TEMPLATE)
                .lines()
                .take(2)
                .collect::<Vec<_>>(),
            ["**👤 User info**", "**🆔 ID:** `{user_id}`"]
        );
        assert_eq!(
            decorate_template(Some("🔨"), d::MOD_REASON_DM_TEMPLATE),
            "🔨 A moderator has requested your removal from **{guild}**.\n⚡ Action: {action}\n📝 Reason: {reason}\n— {actor}"
        );
    }

    #[test]
    fn decoration_never_touches_values_or_unmapped_lines() {
        let template = "{body}\nNote: {x}\n**Unmapped:** {y}\n  **Reason:** {z}";
        assert_eq!(decorate_template(Some("🔨"), template), template);
        assert_eq!(decorate_template(Some("🔨"), ""), "");
        assert_eq!(
            decorate_template(None, "Title\nReason: {r}"),
            "Title\n📝 Reason: {r}"
        );
        let vars = BTreeMap::from([("r".to_string(), "**Reason:** {emoji} 🔨".to_string())]);
        let rendered =
            crate::templates::render(&decorate_template(Some("🔨"), "Banned\nReason: {r}"), &vars);
        assert_eq!(rendered, "🔨 Banned\n📝 Reason: **Reason:** {emoji} 🔨");
    }

    #[test]
    fn preview_data_comes_from_this_map() {
        for (event, shipped) in crate::defaults::rendered_log_templates() {
            let data = preview(event, shipped);
            assert_eq!(data.title, lookup(EVENTS, event), "{event}");
            for line in shipped.lines().skip(1).filter(|l| l.starts_with("**")) {
                let name = line_label(line).unwrap();
                assert_eq!(
                    data.labels.get(name).copied(),
                    label(name),
                    "{event}: {name}"
                );
                assert!(data.labels.contains_key(name), "{event}: {name}");
            }
            assert_eq!(data.actions.is_empty(), event != "mod_action");
        }
        assert_eq!(preview("unknown", "x").title, None);
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
