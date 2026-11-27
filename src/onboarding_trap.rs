//! Configuration shared by the onboarding trap's writer and Gateway matcher.
use crate::types::RuleDef;
use anyhow::{Context, Result, ensure};

pub const RULE_ID: &str = "onboarding_bot_trap";
pub const DEFAULT_JOIN_WINDOW: u32 = 30;
const MAX_JOIN_WINDOW: u64 = 1440;
const MAX_CLEANUP_WINDOW: u64 = 60;
const MAX_REMOVAL_ROLES: usize = 5;

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

pub fn removal_role_ids(guild_id: u64, rule: &RuleDef) -> Option<Vec<u64>> {
    let Some(value) = rule.params.get("remove_role_ids") else {
        return Some(Vec::new());
    };
    let values = value.as_array()?;
    if values.len() > MAX_REMOVAL_ROLES || (guild_id == 0 && !values.is_empty()) {
        return None;
    }
    let trap_role = role_id(rule);
    let mut ids = Vec::with_capacity(values.len());
    for value in values {
        let id = value.as_str()?.parse::<u64>().ok()?;
        if id == 0 || id == guild_id || Some(id) == trap_role || ids.contains(&id) {
            return None;
        }
        ids.push(id);
    }
    Some(ids)
}

/// Manage Server controls the saved removal list and who it can affect in live mode.
/// An unchanged policy can be retained during ordinary edits or switched off.
pub fn role_removal_requires_manage(guild_id: u64, current: &RuleDef, next: &RuleDef) -> bool {
    let roles = removal_role_ids(guild_id, next);
    removal_role_ids(guild_id, current) != roles
        || (next.enabled
            && !next.dry_run
            && roles.is_some_and(|ids| !ids.is_empty())
            && (!current.enabled
                || current.dry_run
                || role_id(current) != role_id(next)
                || join_window_minutes(current) != join_window_minutes(next)
                || current.whitelist_roles != next.whitelist_roles
                || current.whitelist_users != next.whitelist_users
                || current.whitelist_channels.contains(&0) != next.whitelist_channels.contains(&0)
                || !current.skip_moderators
                || !current.log_automod
                || !current.open_case
                || !has_kick_action(current)))
}

fn has_kick_action(rule: &RuleDef) -> bool {
    matches!(rule.actions.as_slice(), [action] if action.action == "kick" && action.at == 1)
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
        removal_role_ids(u64::try_from(guild_id).unwrap_or_default(), rule).is_some(),
        "Choose up to five distinct server roles other than @everyone and the trap role to remove"
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
        has_kick_action(rule),
        "The onboarding trap requires one kick action"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn removal_roles_are_opt_in_and_use_distinct_guild_role_strings() {
        let mut rule = crate::defaults::rule(RULE_ID);
        assert_eq!(removal_role_ids(10, &rule), Some(vec![]));
        rule.params["role_id"] = json!("20");
        rule.params["remove_role_ids"] = json!(["22", "21"]);
        assert_eq!(removal_role_ids(10, &rule), Some(vec![22, 21]));
        assert!(validate(10, &rule).is_ok());
        assert_eq!(removal_role_ids(0, &rule), None);
        for invalid in [
            json!(null),
            json!([22]),
            json!(["0"]),
            json!(["10"]),
            json!(["20"]),
            json!(["22", "22"]),
            json!(["21", "22", "23", "24", "25", "26"]),
        ] {
            rule.params["remove_role_ids"] = invalid;
            assert!(validate(10, &rule).is_err());
        }
    }

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
