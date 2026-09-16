//! Public installation topology checks. No Discord, Bunny, or AWS account is used.
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::state::Conductor;

pub const LICENSE_ID: &str = "LicenseRef-PolyForm-Noncommercial-1.0.0";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    Local,
    Bunny,
    Aws,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseKind {
    Sqlite,
    Libsql,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstallPlan {
    pub profile: Profile,
    pub public_origin: String,
    pub core_database: DatabaseKind,
    pub guild_database: DatabaseKind,
    #[serde(default)]
    pub core_url: String,
    #[serde(default)]
    pub guild_url: String,
    #[serde(default)]
    pub backup_root: Option<PathBuf>,
    #[serde(default)]
    pub evidence_root: Option<PathBuf>,
    #[serde(default)]
    pub restore_target: Option<PathBuf>,
    #[serde(default)]
    pub update_root: Option<PathBuf>,
    #[serde(default)]
    pub artifact_root: Option<PathBuf>,
    #[serde(default)]
    pub capture_core: Option<PathBuf>,
    #[serde(default)]
    pub shared_guild_store: Option<PathBuf>,
    #[serde(default)]
    pub dedicated_guild_store: Option<PathBuf>,
    #[serde(default)]
    pub api_embedded_ui: bool,
    #[serde(default)]
    pub frontend_storage: bool,
    #[serde(default)]
    pub private_ops_required: bool,
    #[serde(default)]
    pub license: String,
}

pub fn deferred_live_checks() -> Vec<&'static str> {
    vec![
        "Live Bunny Database TLS and production credentials",
        "Live Magic Container deploy, private-zone confirmation, and CDN origin-pull",
        "Live Discord Gateway, OAuth redirect, and guild permission checks",
        "Live Bunny evidence zone privacy and attachment capture",
        "Live AWS account, VPC, and billed resource provisioning",
        "Provider-billed cost at current Bunny/AWS list prices",
        "Multi-replica session stores and writer leases",
    ]
}

pub fn help_text() -> &'static str {
    "jmodguard-conductor [recover-update | init-local --root PATH | validate-install --plan FILE]\n\
     Run with no command to serve Administration. Public install docs: docs/INSTALL.md.\n"
}

pub fn current_report(conductor: &Conductor) -> Value {
    let cfg = &conductor.cfg;
    json!({
        "license": LICENSE_ID,
        "private_ops_required": false,
        "operations_available": conductor.operations_available(),
        "backup_available": conductor.backup_available(),
        "capture_available": conductor.capture_available(),
        "moves_available": crate::guild_move::moves_available(cfg),
        "discovery_available": crate::release::discovery_available(cfg),
        "update_available": crate::update::updates_available(cfg),
        "recover": crate::update::recover_status(cfg),
        "owner_setup": owner_setup_steps(),
        "docs": public_docs(),
        "deferred_live_checks": deferred_live_checks(),
        "acceptance": acceptance_matrix(),
        "note": "Local validation is not live Bunny, Discord, or AWS proof.",
    })
}

pub fn inferred_local_plan(cfg: &Config) -> InstallPlan {
    let capture_core = cfg
        .capture_stores
        .iter()
        .find(|store| store.role == "core")
        .map(|store| store.path.clone());
    let shared_guild_store = cfg
        .guild_stores
        .iter()
        .find(|store| store.role == "shared")
        .map(|store| store.path.clone());
    let dedicated_guild_store = cfg
        .guild_stores
        .iter()
        .find(|store| store.role == "dedicated")
        .map(|store| store.path.clone());
    InstallPlan {
        profile: Profile::Local,
        public_origin: if cfg.public_url.is_empty() {
            "http://127.0.0.1:8081".into()
        } else {
            cfg.public_url.clone()
        },
        core_database: DatabaseKind::Sqlite,
        guild_database: DatabaseKind::Sqlite,
        core_url: String::new(),
        guild_url: String::new(),
        backup_root: cfg.backup_root.clone(),
        evidence_root: cfg.evidence_root.clone(),
        restore_target: cfg.restore_target.clone(),
        update_root: cfg.update_root.clone(),
        artifact_root: cfg.artifact_root.clone(),
        capture_core,
        shared_guild_store,
        dedicated_guild_store,
        api_embedded_ui: true,
        frontend_storage: false,
        private_ops_required: false,
        license: LICENSE_ID.into(),
    }
}

pub fn validate_plan(plan: &InstallPlan) -> Result<Value> {
    ensure!(
        !plan.private_ops_required,
        "public installation does not require the private ops repository"
    );
    if !plan.license.is_empty() {
        ensure!(
            plan.license == LICENSE_ID,
            "license must remain {LICENSE_ID}"
        );
    }
    reject_unsupported_engine(&plan.core_url)?;
    reject_unsupported_engine(&plan.guild_url)?;
    reject_credentials(&plan.core_url, "core database URL")?;
    reject_credentials(&plan.guild_url, "guild database URL")?;
    match plan.profile {
        Profile::Local => validate_local(plan)?,
        Profile::Bunny => validate_bunny(plan)?,
        Profile::Aws => validate_aws(plan)?,
    }
    check_disjoint_roots(plan)?;
    if let Some(restore) = &plan.restore_target {
        ensure!(
            !restore.exists(),
            "restore target must stay absent until a restore job ({})",
            restore.display()
        );
    }
    if let (DatabaseKind::Libsql, DatabaseKind::Libsql) =
        (&plan.core_database, &plan.guild_database)
    {
        ensure!(
            !plan.core_url.is_empty() && !plan.guild_url.is_empty(),
            "libSQL core and guild URLs must both be set"
        );
        ensure!(
            plan.core_url.trim_end_matches('/') != plan.guild_url.trim_end_matches('/'),
            "core and guild databases must be distinct"
        );
    }
    Ok(json!({
        "ok": true,
        "profile": plan.profile,
        "license": LICENSE_ID,
        "private_ops_required": false,
        "deferred_live_checks": deferred_live_checks(),
        "docs": public_docs(),
    }))
}

pub fn init_local(root: &Path) -> Result<Value> {
    ensure!(root.is_absolute(), "install root must be an absolute path");
    fs::create_dir_all(root).with_context(|| format!("create {}", root.display()))?;
    let backup = root.join("backup");
    let evidence = root.join("evidence");
    let update = root.join("update");
    let artifacts = root.join("artifacts");
    let capture = root.join("capture");
    let guild = root.join("guild");
    let restore = root.join("restore");
    for dir in [&backup, &evidence, &update, &artifacts, &capture, &guild] {
        fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
    }
    ensure!(
        !restore.exists(),
        "restore target must remain absent: {}",
        restore.display()
    );
    let plan = InstallPlan {
        profile: Profile::Local,
        public_origin: "http://127.0.0.1:8081".into(),
        core_database: DatabaseKind::Sqlite,
        guild_database: DatabaseKind::Sqlite,
        core_url: String::new(),
        guild_url: String::new(),
        backup_root: Some(backup.clone()),
        evidence_root: Some(evidence.clone()),
        restore_target: Some(restore.clone()),
        update_root: Some(update.clone()),
        artifact_root: Some(artifacts.clone()),
        capture_core: Some(capture.join("core.db")),
        shared_guild_store: Some(guild.join("shared.db")),
        dedicated_guild_store: Some(guild.join("dedicated.db")),
        api_embedded_ui: true,
        frontend_storage: false,
        private_ops_required: false,
        license: LICENSE_ID.into(),
    };
    let report = validate_plan(&plan)?;
    fs::write(
        root.join("plan.json"),
        serde_json::to_string_pretty(&plan).context("serialize plan")?,
    )?;
    write_env_examples(root, &plan)?;
    fs::write(root.join("NEXT-STEPS.txt"), next_steps(root))?;
    Ok(json!({
        "ok": true,
        "root": root,
        "plan": plan,
        "validation": report,
        "restore_target": restore,
        "private_ops_required": false,
    }))
}

pub fn validate_plan_file(path: &Path) -> Result<Value> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let plan: InstallPlan = serde_json::from_str(&text).context("install plan JSON")?;
    validate_plan(&plan)
}

fn public_docs() -> Value {
    json!({
        "install": "docs/INSTALL.md",
        "local": "docs/SETUP.md",
        "bunny": "docs/DEPLOY-BUNNY.md",
        "aws": "docs/DEPLOY-AWS.md",
        "cost": "docs/COST.md",
        "backup": "docs/BACKUP-RESTORE.md",
        "updates": "docs/UPDATES.md",
        "license": "LICENSE.md",
    })
}

fn owner_setup_steps() -> Vec<&'static str> {
    vec![
        "Create a fresh jmodguard-core-1 database or empty local SQLite file",
        "Start jmodguard-bot first so migrations and defaults finish",
        "Start jmodguard-api with OAuth credentials and the public origin",
        "Configure conductor control store, owner identity, and conductor-only OAuth",
        "Keep backup, evidence, restore, update, and capture paths disjoint",
        "Record the backup key before the first portable export",
        "Practice restore into an absent restore target, then recover-update independently",
    ]
}

fn acceptance_matrix() -> Value {
    json!({
        "isolation": "crates/conductor/tests/process.rs and control.rs",
        "updates": "crates/conductor/tests/updates.rs",
        "moves": "crates/conductor/tests/moves.rs",
        "backup_recovery": "crates/conductor/tests/backup.rs",
        "public_install": "crates/conductor/tests/install.rs",
        "dual_process": "crates/integration-tests/tests/dual_process_smoke.rs",
    })
}

fn validate_local(plan: &InstallPlan) -> Result<()> {
    let origin = parse_origin(&plan.public_origin)?;
    ensure!(
        origin.scheme() == "http" || origin.scheme() == "https",
        "local origin must be http(s)"
    );
    if origin.scheme() == "http" {
        ensure!(
            is_loopback(&origin),
            "http local origin must be loopback; use https for a public host"
        );
    }
    ensure!(
        plan.core_database == DatabaseKind::Sqlite || plan.core_database == DatabaseKind::Libsql,
        "local core database must be sqlite or libsql"
    );
    ensure!(
        plan.api_embedded_ui,
        "local topology uses the API embedded UI"
    );
    ensure!(
        !plan.frontend_storage,
        "local topology does not use Bunny Storage for the admin UI"
    );
    Ok(())
}

fn validate_bunny(plan: &InstallPlan) -> Result<()> {
    let origin = parse_origin(&plan.public_origin)?;
    ensure!(
        origin.scheme() == "https",
        "Bunny public origin must be https"
    );
    ensure!(
        !is_loopback(&origin),
        "Bunny public origin must not be localhost"
    );
    ensure!(
        plan.core_database == DatabaseKind::Libsql && plan.guild_database == DatabaseKind::Libsql,
        "Bunny topology uses distinct remote libSQL databases"
    );
    ensure!(
        !plan.api_embedded_ui,
        "Bunny API containers do not embed the admin UI"
    );
    ensure!(
        plan.frontend_storage,
        "Bunny admin UI is published to Storage/CDN from public artifacts"
    );
    Ok(())
}

fn validate_aws(plan: &InstallPlan) -> Result<()> {
    let origin = parse_origin(&plan.public_origin)?;
    if origin.scheme() == "http" {
        ensure!(
            is_loopback(&origin),
            "AWS public origin must be https except for loopback testing"
        );
    } else {
        ensure!(
            origin.scheme() == "https",
            "AWS public origin must be https"
        );
    }
    ensure!(
        matches!(
            (&plan.core_database, &plan.guild_database),
            (DatabaseKind::Sqlite, DatabaseKind::Sqlite)
                | (DatabaseKind::Libsql, DatabaseKind::Libsql)
                | (DatabaseKind::Sqlite, DatabaseKind::Libsql)
                | (DatabaseKind::Libsql, DatabaseKind::Sqlite)
        ),
        "AWS path uses local SQLite or compatible libSQL, not RDS/Postgres"
    );
    Ok(())
}

fn check_disjoint_roots(plan: &InstallPlan) -> Result<()> {
    let mut labeled: Vec<(&str, &Path)> = Vec::new();
    for (label, path) in [
        ("backup_root", plan.backup_root.as_deref()),
        ("evidence_root", plan.evidence_root.as_deref()),
        ("restore_target", plan.restore_target.as_deref()),
        ("update_root", plan.update_root.as_deref()),
        ("artifact_root", plan.artifact_root.as_deref()),
        ("capture_core", plan.capture_core.as_deref()),
        ("shared_guild_store", plan.shared_guild_store.as_deref()),
        (
            "dedicated_guild_store",
            plan.dedicated_guild_store.as_deref(),
        ),
    ] {
        if let Some(path) = path {
            labeled.push((label, path));
        }
    }
    for i in 0..labeled.len() {
        for j in i + 1..labeled.len() {
            let (left_name, left) = labeled[i];
            let (right_name, right) = labeled[j];
            ensure!(
                left != right && !left.starts_with(right) && !right.starts_with(left),
                "{left_name} must not overlap {right_name}"
            );
        }
    }
    Ok(())
}

fn parse_origin(raw: &str) -> Result<url::Url> {
    let url = url::Url::parse(raw).context("public origin must be an absolute URL")?;
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "public origin must not contain credentials"
    );
    ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "public origin must not contain a query or fragment"
    );
    ensure!(
        url.path() == "/" || url.path().is_empty(),
        "public origin must not include a path"
    );
    Ok(url)
}

fn is_loopback(url: &url::Url) -> bool {
    matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
}

fn reject_unsupported_engine(raw: &str) -> Result<()> {
    let lower = raw.to_ascii_lowercase();
    for needle in [
        "postgres://",
        "postgresql://",
        "mysql://",
        "mariadb://",
        "rds.amazonaws.com",
        "aurora",
    ] {
        ensure!(
            !lower.contains(needle),
            "unsupported database engine in URL; use SQLite or compatible libSQL"
        );
    }
    Ok(())
}

fn reject_credentials(raw: &str, label: &str) -> Result<()> {
    if raw.trim().is_empty() {
        return Ok(());
    }
    let url = url::Url::parse(raw).with_context(|| format!("{label} must be an absolute URL"))?;
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "{label} must not embed credentials"
    );
    if let Some(query) = url.query() {
        let lower = query.to_ascii_lowercase();
        ensure!(
            !lower.contains("token=")
                && !lower.contains("auth=")
                && !lower.contains("password=")
                && !lower.contains("secret="),
            "{label} must not carry secrets in the query"
        );
    }
    Ok(())
}

fn write_env_examples(root: &Path, plan: &InstallPlan) -> Result<()> {
    let backup = display_opt(&plan.backup_root);
    let evidence = display_opt(&plan.evidence_root);
    let restore = display_opt(&plan.restore_target);
    let update = display_opt(&plan.update_root);
    let artifacts = display_opt(&plan.artifact_root);
    let capture = display_opt(&plan.capture_core);
    let shared = display_opt(&plan.shared_guild_store);
    let dedicated = display_opt(&plan.dedicated_guild_store);
    fs::write(
        root.join("conductor.env.example"),
        format!(
            "# Fill locally. Never commit secrets.\n\
             JMODGUARD_CONDUCTOR_BIND=127.0.0.1:8081\n\
             JMODGUARD_CONDUCTOR_DB={root}/conductor.db\n\
             JMODGUARD_CONDUCTOR_PUBLIC_URL=http://127.0.0.1:8081\n\
             JMODGUARD_CONDUCTOR_OWNER_ID=\n\
             JMODGUARD_CONDUCTOR_OAUTH_CLIENT_ID=\n\
             JMODGUARD_CONDUCTOR_OAUTH_CLIENT_SECRET=\n\
             JMODGUARD_CONDUCTOR_BOT_HEALTH_URL=http://127.0.0.1:8080/health\n\
             JMODGUARD_CONDUCTOR_API_HEALTH_URL=http://127.0.0.1:8080/health\n\
             JMODGUARD_CONDUCTOR_BACKUP_ROOT={backup}\n\
             JMODGUARD_CONDUCTOR_EVIDENCE_ROOT={evidence}\n\
             JMODGUARD_CONDUCTOR_RESTORE_TARGET={restore}\n\
             JMODGUARD_CONDUCTOR_UPDATE_ROOT={update}\n\
             JMODGUARD_CONDUCTOR_ARTIFACT_ROOT={artifacts}\n\
             JMODGUARD_CONDUCTOR_CAPTURE_STORES=[{{ \"role\": \"core\", \"path\": \"{capture}\" }}]\n\
             JMODGUARD_CONDUCTOR_GUILD_STORES=[{{ \"id\": \"shared\", \"role\": \"shared\", \"path\": \"{shared}\" }},{{ \"id\": \"dedicated\", \"role\": \"dedicated\", \"path\": \"{dedicated}\" }}]\n",
            root = root.display()
        ),
    )?;
    fs::write(
        root.join("bot.env.example"),
        format!(
            "# Fill locally. Never commit secrets. Start the bot before the API.\n\
             JMODGUARD_DISCORD_TOKEN=\n\
             JMODGUARD_DB_PATH={capture}\n\
             JMODGUARD_HEALTH_BIND=127.0.0.1:18080\n"
        ),
    )?;
    fs::write(
        root.join("api.env.example"),
        format!(
            "# Fill locally. Never commit secrets. API has no bot token.\n\
             JMODGUARD_DB_PATH={capture}\n\
             JMODGUARD_WEB_BIND=127.0.0.1:8080\n\
             JMODGUARD_WEB_BASE_URL=http://127.0.0.1:8080\n\
             JMODGUARD_OAUTH_CLIENT_ID=\n\
             JMODGUARD_OAUTH_CLIENT_SECRET=\n\
             JMODGUARD_CONDUCTOR_URL=http://127.0.0.1:8081\n"
        ),
    )?;
    Ok(())
}

fn display_opt(path: &Option<PathBuf>) -> String {
    path.as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_default()
}

fn next_steps(root: &Path) -> String {
    format!(
        "Public local install created at {}.\n\n\
         1. Copy the *.env.example files, fill secrets locally, and never git-add them.\n\
         2. Follow docs/INSTALL.md. Start jmodguard-bot, then jmodguard-api, then this conductor.\n\
         3. Owner login stamps conductor operator identity. Guild Discord credentials stay out.\n\
         4. Record the backup key before the first portable export (docs/BACKUP-RESTORE.md).\n\
         5. Restore only into the absent restore/ directory. After conductor self-replacement,\n\
            run `jmodguard-conductor recover-update` independently of OAuth and private ops.\n\
         6. Optional GitHub Actions in a private ops repo can lock Bunny Storage publishes.\n\
            They are not required. Buildkite already publishes public bot/API/conductor/frontend artifacts.\n\
         7. Live Bunny, Discord, and AWS provider checks remain deferred; see docs/INSTALL.md.\n\
         License: {LICENSE_ID} (LICENSE.md). Do not promise RDS/Postgres.\n",
        root.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "jmodguard-install-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn local_plan(root: &Path) -> InstallPlan {
        InstallPlan {
            profile: Profile::Local,
            public_origin: "http://127.0.0.1:8081".into(),
            core_database: DatabaseKind::Sqlite,
            guild_database: DatabaseKind::Sqlite,
            core_url: String::new(),
            guild_url: String::new(),
            backup_root: Some(root.join("backup")),
            evidence_root: Some(root.join("evidence")),
            restore_target: Some(root.join("restore")),
            update_root: Some(root.join("update")),
            artifact_root: Some(root.join("artifacts")),
            capture_core: Some(root.join("capture/core.db")),
            shared_guild_store: Some(root.join("guild/shared.db")),
            dedicated_guild_store: Some(root.join("guild/dedicated.db")),
            api_embedded_ui: true,
            frontend_storage: false,
            private_ops_required: false,
            license: LICENSE_ID.into(),
        }
    }

    #[test]
    fn local_plan_accepts_loopback_sqlite() {
        let root = scratch("local");
        let report = validate_plan(&local_plan(&root)).unwrap();
        assert_eq!(report["ok"], true);
        assert_eq!(report["private_ops_required"], false);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bunny_http_origin_is_rejected() {
        let root = scratch("bunny-http");
        let mut plan = local_plan(&root);
        plan.profile = Profile::Bunny;
        plan.public_origin = "http://cdn.example".into();
        plan.core_database = DatabaseKind::Libsql;
        plan.guild_database = DatabaseKind::Libsql;
        plan.core_url = "libsql://core.lite.bunnydb.net".into();
        plan.guild_url = "libsql://guild.lite.bunnydb.net".into();
        plan.api_embedded_ui = false;
        plan.frontend_storage = true;
        assert!(
            validate_plan(&plan)
                .unwrap_err()
                .to_string()
                .contains("https")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bunny_plan_accepts_distinct_libsql_and_storage() {
        let root = scratch("bunny-ok");
        let mut plan = local_plan(&root);
        plan.profile = Profile::Bunny;
        plan.public_origin = "https://admin.example".into();
        plan.core_database = DatabaseKind::Libsql;
        plan.guild_database = DatabaseKind::Libsql;
        plan.core_url = "libsql://core.lite.bunnydb.net".into();
        plan.guild_url = "libsql://guild.lite.bunnydb.net".into();
        plan.api_embedded_ui = false;
        plan.frontend_storage = true;
        assert_eq!(validate_plan(&plan).unwrap()["ok"], true);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn overlapping_roots_are_rejected() {
        let root = scratch("overlap");
        let mut plan = local_plan(&root);
        plan.evidence_root = plan.backup_root.clone();
        assert!(
            validate_plan(&plan)
                .unwrap_err()
                .to_string()
                .contains("must not overlap")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn postgres_and_rds_urls_are_rejected() {
        let root = scratch("rds");
        let mut plan = local_plan(&root);
        plan.profile = Profile::Aws;
        plan.public_origin = "https://admin.example".into();
        plan.core_url = "postgres://db.rds.amazonaws.com/jmodguard".into();
        assert!(
            validate_plan(&plan)
                .unwrap_err()
                .to_string()
                .contains("unsupported database engine")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn private_ops_required_is_rejected() {
        let root = scratch("ops");
        let mut plan = local_plan(&root);
        plan.private_ops_required = true;
        assert!(
            validate_plan(&plan)
                .unwrap_err()
                .to_string()
                .contains("does not require the private ops")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn credentials_in_urls_are_rejected() {
        let root = scratch("creds");
        let mut plan = local_plan(&root);
        plan.profile = Profile::Bunny;
        plan.public_origin = "https://admin.example".into();
        plan.core_database = DatabaseKind::Libsql;
        plan.guild_database = DatabaseKind::Libsql;
        plan.core_url = "libsql://user:secret@core.example".into();
        plan.guild_url = "libsql://guild.example".into();
        plan.api_embedded_ui = false;
        plan.frontend_storage = true;
        assert!(
            validate_plan(&plan)
                .unwrap_err()
                .to_string()
                .contains("must not embed credentials")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn init_local_writes_public_layout_without_restore_dir() {
        let root = scratch("init");
        let report = init_local(&root).unwrap();
        assert_eq!(report["ok"], true);
        assert!(root.join("plan.json").is_file());
        assert!(root.join("backup").is_dir());
        assert!(root.join("NEXT-STEPS.txt").is_file());
        assert!(!root.join("restore").exists());
        let steps = fs::read_to_string(root.join("NEXT-STEPS.txt")).unwrap();
        assert!(steps.contains("private ops"));
        assert!(steps.contains("not required"));
        let env = fs::read_to_string(root.join("conductor.env.example")).unwrap();
        assert!(!env.to_ascii_lowercase().contains("sk-"));
        assert!(env.contains("JMODGUARD_CONDUCTOR_OAUTH_CLIENT_SECRET="));
        validate_plan_file(&root.join("plan.json")).unwrap();
        let _ = fs::remove_dir_all(root);
    }
}
