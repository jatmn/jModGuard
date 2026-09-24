//! Database startup shared by the API executable and the offline process probe.
//!
//! The probe does not bind HTTP or call Discord. It opens with the API's
//! non-migrating options and commits a settings row the bot process can read.

use crate::config::ApiProcessConfig;
use crate::db::{self, Db};
use crate::web::state::ApiState;
use anyhow::{Context as _, Result};
use std::path::PathBuf;

pub async fn open_database(cfg: &ApiProcessConfig) -> Result<Db> {
    let opts = cfg.db_open_options();
    let (attempts, delay) = db::startup_open_retry(&opts);
    Db::open_with_until_ready(opts, attempts, delay).await
}

/// Open the shared database without migrating, then commit one prefix change.
pub async fn write_shared_config(cfg: ApiProcessConfig) -> Result<()> {
    let guild_id: i64 = std::env::var("JMODGUARD_PROBE_GUILD")
        .context("JMODGUARD_PROBE_GUILD")?
        .parse()
        .context("JMODGUARD_PROBE_GUILD")?;
    let prefix = std::env::var("JMODGUARD_PROBE_PREFIX").context("JMODGUARD_PROBE_PREFIX")?;
    let dir = PathBuf::from(std::env::var("JMODGUARD_PROBE_DIR").context("JMODGUARD_PROBE_DIR")?);
    let migrate = cfg.db_open_options().migrate;
    std::fs::write(
        dir.join("api-migrate"),
        if migrate { "true" } else { "false" },
    )?;
    let db = open_database(&cfg).await?;
    let app = ApiState::new(cfg, db)?;
    db::patch_settings(&app.db, guild_id, Some(Some(prefix)), None, None).await?;
    let seen = db::get_settings(&app.db, guild_id).await?;
    std::fs::write(dir.join("api-wrote"), &seen.prefix)?;
    Ok(())
}
