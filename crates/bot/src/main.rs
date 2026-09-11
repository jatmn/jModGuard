//! jModGuard Discord bot process (gateway only — health listener optional).

use anyhow::{Context as _, Result};
use jmodguard_bot::config::BotProcessConfig;
use jmodguard_bot::db::Db;
use jmodguard_bot::handlers::Handler;
use jmodguard_bot::nickname_worker::reconcile_nickname_job;
use jmodguard_bot::state::BotState;
use jmodguard_bot::{db, init_tracing, shutdown_signal};
use serenity::cache::Settings as CacheSettings;
use serenity::model::gateway::GatewayIntents;
use std::sync::Arc;
use std::time::Duration;

async fn drain_nickname_reconciles(
    app: &Arc<BotState>,
    http: &Arc<serenity::http::Http>,
    identity: &tokio::sync::OnceCell<serenity::model::id::UserId>,
) {
    // The bot is the sole nickname side-effect worker. Drain the
    // durable request- and membership-checked outbox every tick so the last
    // committed DB value wins and Discord failures are retried.
    let now = chrono::Utc::now().timestamp();
    match db::pending_nickname_reconciles(&app.db, now, 25).await {
        Ok(pending) => {
            if pending.is_empty() {
                return;
            }
            let me = match identity
                .get_or_try_init(|| async {
                    let user = tokio::time::timeout(
                        jmodguard_bot::discord::DISCORD_REST_TIMEOUT,
                        http.get_current_user(),
                    )
                    .await
                    .context("timed out fetching bot identity")??;
                    Ok::<_, anyhow::Error>(user.id)
                })
                .await
            {
                Ok(id) => *id,
                Err(error) => {
                    tracing::warn!(
                        "nickname reconciliation deferred until Discord identity is available: {error:#}"
                    );
                    return;
                }
            };
            let mut active = tokio::task::JoinSet::new();
            for job in pending {
                while active.len() >= 4 {
                    if let Some(Err(error)) = active.join_next().await {
                        tracing::error!("nickname reconciliation task panicked: {error}");
                    }
                }
                active.spawn(reconcile_nickname_job(
                    Arc::clone(app),
                    Arc::clone(http),
                    me,
                    job,
                ));
            }
            while let Some(result) = active.join_next().await {
                if let Err(error) = result {
                    tracing::error!("nickname reconciliation task panicked: {error}");
                }
            }
        }
        Err(error) => {
            tracing::warn!("failed to load pending nickname reconciliations: {error:#}")
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = BotProcessConfig::load()?;

    if cfg.discord.token.is_empty() {
        anyhow::bail!(
            "no Discord token configured. Set `token` in config.toml \
             (see config.example.toml) or JMODGUARD_DISCORD_TOKEN."
        );
    }

    init_tracing(&cfg.log, "bot");

    // Liveness must accept before database retry. Platform probes hit this
    // port while `open_with_until_ready` waits on a waking database or a
    // local SQLite lock.
    if let Some(bind) = cfg.health_bind() {
        let listener = jmodguard_bot::health::bind(&bind).await?;
        tokio::spawn(async move {
            if let Err(error) = jmodguard_bot::health::serve_listener(listener).await {
                tracing::error!("health listener exited: {error:#}");
            }
        });
    }

    let opts = cfg.db_open_options();
    let (attempts, delay) = db::startup_open_retry(&opts);
    let db = tokio::select! {
        db = Db::open_with_until_ready(opts, attempts, delay) => db.with_context(|| {
            if cfg.is_remote_db() {
                format!(
                    "failed to open remote database {} after waiting for a transient connect/bootstrap",
                    cfg.db.url
                )
            } else {
                format!("failed to open database {}", cfg.db.path.display())
            }
        })?,
        _ = shutdown_signal() => {
            tracing::info!("shutdown during database open");
            tracing::info!("bot process stopped");
            return Ok(());
        }
    };
    tracing::info!(
        "database ready (migrate=true) at {}",
        if cfg.db.url.is_empty() {
            cfg.db.path.display().to_string()
        } else {
            cfg.db.url.clone()
        }
    );

    let app = BotState::new(cfg.clone(), db)?;

    // --- Discord client (needed for slash re-sync after config polls) --------
    let intents = GatewayIntents::GUILDS
        | GatewayIntents::GUILD_MEMBERS
        | GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::GUILD_MODERATION;

    let mut cache_settings = CacheSettings::default();
    cache_settings.max_messages = 0; // no general message history in RAM or SQL
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

    let http = Arc::clone(&client.http);
    let cache = Arc::clone(&client.cache);

    if cfg.enrollment.enabled {
        let app = Arc::clone(&app);
        let http = Arc::clone(&http);
        let cache = Arc::clone(&cache);
        let mut repair = jmodguard_bot::enrollment_repair::EnrollmentRepair::new(
            cfg.enrollment.clone(),
            std::time::Instant::now(),
        )?;
        tokio::spawn(async move {
            let mut tick =
                tokio::time::interval(Duration::from_secs(u64::from(app.cfg.enrollment.poll_secs)));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! { _ = shutdown_signal() => break, _ = tick.tick() => {} }
                let Some(source) =
                    jmodguard_bot::enrollment_repair::DiscordMembership::from_cache(&http, &cache)
                else {
                    continue;
                };
                let result = tokio::select! {
                    _ = shutdown_signal() => break,
                    result = repair.step(&app.db, &source) => result,
                };
                match result {
                    Ok(Some(report)) if report.failed > 0 => tracing::warn!(
                        failed = report.failed,
                        checked = report.checked,
                        "membership repair incomplete; failed guilds will be revisited next sweep"
                    ),
                    Err(error) => tracing::warn!("membership discovery deferred: {error:#}"),
                    _ => {}
                }
            }
        });
    }

    // One sequential administrative worker plus bounded periodic history cleanup.
    {
        let app = Arc::clone(&app);
        let http = Arc::clone(&http);
        let cache = Arc::clone(&cache);
        tokio::spawn(async move {
            let mut poll = tokio::time::interval(Duration::from_secs(u64::from(
                app.cfg.action_worker.poll_secs,
            )));
            let mut cleanup = tokio::time::interval(Duration::from_secs(u64::from(
                app.cfg.action_worker.cleanup_interval_secs,
            )));
            poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            cleanup.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = shutdown_signal() => break,
                    _ = poll.tick() => {
                        let Some(membership) =
                            jmodguard_bot::enrollment_repair::DiscordMembership::from_cache(&http, &cache)
                        else {
                            continue;
                        };
                        let discord = jmodguard_bot::action_worker::LiveDiscord {
                            http: &http,
                            bot_user_id: membership.bot_user_id,
                        };
                        tokio::select! {
                            _ = shutdown_signal() => break,
                            result = jmodguard_bot::action_worker::drain(&app.db, &discord, &app.cfg.action_worker.limits) => {
                                if let Err(error) = result { tracing::warn!("bot action queue unavailable: {error:#}"); }
                            }
                        }
                    }
                    _ = cleanup.tick() => {
                        if let Err(error) = db::prune_bot_action_history(&app.db, chrono::Utc::now().timestamp(), app.cfg.actions.clone()).await {
                            tracing::warn!("bot action history cleanup failed: {error:#}");
                        }
                    }
                }
            }
        });
    }

    // --- config_version poll (cross-process cache invalidation) --------------
    let poll_secs = cfg.bot.config_poll_secs.max(1);
    {
        let app = Arc::clone(&app);
        let http = Arc::clone(&http);
        tokio::spawn(async move {
            let identity = tokio::sync::OnceCell::new();
            let mut interval = tokio::time::interval(Duration::from_secs(poll_secs));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                drain_nickname_reconciles(&app, &http, &identity).await;
            }
        });
    }
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
    {
        let app = Arc::clone(&app);
        let http = Arc::clone(&http);
        let cache = Arc::clone(&cache);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(poll_secs));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                // Peek slash registry every tick (not gated on config_version) so a
                // future slash-only bump cannot be missed. Do not commit the observed
                // version until Discord re-sync has nothing left to retry — otherwise
                // a transient set_commands failure left that guild stale until the
                // next registry write. Retries are per-failed-guild only (not a full
                // cache sweep) so one bad guild cannot amplify Discord rate limits.
                match app.pending_slash_registry_version().await {
                    Ok(Some(remote)) => {
                        let cached: Vec<u64> =
                            cache.guilds().into_iter().map(|g| g.get()).collect();
                        match app.plan_slash_resync(remote, &cached) {
                            jmodguard_bot::state::SlashResyncPlan::Defer => {
                                tracing::debug!(
                                    "slash re-sync deferred: guild cache empty (will retry)"
                                );
                            }
                            jmodguard_bot::state::SlashResyncPlan::Sync { guilds, invalidate } => {
                                tracing::info!(
                                    "slash_registry_version changed; re-syncing Discord slash \
                                     commands ({} guild(s))",
                                    guilds.len()
                                );
                                if invalidate {
                                    app.invalidate_configs();
                                }
                                let mut failed = std::collections::HashSet::new();
                                for gid in guilds {
                                    if !jmodguard_bot::commands::sync_slash_commands_versioned_http(
                                        &app,
                                        &http,
                                        serenity::model::id::GuildId::new(gid),
                                        remote,
                                    )
                                    .await
                                    {
                                        failed.insert(gid);
                                    }
                                }
                                let failed_n = failed.len();
                                if app.finish_slash_resync(remote, failed) {
                                    tracing::info!(
                                        remote,
                                        "slash registry reconciliation committed"
                                    );
                                } else {
                                    tracing::warn!(
                                        "slash re-sync incomplete ({failed_n} guild(s) failed); \
                                         will retry those guilds next poll"
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

    // --- periodic cleanup ----------------------------------------------------
    {
        let app = Arc::clone(&app);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                app.automod.prune();
                app.image_spam.prune();
            }
        });
    }

    tracing::info!("starting shards (bot process; no public HTTP API)");
    let shard_manager = client.shard_manager.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("stopping Discord shards");
        shard_manager.shutdown_all().await;
    });
    if let Err(error) = client.start().await {
        anyhow::bail!("discord client error: {error:#}");
    }
    tracing::info!("bot process stopped");
    Ok(())
}
