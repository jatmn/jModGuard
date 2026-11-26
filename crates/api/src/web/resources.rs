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
use axum::response::{IntoResponse, Response};
use jmodguard::cgroup::CgroupRelation;
use jmodguard::config::HostingEstimateConfig;
use jmodguard::db::{self, ResourceRecords};
use jmodguard::resources::{
    self, HOUR_SECS, MINUTE_SECS, ProcessStatus, Rollup, RollupBuilder, latency_quantile_ms,
};
use serde::{Deserialize, Serialize};
use serde_json::value::{RawValue, to_raw_value};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tower_sessions::cookie::time;
use tower_sessions::session::{Id, Record};
use tower_sessions::{MemoryStore, SessionStore, session_store};

type Reply = Result<Response, (StatusCode, Json<Value>)>;

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
    /// Completion time of the last read attempt, including failures.
    checked_at: Instant,
    to_secs: i64,
    prepared: Option<Arc<PreparedHistory>>,
    stale: bool,
}

impl CachedHistory {
    fn result(&self) -> (Option<Arc<PreparedHistory>>, &'static str) {
        let status = match (&self.prepared, self.stale) {
            (None, _) => "unavailable",
            (Some(_), true) => "stale",
            (Some(_), false) => "available",
        };
        (self.prepared.clone(), status)
    }
}

/// Encoded history and saved bot status from one database snapshot.
struct PreparedHistory {
    bot: Box<RawValue>,
    api: Box<RawValue>,
    bot_status: Option<ProcessStatus>,
}

impl PreparedHistory {
    fn new(
        records: ResourceRecords,
        from_secs: i64,
        to_secs: i64,
        bucket: i64,
    ) -> serde_json::Result<Self> {
        Ok(Self {
            bot: to_raw_value(&history_json(&records.bot, from_secs, to_secs, bucket))?,
            api: to_raw_value(&history_json(&records.api, from_secs, to_secs, bucket))?,
            bot_status: records
                .statuses
                .into_iter()
                .find(|(process, _, _)| process == "bot")
                .map(|(_, _, status)| status),
        })
    }
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
    let (prepared, history_status) = history(&app, range, now_ms / 1000).await;
    let api_status = ProcessStatus::current(api_interval_ms());
    Ok(build_response(
        range,
        now_ms,
        prepared.as_deref(),
        history_status,
        &api_status,
        &app.cfg.web.hosting_estimate,
    ))
}

fn memory_billing(config: &HostingEstimateConfig) -> Value {
    let rates = config.enabled.then(|| {
        json!({
            "memory_usd_per_block_hour": config.memory_usd_per_block_hour,
            "cpu_usd_per_core_hour": config.cpu_usd_per_core_hour,
        })
    });
    json!({
        "block_mib": HostingEstimateConfig::MEMORY_BLOCK_MIB,
        "rates": rates,
    })
}

fn api_interval_ms() -> u64 {
    u64::try_from(jmodguard::cgroup::SAMPLE_INTERVAL.as_millis()).unwrap_or(5_000)
}

async fn history(
    app: &Arc<ApiState>,
    range: ResourceRange,
    now_secs: i64,
) -> (Option<Arc<PreparedHistory>>, &'static str) {
    let (span, window, bucket) = range.shape();
    let to_secs = now_secs - now_secs.rem_euclid(bucket) + bucket;
    {
        let cache = app.resource_cache.entries.lock().await;
        if let Some(entry) = cache.get(&range)
            && entry.checked_at.elapsed() < HISTORY_TTL
            && entry.to_secs == to_secs
        {
            return entry.result();
        }
    }
    // One history refresh at a time; a waiting reader reuses its result.
    let _gate = app.resource_cache.gate.lock().await;
    {
        let cache = app.resource_cache.entries.lock().await;
        if let Some(entry) = cache.get(&range)
            && entry.checked_at.elapsed() < HISTORY_TTL
            && entry.to_secs == to_secs
        {
            return entry.result();
        }
    }
    let from_secs = to_secs - window;
    let read = tokio::time::timeout(
        HISTORY_DEADLINE,
        db::resource_records(&app.db, span, from_secs, to_secs),
    )
    .await;
    let checked_at = Instant::now();
    let prepared = match read {
        Ok(Ok(records)) => PreparedHistory::new(records, from_secs, to_secs, bucket)
            .ok()
            .map(Arc::new),
        _ => None,
    };
    let mut cache = app.resource_cache.entries.lock().await;
    let (prepared, stale) = match prepared {
        Some(prepared) => (Some(prepared), false),
        None => {
            let last = cache
                .remove(&range)
                .filter(|entry| entry.to_secs == to_secs);
            (last.and_then(|entry| entry.prepared), true)
        }
    };
    let entry = cache.entry(range).insert_entry(CachedHistory {
        checked_at,
        to_secs,
        prepared,
        stale,
    });
    entry.get().result()
}

#[derive(Serialize)]
struct ResourcesOverview<'a> {
    bucket_secs: i64,
    cgroup_relation: Value,
    from_secs: i64,
    generated_at_ms: i64,
    history_status: &'static str,
    memory_billing: Value,
    processes: ResourceProcesses<'a>,
    provider: Value,
    range: &'static str,
    rows_read: Value,
    span_secs: i64,
    to_secs: i64,
}

#[derive(Serialize)]
struct ResourceProcesses<'a> {
    api: ResourceProcessView<'a>,
    bot: ResourceProcessView<'a>,
}

#[derive(Serialize)]
struct ResourceProcessView<'a> {
    current: Value,
    history: Option<&'a RawValue>,
}

fn build_response(
    range: ResourceRange,
    now_ms: i64,
    prepared: Option<&PreparedHistory>,
    history_status: &'static str,
    api_status: &ProcessStatus,
    estimate: &HostingEstimateConfig,
) -> Response {
    let (span, window, bucket) = range.shape();
    let now_secs = now_ms / 1000;
    let to_secs = now_secs - now_secs.rem_euclid(bucket) + bucket;
    let from_secs = to_secs - window;
    let bot_status = prepared.and_then(|history| history.bot_status.as_ref());
    let (memory_relation, cpu_relation) =
        bot_status.map_or((CgroupRelation::Unknown, CgroupRelation::Unknown), |bot| {
            (
                bot.cgroup_scopes.memory_relation(&api_status.cgroup_scopes),
                bot.cgroup_scopes.cpu_relation(&api_status.cgroup_scopes),
            )
        });
    let estimate_relation = |relation| match relation {
        CgroupRelation::Unverified if estimate.assume_disjoint_cgroups => {
            json!("operator_declared_disjoint")
        }
        _ => json!(relation),
    };
    Json(ResourcesOverview {
        bucket_secs: bucket,
        cgroup_relation: json!({
            "memory": estimate_relation(memory_relation),
            "cpu": estimate_relation(cpu_relation),
        }),
        from_secs,
        generated_at_ms: now_ms,
        history_status,
        memory_billing: memory_billing(estimate),
        processes: ResourceProcesses {
            api: ResourceProcessView {
                current: current(Some(api_status), now_ms),
                history: prepared.map(|history| history.api.as_ref()),
            },
            bot: ResourceProcessView {
                current: current(bot_status, now_ms),
                history: prepared.map(|history| history.bot.as_ref()),
            },
        },
        // libsql does not report rows read to the client, and no provider
        // billing API is called. Both stay unavailable rather than estimated.
        provider: json!({"status": "not_configured", "reason": "no_scoped_read_only_key"}),
        range: range.label(),
        rows_read: json!({"status": "unavailable", "reason": "not_reported_by_database_client"}),
        span_secs: span,
        to_secs,
    })
    .into_response()
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
    json!({
        "sampled_at_ms": status.sampled_at_ms,
        "interval_ms": status.interval_ms,
        "info": status.info,
        "cpu_tenths": status.cpu_tenths,
        "memory": status.memory,
        "disk": status.disk,
        "gauges": status.gauges,
        "availability": if status.sampled_at_ms.is_some() { "available" } else { "unavailable" },
        "age_ms": age_ms,
        "stale_after_ms": stale_after_ms,
        "stale": age_ms.is_none_or(|age| age > stale_after_ms),
        "uptime_ms": (status.info.started_at_ms > 0).then(|| now_ms.saturating_sub(status.info.started_at_ms)),
    })
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
pub async fn run_rollups(log_dir: Option<std::path::PathBuf>, poll_start: tokio::time::Instant) {
    let mut builder = RollupBuilder::default();
    let mut tick = jmodguard::idle_poll::IdlePoll::ApiResources
        .interval(poll_start, jmodguard::cgroup::SAMPLE_INTERVAL);
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
