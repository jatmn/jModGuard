use super::*;
use crate::custom_rules::MAX_CUSTOM_RULES_PER_GUILD;
use crate::types::{ActionDef, RuleDef};
use serde_json::json;
use std::path::PathBuf;

struct Fixture {
    db: Db,
    path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let path = tmp_db_path("custom_rules");
        Self {
            db: Db::open(&path, 2).unwrap(),
            path,
        }
    }
    async fn scope(&self, guild: i64) -> ConfigurationScope {
        ConfigurationScope::Guild(enroll_guild(&self.db, guild, 100).await.unwrap().unwrap())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        cleanup_db(&self.path);
    }
}

fn custom_rule(dry_run: bool, pattern: &str) -> RuleDef {
    RuleDef {
        name: "custom".into(),
        description: String::new(),
        enabled: true,
        dry_run,
        params: json!({ "kind": "pattern", "pattern": pattern }),
        actions: vec![ActionDef {
            at: 1,
            action: "delete".into(),
            duration_minutes: 0,
            delete_days: 0,
            reason: "match".into(),
        }],
        whitelist_roles: vec![],
        whitelist_channels: vec![],
        whitelist_users: vec![],
        skip_moderators: false,
        include_channels: vec![],
        log_automod: true,
        open_case: true,
    }
}

fn custom_id(n: usize) -> String {
    format!("custom:{:016x}", n as u128)
}

fn write_scope(request: &ConfigurationScope) -> ConfigurationWriteScope {
    ConfigurationWriteScope::new(request.clone(), request.clone()).unwrap()
}

#[tokio::test]
async fn custom_rule_cap_dry_run_create_and_updates() {
    let f = Fixture::new();
    let request = f.scope(10).await;
    let scope = write_scope(&request);

    let err = upsert_enrolled_rule(
        &f.db,
        scope.clone(),
        "custom:0123456789abcdef",
        &custom_rule(false, "scam"),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("dry-run"));
    assert!(
        get_rule(&f.db, 10, "custom:0123456789abcdef")
            .await
            .unwrap()
            .is_none()
    );
    upsert_enrolled_rule(
        &f.db,
        scope.clone(),
        "custom:0123456789abcdef",
        &custom_rule(true, "scam"),
    )
    .await
    .unwrap();
    upsert_enrolled_rule(
        &f.db,
        scope.clone(),
        "custom:0123456789abcdef",
        &custom_rule(false, "scam"),
    )
    .await
    .unwrap();
    assert!(
        !get_rule(&f.db, 10, "custom:0123456789abcdef")
            .await
            .unwrap()
            .unwrap()
            .dry_run
    );

    delete_enrolled_rule(&f.db, scope.clone(), "custom:0123456789abcdef")
        .await
        .unwrap();
    for i in 0..MAX_CUSTOM_RULES_PER_GUILD {
        let id = custom_id(i);
        upsert_enrolled_rule(&f.db, scope.clone(), &id, &custom_rule(true, "scam"))
            .await
            .unwrap();
    }
    let overflow = custom_id(MAX_CUSTOM_RULES_PER_GUILD);
    let err = upsert_enrolled_rule(&f.db, scope.clone(), &overflow, &custom_rule(true, "scam"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("custom rules"));

    let existing = custom_id(0);
    upsert_enrolled_rule(&f.db, scope.clone(), &existing, &custom_rule(true, "scam"))
        .await
        .unwrap();
}

#[tokio::test]
async fn custom_rule_validation_rejects_bad_patterns_and_ban_days() {
    let f = Fixture::new();
    let scope = write_scope(&f.scope(10).await);
    let long = "a".repeat(513);
    let err = upsert_enrolled_rule(
        &f.db,
        scope.clone(),
        "custom:0123456789abcdef",
        &custom_rule(true, &long),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("pattern") || err.to_string().contains("long"));

    for pattern in ["(?=a)", "(?<=a)", r"(\w)\1"] {
        let err = upsert_enrolled_rule(
            &f.db,
            scope.clone(),
            "custom:0123456789abcdef",
            &custom_rule(true, pattern),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("regex") || err.to_string().contains("invalid"));
    }

    let mut ban = custom_rule(true, "scam");
    ban.actions[0].action = "ban".into();
    ban.actions[0].delete_days = 8;
    let err = upsert_enrolled_rule(&f.db, scope.clone(), "custom:0123456789abcdef", &ban)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("delete_days"));
}

#[tokio::test]
async fn custom_rules_reject_global_scope() {
    let f = Fixture::new();
    let scope =
        ConfigurationWriteScope::new(ConfigurationScope::Global, ConfigurationScope::Global)
            .unwrap();
    let err = upsert_enrolled_rule(
        &f.db,
        scope,
        "custom:0123456789abcdef",
        &custom_rule(true, "scam"),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("global"));
}

#[tokio::test]
async fn apply_control_rejects_live_create_without_row() {
    let f = Fixture::new();
    placed_before_erasure(&f.db, 10).await;
    let enrollment = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let id = "custom:0123456789abcdef".to_string();
    let rule = custom_rule(false, "scam");
    let wire = ScopeWire {
        global: false,
        guild_id: 10,
        enrollment_id: enrollment.id.clone(),
        joined_at_ns: enrollment.joined_at_ns,
    };
    let op = ControlOp::UpsertRule {
        request: wire.clone(),
        target: wire,
        id: id.clone(),
        rule,
    };
    let err = apply_control(&f.db, None, op, None).await.unwrap_err();
    assert!(err.to_string().contains("dry-run"));
    assert!(get_rule(&f.db, 10, &id).await.unwrap().is_none());
}
