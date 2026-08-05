//! jModGuard — a data-driven Discord moderation bot.
//!
//! - Automod: mention spam, attachment floods, new-member URLs, badwords
//!   (bypass-resistant), invite links, message velocity, account age.
//! - Commands are data-driven: stored per-guild with global fallback,
//!   editable via the WebUI, mirrored to Discord slash commands.
//! - Per-event log channels with editable templates.
//! - Multi-server: per-guild config with global scoping and copy-from-server.

mod automod;
mod commands;
mod config;
mod db;
mod defaults;
mod handlers;
mod logging;
mod normalize;
mod state;
mod templates;
mod types;
mod web;

use anyhow::{Context as _, Result};
use serenity::cache::Settings as CacheSettings;
use serenity::model::gateway::GatewayIntents;
use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::db::Db;
use crate::handlers::Handler;
use crate::state::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::load()?;

    if cfg.discord.token.is_empty() {
        anyhow::bail!(
            "no Discord token configured. Set `token` in config.toml \
             (see config.example.toml) or JMODGUARD_DISCORD_TOKEN."
        );
    }

    init_tracing(&cfg);

    let db = Db::open(&cfg.db.path, cfg.db.pool_size)
        .with_context(|| format!("failed to open database {}", cfg.db.path.display()))?;
    tracing::info!("database ready at {}", cfg.db.path.display());

    let app = AppState::new(cfg.clone(), db)?;

    // --- WebUI server (binds before the Discord client so it is always up) ----
    let web_app = web::router(Arc::clone(&app));
    let web_bind = app.cfg.web.bind.clone();
    let listener = tokio::net::TcpListener::bind(&web_bind)
        .await
        .with_context(|| format!("failed to bind WebUI on {web_bind}"))?;
    tracing::info!("WebUI listening on http://{web_bind}");
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, web_app).await {
            tracing::error!("web server error: {e}");
        }
    });

    // --- periodic cleanup ----------------------------------------------------
    let retention_default = cfg.bot.retention_days;
    {
        let app = Arc::clone(&app);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                app.automod.prune();
                let retention = app
                    .configs
                    .get(&app.db, 0)
                    .await
                    .map(|c| c.settings.retention_days)
                    .unwrap_or(retention_default);
                if let Err(e) = db::purge_old_messages(&app.db, retention).await {
                    tracing::warn!("message retention cleanup failed: {e:#}");
                }
                if let Ok(stats) = db::stats(&app.db).await {
                    tracing::debug!(
                        "housekeeping: {} messages, {} events, {} members tracked",
                        stats.messages,
                        stats.events,
                        stats.members
                    );
                }
            }
        });
    }

    // --- Discord client -------------------------------------------------------
    let intents = GatewayIntents::GUILD_MEMBERS
        | GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::GUILD_MODERATION;

    let mut cache_settings = CacheSettings::default();
    cache_settings.max_messages = 0; // we track messages in SQLite, not RAM
    cache_settings.cache_guilds = true;
    cache_settings.cache_channels = true;
    cache_settings.cache_users = true;

    let mut client = serenity::client::ClientBuilder::new(&cfg.discord.token, intents)
        .cache_settings(cache_settings)
        .event_handler(Handler { app: Arc::clone(&app) })
        .await
        .context("failed to create discord client")?;

    tracing::info!("starting shards");
    client.start().await.context("discord client error")?;
    Ok(())
}

fn init_tracing(cfg: &Config) {
    let level = cfg.log.level.clone();
    let dir = cfg.log.directory.clone();
    // Our crate logs per-message diagnostics at debug; everything else at the
    // configured level. This keeps the log file useful without drowning in
    // serenity internals.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(format!("{level},jmodguard=debug")));

    let fmt = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(false);

    if dir.as_os_str().is_empty() {
        fmt.try_init().ok();
        return;
    }

    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            "failed to create log dir {}: {e}; falling back to console",
            dir.display()
        );
        fmt.try_init().ok();
        return;
    }
    let file_appender = tracing_appender::rolling::daily(&dir, "jmodguard.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    // The guard must live for the process lifetime: dropping it shuts down
    // the writer thread and silently loses all log output.
    std::mem::forget(guard);
    fmt.with_writer(non_blocking).try_init().ok();
}
