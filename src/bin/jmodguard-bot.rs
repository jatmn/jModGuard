//! jModGuard Discord bot process (gateway only — health listener optional).

use anyhow::{Context as _, Result};
use jmodguard::config::Config;
use jmodguard::db::Db;
use jmodguard::handlers::Handler;
use jmodguard::state::AppState;
use jmodguard::{db, init_tracing, shutdown_signal};
use serenity::builder::EditMember;
use serenity::cache::Settings as CacheSettings;
use serenity::model::gateway::GatewayIntents;
use std::sync::Arc;
use std::time::Duration;

async fn reconcile_nickname_job(
    app: Arc<AppState>,
    http: Arc<serenity::http::Http>,
    me: serenity::model::id::UserId,
    job: db::NicknameReconcileJob,
) {
    let Ok(guild_id_u64) = u64::try_from(job.guild_id) else {
        tracing::error!(
            guild_id = job.guild_id,
            "invalid guild id in nickname outbox"
        );
        return;
    };
    let guild = serenity::model::id::GuildId::new(guild_id_u64);
    let edit = tokio::time::timeout(
        jmodguard::discord::DISCORD_REST_TIMEOUT,
        guild.edit_member(&http, me, EditMember::new().nickname(job.nickname.clone())),
    )
    .await;
    match edit {
        Err(_) => {
            let now = chrono::Utc::now().timestamp();
            let message = "Discord nickname request timed out";
            if let Err(error) =
                db::schedule_nickname_reconcile_retry(&app.db, &job, now, "timeout", message).await
            {
                tracing::error!(
                    guild_id = job.guild_id,
                    "failed to persist nickname reconciliation timeout: {error:#}"
                );
            }
        }
        Ok(Ok(_)) => {
            if let Err(error) =
                db::acknowledge_nickname_reconcile(&app.db, job.guild_id, job.generation).await
            {
                tracing::warn!(
                    guild_id = job.guild_id,
                    "failed to acknowledge nickname reconciliation: {error:#}"
                );
            }
        }
        Ok(Err(error)) => {
            let now = chrono::Utc::now().timestamp();
            use jmodguard::discord::NicknameFailureClass;
            let (class, code) = jmodguard::discord::classify_nickname_failure(&error);
            let message = error.to_string();
            let result = match class {
                NicknameFailureClass::Transient => {
                    db::schedule_nickname_reconcile_retry(&app.db, &job, now, &code, &message)
                        .await
                        .map(|exhausted| {
                            if exhausted {
                                tracing::error!(
                                    guild_id = job.guild_id,
                                    attempts = job.attempt_count + 1,
                                    "nickname reconciliation exhausted its retry budget: {message}"
                                );
                            } else {
                                tracing::warn!(
                                    guild_id = job.guild_id,
                                    attempt = job.attempt_count + 1,
                                    "nickname reconciliation scheduled for retry: {message}"
                                );
                            }
                        })
                }
                NicknameFailureClass::Dormant => {
                    db::finish_nickname_reconcile(&app.db, &job, "dormant", &code, &message, now)
                        .await
                        .map(|()| {
                            tracing::warn!(
                                guild_id = job.guild_id,
                                "nickname reconciliation dormant until guild rejoin: {message}"
                            )
                        })
                }
                NicknameFailureClass::Terminal => {
                    db::finish_nickname_reconcile(&app.db, &job, "terminal", &code, &message, now)
                        .await
                        .map(|()| {
                            tracing::error!(
                                guild_id = job.guild_id,
                                "nickname reconciliation needs operator action: {message}"
                            )
                        })
                }
            };
            if let Err(state_error) = result {
                tracing::error!(
                    guild_id = job.guild_id,
                    "failed to persist nickname reconciliation failure: {state_error:#}"
                );
            }
        }
    }
}

async fn drain_nickname_reconciles(app: &Arc<AppState>, http: &Arc<serenity::http::Http>) {
    // The bot is the sole nickname side-effect worker. Drain the
    // durable generation-checked outbox every tick so the last
    // committed DB value wins and Discord failures are retried.
    let now = chrono::Utc::now().timestamp();
    match db::pending_nickname_reconciles(&app.db, now, 25).await {
        Ok(pending) => {
            let me = match app.bot_user_id().await {
                Ok(id) => id,
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
    let cfg = Config::load()?;

    if cfg.discord.token.is_empty() {
        anyhow::bail!(
            "no Discord token configured. Set `token` in config.toml \
             (see config.example.toml) or JMODGUARD_DISCORD_TOKEN."
        );
    }

    init_tracing(&cfg, "bot");

    // Liveness must accept before database retry. Platform probes hit this
    // port while `open_with_until_ready` waits on a waking database or a
    // local SQLite lock.
    if let Some(bind) = cfg.health_bind() {
        let listener = jmodguard::health::bind(&bind).await?;
        tokio::spawn(async move {
            if let Err(error) = jmodguard::health::serve_listener(listener).await {
                tracing::error!("health listener exited: {error:#}");
            }
        });
    }

    let opts = cfg.db_open_options(true);
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

    let app = AppState::new(cfg.clone(), db)?;

    // --- Discord client (needed for slash re-sync after config polls) --------
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

    let http = Arc::clone(&client.http);
    let cache = Arc::clone(&client.cache);

    // --- config_version poll (cross-process cache invalidation) --------------
    let poll_secs = cfg.bot.config_poll_secs.max(1);
    {
        let app = Arc::clone(&app);
        let http = Arc::clone(&http);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(poll_secs));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                drain_nickname_reconciles(&app, &http).await;
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
                            jmodguard::state::SlashResyncPlan::Defer => {
                                tracing::debug!(
                                    "slash re-sync deferred: guild cache empty (will retry)"
                                );
                            }
                            jmodguard::state::SlashResyncPlan::Sync { guilds, invalidate } => {
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
                                    if !jmodguard::commands::sync_slash_commands_versioned_http(
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
