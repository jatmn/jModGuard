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
    release.validate()?;
    let schema = db::core_schema_version(db).await?;
    ensure!(
        schema >= release.min_schema && schema <= release.max_schema,
        "candidate cannot open current database schema"
    );
    let own = db::component_update(db, &release.component).await?;
    if release.component != "webui" {
        let current = own
            .observed
            .as_ref()
            .context("current service has not been observed")?;
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
    let counterpart = db::component_update(db, observed_component).await?;
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
    Ok(())
}
