//! OCI release inspection, extraction and Bunny Storage publication shared by
//! the owner API and the role-specific service launcher.

pub mod oci;
pub mod storage;

use anyhow::{Context, Result, ensure};
use jmodguard::db::{self, ComponentRelease, Db};

/// Apply the same schema/counterpart gate at owner admission and again at the
/// launcher immediately before a switch. An unobserved service cannot prove
/// that the new binary and the retained rollback binary share an ABI.
pub async fn compatible(db: &Db, release: &ComponentRelease, now: i64) -> Result<()> {
    compatible_control(&db::UpdateControl::from_application(db), release, now).await
}

pub async fn compatible_control(
    db: &db::UpdateControl,
    release: &ComponentRelease,
    now: i64,
) -> Result<()> {
    release.validate()?;
    let schema = db.schema_version().await?;
    ensure!(
        schema >= release.min_schema && schema <= release.max_schema,
        "candidate cannot open current database schema"
    );
    let own = db.component(&release.component).await?;
    if release.component != "webui" {
        launcher_compatible(release, &release.base_abi)?;
        let current = own
            .observed
            .as_ref()
            .context("current service has not been observed")?;
        ensure!(
            current.update_protocol >= db::UPDATE_CONTROL_PROTOCOL && current.launcher_revision > 0,
            "service needs the launcher-aware baseline before automatic prerequisite updates"
        );
        ensure!(
            own.observed_at.is_some_and(|seen| now - seen < 7200),
            "current service observation is stale"
        );
        ensure!(
            current.base_abi == release.base_abi,
            "candidate needs a different base runtime ABI"
        );
        ensure!(
            release.result_schema >= current.min_schema
                && release.result_schema <= current.max_schema,
            "current rollback binary cannot open candidate schema"
        );
    }
    let observed_component = match release.component.as_str() {
        "bot" => "api",
        "api" => "bot",
        "webui" => "api",
        _ => return Err(anyhow::anyhow!("invalid component")),
    };
    let counterpart = db.component(observed_component).await?;
    let observed = counterpart
        .observed
        .context("counterpart release has not been observed")?;
    ensure!(
        counterpart
            .observed_at
            .is_some_and(|seen| now - seen < 7200),
        "counterpart observation is stale"
    );
    ensure!(
        schema >= observed.min_schema && schema <= observed.max_schema,
        "counterpart cannot use current schema"
    );
    ensure!(
        release.result_schema >= observed.min_schema
            && release.result_schema <= observed.max_schema,
        "counterpart cannot open candidate schema"
    );
    ensure!(
        observed.api_capability >= release.min_api && observed.api_capability <= release.max_api,
        "candidate requires a different API capability"
    );
    if release.component != "webui" {
        let current = own
            .observed
            .as_ref()
            .context("current service is unknown")?;
        // A guild migration is just as irreversible as a core migration. Legacy
        // artifacts cannot prove that their guild format matches the baseline.
        let guild = release
            .upgrade
            .as_ref()
            .context("candidate lacks guild schema metadata; use a coordinated baseline release")?
            .guild_schema;
        ensure!(
            current.guild_schema == Some(guild) && observed.guild_schema == Some(guild),
            "database schema changes require Update bot and API together"
        );
    }
    Ok(())
}

/// Launcher-only admission depends on the fixed handoff/control/ABI envelope,
/// not the bundled application's target schema (which may require migration).
pub fn launcher_compatible(release: &ComponentRelease, base_abi: &str) -> Result<()> {
    release.validate()?;
    ensure!(
        matches!(release.component.as_str(), "bot" | "api")
            && release.base_abi == base_abi
            && release
                .upgrade
                .as_ref()
                .is_some_and(|v| v.control_protocol == db::UPDATE_CONTROL_PROTOCOL)
            && release.launcher.is_some(),
        "launcher requires a different update baseline or base runtime"
    );
    Ok(())
}

/// Display advice only. Execution always checks its own compiled capability;
/// neither a heartbeat nor installed_launcher is authority to spawn a child.
pub fn launcher_status(row: &db::ComponentUpdate, now: i64) -> serde_json::Value {
    let Some(release) = &row.candidate else {
        return serde_json::json!({"state":"unknown","reason":"Check a release first."});
    };
    let Some(seen) = &row.observed else {
        return serde_json::json!({"state":"unknown","reason":"Running launcher has not been observed."});
    };
    if seen.update_protocol < db::UPDATE_CONTROL_PROTOCOL
        || seen.launcher_revision == 0
        || row
            .observed_at
            .is_none_or(|at| at > now || now - at >= 7200)
    {
        return serde_json::json!({"state":"unknown","reason":"Launcher capability is unknown or stale; verify the launcher-aware baseline."});
    }
    if let Err(error) = launcher_compatible(release, &seen.base_abi) {
        return serde_json::json!({"state":"blocked","reason":error.to_string()});
    }
    let contract = release.launcher.as_ref().expect("validated contract");
    let required = seen.launcher_revision < contract.min_revision;
    let newer = seen.launcher_revision < contract.revision;
    serde_json::json!({"state":if required { "required_first" } else if newer { "optional" } else { "satisfied" },
        "running":seen.launcher_revision,"minimum":contract.min_revision,"available":contract.revision,
        "installable":seen.self_update_enabled && newer && !required,
        "reason":if required { "Update will install the required launcher first, then the application." }
        else if newer { "Current launcher supports this application. A newer launcher is optional." }
        else { "Current launcher supports this application; no launcher update is required." }})
}
