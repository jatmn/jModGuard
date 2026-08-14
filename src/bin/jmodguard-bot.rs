//! jModGuard Discord bot process (gateway only — no HTTP bind).

use anyhow::{Context as _, Result};
use jmodguard::config::Config;
use jmodguard::db::Db;
use jmodguard::handlers::Handler;
use jmodguard::state::AppState;
use jmodguard::{db, init_tracing};
use serenity::builder::EditMember;
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

    init_tracing(&cfg, "bot");

    let opts = cfg.db_open_options(true);
    let db = Db::open_with(opts).with_context(|| {
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
        let cache = Arc::clone(&cache);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(poll_secs));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                // Track whether this tick already cleared the cache so a command
                // write (bumps both versions) does not invalidate twice.
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
                // The bot is the sole nickname side-effect worker. Drain the
                // durable generation-checked outbox every tick so the last
                // committed DB value wins and Discord failures are retried.
                let now = chrono::Utc::now().timestamp();
                match db::pending_nickname_reconciles(&app.db, now, 25).await {
                    Ok(pending) => {
                        let me = cache.current_user().id;
                        for job in pending {
                            let Ok(guild_id_u64) = u64::try_from(job.guild_id) else {
                                tracing::error!(
                                    guild_id = job.guild_id,
                                    "invalid guild id in nickname outbox"
                                );
                                continue;
                            };
                            let guild = serenity::model::id::GuildId::new(guild_id_u64);
                            match guild
                                .edit_member(
                                    &http,
                                    me,
                                    EditMember::new().nickname(job.nickname.clone()),
                                )
                                .await
                            {
                                Ok(_) => {
                                    if let Err(error) = db::acknowledge_nickname_reconcile(
                                        &app.db,
                                        job.guild_id,
                                        job.generation,
                                    )
                                    .await
                                    {
                                        tracing::warn!(
                                            guild_id = job.guild_id,
                                            "failed to acknowledge nickname reconciliation: {error:#}"
                                        );
                                    }
                                }
                                Err(error) => {
                                    use jmodguard::discord::NicknameFailureClass;
                                    let (class, code) =
                                        jmodguard::discord::classify_nickname_failure(&error);
                                    let message = error.to_string();
                                    let result = match class {
                                        NicknameFailureClass::Transient => {
                                            db::schedule_nickname_reconcile_retry(
                                                &app.db, &job, now, &code, &message,
                                            )
                                            .await
                                            .map(|exhausted| {
                                                if exhausted {
                                                    tracing::error!(guild_id = job.guild_id, attempts = job.attempt_count + 1, "nickname reconciliation exhausted its retry budget: {message}");
                                                } else {
                                                    tracing::warn!(guild_id = job.guild_id, attempt = job.attempt_count + 1, "nickname reconciliation scheduled for retry: {message}");
                                                }
                                            })
                                        }
                                        NicknameFailureClass::Dormant => db::finish_nickname_reconcile(
                                            &app.db, &job, "dormant", &code, &message, now,
                                        ).await.map(|()| tracing::warn!(guild_id = job.guild_id, "nickname reconciliation dormant until guild rejoin: {message}")),
                                        NicknameFailureClass::Terminal => db::finish_nickname_reconcile(
                                            &app.db, &job, "terminal", &code, &message, now,
                                        ).await.map(|()| tracing::error!(guild_id = job.guild_id, "nickname reconciliation needs operator action: {message}")),
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
                    }
                    Err(error) => {
                        tracing::warn!("failed to load pending nickname reconciliations: {error:#}")
                    }
                }
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
                                if invalidate && !config_changed {
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
                                    app.commit_slash_registry_version(remote);
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

    tracing::info!("starting shards (bot process; no HTTP bind)");
    client.start().await.context("discord client error")?;
    Ok(())
}
