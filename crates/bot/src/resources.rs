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
async fn collect_api_minutes(app: &BotState, saved_through: Option<i64>) -> Vec<Rollup> {
    let Ok(Some(link)) = jmodguard::read_link::try_read_link(
        &app.cfg.bot.read_target,
        app.cfg.bot.control_secret.as_str(),
    ) else {
        return Vec::new();
    };
    let fetch = tokio::task::spawn_blocking(move || {
        link.fetch_resource_rollups_after_blocking(saved_through)
    });
    match tokio::time::timeout(jmodguard::read_link::RESOURCE_FETCH_TIMEOUT, fetch).await {
        Ok(Ok(Some(response))) => response.minutes,
        _ => Vec::new(),
    }
}

fn log_dir(app: &BotState) -> Option<std::path::PathBuf> {
    let dir = &app.cfg.log.directory;
    (!dir.as_os_str().is_empty()).then(|| dir.clone())
}

/// Collects a minute and advances the retry state only after the database commits.
async fn save_recent_minutes(
    app: &BotState,
    backlog: &mut Vec<Rollup>,
    api_saved_through: &mut Option<i64>,
    now_secs: i64,
) -> Result<anyhow::Result<()>, tokio::time::error::Elapsed> {
    let api: Vec<Rollup> = collect_api_minutes(app, *api_saved_through)
        .await
        .into_iter()
        // Older APIs return the full buffer even if asked for a suffix.
        .filter(|rollup| api_saved_through.is_none_or(|saved| rollup.start_secs > saved))
        .collect();
    let mut minutes: Vec<(ResourceProcess, Rollup)> = backlog
        .iter()
        .cloned()
        .map(|rollup| (ResourceProcess::Bot, rollup))
        .collect();
    minutes.extend(api.into_iter().map(|rollup| (ResourceProcess::Api, rollup)));
    let status = ProcessStatus::current(STATUS_INTERVAL_MS);
    let write = tokio::time::timeout(
        Duration::from_secs(30),
        db::record_resource_rollups(
            &app.db,
            minutes,
            Some((ResourceProcess::Bot, status)),
            now_secs,
        ),
    )
    .await?;
    Ok(write.map(|write| {
        let saved: std::collections::HashSet<i64> =
            write.bot_minute_starts.iter().copied().collect();
        backlog.retain(|rollup| !saved.contains(&rollup.start_secs));
        if let Some(through) = write.api_minute_starts.iter().copied().max() {
            *api_saved_through = Some(through);
        }
    }))
}

/// Runs until shutdown.
pub async fn run(
    app: Arc<BotState>,
    shards: Arc<ShardManager>,
    cache: Arc<Cache>,
    poll_start: tokio::time::Instant,
) {
    let mut builder = RollupBuilder::default();
    let mut backlog: Vec<Rollup> = Vec::new();
    let mut failing = false;
    // The API keeps its recent minutes; send only those not saved yet.
    let mut api_saved_through = None;
    let mut tick = jmodguard::idle_poll::IdlePoll::BotResources
        .interval(poll_start, jmodguard::cgroup::SAMPLE_INTERVAL);
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
        match save_recent_minutes(&app, &mut backlog, &mut api_saved_through, now_secs).await {
            Ok(Ok(())) => failing = false,
            Ok(Err(error)) if !failing && !db::is_primary_outage(&error) => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::{
        Body, Response, Server,
        service::{make_service_fn, service_fn},
    };
    use jmodguard::config::{BotProcessConfig, ControlSecret};
    use jmodguard::read_link::{RESOURCE_READ_PATH, ResourceReadResponse};
    use std::sync::Mutex;

    #[tokio::test]
    async fn collection_retries_the_last_committed_api_cursor() {
        const FIRST: i64 = 1_800_000_000;
        const LAST: i64 = FIRST + 120;
        const NOW: i64 = LAST + 60;

        let received = Arc::new(Mutex::new(Vec::new()));
        let peer = Server::bind(&"127.0.0.1:0".parse().unwrap());
        let address = peer.local_addr();
        let cursors = Arc::clone(&received);
        let server = tokio::spawn(peer.serve(make_service_fn(move |_| {
            let cursors = Arc::clone(&cursors);
            async move {
                Ok::<_, hyper::Error>(service_fn(move |req: hyper::Request<Body>| {
                    let cursors = Arc::clone(&cursors);
                    async move {
                        assert_eq!(req.uri().path(), RESOURCE_READ_PATH);
                        let cursor = req.headers().get("x-jmodguard-saved-through").map(|value| {
                            let (minute, _) = value.to_str().unwrap().split_once(':').unwrap();
                            minute.parse::<i64>().unwrap()
                        });
                        hyper::body::to_bytes(req.into_body()).await?;
                        let first_collection = {
                            let mut cursors = cursors.lock().unwrap();
                            cursors.push(cursor);
                            cursors.len() == 1
                        };
                        // Model an older API that returns its whole buffer.
                        let end = if first_collection { FIRST } else { LAST };
                        let payload = ResourceReadResponse {
                            minutes: (FIRST..=end)
                                .step_by(60)
                                .map(|start| Rollup::empty(start, 60))
                                .collect(),
                            copy: "test peer".into(),
                        };
                        Ok::<_, hyper::Error>(Response::new(Body::from(
                            serde_json::to_vec(&payload).unwrap(),
                        )))
                    }
                }))
            }
        })));

        let directory = std::env::temp_dir().join(format!(
            "bot_resource_replay_{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir(&directory).unwrap();
        let db = db::Db::open(&directory.join("core.db"), 1).unwrap();
        let mut cfg = BotProcessConfig::default();
        cfg.bot.read_target = address.to_string();
        cfg.bot.control_secret = ControlSecret::new("r".repeat(32));
        let app = BotState::new(cfg, db).unwrap();

        let mut backlog = Vec::new();
        let mut cursor = None;
        save_recent_minutes(&app, &mut backlog, &mut cursor, NOW)
            .await
            .unwrap()
            .unwrap();

        // The deferred foreign key makes the real write fail at COMMIT.
        app.db
            .with_conn(|conn| {
                conn.execute_batch_bounded(
                    "CREATE TABLE missing_parent (id INTEGER PRIMARY KEY);
                     CREATE TABLE deferred_reference (
                         id INTEGER REFERENCES missing_parent(id) DEFERRABLE INITIALLY DEFERRED
                     );
                     CREATE TRIGGER fail_resource_commit AFTER INSERT ON resource_rollups
                         WHEN NEW.process = 'api' AND NEW.span_secs = 60
                     BEGIN INSERT INTO deferred_reference VALUES (NEW.start_secs); END;",
                    64 * 1024,
                )
            })
            .await
            .unwrap();
        let failure = save_recent_minutes(&app, &mut backlog, &mut cursor, NOW)
            .await
            .unwrap()
            .unwrap_err();
        assert!(format!("{failure:#}").contains("FOREIGN KEY constraint failed"));
        let committed = db::resource_records(&app.db, 60, FIRST, NOW).await.unwrap();
        assert_eq!(committed.api.len(), 1);

        app.db
            .with_conn(|conn| {
                conn.execute_batch_bounded("DROP TRIGGER fail_resource_commit;", 4096)
            })
            .await
            .unwrap();
        for _ in 0..2 {
            save_recent_minutes(&app, &mut backlog, &mut cursor, NOW)
                .await
                .unwrap()
                .unwrap();
        }
        assert_eq!(
            *received.lock().unwrap(),
            [None, Some(FIRST), Some(FIRST), Some(LAST)],
            "the cursor sent on the wire must only advance after a commit"
        );
        let committed = db::resource_records(&app.db, 60, FIRST, NOW).await.unwrap();
        assert_eq!(
            committed
                .api
                .into_iter()
                .map(|m| m.start_secs)
                .collect::<Vec<_>>(),
            [FIRST, FIRST + 60, LAST]
        );

        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        drop(app);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
