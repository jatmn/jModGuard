//! Independent exact-text spam policy. Matching is trim-only equality.
use anyhow::{Result, ensure};
use serde::Deserialize;

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
        let mut value: Self = serde_json::from_value(rule.params.clone())?;
        ensure!(
            value.mode == MODE,
            "repeat-text detection requires exact_text"
        );
        ensure!(
            (1..=600).contains(&value.window_secs),
            "text window must be 1–600 seconds"
        );
        ensure!(
            (2..=10).contains(&value.min_channels),
            "text channel threshold must be 2–10"
        );
        ensure!(
            (1..=2000).contains(&value.min_length),
            "text minimum length must be 1–2000 Unicode scalars"
        );
        if rule.dry_run {
            ensure!(
                rule.actions.iter().all(|a| a.action == "log"),
                "repeat-text test mode cannot configure punitive actions"
            );
            value.enforce = false;
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
            value.enforce = true;
        }
        Ok(value)
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
                reason: "text spam".into(),
            },
            crate::types::ActionDef {
                at: 1,
                action: "ban".into(),
                duration_minutes: 0,
                delete_days: 0,
                reason: "text spam".into(),
            },
        ];
        assert!(Policy::parse(&r).unwrap().enforce);
        r.actions[0].action = "timeout".into();
        assert!(Policy::parse(&r).is_err());
    }
}
