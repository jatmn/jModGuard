//! Independent exact-text spam policy. Matching is trim-only equality.
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::Value;

pub const MODE: &str = "exact_text";
pub const DEFAULT_MIN_LENGTH: usize = 20;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub mode: String,
    pub window_secs: u64,
    pub min_channels: usize,
    pub min_length: usize,
    #[serde(default, skip_deserializing)]
    pub enforce: bool,
}

impl Policy {
    pub fn parse(rule: &crate::types::RuleDef) -> Result<Self> {
        let [mode, window_secs, min_channels, min_length] = match &rule.params {
            Value::Object(params) => {
                ensure!(
                    params.len() == 4,
                    "repeat-text parameters must contain exactly mode, window_secs, min_channels, and min_length"
                );
                let field = |key: &str| {
                    params
                        .get(key)
                        .with_context(|| format!("missing repeat-text parameter `{key}`"))
                };
                [
                    field("mode")?,
                    field("window_secs")?,
                    field("min_channels")?,
                    field("min_length")?,
                ]
            }
            // The previous struct decoder also accepted fields in declaration order.
            Value::Array(params) => {
                let [mode, window_secs, min_channels, min_length] = params.as_slice() else {
                    bail!("repeat-text parameter arrays must contain four elements");
                };
                [mode, window_secs, min_channels, min_length]
            }
            _ => bail!("repeat-text parameters must be an object or a four-element array"),
        };
        ensure!(
            mode.as_str() == Some(MODE),
            "repeat-text detection requires exact_text"
        );
        let window_secs = window_secs
            .as_u64()
            .filter(|secs| (1..=600).contains(secs))
            .context("text window must be 1–600 seconds")?;
        let min_channels = min_channels
            .as_u64()
            .filter(|channels| (2..=10).contains(channels))
            .context("text channel threshold must be 2–10")?;
        let min_length = min_length
            .as_u64()
            .filter(|length| (1..=2000).contains(length))
            .context("text minimum length must be 1–2000 Unicode scalars")?;
        if rule.dry_run {
            ensure!(
                rule.actions.iter().all(|a| a.action == "log"),
                "repeat-text test mode cannot configure punitive actions"
            );
        } else {
            let actions: std::collections::BTreeSet<_> =
                rule.actions.iter().map(|a| a.action.as_str()).collect();
            ensure!(
                actions.contains("delete") && actions.contains("ban"),
                "text enforcement requires delete and ban"
            );
            ensure!(
                actions
                    .iter()
                    .all(|a| matches!(*a, "delete" | "ban" | "log")),
                "text enforcement cannot use the generic punishment table"
            );
        }
        Ok(Self {
            mode: MODE.into(),
            window_secs,
            min_channels: min_channels as usize,
            min_length: min_length as usize,
            enforce: !rule.dry_run,
        })
    }
}

pub fn normalize(text: &str) -> String {
    text.trim().to_string()
}

pub fn long_enough(text: &str, minimum: usize) -> bool {
    !text.is_empty() && text.chars().count() >= minimum
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn trim_only_equality_and_minimum_length() {
        assert_eq!(normalize("  Hello  "), "Hello");
        assert_ne!(normalize("Hello"), "hello");
        assert_ne!(normalize("Hello  there"), "Hello there");
        assert!(long_enough(
            &normalize("abcdefghijklmnopqrst"),
            DEFAULT_MIN_LENGTH
        ));
        assert!(!long_enough(&normalize("ok"), DEFAULT_MIN_LENGTH));
        assert!(!long_enough(&normalize("   "), DEFAULT_MIN_LENGTH));
    }

    #[test]
    fn policy_defaults_are_test_mode() {
        let mut r = crate::defaults::default_rules()
            .into_iter()
            .find(|rule| rule.params.get("mode").and_then(|v| v.as_str()) == Some(MODE))
            .unwrap();
        let parsed = Policy::parse(&r).unwrap();
        assert!(!parsed.enforce);
        assert_eq!(parsed.min_length, DEFAULT_MIN_LENGTH);
        assert_eq!(parsed.min_channels, 3);
        r.dry_run = false;
        r.actions = vec![
            crate::types::ActionDef {
                at: 1,
                action: "delete".into(),
                duration_minutes: 0,
                delete_days: 0,
                purge_last_hour: false,
                reason: "text spam".into(),
            },
            crate::types::ActionDef {
                at: 1,
                action: "ban".into(),
                duration_minutes: 0,
                delete_days: 0,
                purge_last_hour: false,
                reason: "text spam".into(),
            },
        ];
        assert!(Policy::parse(&r).unwrap().enforce);
        r.actions[0].action = "timeout".into();
        assert!(Policy::parse(&r).is_err());
    }

    #[test]
    fn policy_checks_parameter_keys_types_and_arity() {
        let mut rule = crate::defaults::rule("repeat_text");
        rule.params = json!([MODE, 600, 2, 2000]);
        let positional = Policy::parse(&rule).unwrap();
        assert_eq!(positional.window_secs, 600);
        assert_eq!(positional.min_channels, 2);
        assert_eq!(positional.min_length, 2000);
        assert!(!positional.enforce);

        let deserialized: Policy = serde_json::from_str(r#"["exact_text",600,2,2000]"#).unwrap();
        assert_eq!(deserialized.min_length, positional.min_length);

        for params in [
            json!({"mode": MODE, "window_secs": 60, "min_channels": 3, "min_length": 20, "enforce": true}),
            json!({"mode": MODE, "window_secs": 60, "min_channels": 3}),
            json!({"mode": MODE, "window_secs": 60, "min_channels": 3, "min_length": 20, "extra": 0}),
            json!({"mode": MODE, "window_secs": 60, "min_channels": 3.0, "min_length": 20}),
            json!([MODE, 60, 3]),
            json!([MODE, 60, 3, 20, true]),
        ] {
            assert!(serde_json::from_str::<Policy>(&params.to_string()).is_err());
            rule.params = params;
            assert!(Policy::parse(&rule).is_err(), "accepted {}", rule.params);
        }
    }
}
