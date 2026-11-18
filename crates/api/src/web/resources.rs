//! Owner-only Bot Resources: current samples for the bot and API plus their
//! rolled-up history. The API's current values are read live from this
//! process; the bot's come from the status row it saves each minute. History
//! reads are cached briefly so refreshing the page cannot multiply database
//! reads. No provider billing API is called.
use super::state::ApiState;
use super::usage::authorize;
use axum::Json;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use jmodguard::db::{self, ResourceRecords};
use jmodguard::resources::{
    self, HOUR_SECS, MINUTE_SECS, ProcessStatus, Rollup, RollupBuilder, latency_quantile_ms,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tower_sessions::cookie::time;
use tower_sessions::session::{Id, Record};
use tower_sessions::{MemoryStore, SessionStore, session_store};

type Reply = Result<Json<Value>, (StatusCode, Json<Value>)>;

/// History reads are reused this long; the bot saves one minute at a time.
const HISTORY_TTL: Duration = Duration::from_secs(20);
/// A database read slower than this serves the last history, marked stale.
const HISTORY_DEADLINE: Duration = Duration::from_secs(8);
/// Grace past two sampler intervals before a value is marked stale.
const STALE_GRACE_MS: u64 = 5_000;
/// Label breakdown rows returned per family.
const TOP_LABELS: usize = 12;
/// Sessions tracked for the active-session gauge; matches the store's own growth.
const MAX_TRACKED_SESSIONS: usize = 100_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResourceRange {
    Hour,
    Day,
    Week,
    Month,
}

impl ResourceRange {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "1h" => Some(Self::Hour),
            "24h" => Some(Self::Day),
            "7d" => Some(Self::Week),
            "30d" => Some(Self::Month),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Hour => "1h",
            Self::Day => "24h",
            Self::Week => "7d",
            Self::Month => "30d",
        }
    }

    /// (stored span, window, chart bucket) in seconds. Every range draws at
    /// most 180 points per process.
    fn shape(self) -> (i64, i64, i64) {
        match self {
            Self::Hour => (MINUTE_SECS, 3_600, 60),
            Self::Day => (MINUTE_SECS, 86_400, 600),
            Self::Week => (HOUR_SECS, 7 * 86_400, HOUR_SECS),
            Self::Month => (HOUR_SECS, 30 * 86_400, 4 * HOUR_SECS),
        }
    }
}

struct CachedHistory {
    read_at: Instant,
    /// The window end the rows were read for.
    to_secs: i64,
    records: ResourceRecords,
    /// Set when a refresh could not reach the database; stays until a successful read.
    stale: bool,
}

#[derive(Default)]
pub struct ResourceCache {
    entries: tokio::sync::Mutex<HashMap<ResourceRange, CachedHistory>>,
    gate: tokio::sync::Mutex<()>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourcesQuery {
    range: Option<String>,
}

fn error(status: StatusCode, message: &'static str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error": message})))
}

/// `GET` or `POST /api/owner/resources?range=1h|24h|7d|30d` (default 24h).
pub async fn overview(
    State(app): State<Arc<ApiState>>,
    session: tower_sessions::Session,
    headers: HeaderMap,
    Query(query): Query<ResourcesQuery>,
) -> Reply {
    authorize(&app, &session, &headers).await?;
    let range = ResourceRange::parse(query.range.as_deref().unwrap_or("24h"))
        .ok_or_else(|| error(StatusCode::BAD_REQUEST, "range must be 1h, 24h, 7d or 30d"))?;
    let now_ms = resources::unix_ms_now();
    let (records, history_status) = history(&app, range, now_ms / 1000).await;
    let api_status = ProcessStatus::current(api_interval_ms());
    Ok(Json(build_response(
        range,
        now_ms,
        records.as_ref(),
        history_status,
        &api_status,
    )))
}

fn api_interval_ms() -> u64 {
    u64::try_from(jmodguard::cgroup::SAMPLE_INTERVAL.as_millis()).unwrap_or(5_000)
}

async fn history(
    app: &Arc<ApiState>,
    range: ResourceRange,
    now_secs: i64,
) -> (Option<ResourceRecords>, &'static str) {
    let (span, window, bucket) = range.shape();
    let to_secs = now_secs - now_secs.rem_euclid(bucket) + bucket;
    {
        let cache = app.resource_cache.entries.lock().await;
        if let Some(entry) = cache.get(&range)
            && entry.read_at.elapsed() < HISTORY_TTL
            && entry.to_secs == to_secs
        {
            let status = if entry.stale { "stale" } else { "available" };
            return (Some(entry.records.clone()), status);
        }
    }
    // One history read at a time; a waiting refresh reuses its result.
    let _gate = app.resource_cache.gate.lock().await;
    {
        let cache = app.resource_cache.entries.lock().await;
        if let Some(entry) = cache.get(&range)
            && entry.read_at.elapsed() < HISTORY_TTL
            && entry.to_secs == to_secs
        {
            let status = if entry.stale { "stale" } else { "available" };
            return (Some(entry.records.clone()), status);
        }
    }
    let read = tokio::time::timeout(
        HISTORY_DEADLINE,
        db::resource_records(&app.db, span, to_secs - window, to_secs),
    )
    .await;
    let mut cache = app.resource_cache.entries.lock().await;
    match read {
        Ok(Ok(records)) => {
            cache.insert(
                range,
                CachedHistory {
                    read_at: Instant::now(),
                    to_secs,
                    records: records.clone(),
                    stale: false,
                },
            );
            (Some(records), "available")
        }
        _ => {
            let stale_entry = cache
                .get(&range)
                .filter(|entry| entry.to_secs == to_secs)
                .map(|entry| CachedHistory {
                    read_at: entry.read_at,
                    to_secs: entry.to_secs,
                    records: entry.records.clone(),
                    stale: true,
                });
            if let Some(entry) = stale_entry {
                let records = entry.records.clone();
                cache.insert(range, entry);
                (Some(records), "stale")
            } else {
                (None, "unavailable")
            }
        }
    }
}

fn build_response(
    range: ResourceRange,
    now_ms: i64,
    records: Option<&ResourceRecords>,
    history_status: &str,
    api_status: &ProcessStatus,
) -> Value {
    let (span, window, bucket) = range.shape();
    let now_secs = now_ms / 1000;
    let to_secs = now_secs - now_secs.rem_euclid(bucket) + bucket;
    let from_secs = to_secs - window;
    let bot_status = records.and_then(|records| {
        records
            .statuses
            .iter()
            .find(|(process, _, _)| process == "bot")
            .map(|(_, _, status)| status.clone())
    });
    let process = |status: Option<&ProcessStatus>, rows: Option<&Vec<Rollup>>| {
        json!({
            "current": current(status, now_ms),
            "history": rows.map(|rows| history_json(rows, from_secs, to_secs, bucket)),
        })
    };
    json!({
        "generated_at_ms": now_ms,
        "range": range.label(),
        "span_secs": span,
        "bucket_secs": bucket,
        "from_secs": from_secs,
        "to_secs": to_secs,
        "history_status": history_status,
        "processes": {
            "bot": process(bot_status.as_ref(), records.map(|r| &r.bot)),
            "api": process(Some(api_status), records.map(|r| &r.api)),
        },
        // libsql does not report rows read to the client, and no provider
        // billing API is called. Both stay unavailable rather than estimated.
        "rows_read": {"status": "unavailable", "reason": "not_reported_by_database_client"},
        "provider": {"status": "not_configured", "reason": "no_scoped_read_only_key"},
    })
}

/// The current view with its freshness. A missing status is unavailable;
/// a status older than two refresh intervals is stale.
fn current(status: Option<&ProcessStatus>, now_ms: i64) -> Value {
    let Some(status) = status else {
        return json!({"availability": "unavailable", "stale": false});
    };
    let stale_after_ms = status.interval_ms.saturating_mul(2) + STALE_GRACE_MS;
    let age_ms = status
        .sampled_at_ms
        .map(|at| u64::try_from(now_ms.saturating_sub(at)).unwrap_or(0));
    let mut value = serde_json::to_value(status).unwrap_or(Value::Null);
    value["availability"] = json!(if status.sampled_at_ms.is_some() {
        "available"
    } else {
        "unavailable"
    });
    value["age_ms"] = json!(age_ms);
    value["stale_after_ms"] = json!(stale_after_ms);
    value["stale"] = json!(age_ms.is_none_or(|age| age > stale_after_ms));
    value["uptime_ms"] = json!(
        (status.info.started_at_ms > 0).then(|| now_ms.saturating_sub(status.info.started_at_ms))
    );
    value
}

/// Downsamples stored rollups onto a fixed grid of `bucket`-second points.
/// A bucket without rows has null values (drawn as a gap, never as zero).
fn history_json(rows: &[Rollup], from_secs: i64, to_secs: i64, bucket: i64) -> Value {
    let mut points = Vec::new();
    let mut total = Rollup::empty(from_secs, to_secs - from_secs);
    let mut rows = rows
        .iter()
        .filter(|row| row.start_secs >= from_secs && row.start_secs < to_secs)
        .peekable();
    let mut start = from_secs;
    while start < to_secs {
        let mut merged = Rollup::empty(start, bucket);
        while let Some(row) = rows.next_if(|row| row.start_secs < start + bucket) {
            merged.absorb(row);
            total.absorb(row);
        }
        points.push(point_json(&merged));
        start += bucket;
    }
    let restarts = total
        .data
        .starts
        .iter()
        .filter(|at| **at >= from_secs * 1000)
        .count();
    let window_minutes = (to_secs - from_secs) / 60;
    json!({
        "points": points,
        "minutes": total.data.minutes,
        "coverage": if window_minutes > 0 { f64::from(total.data.minutes) / window_minutes as f64 } else { 0.0 },
        "cpu_avg_tenths": total.cpu_avg_tenths,
        "cpu_peak_tenths": total.cpu_max_tenths,
        "memory_avg_bytes": total.memory_avg_bytes,
        "memory_peak_bytes": total.memory_max_bytes,
        "totals": total.data.counters,
        "per_minute": per_minute(&total),
        "p50_ms": latency_quantile_ms(&total.data.latency, 0.5),
        "p95_ms": latency_quantile_ms(&total.data.latency, 0.95),
        "latency_requests": total.data.latency.iter().sum::<u64>(),
        "breakdowns": breakdowns(&total.data.labelled),
        "gauges_peak": total.data.gauges,
        "restarts": (total.data.minutes > 0).then_some(restarts),
        "starts": total.data.starts,
    })
}

fn per_minute(rollup: &Rollup) -> Map<String, Value> {
    let minutes = f64::from(rollup.data.minutes);
    if minutes == 0.0 {
        return Map::new();
    }
    rollup
        .data
        .counters
        .iter()
        .map(|(name, value)| (name.clone(), json!(*value as f64 / minutes)))
        .collect()
}

fn point_json(rollup: &Rollup) -> Value {
    if rollup.data.minutes == 0 {
        return json!({"t": rollup.start_secs, "minutes": 0});
    }
    json!({
        "t": rollup.start_secs,
        "minutes": rollup.data.minutes,
        "cpu_avg_tenths": rollup.cpu_avg_tenths,
        "cpu_max_tenths": rollup.cpu_max_tenths,
        "memory_avg_bytes": rollup.memory_avg_bytes,
        "memory_max_bytes": rollup.memory_max_bytes,
        "rates": per_minute(rollup),
        "p95_ms": latency_quantile_ms(&rollup.data.latency, 0.95),
        "gauges": rollup.data.gauges,
    })
}

fn breakdowns(labelled: &BTreeMap<String, BTreeMap<String, u64>>) -> Value {
    labelled
        .iter()
        .map(|(family, labels)| {
            let mut rows: Vec<(&String, &u64)> = labels.iter().collect();
            rows.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
            rows.truncate(TOP_LABELS);
            (family.clone(), json!(rows))
        })
        .collect::<Map<String, Value>>()
        .into()
}

/// Counts this API's requests: total, 4xx and 5xx, latency to response
/// headers, and response bytes when the length is known. Liveness and
/// readiness probes are not counted.
pub async fn count_request(request: Request, next: Next) -> Response {
    let probe = matches!(
        request.uri().path(),
        "/health" | "/health/" | "/ready" | "/ready/"
    );
    let started = Instant::now();
    let response = next.run(request).await;
    if probe {
        return response;
    }
    resources::count("api_requests", 1);
    match response.status().as_u16() {
        400..=499 => resources::count("api_4xx", 1),
        500..=599 => resources::count("api_5xx", 1),
        _ => {}
    }
    resources::observe_latency_ms(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
    let length = response
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok()?.parse::<u64>().ok())
        .or_else(|| axum::body::HttpBody::size_hint(response.body()).exact());
    if let Some(length) = length {
        resources::count("api_bytes_out", length);
    }
    response
}

fn tracked_sessions() -> &'static Mutex<HashMap<Id, time::OffsetDateTime>> {
    static SESSIONS: std::sync::OnceLock<Mutex<HashMap<Id, time::OffsetDateTime>>> =
        std::sync::OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Unexpired sessions this process has stored.
fn active_sessions() -> usize {
    let now = time::OffsetDateTime::now_utc();
    let mut sessions = tracked_sessions()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    sessions.retain(|_, expiry| *expiry > now);
    sessions.len()
}

fn track_session(record: &Record) {
    let mut sessions = tracked_sessions()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if sessions.len() >= MAX_TRACKED_SESSIONS {
        let now = time::OffsetDateTime::now_utc();
        sessions.retain(|_, expiry| *expiry > now);
    }
    if sessions.len() < MAX_TRACKED_SESSIONS || sessions.contains_key(&record.id) {
        sessions.insert(record.id, record.expiry_date);
    }
}

fn forget_session(id: &Id) {
    tracked_sessions()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(id);
}

/// The in-memory session store, observed for the active-session count.
#[derive(Clone, Debug)]
pub struct CountedStore(pub MemoryStore);

#[async_trait::async_trait]
impl SessionStore for CountedStore {
    async fn create(&self, record: &mut Record) -> session_store::Result<()> {
        self.0.create(record).await?;
        track_session(record);
        Ok(())
    }

    async fn save(&self, record: &Record) -> session_store::Result<()> {
        self.0.save(record).await?;
        track_session(record);
        Ok(())
    }

    async fn load(&self, id: &Id) -> session_store::Result<Option<Record>> {
        let record = self.0.load(id).await?;
        match &record {
            None => forget_session(id),
            Some(record) => track_session(record),
        }
        Ok(record)
    }

    async fn delete(&self, id: &Id) -> session_store::Result<()> {
        self.0.delete(id).await?;
        forget_session(id);
        Ok(())
    }
}

/// Folds this API's samples into minutes kept for the bot to collect.
pub async fn run_rollups(log_dir: Option<std::path::PathBuf>) {
    let mut builder = RollupBuilder::default();
    let mut tick = tokio::time::interval(jmodguard::cgroup::SAMPLE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = jmodguard::shutdown_signal() => break,
            _ = tick.tick() => {}
        }
        resources::set_gauge(
            "sessions_active",
            i64::try_from(active_sessions()).unwrap_or(i64::MAX),
        );
        let now_secs = resources::unix_ms_now() / 1000;
        let Some(minute) = builder.observe(now_secs, jmodguard::cgroup::latest_container_sample())
        else {
            continue;
        };
        resources::publish_recent(minute);
        let dir = log_dir.clone();
        if let Ok(disk) =
            tokio::task::spawn_blocking(move || resources::measure_disk(dir.as_deref())).await
        {
            resources::set_disk(disk);
        }
    }
}

#[cfg(test)]
mod tests;
