//! jModGuard Discord bot process (gateway only — health listener optional).

use anyhow::{Context as _, Result};
use jmodguard_bot::config::BotProcessConfig;
use jmodguard_bot::handlers::Handler;
use jmodguard_bot::logging;
use jmodguard_bot::nickname_worker::reconcile_nickname_job;
use jmodguard_bot::state::BotState;
use jmodguard_bot::{db, init_tracing, shutdown_signal};
use serenity::cache::Settings as CacheSettings;
use serenity::model::gateway::GatewayIntents;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
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

// Serenity invokes rate-limit callbacks synchronously before it waits for the
// provider deadline. Commit the shared request gate before that wait can be
// cancelled or the process can shut down.
fn persist_member_sync_rate_limit(db: &db::Db, bot_id: i64, deadline_ms: i64) -> Result<()> {
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(db::extend_member_sync_request_after(
            db,
            bot_id,
            deadline_ms,
        ))
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = BotProcessConfig::load()?;
    let offline_cache =
        std::env::var("JMODGUARD_PROCESS_PROBE").ok().as_deref() == Some("bot-cache");

    if !offline_cache && cfg.discord.token.is_empty() {
        anyhow::bail!(
            "no Discord token configured. Set `token` in config.toml \
             (see config.example.toml) or JMODGUARD_DISCORD_TOKEN."
        );
    }

    init_tracing(&cfg.log, "bot");

    // Liveness must accept before database retry. Platform probes hit this
    // port while `open_with_until_ready` waits on a waking database or a
    // local SQLite lock.
    if !offline_cache && let Some(bind) = cfg.health_bind() {
        let listener = jmodguard_bot::health::bind(&bind).await?;
        tokio::spawn(async move {
            if let Err(error) = jmodguard_bot::health::serve_listener(listener).await {
                tracing::error!("health listener exited: {error:#}");
            }
        });
    }

    if offline_cache {
        jmodguard_bot::startup::observe_shared_config(cfg).await?;
        tracing::info!("bot process stopped");
        return Ok(());
    }

    let db = tokio::select! {
        db = jmodguard_bot::startup::open_database(&cfg) => db.with_context(|| {
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

    // Serenity retries 429s after the provider's Retry-After value. The slow
    // importer also records that deadline if its own request timeout cancels
    // the in-progress wait, so a restart or next guild cannot undercut it.
    let rate_limit_until_ms = Arc::new(AtomicI64::new(0));
    let sync_bot_id = Arc::new(AtomicI64::new(0));
    let mut discord_http = serenity::http::Http::new(&cfg.discord.token);
    if let Some(limiter) = discord_http.ratelimiter.as_mut() {
        let until = Arc::clone(&rate_limit_until_ms);
        let bot_id = Arc::clone(&sync_bot_id);
        let pace_db = app.db.clone();
        limiter.set_ratelimit_callback(Box::new(move |info| {
            if info.global
                || info.path.contains("/members")
                || info.path.contains("/bans")
                || info.path.contains("/audit-logs")
            {
                let deadline = chrono::Utc::now()
                    .timestamp_millis()
                    .saturating_add(info.timeout.as_millis().min(i64::MAX as u128) as i64);
                until.fetch_max(deadline, Ordering::Relaxed);
                let bot_id = bot_id.load(Ordering::Relaxed);
                if bot_id > 0
                    && let Err(error) = persist_member_sync_rate_limit(
                        &pace_db,
                        bot_id,
                        until.load(Ordering::Relaxed),
                    )
                {
                    tracing::warn!("failed to persist Discord retry deadline: {error:#}");
                }
            }
        }));
    }
    let mut client = serenity::client::ClientBuilder::new_with_http(discord_http, intents)
        .cache_settings(cache_settings)
        .event_handler(Handler {
            app: Arc::clone(&app),
        })
        .await
        .context("failed to create discord client")?;

    let http = Arc::clone(&client.http);
    let cache = Arc::clone(&client.cache);

    // One process-wide importer shares its Discord request budget across all
    // enrolled guilds. The job claim/cursor lives in the guild store.
    {
        let app = Arc::clone(&app);
        let http = Arc::clone(&http);
        let cache = Arc::clone(&cache);
        let rate_limit_until_ms = Arc::clone(&rate_limit_until_ms);
        let sync_bot_id = Arc::clone(&sync_bot_id);
        tokio::spawn(async move {
            jmodguard_bot::member_sync::run(app, http, cache, rate_limit_until_ms, sync_bot_id)
                .await
        });
    }

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
                    result = repair.step(&app.db, &app.tenant, &source) => result,
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
                            result = jmodguard_bot::action_worker::drain_with_state(&app, &discord, &app.cfg.action_worker.limits) => {
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

    {
        let app = Arc::clone(&app);
        tokio::spawn(async move {
            let mut poll = tokio::time::interval(Duration::from_secs(1));
            poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = shutdown_signal() => break,
                    _ = poll.tick() => {
                        jmodguard_bot::cases::retry_deferred_discord_audit_links(&app, None).await;
                    }
                }
            }
        });
    }

    {
        let app = Arc::clone(&app);
        tokio::spawn(async move {
            let mut poll = tokio::time::interval(Duration::from_secs(5));
            poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = shutdown_signal() => break,
                    _ = poll.tick() => {
                        loop {
                            match app.tenant.drive_erasure().await {
                                Ok(true) => {}
                                Ok(false) => break,
                                Err(error) => {
                                    tracing::warn!("guild erasure deferred: {error:#}");
                                    break;
                                }
                            }
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
                app.text_spam.prune();
            }
        });
    }

    if !jmodguard_bot::gateway_budget::wait_for_gateway_identify(&http, &app.db).await? {
        tracing::info!("shutdown before gateway identify");
        tracing::info!("bot process stopped");
        return Ok(());
    }

    tracing::info!("starting shards (bot process; no public HTTP API)");
    let shard_manager = client.shard_manager.clone();
    let shutdown_app = Arc::clone(&app);
    let shutdown_http = Arc::clone(&http);
    let shutdown_cache = Arc::clone(&cache);
    tokio::spawn(async move {
        shutdown_signal().await;
        shutdown_app.stop_startup_notice_task().await;
        let content = logging::shutdown_notice_content(shutdown_app.start.elapsed().as_secs());
        let guild_ids = shutdown_cache.guilds();
        let attempted = guild_ids.len();
        let sent =
            logging::emit_shutdown_notices(&shutdown_app, shutdown_http, guild_ids, content).await;
        tracing::info!(sent, attempted, "shutdown notice phase complete");
        tracing::info!("stopping Discord shards");
        shard_manager.shutdown_all().await;
    });
    if let Err(error) = client.start().await {
        anyhow::bail!("discord client error: {error:#}");
    }
    tracing::info!("bot process stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn callback_deadline_is_committed_before_request_can_be_cancelled() {
        let path = std::env::temp_dir().join(format!(
            "bot_member_sync_rate_limit_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = db::Db::open(&path, 2).unwrap();
        db::claim_member_sync_request(&db, 42, "first".into(), 100_000, 15_000, 90_000)
            .await
            .unwrap()
            .unwrap();
        persist_member_sync_rate_limit(&db, 42, 250_000).unwrap();
        drop(db);

        let reopened = db::Db::open(&path, 2).unwrap();
        assert!(
            db::claim_member_sync_request(
                &reopened,
                42,
                "after-restart".into(),
                190_001,
                15_000,
                90_000,
            )
            .await
            .unwrap()
            .is_none()
        );
        assert!(
            db::claim_member_sync_request(
                &reopened,
                42,
                "after-restart".into(),
                250_000,
                15_000,
                90_000,
            )
            .await
            .unwrap()
            .is_some()
        );
        drop(reopened);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
}
