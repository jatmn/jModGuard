//! jModGuard API / WebUI process (axum only — no Discord gateway).

use anyhow::{Context as _, Result};
use jmodguard::config::Config;
use jmodguard::db::Db;
use jmodguard::state::AppState;
use jmodguard::{init_tracing, web};
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::load()?;

    if cfg.web.oauth_client_id.is_empty() || cfg.web.oauth_client_secret.is_empty() {
        anyhow::bail!(
            "Discord OAuth credentials required for jmodguard-api. Set \
             `oauth_client_id` / `oauth_client_secret` in config.toml \
             (see config.example.toml) or JMODGUARD_OAUTH_CLIENT_ID / \
             JMODGUARD_OAUTH_CLIENT_SECRET."
        );
    }

    // WebUI unban calls Discord REST with the bot token.
    if cfg.discord.token.is_empty() {
        anyhow::bail!(
            "Discord bot token required for jmodguard-api (WebUI unban). Set \
             `token` in config.toml or JMODGUARD_DISCORD_TOKEN."
        );
    }

    init_tracing(&cfg, "api");

    if cfg.bot.owner_id == 0 {
        tracing::warn!(
            "bot.owner_id is 0 — global WebUI config edits (scope=global / guild_id=0) \
             are blocked until you set your Discord user ID in config.toml"
        );
    }

    // API expects the bot (or a one-shot migrate) to have applied schema already.
    let opts = cfg.db_open_options(false);
    let db = Db::open_with(opts).with_context(|| {
        format!(
            "failed to open database {} (API opens with migrate=false — start jmodguard-bot once first, or apply migrations)",
            if cfg.db.url.is_empty() {
                cfg.db.path.display().to_string()
            } else {
                cfg.db.url.clone()
            }
        )
    })?;
    tracing::info!(
        "database ready (migrate=false) at {}",
        if cfg.db.url.is_empty() {
            cfg.db.path.display().to_string()
        } else {
            cfg.db.url.clone()
        }
    );

    let app = AppState::new(cfg.clone(), db)?;

    // Cross-process cache invalidation (bot ↔ API config writes).
    let poll_secs = cfg.bot.config_poll_secs.max(1);
    {
        let app = Arc::clone(&app);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(poll_secs));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                match app.poll_config_version().await {
                    Ok(true) => {
                        tracing::info!("config_version changed; guild config cache invalidated")
                    }
                    Ok(false) => {}
                    Err(e) => tracing::warn!("config_version poll failed: {e:#}"),
                }
            }
        });
    }

    let web_app = web::router(Arc::clone(&app));
    let web_bind = app.cfg.web.bind.clone();
    let listener = tokio::net::TcpListener::bind(&web_bind)
        .await
        .with_context(|| format!("failed to bind API on {web_bind}"))?;
    tracing::info!("API/WebUI listening on http://{web_bind}");
    axum::serve(listener, web_app)
        .await
        .context("web server error")?;
    Ok(())
}
