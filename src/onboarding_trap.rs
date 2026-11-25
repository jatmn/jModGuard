//! Configuration shared by the onboarding trap's writer and Gateway matcher.
use crate::types::RuleDef;
use anyhow::{Context, Result, ensure};

pub const RULE_ID: &str = "onboarding_bot_trap";
pub const DEFAULT_JOIN_WINDOW: u32 = 30;
const MAX_JOIN_WINDOW: u64 = 1440;
const MAX_CLEANUP_WINDOW: u64 = 60;

pub fn role_id(rule: &RuleDef) -> Option<u64> {
    let value = rule.params.get("role_id")?;
    value
        .as_u64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|id| *id != 0)
}

/// Keep legacy numeric snowflakes exact before JavaScript sees them.
pub(crate) fn normalize_role_id(rule: &mut RuleDef) {
    if let Some(id) = role_id(rule) {
        rule.params["role_id"] = id.to_string().into();
    }
}

pub fn join_window_minutes(rule: &RuleDef) -> Option<u32> {
    let minutes = match rule.params.get("join_window_minutes") {
        Some(value) => value.as_u64()?,
        None => u64::from(DEFAULT_JOIN_WINDOW),
    };
    (minutes <= MAX_JOIN_WINDOW).then_some(minutes as u32)
}

pub fn cleanup_window_minutes(rule: &RuleDef) -> Option<u32> {
    let minutes = match rule.params.get("cleanup_window_minutes") {
        Some(value) => value.as_u64()?,
        None => 0,
    };
    (minutes <= MAX_CLEANUP_WINDOW).then_some(minutes as u32)
}

pub(crate) fn validate(guild_id: i64, rule: &RuleDef) -> Result<()> {
    let role = role_id(rule);
    if let Some(value) = rule.params.get("role_id")
        && !value.is_null()
    {
        let role = role.context("Choose a valid onboarding trap role")?;
        ensure!(
            guild_id > 0 && role != guild_id as u64,
            "Choose a server role other than @everyone for the onboarding trap"
        );
    }
    ensure!(
        !rule.enabled || role.is_some(),
        "Choose a role before enabling the onboarding trap"
    );
    ensure!(
        join_window_minutes(rule).is_some(),
        "The onboarding trap join window must be 0–1440 minutes"
    );
    ensure!(
        cleanup_window_minutes(rule).is_some(),
        "The onboarding trap cleanup window must be 0–60 minutes"
    );
    ensure!(
        rule.skip_moderators,
        "The onboarding trap must skip moderators"
    );
    ensure!(
        rule.log_automod && rule.open_case,
        "The onboarding trap must log matches and open cases"
    );
    ensure!(
        matches!(rule.actions.as_slice(), [action] if action.action == "kick" && action.at == 1),
        "The onboarding trap requires one kick action"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shipped_trap_is_off_and_enabled_traps_require_guild_role_and_safety_settings() {
        let mut rule = crate::types::RULE_IDS
            .into_iter()
            .zip(crate::defaults::default_rules())
            .find(|(id, _)| *id == RULE_ID)
            .unwrap()
            .1;
        assert!(!rule.enabled);
        assert!(rule.dry_run && rule.skip_moderators && rule.log_automod && rule.open_case);
        assert_eq!(join_window_minutes(&rule), Some(30));
        assert_eq!(cleanup_window_minutes(&rule), Some(0));
        assert_eq!(
            rule.actions[0].reason,
            "[Automod] Kicked for suspected bot (onboarding)"
        );
        validate(0, &rule).unwrap();

        rule.enabled = true;
        assert!(validate(10, &rule).is_err());
        rule.params["role_id"] = json!("10");
        assert!(validate(10, &rule).is_err(), "@everyone");
        rule.params["role_id"] = json!("20");
        assert!(validate(0, &rule).is_err(), "global");
        validate(10, &rule).unwrap();

        rule.params["join_window_minutes"] = json!(0);
        validate(10, &rule).unwrap();
        rule.params["join_window_minutes"] = json!(1441);
        assert!(validate(10, &rule).is_err());
        rule.params["join_window_minutes"] = json!(30);
        rule.params["cleanup_window_minutes"] = json!(60);
        validate(10, &rule).unwrap();
        for invalid in [json!(61), json!(-1), json!("20"), json!(null)] {
            rule.params["cleanup_window_minutes"] = invalid;
            assert!(validate(10, &rule).is_err());
        }
        rule.params["cleanup_window_minutes"] = json!(0);
        rule.skip_moderators = false;
        assert!(validate(10, &rule).is_err());
        rule.skip_moderators = true;
        rule.actions.push(rule.actions[0].clone());
        assert!(validate(10, &rule).is_err());
    }
}
