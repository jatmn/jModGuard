//! Discord log presentation shared with the WebUI. No Discord client dependency.
use std::collections::BTreeMap;

use serde::Serialize;

pub const RED: u32 = 0xff6666;
pub const YELLOW: u32 = 0xfdfd96;
pub const ORANGE: u32 = 0xffb347;
pub const GREEN: u32 = 0x77dd77;

/// Presentation metadata, also supplied to the live template preview.
#[derive(Debug, Clone, Serialize)]
pub struct Style {
    pub title: String,
    pub color: u32,
    pub action_colors: BTreeMap<&'static str, u32>,
    pub removal_color: Option<u32>,
}

pub fn style(event: &str) -> Style {
    let title = crate::defaults::rendered_log_templates()
        .into_iter()
        .find(|(name, _)| *name == event)
        .map(|(_, template)| template.lines().next().unwrap_or("Log event"))
        .unwrap_or("Log event");
    let color = match event {
        "member_ban"
        | "timeout_add"
        | "message_delete"
        | "message_bulk_delete"
        | "automod_delete"
        | "member_leave"
        | "thread_delete"
        | "voice_leave" => RED,
        "member_join" | "member_unban" | "timeout_remove" | "voice_join" | "thread_create" => GREEN,
        "member_rejoin" | "system" => YELLOW,
        _ => ORANGE,
    };
    let action_colors = match event {
        "role_change" => BTreeMap::from([("created", GREEN), ("deleted", RED)]),
        "mod_action" | "automod" => BTreeMap::from([("warn", YELLOW), ("warning", YELLOW)]),
        _ => BTreeMap::new(),
    };
    Style {
        title: title.into(),
        color,
        action_colors,
        removal_color: (event == "member_role_update").then_some(RED),
    }
}

impl Style {
    pub fn accent(&self, vars: &BTreeMap<String, String>) -> u32 {
        if let Some(color) = self.removal_color
            && vars
                .get("removed")
                .is_some_and(|v| !v.is_empty() && v != "none")
        {
            return color;
        }
        vars.get("action")
            .and_then(|v| {
                self.action_colors
                    .get(v.trim().to_ascii_lowercase().as_str())
            })
            .copied()
            .unwrap_or(self.color)
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub value: String,
    pub inline: bool,
}

/// One embed only. No author/footer are emitted, so their limits cannot be exceeded.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LogEmbed {
    pub title: String,
    pub color: u32,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub fields: Vec<Field>,
}

/// Clip Unicode scalar boundaries, counting UTF-16 units like Discord content.
pub fn clip(text: &str, limit: usize) -> String {
    if text.encode_utf16().count() <= limit {
        return text.into();
    }
    if limit == 0 {
        return String::new();
    }
    let mut units = 0;
    let mut out = String::new();
    for ch in text.chars() {
        if units + ch.len_utf16() > limit - 1 {
            break;
        }
        out.push(ch);
        units += ch.len_utf16();
    }
    out.push('…');
    out
}

impl LogEmbed {
    /// Deterministic title, description, then field-order budget. Empty or excess
    /// fields are omitted. Reapply after edits as well as initial rendering.
    pub fn bounded(mut self) -> Self {
        let mut left = 6000;
        self.title = clip(&self.title, 256);
        left -= self.title.encode_utf16().count();
        self.description = clip(&self.description, 4096.min(left));
        left -= self.description.encode_utf16().count();
        let mut fields = Vec::new();
        for mut field in self.fields.into_iter().take(25) {
            if left < 2 {
                break;
            }
            field.name = clip(&field.name, 256.min(left - 1));
            left -= field.name.encode_utf16().count();
            field.value = clip(&field.value, 1024.min(left));
            left -= field.value.encode_utf16().count();
            if !field.name.is_empty() && !field.value.is_empty() {
                fields.push(field);
            }
        }
        self.fields = fields;
        self
    }
}

/// Only exact shipped strings are layouts. Parse the trusted template BEFORE
/// substituting values; message text resembling a field cannot inject fields.
/// Custom text remains a description, including old defaults deliberately restored.
pub fn render(event: &str, template: &str, vars: &BTreeMap<String, String>) -> LogEmbed {
    let style = style(event);
    let mut embed = LogEmbed {
        color: style.accent(vars),
        title: style.title,
        description: String::new(),
        fields: Vec::new(),
    };
    let shipped = crate::defaults::rendered_log_templates()
        .into_iter()
        .any(|(name, text)| name == event && text == template);
    if shipped {
        for line in template.lines().skip(1).filter(|line| !line.is_empty()) {
            if let Some((name, value)) = line.strip_prefix("**").and_then(|s| s.split_once(":** "))
            {
                let value = crate::templates::render_bounded(value, vars, 1024);
                embed.fields.push(Field {
                    name: name.into(),
                    value: if value.is_empty() {
                        "—".into()
                    } else {
                        value
                    },
                    inline: false,
                });
            }
        }
    } else {
        embed.description = crate::templates::render_bounded(template, vars, 4096);
    }
    embed.bounded()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_event_has_the_expected_accent() {
        let groups = [
            (
                RED,
                "member_ban timeout_add message_delete message_bulk_delete automod_delete member_leave thread_delete voice_leave",
            ),
            (YELLOW, "member_rejoin system"),
            (
                ORANGE,
                "member_kick message_edit automod mod_action nickname_change username_change member_role_update role_change thread_update voice_move plan_executed",
            ),
            (
                GREEN,
                "member_join member_unban timeout_remove voice_join thread_create",
            ),
        ];
        let mut covered = Vec::new();
        for (color, names) in groups {
            for event in names.split_whitespace() {
                assert_eq!(style(event).accent(&BTreeMap::new()), color, "{event}");
                covered.push(event);
            }
        }
        covered.sort_unstable();
        let mut expected = crate::types::EVENT_TYPES
            .into_iter()
            .chain(crate::types::ACTIVITY_LOG_EVENTS)
            .collect::<Vec<_>>();
        expected.sort_unstable();
        assert_eq!(covered, expected);
        for (event, key, value, color) in [
            ("role_change", "action", "created", GREEN),
            ("role_change", "action", "deleted", RED),
            ("role_change", "action", "updated", ORANGE),
            ("member_role_update", "removed", "<@&123>", RED),
            ("member_role_update", "removed", "none", ORANGE),
            ("mod_action", "action", "warn", YELLOW),
            ("automod", "action", "warning", YELLOW),
        ] {
            assert_eq!(
                style(event).accent(&BTreeMap::from([(key.into(), value.into())])),
                color
            );
        }
    }

    #[test]
    fn defaults_use_fields_custom_templates_keep_description() {
        let vars = BTreeMap::from([
            ("mention".into(), "<@123>".into()),
            ("content".into(), "hello\n**Injected:** evil".into()),
            ("channel".into(), "<#456>".into()),
        ]);
        let template = crate::defaults::default_templates()
            .into_iter()
            .find(|(e, _)| *e == "message_delete")
            .unwrap()
            .1;
        let embed = render("message_delete", template, &vars);
        assert_eq!(embed.title, "Message deleted");
        assert_eq!(embed.color, RED);
        assert!(embed.description.is_empty());
        assert_eq!(embed.fields.len(), 7);
        assert_eq!(embed.fields[0].name, "Channel");
        assert_eq!(embed.fields[0].value, "<#456>");
        assert_eq!(embed.fields[2].value, "<@123>");
        assert_eq!(embed.fields[4].value, "hello\n**Injected:** evil");
        let custom = render("message_delete", "My wording: {content}", &vars);
        assert_eq!(custom.description, "My wording: hello\n**Injected:** evil");
        assert!(custom.fields.is_empty());
        assert_eq!(custom.color, RED);
    }

    #[test]
    fn bounds_each_component_and_combined_utf16_without_splitting_scalars() {
        let input = LogEmbed {
            title: "🦀".repeat(200),
            color: RED,
            description: "🌕".repeat(3000),
            fields: (0..30)
                .map(|_| Field {
                    name: "n".repeat(300),
                    value: "🦀".repeat(800),
                    inline: false,
                })
                .collect(),
        };
        let result = input.clone().bounded();
        assert_eq!(input.bounded(), result);
        assert!(result.title.ends_with('…'));
        assert!(result.title.encode_utf16().count() <= 256);
        assert!(result.description.encode_utf16().count() <= 4096);
        assert!(result.fields.len() <= 25);
        let mut total =
            result.title.encode_utf16().count() + result.description.encode_utf16().count();
        for field in &result.fields {
            assert!(field.name.encode_utf16().count() <= 256);
            assert!(field.value.encode_utf16().count() <= 1024);
            total += field.name.encode_utf16().count() + field.value.encode_utf16().count();
        }
        assert_eq!(total, 6000);
        let many = LogEmbed {
            title: "x".into(),
            description: String::new(),
            fields: (0..30)
                .map(|_| Field {
                    name: "n".into(),
                    value: "v".into(),
                    inline: false,
                })
                .collect(),
            color: GREEN,
        }
        .bounded();
        assert_eq!(many.fields.len(), 25);
        assert_eq!(clip("🦀", 1), "…");
        assert_eq!(clip("x", 0), "");
        assert_eq!(clip("abc", 3), "abc");
    }
}
