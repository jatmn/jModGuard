//! Bot Resources: folds the bot's samples into minute rollups, collects the
//! API's completed minutes over the private read link, and saves both with
//! the bot's current status. Saving needs the writer lease; a minute that
//! cannot be saved is retried with the next one, within a bounded backlog.

use crate::state::BotState;
use jmodguard::db::{self, ResourceProcess};
use jmodguard::resources::{self, ProcessStatus, Rollup, RollupBuilder};
use serenity::cache::Cache;
use serenity::gateway::ShardManager;
use std::sync::Arc;
use std::time::Duration;

/// Minutes kept for a retry while saving fails (the database or the lease
/// is unavailable). Older unsaved minutes are dropped, not estimated.
const BACKLOG_MINUTES: usize = 30;
/// The bot's status row is refreshed with every saved minute.
pub const STATUS_INTERVAL_MS: u64 = 60_000;

/// Counts one handled Gateway event by its type.
pub(crate) fn gateway_event(kind: &'static str) {
    resources::count("gateway_events", 1);
    resources::count_labelled("gateway", kind, 1);
}

/// Counts one rate-limit wait by method and redacted route.
pub(crate) fn rate_limit_wait(method: &str, route: &str, global: bool) {
    resources::count("discord_rate_limit_events", 1);
    let label = if global {
        "global".to_owned()
    } else {
        format!("{method} {route}")
    };
    resources::count_labelled("discord_rate_limits", &label, 1);
}

/// Refreshes the bot's point-in-time gauges. Unknown values are cleared so
/// they read as unavailable rather than as a stale number.
async fn refresh_gauges(app: &BotState, shards: &ShardManager, cache: &Cache) {
    resources::set_gauge(
        "guilds",
        i64::try_from(cache.guilds().len()).unwrap_or(i64::MAX),
    );
    let (latency, connected, total) = {
        let runners = shards.runners.lock().await;
        let latency = runners
            .values()
            .filter_map(|runner| runner.latency)
            .max()
            .map(|latency| i64::try_from(latency.as_millis()).unwrap_or(i64::MAX));
        let connected = runners
            .values()
            .filter(|runner| runner.stage == serenity::gateway::ConnectionStage::Connected)
            .count();
        (latency, connected, runners.len())
    };
    match latency {
        Some(ms) => resources::set_gauge("shard_latency_ms", ms),
        None => resources::clear_gauge("shard_latency_ms"),
    }
    resources::set_gauge(
        "shards_connected",
        i64::try_from(connected).unwrap_or(i64::MAX),
    );
    resources::set_gauge("shards", i64::try_from(total).unwrap_or(i64::MAX));
    match app.db.leased_generation() {
        Some(generation) => resources::set_gauge("lease_generation", generation),
        None => resources::clear_gauge("lease_generation"),
    }
    match app.activation.authority_remaining() {
        Some(left) => resources::set_gauge(
            "lease_margin_ms",
            i64::try_from(left.as_millis()).unwrap_or(i64::MAX),
        ),
        None => resources::clear_gauge("lease_margin_ms"),
    }
    let (items, bytes) = app.snapshots.pending();
    resources::set_gauge(
        "capture_queue_items",
        i64::try_from(items).unwrap_or(i64::MAX),
    );
    resources::set_gauge(
        "capture_queue_bytes",
        i64::try_from(bytes).unwrap_or(i64::MAX),
    );
}

/// The API's completed minutes, or none when the link is off or unreachable.
async fn collect_api_minutes(app: &BotState) -> Vec<Rollup> {
    let Ok(Some(link)) = jmodguard::read_link::try_read_link(
        &app.cfg.bot.read_target,
        app.cfg.bot.control_secret.as_str(),
    ) else {
        return Vec::new();
    };
    let fetch = tokio::task::spawn_blocking(move || link.fetch_resource_rollups_blocking());
    match tokio::time::timeout(jmodguard::read_link::RESOURCE_FETCH_TIMEOUT, fetch).await {
        Ok(Ok(Some(response))) => response.minutes,
        _ => Vec::new(),
    }
}

fn log_dir(app: &BotState) -> Option<std::path::PathBuf> {
    let dir = &app.cfg.log.directory;
    (!dir.as_os_str().is_empty()).then(|| dir.clone())
}

/// Runs until shutdown.
pub async fn run(app: Arc<BotState>, shards: Arc<ShardManager>, cache: Arc<Cache>) {
    let mut builder = RollupBuilder::default();
    let mut backlog: Vec<Rollup> = Vec::new();
    let mut failing = false;
    // The API keeps its recent minutes; send only those not saved yet.
    let mut api_saved_through = i64::MIN;
    let mut tick = tokio::time::interval(jmodguard::cgroup::SAMPLE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = crate::shutdown_signal() => break,
            _ = tick.tick() => {}
        }
        refresh_gauges(&app, &shards, &cache).await;
        let now_secs = chrono::Utc::now().timestamp();
        let Some(minute) = builder.observe(now_secs, jmodguard::cgroup::latest_container_sample())
        else {
            continue;
        };
        backlog.push(minute);
        if backlog.len() > BACKLOG_MINUTES {
            backlog.drain(..backlog.len() - BACKLOG_MINUTES);
        }
        let dir = log_dir(&app);
        if let Ok(disk) =
            tokio::task::spawn_blocking(move || resources::measure_disk(dir.as_deref())).await
        {
            resources::set_disk(disk);
        }
        if !app.activation.may_record() {
            continue;
        }
        let api: Vec<Rollup> = collect_api_minutes(&app)
            .await
            .into_iter()
            .filter(|rollup| rollup.start_secs > api_saved_through)
            .collect();
        let api_through = api.iter().map(|rollup| rollup.start_secs).max();
        let mut minutes: Vec<(ResourceProcess, Rollup)> = backlog
            .iter()
            .cloned()
            .map(|rollup| (ResourceProcess::Bot, rollup))
            .collect();
        minutes.extend(api.into_iter().map(|rollup| (ResourceProcess::Api, rollup)));
        let status = ProcessStatus::current(STATUS_INTERVAL_MS);
        match tokio::time::timeout(
            Duration::from_secs(30),
            db::record_resource_rollups(
                &app.db,
                minutes,
                Some((ResourceProcess::Bot, status)),
                now_secs,
            ),
        )
        .await
        {
            Ok(Ok(_)) => {
                backlog.clear();
                failing = false;
                if let Some(through) = api_through {
                    api_saved_through = through;
                }
            }
            Ok(Err(error)) if !failing => {
                failing = true;
                tracing::warn!("resource rollup save failed; retrying each minute: {error:#}");
            }
            Ok(Err(error)) => tracing::debug!("resource rollup save deferred: {error:#}"),
            Err(_) if !failing => {
                failing = true;
                tracing::warn!("resource rollup save timed out; retrying each minute");
            }
            Err(_) => tracing::debug!("resource rollup save timed out"),
        }
    }
}
