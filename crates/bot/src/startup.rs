//! Database startup shared by the bot executable and the offline process probe.
//!
//! The probe stops before the Discord gateway. It still uses this open path and
//! `BotState::poll_config_version`, which is what the running bot calls after a
//! cross-process `config_version` bump.

use crate::config::BotProcessConfig;
use crate::db::{self, Db};
use crate::state::BotState;
use anyhow::{Context as _, Result, bail};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub async fn open_database(cfg: &BotProcessConfig) -> Result<Db> {
    let opts = cfg.db_open_options();
    let (attempts, delay) = db::startup_open_retry(&opts);
    Db::open_with_until_ready(opts, attempts, delay).await
}

/// Warm the guild cache, then wait until a committed cross-process settings
/// write is visible through `poll_config_version`.
pub async fn observe_shared_config(cfg: BotProcessConfig) -> Result<()> {
    let guild_id: u64 = std::env::var("JMODGUARD_PROBE_GUILD")
        .context("JMODGUARD_PROBE_GUILD")?
        .parse()
        .context("JMODGUARD_PROBE_GUILD")?;
    let expect = std::env::var("JMODGUARD_PROBE_EXPECT").context("JMODGUARD_PROBE_EXPECT")?;
    let dir = PathBuf::from(std::env::var("JMODGUARD_PROBE_DIR").context("JMODGUARD_PROBE_DIR")?);
    let migrate = cfg.db_open_options().migrate;
    std::fs::write(
        dir.join("bot-migrate"),
        if migrate { "true" } else { "false" },
    )?;
    let db = open_database(&cfg).await?;
    let app = BotState::new(cfg, db)?;
    let before = app.configs.get(&app.db, guild_id).await?;
    std::fs::write(dir.join("bot-ready"), &before.settings.prefix)?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let _ = app.poll_config_version().await?;
        let current = app.configs.get(&app.db, guild_id).await?;
        if current.settings.prefix == expect {
            std::fs::write(dir.join("bot-refreshed"), &current.settings.prefix)?;
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "bot cache stayed at {:?} waiting for {expect:?}",
                current.settings.prefix
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
