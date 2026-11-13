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
use std::sync::Arc;
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

/// Build state, including tenant stores and the bot's read-only DB, using the
/// core database backend's startup retry classification and backoff budget.
/// With a local core, remote guild-store transport failures are not retried.
/// Never retry a schema refusal or overlap a still-running synchronous attempt.
pub async fn build_state(cfg: BotProcessConfig, db: Db) -> Result<Arc<BotState>> {
    let budget = db::startup_retry_budget(db.is_remote());
    build_state_with_budget(cfg, db, budget).await
}

async fn build_state_with_budget(
    cfg: BotProcessConfig,
    db: Db,
    budget: db::StartupOpenBudget,
) -> Result<Arc<BotState>> {
    let deadline = Instant::now() + budget.budget;
    let mut attempt = 0_u32;
    loop {
        attempt = attempt.saturating_add(1);
        let cfg = cfg.clone();
        let connection = db.clone();
        let result = tokio::task::spawn_blocking(move || BotState::new(cfg, connection))
            .await
            .context("bot state startup worker failed")?;
        match result {
            Ok(app) => return Ok(app),
            Err(error) => {
                if db::guild_data::StoreCheckError::is_schema_refusal(&error)
                    || !db::is_startup_retryable(&error, db.is_remote())
                {
                    return Err(error);
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                let backoff =
                    db::startup_backoff(attempt, budget.initial_backoff, budget.max_backoff)
                        .min(remaining);
                if backoff.is_zero() {
                    return Err(error).context("timed out retrying bot state startup");
                }
                tracing::info!(
                    attempt,
                    backoff_ms = backoff.as_millis(),
                    "bot state database not ready; retrying: {error:#}"
                );
                tokio::time::sleep(backoff).await;
                if Instant::now() >= deadline {
                    return Err(error).context("timed out retrying bot state startup");
                }
            }
        }
    }
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
    let app = build_state(cfg, db).await?;
    if !crate::activation::claim_writer_lease(
        app.cfg.bot.control_secret.as_str(),
        &app.db,
        &app.activation,
        Some(&app.tenant),
    )
    .await?
    {
        bail!("bot probe stopped before it held the writer lease");
    }
    app.install_control_actor_lookup(|_actor, _op| {
        Some(db::CachedGuildActor {
            guild_owner_id: 0,
            permissions: 1 << 5,
        })
    });
    if let Some(bound) = crate::control_server::spawn(Arc::clone(&app))? {
        write_probe_marker(&dir, "writer-port", &bound.port().to_string())?;
    } else {
        tracing::error!("control listener is off; the probe continues without a writer port");
    }
    let enrolled = db::enroll_guild(&app.db, guild_id as i64, 1)
        .await?
        .context("probe guild was not enrolled")?;
    write_probe_marker(&dir, "writer-scope", &enrolled.id)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use db::SqlExecutor;

    #[tokio::test(flavor = "multi_thread")]
    async fn state_startup_retries_the_real_read_database_open() {
        let path =
            std::env::temp_dir().join(format!("state-startup-{:032x}.db", rand::random::<u128>()));
        let db = Db::open(&path, 2).unwrap();
        let marker = db
            .with_conn_sync(|conn| {
                let marker = db::meta_string_conn(conn, "bootstrap_complete")?.unwrap();
                conn.execute_bounded(
                    "DELETE FROM meta WHERE key = 'bootstrap_complete'",
                    db::params![],
                    1024 * 1024,
                )?;
                Ok(marker)
            })
            .unwrap();
        let error = BotState::new(BotProcessConfig::default(), db.clone())
            .err()
            .unwrap();
        assert!(db::is_schema_not_ready(&error), "{error:#}");
        let restore = db.clone();
        let restoring = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            restore
                .with_conn(move |conn| {
                    conn.execute_bounded(
                        "INSERT INTO meta(key, value) VALUES ('bootstrap_complete', ?1)",
                        db::params![marker],
                        1024 * 1024,
                    )?;
                    Ok(())
                })
                .await
                .unwrap();
        });
        let result = build_state_with_budget(
            BotProcessConfig::default(),
            db.clone(),
            db::StartupOpenBudget {
                budget: Duration::from_secs(5),
                initial_backoff: Duration::from_millis(20),
                max_backoff: Duration::from_millis(50),
            },
        )
        .await;
        restoring.await.unwrap();
        let app = result.unwrap();
        assert!(app.reads.is_read_only());
        assert!(!app.activation.allows_discord_connect());
        drop(app);
        drop(db);
        crate::tenant::remove_sidecars(&path);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[test]
    fn read_database_context_preserves_remote_timeout_classification() {
        let timeout = db::test_database_operation_timeout_error()
            .context("failed to open the bot read database");
        assert!(db::is_startup_retryable(&timeout, true));
        assert!(!db::is_startup_retryable(&timeout, false));
        let auth = anyhow::anyhow!("unauthorized").context("failed to open the bot read database");
        assert!(!db::is_startup_retryable(&auth, true));
    }
}
