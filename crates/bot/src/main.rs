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
use std::time::Duration;

async fn drain_nickname_reconciles(
    app: &Arc<BotState>,
    http: &Arc<serenity::http::Http>,
    identity: &tokio::sync::OnceCell<serenity::model::id::UserId>,
    retry: &mut jmodguard_bot::discord::RestRetry,
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
            if !retry.due(std::time::Instant::now()) {
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
                    let class = jmodguard_bot::discord::classify_retry_error(&error);
                    retry.fail(
                        if class == jmodguard_bot::discord::RetryClass::Unauthorized {
                            class
                        } else {
                            jmodguard_bot::discord::RetryClass::Transient
                        },
                        std::time::Instant::now(),
                        30,
                    );
                    if class == jmodguard_bot::discord::RetryClass::Unauthorized {
                        tracing::error!(
                            "nickname identity lookup stopped: Discord rejected the bot token"
                        );
                    } else {
                        tracing::warn!(
                            "nickname reconciliation deferred until Discord identity is available: {error:#}"
                        );
                    }
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

async fn idle_until_shutdown() {
    shutdown_signal().await;
    jmodguard::log_shutdown_finished("bot");
    tracing::info!("bot process stopped");
}

/// Release the writer lease before an already-fatal authentication exit or hold.
/// Running REST workers keep their existing parked behavior; only startup
/// discovery and the gateway's fatal 4004 return use this process boundary.
async fn auth_failure_exit_code(
    app: &BotState,
    error: &serenity::Error,
    launcher_revision: Option<u32>,
) -> Option<i32> {
    let code = jmodguard_bot::discord::fatal_auth_exit_code(error)?;
    app.activation.begin_shutdown();
    app.activation.note_lease_lost();
    jmodguard::set_process_ready(false);
    if let Err(error) = db::release_active_bot(&app.db, app.activation.instance()).await {
        tracing::warn!("active-bot lease release after token rejection failed: {error:#}");
    }
    if !jmodguard::launcher_child::launcher_holds_auth_exit(launcher_revision) {
        tracing::error!(
            "Discord rejected the bot token; this launcher cannot hold auth exits. Bot held in-process; fix the token and restart the container"
        );
        idle_until_shutdown().await;
        // SIGTERM intentionally ends this old-launcher hold with a clean exit (0).
        return Some(0);
    }
    Some(code)
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
    jmodguard::set_shutdown_context("bot");

    // Liveness must accept before database retry. Platform probes hit this
    // port while `open_with_until_ready` waits on a waking database or a
    // local SQLite lock.
    if !offline_cache && let Some(bind) = cfg.health_bind() {
        let listener = jmodguard_bot::health::bind(&bind).await?;
        tokio::spawn(async move {
            if let Err(error) = jmodguard_bot::health::serve_listener_as(listener, "bot").await {
                tracing::error!("health listener exited: {error:#}");
            }
        });
    }

    if offline_cache {
        jmodguard_bot::startup::observe_shared_config(cfg).await?;
        tracing::info!("bot process stopped");
        return Ok(());
    }

    if let Some(message) = jmodguard::launcher_child::refusal_message() {
        tracing::error!("{message}");
        idle_until_shutdown().await;
        return Ok(());
    }

    jmodguard::cgroup::spawn_container_usage_sampler();

    let db = tokio::select! {
        db = jmodguard_bot::startup::open_database(&cfg) => db.with_context(|| {
            if cfg.is_remote_db() {
                format!(
                    "failed to open remote database {} after waiting for a transient connect/bootstrap",
                    cfg.db.log_label()
                )
            } else {
                format!("failed to open database {}", cfg.db.path.display())
            }
        })?,
        _ = shutdown_signal() => {
            tracing::info!("shutdown during database open");
            jmodguard::log_shutdown_finished("bot");
            tracing::info!("bot process stopped");
            return Ok(());
        }
    };
    tracing::info!("database ready (migrate=true) at {}", cfg.db.log_label());

    let app = BotState::new(cfg.clone(), db)?;
    let activation = Arc::clone(&app.activation);
    match jmodguard_bot::activation::claim_writer_lease(
        app.cfg.bot.control_secret.as_str(),
        &app.db,
        &activation,
        Some(&app.tenant),
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => {
            tracing::info!("bot stopped before it held the active-bot lease");
            jmodguard::log_shutdown_finished("bot");
            tracing::info!("bot process stopped");
            return Ok(());
        }
        Err(error) => return Err(error),
    }
    if !activation.allows_discord_connect() {
        anyhow::bail!("active-bot lease was not held");
    }
    if jmodguard_bot::control_server::spawn(Arc::clone(&app))?.is_none() {
        tracing::error!(
            "control listener is off; moderation continues and API writes are unavailable"
        );
    }

    // --- Discord client (needed for slash re-sync after config polls) --------
    // Guild voice states is not a privileged intent. Voice join, leave, and
    // move logs need it. Nickname, username, and member-role logs use the
    // existing server members intent. Thread logs use the existing guilds intent.
    let intents = GatewayIntents::GUILDS
        | GatewayIntents::GUILD_MEMBERS
        | GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::GUILD_MODERATION
        | GatewayIntents::GUILD_VOICE_STATES;

    let mut cache_settings = CacheSettings::default();
    cache_settings.max_messages = 0; // no general message history in RAM or SQL
    cache_settings.cache_guilds = true;
    cache_settings.cache_channels = true;
    cache_settings.cache_users = true;

    let discord_http = serenity::http::Http::new(&cfg.discord.token);
    let mut client = serenity::client::ClientBuilder::new_with_http(discord_http, intents)
        .cache_settings(cache_settings)
        .event_handler(Handler {
            app: Arc::clone(&app),
        })
        .await
        .context("failed to create discord client")?;

    let http = Arc::clone(&client.http);
    let cache = Arc::clone(&client.cache);
    app.install_discord_cache(Arc::clone(&cache));
    app.install_http_member_lookup(Arc::clone(&http));

    // One process-wide importer shares its Discord request budget across all
    // enrolled guilds. The job claim/cursor lives in the guild store.
    {
        let app = Arc::clone(&app);
        let http = Arc::clone(&http);
        let cache = Arc::clone(&cache);
        tokio::spawn(async move { jmodguard_bot::member_sync::run(app, http, cache).await });
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
                if !app.activation.may_record() {
                    continue;
                }
                let Some(source) =
                    jmodguard_bot::enrollment_repair::DiscordMembership::from_cache(&http, &cache)
                else {
                    continue;
                };
                let result = tokio::select! {
                    _ = shutdown_signal() => break,
                    result = repair.step_for_bot(&app, &source) => result,
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

    {
        let app = Arc::clone(&app);
        let http = Arc::clone(&http);
        tokio::spawn(async move {
            jmodguard_bot::scheduled_work::run(app, http).await;
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
                        if !app.activation.may_record() {
                            continue;
                        }
                        let Some(membership) =
                            jmodguard_bot::enrollment_repair::DiscordMembership::from_cache(&http, &cache)
                        else {
                            continue;
                        };
                        let discord = jmodguard_bot::action_worker::LiveDiscord {
                            http: &http,
                            bot_user_id: membership.bot_user_id,
                            activation: &app.activation,
                        };
                        tokio::select! {
                            _ = shutdown_signal() => break,
                            result = jmodguard_bot::action_worker::drain_with_state(&app, &discord, &app.cfg.action_worker.limits) => {
                                if let Err(error) = result { tracing::warn!("bot action queue unavailable: {error:#}"); }
                            }
                        }
                    }
                    _ = cleanup.tick() => {
                        if !app.activation.may_record() {
                            continue;
                        }
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
                        if !app.activation.may_record() {
                            continue;
                        }
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
                        if !app.activation.may_record() {
                            continue;
                        }
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

    {
        let app = Arc::clone(&app);
        tokio::spawn(async move {
            let mut cursor = 0;
            let mut poll = tokio::time::interval(Duration::from_secs(5));
            poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = shutdown_signal() => break,
                    _ = poll.tick() => {
                        if app.activation.may_record()
                            && let Err(error) = jmodguard_bot::gateway_ban_persistence::drain(&app, &mut cursor).await {
                            tracing::warn!("gateway ban inbox recovery will retry: {error:#}");
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
            let mut identity_retry = jmodguard_bot::discord::RestRetry::default();
            let mut interval = tokio::time::interval(Duration::from_secs(poll_secs));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                if !app.activation.may_record() {
                    continue;
                }
                drain_nickname_reconciles(&app, &http, &identity, &mut identity_retry).await;
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
                if !app.activation.may_record() {
                    continue;
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
                            jmodguard_bot::state::SlashResyncPlan::Defer => {
                                tracing::debug!(
                                    "slash re-sync deferred: guild cache empty (will retry)"
                                );
                            }
                            jmodguard_bot::state::SlashResyncPlan::Sync { guilds, invalidate } => {
                                tracing::debug!(
                                    "slash registry reconciliation pending ({} guild(s))",
                                    guilds.len()
                                );
                                if invalidate {
                                    app.invalidate_configs();
                                }
                                let mut failed = std::collections::HashSet::new();
                                for gid in guilds {
                                    if !jmodguard_bot::commands::sync_slash_commands_versioned_http(
                                        &app,
                                        (&cache, http.as_ref()),
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
                                    tracing::debug!(
                                        "slash re-sync pending ({failed_n} guild(s)); \
                                         waiting for retry deadlines or registry/membership changes"
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
                app.attachment_burst.prune();
                app.text_spam.prune();
            }
        });
    }

    let may_identify =
        match jmodguard_bot::gateway_budget::wait_for_gateway_identify(&http, &app.db).await {
            Err(error) => {
                if let Some(discord_error) = error.downcast_ref::<serenity::Error>()
                    && let Some(code) = auth_failure_exit_code(
                        &app,
                        discord_error,
                        jmodguard::launcher_child::detected_launcher_revision(),
                    )
                    .await
                {
                    std::process::exit(code);
                }
                return Err(error);
            }
            result => result?,
        };
    if !may_identify {
        tracing::info!("shutdown before gateway identify");
        activation.begin_shutdown();
        activation.note_lease_lost();
        jmodguard::set_process_ready(false);
        if let Err(error) = db::release_active_bot(&app.db, activation.instance()).await {
            tracing::warn!("active-bot lease release failed: {error:#}");
        }
        jmodguard::log_shutdown_finished("bot");
        tracing::info!("bot process stopped");
        return Ok(());
    }

    tracing::info!("starting shards (bot process; no public HTTP API)");
    let shard_manager = client.shard_manager.clone();
    let shutdown_app = Arc::clone(&app);
    let shutdown_http = Arc::clone(&http);
    let shutdown_cache = Arc::clone(&cache);
    let shutdown_activation = Arc::clone(&activation);
    let shutdown_db = app.db.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        shutdown_activation.begin_shutdown();
        jmodguard::set_process_ready(false);
        let content = logging::shutdown_notice_content(shutdown_app.start.elapsed().as_secs());
        let guild_ids = shutdown_cache.guilds();
        let attempted = guild_ids.len();
        jmodguard_bot::activation::drain_for_shutdown_notices(&shutdown_activation, async move {
            shutdown_app.stop_startup_notice_task().await;
            let sent =
                logging::emit_shutdown_notices(&shutdown_app, shutdown_http, guild_ids, content)
                    .await;
            tracing::info!(sent, attempted, "shutdown notice phase complete");
        })
        .await;
        if let Err(error) =
            db::release_active_bot(&shutdown_db, shutdown_activation.instance()).await
        {
            tracing::warn!("active-bot lease release failed: {error:#}");
        }
        tracing::info!("stopping Discord shards");
        shard_manager.shutdown_all().await;
    });
    if let Err(error) = client.start().await {
        if let Some(code) = auth_failure_exit_code(
            &app,
            &error,
            jmodguard::launcher_child::detected_launcher_revision(),
        )
        .await
        {
            std::process::exit(code);
        }
        anyhow::bail!("discord client error: {error:#}");
    }
    jmodguard::log_shutdown_finished("bot");
    tracing::info!("bot process stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn startup_401_and_initial_or_ready_gateway_4004_exit_or_idle_after_shutdown() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let path =
            std::env::temp_dir().join(format!("auth-exit-{:032x}.db", rand::random::<u128>()));
        let db = db::Db::open(&path, 2).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let received = stream.read(&mut request).await.unwrap();
            assert!(received > 0, "gateway fixture must receive a request");
            let body = r#"{"code":0,"message":"fixture rejection"}"#;
            stream.write_all(format!("HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        });
        let http = serenity::http::HttpBuilder::new("fixture")
            .proxy(format!("http://{addr}"))
            .ratelimiter_disabled(true)
            .build();
        let startup = tokio::time::timeout(
            Duration::from_secs(2),
            jmodguard_bot::gateway_budget::wait_for_gateway_identify(&http, &db),
        )
        .await
        .unwrap()
        .unwrap_err();
        server.await.unwrap();
        let startup = startup.downcast::<serenity::Error>().unwrap();
        assert_eq!(
            jmodguard_bot::discord::http_error_response(&startup),
            Some((401, 0))
        );
        let gateway =
            serenity::Error::Gateway(serenity::gateway::GatewayError::InvalidAuthentication);
        for (error, was_ready) in [(&startup, false), (&gateway, false), (&gateway, true)] {
            for revision in [None, Some(7), Some(8), Some(9), Some(10), Some(6)] {
                for fail_release in [false, true] {
                    let app = BotState::new(Default::default(), db.clone()).unwrap();
                    assert!(
                        jmodguard_bot::activation::wait_for_writer_lock(&db, &app.activation)
                            .await
                            .unwrap()
                    );
                    if fail_release {
                        db.with_conn(|conn| {
                            conn.execute_batch_bounded(
                                "CREATE TRIGGER reject_lease_release BEFORE DELETE ON meta
                                 WHEN OLD.key = 'active_bot_lease'
                                 BEGIN SELECT RAISE(ABORT, 'fixture release failure'); END;",
                                65_536,
                            )?;
                            Ok(())
                        })
                        .await
                        .unwrap();
                        let error = db::release_active_bot(&db, app.activation.instance())
                            .await
                            .unwrap_err();
                        assert!(format!("{error:#}").contains("fixture release failure"));
                    }
                    jmodguard::set_process_ready(was_ready);
                    let exit = auth_failure_exit_code(&app, error, revision);
                    tokio::pin!(exit);
                    let result = tokio::time::timeout(Duration::from_secs(1), &mut exit).await;
                    if revision == Some(6) {
                        assert!(
                            result.is_err(),
                            "old launcher must never receive an auth exit"
                        );
                    } else {
                        assert_eq!(
                            result.unwrap(),
                            Some(jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE)
                        );
                    }
                    assert!(app.activation.is_shutting_down());
                    assert!(!app.activation.may_record());
                    assert!(!app.activation.allows_discord_connect());
                    assert!(!jmodguard::process_ready());
                    if fail_release {
                        db.with_conn(|conn| {
                            conn.execute_batch_bounded(
                                "DROP TRIGGER reject_lease_release",
                                65_536,
                            )?;
                            Ok(())
                        })
                        .await
                        .unwrap();
                        if revision == Some(6) {
                            // Database recovery must not end the in-process hold.
                            assert!(
                                tokio::time::timeout(Duration::from_millis(50), &mut exit)
                                    .await
                                    .is_err()
                            );
                        }
                        db::release_active_bot(&db, app.activation.instance())
                            .await
                            .unwrap();
                    }
                    // A fresh activation can claim immediately after release, as on restart.
                    let next = jmodguard_bot::activation::Activation::new();
                    assert!(
                        db::claim_active_bot(
                            &db,
                            next.instance(),
                            chrono::Utc::now().timestamp_millis(),
                            Arc::new(std::sync::atomic::AtomicBool::new(false))
                        )
                        .await
                        .unwrap()
                    );
                    assert!(!next.is_shutting_down());
                    db::release_active_bot(&db, next.instance()).await.unwrap();
                }
            }
        }
        assert_eq!(
            jmodguard_bot::discord::fatal_auth_exit_code(&serenity::Error::Gateway(
                serenity::gateway::GatewayError::DisallowedGatewayIntents
            )),
            None
        );
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        jmodguard::tenant::remove_sidecars(&path);
    }
}
