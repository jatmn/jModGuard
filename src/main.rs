//! jModGuard — a data-driven Discord moderation bot.
//!
//! - Automod: mention spam, attachment floods, new-member URLs, badwords
//!   (bypass-resistant), invite links, message velocity, account age.
//! - Commands are data-driven: stored per-guild with global fallback,
//!   editable via the WebUI, mirrored to Discord slash commands.
//! - Per-event log channels with editable templates.
//! - Multi-server: per-guild config with global scoping and copy-from-server.

use anyhow::{Context as _, Result};
use jmodguard::config::Config;
use jmodguard::db::{self, Db};
use jmodguard::handlers::Handler;
use jmodguard::state::AppState;
use jmodguard::{init_tracing, web};
use serenity::cache::Settings as CacheSettings;
use serenity::model::gateway::GatewayIntents;
use std::sync::Arc;
use std::time::Duration;

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

    let db = Db::open_with(cfg.db_open_options(true)).with_context(|| {
        format!(
            "failed to open database {}",
            if cfg.db.url.is_empty() {
                cfg.db.path.display().to_string()
            } else {
                cfg.db.url.clone()
            }
        )
    })?;
    tracing::info!(
        "database ready at {}",
        if cfg.db.url.is_empty() {
            cfg.db.path.display().to_string()
        } else {
            cfg.db.url.clone()
        }
    );

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
    {
        let app = Arc::clone(&app);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                app.automod.prune();
                if let Err(e) = db::purge_old_messages(&app.db).await {
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
        .event_handler(Handler {
            app: Arc::clone(&app),
        })
        .await
        .context("failed to create discord client")?;

    // Poll durable versions so WebUI writes are visible without a restart.
    // Keep slash-command retries per guild: a transient Discord failure must
    // not leave the whole registry permanently stale.
    {
        let app = Arc::clone(&app);
        let http = Arc::clone(&client.http);
        let cache = Arc::clone(&client.cache);
        let poll_secs = cfg.bot.config_poll_secs.max(1);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(poll_secs));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let config_changed = match app.poll_config_version().await {
                    Ok(true) => {
                        tracing::info!("config_version changed; guild config cache invalidated");
                        true
                    }
                    Ok(false) => false,
                    Err(e) => {
                        tracing::warn!("config_version poll failed: {e:#}");
                        false
                    }
                };
                match app.pending_slash_registry_version().await {
                    Ok(Some(remote)) => {
                        let cached: Vec<u64> =
                            cache.guilds().into_iter().map(|g| g.get()).collect();
                        match app.plan_slash_resync(remote, &cached) {
                            jmodguard::state::SlashResyncPlan::Defer => {
                                tracing::debug!(
                                    "slash re-sync deferred: guild cache empty (will retry)"
                                );
                            }
                            jmodguard::state::SlashResyncPlan::Done => {
                                app.commit_slash_registry_version(remote);
                            }
                            jmodguard::state::SlashResyncPlan::Sync { guilds, invalidate } => {
                                if invalidate && !config_changed {
                                    app.invalidate_configs();
                                }
                                let mut failed = std::collections::HashSet::new();
                                for gid in guilds {
                                    if !jmodguard::commands::sync_slash_commands_http(
                                        &app,
                                        &http,
                                        serenity::model::id::GuildId::new(gid),
                                    )
                                    .await
                                    {
                                        failed.insert(gid);
                                    }
                                }
                                if app.finish_slash_resync(remote, failed) {
                                    app.commit_slash_registry_version(remote);
                                } else {
                                    tracing::warn!(
                                        "slash re-sync incomplete; will retry failed guilds"
                                    );
                                }
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!("slash_registry_version poll failed: {e:#}"),
                }
            }
        });
    }

    tracing::info!("starting shards");
    client.start().await.context("discord client error")?;
    Ok(())
}
