//! Database startup shared by the bot executable and the offline process probe.
//!
//! The probe stops before the Discord gateway. It still uses this open path and
//! `BotState::poll_config_version`, which is what the running bot calls after a
//! cross-process `config_version` bump.

use crate::config::BotProcessConfig;
use crate::db::{self, Db};
use crate::state::BotState;
use anyhow::{Context as _, Result, bail};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn write_probe_marker(dir: &Path, name: &str, contents: &str) -> Result<()> {
    let path = dir.join(name);
    let tmp = dir.join(format!("{name}.part"));
    std::fs::write(&tmp, contents).with_context(|| format!("write probe marker {name}"))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("publish probe marker {name}"))?;
    Ok(())
}

pub async fn open_database(cfg: &BotProcessConfig) -> Result<Db> {
    let opts = cfg.db_open_options();
    let budget = db::startup_open_retry(&opts);
    Db::open_with_budget(opts, budget).await
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
    write_probe_marker(&dir, "bot-migrate", if migrate { "true" } else { "false" })?;
    let db = open_database(&cfg).await?;
    let app = BotState::new(cfg, db)?;
    let before = app.configs.get(&app.db, guild_id).await?;
    write_probe_marker(&dir, "bot-ready", &before.settings.prefix)?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        // Accept only a poll-driven invalidation. `configs.get` also reloads
        // after the 15s cache TTL, which would hide a broken poll.
        if app.poll_config_version().await? {
            let current = app.configs.get(&app.db, guild_id).await?;
            if current.settings.prefix == expect {
                write_probe_marker(&dir, "bot-refreshed", &current.settings.prefix)?;
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            bail!("bot cache did not refresh to {expect:?} after config poll");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
