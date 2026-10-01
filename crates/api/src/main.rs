//! jModGuard API / WebUI process (axum only — no Discord gateway).

use anyhow::{Context as _, Result};
use jmodguard_api::config::ApiProcessConfig;
use jmodguard_api::web::state::ApiState;
use jmodguard_api::{health, init_tracing, shutdown_signal, web};
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = ApiProcessConfig::load()?;
    let offline_write =
        std::env::var("JMODGUARD_PROCESS_PROBE").ok().as_deref() == Some("api-write");

    if !offline_write
        && (cfg.web.oauth_client_id.is_empty() || cfg.web.oauth_client_secret.is_empty())
    {
        anyhow::bail!(
            "Discord OAuth credentials required for jmodguard-api. Set \
             `oauth_client_id` / `oauth_client_secret` in config.toml \
             (see config.example.toml) or JMODGUARD_OAUTH_CLIENT_ID / \
             JMODGUARD_OAUTH_CLIENT_SECRET."
        );
    }

    init_tracing(&cfg.log, "api");
    if std::env::var("JMODGUARD_UPDATE_MAINTENANCE").as_deref() == Ok("1") {
        return web::recovery::serve(cfg).await;
    }

    if offline_write {
        jmodguard_api::startup::write_shared_config(cfg).await?;
        tracing::info!("API process stopped");
        return Ok(());
    }

    if cfg.bot.owner_id == 0 {
        tracing::warn!(
            "bot.owner_id is 0 — global WebUI config edits (scope=global / guild_id=0) \
             are blocked until you set your Discord user ID in config.toml"
        );
    }

    // Bind first so platform GET/HEAD /health succeeds while the remote
    // database is still waking, the bot is still migrating, or ApiState
    // primes primary version baselines (a blocking remote read). Only
    // /health is live on this socket until axum takes over; other paths
    // return 503. `serve_while` owns every pre-axum step so the bind is
    // never idle between open and `axum::serve`.
    let web_bind = cfg.web.bind.clone();
    let listener = tokio::net::TcpListener::bind(&web_bind)
        .await
        .with_context(|| format!("failed to bind API on {web_bind}"))?;
    tracing::info!("API liveness on http://{web_bind}/health");

    // API expects the bot (or a one-shot migrate) to have applied schema already.
    // Wait briefly so systemd/container start order can race the bot's first migrate.
    let cfg_for_state = cfg.clone();
    let open_cfg = cfg.clone();
    let db_label = if cfg.db.url.is_empty() {
        cfg.db.path.display().to_string()
    } else {
        cfg.db.url.clone()
    };
    let web_app = health::serve_while(&listener, async {
        tokio::select! {
            result = async {
                let db = jmodguard_api::startup::open_database(&open_cfg)
                    .await
                    .with_context(|| {
                        format!(
                            "failed to open database {db_label} (API opens with migrate=false — start jmodguard-bot once first, or apply migrations)"
                        )
                    })?;
                tracing::info!("database ready (migrate=false) at {db_label}");
                // `ApiState::new` and `web::router` are synchronous. Run both
                // off the task that drives `serve_while` or `/health` stalls
                // for the same remote round-trip / router build this listener
                // exists to cover.
                let app = tokio::task::spawn_blocking(move || ApiState::new(cfg_for_state, db))
                    .await
                    .context("app state worker stopped before returning")??;
                let router_app = Arc::clone(&app);
                let web_app = tokio::task::spawn_blocking(move || web::router(router_app))
                    .await
                    .context("router worker stopped before returning")?;
                jmodguard::mark_child_ready().context("failed to signal API readiness")?;
                // Runtime work starts only after the serving artifact exists.
                // Starting the poller before `web::router` succeeded would leave
                // a background task on a process that then failed to listen.
                let poll_secs = app.cfg.bot.config_poll_secs.max(1);
                let update_app = Arc::clone(&app);
                tokio::spawn(async move {
                    let mut interval=tokio::time::interval(Duration::from_secs(3600));
                    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    loop {
                        interval.tick().await;
                        web::updates::poll_all(&update_app).await;
                    }
                });
                tokio::spawn(async move {
                    let mut interval = tokio::time::interval(Duration::from_secs(poll_secs));
                    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    loop {
                        interval.tick().await;
                        match app.poll_config_version().await {
                            Ok(true) => {
                                tracing::info!(
                                    "config_version changed; API read observation updated"
                                )
                            }
                            Ok(false) => {}
                            Err(e) => tracing::warn!("config_version poll failed: {e:#}"),
                        }
                    }
                });
                Ok::<_, anyhow::Error>(Some(web_app))
            } => result,
            _ = shutdown_signal() => {
                tracing::info!("shutdown during API startup");
                Ok(None)
            }
        }
    })
    .await?;

    let Some(web_app) = web_app else {
        tracing::info!("API process stopped");
        return Ok(());
    };

    tracing::info!("API listening on http://{web_bind}");
    axum::serve(listener, web_app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("web server error")?;
    tracing::info!("API process stopped");
    Ok(())
}
