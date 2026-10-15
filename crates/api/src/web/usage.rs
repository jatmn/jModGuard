//! Owner-only physical database usage. Cache lives after authorization and
//! coalesces concurrent browsers so inspection does not amplify remote reads.
use super::state::ApiState;
use super::{session_owner, trusted_web_origin};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use dashmap::DashMap;
use jmodguard::capacity::{
    DATABASE_LIMIT_BYTES, GROWTH_STOP_BYTES, PRESSURE_BYTES, SNAPSHOT_BUDGET_BYTES,
    SNAPSHOT_PAUSE_BYTES,
};
use jmodguard::db::guild_stores::{StorePolicy, StoreRegistration};
use jmodguard::db::usage::{DatabaseUsage, measure_database_usage};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower_sessions::Session;

const OVERVIEW_TTL: Duration = Duration::from_secs(60);
const DETAIL_TTL: Duration = Duration::from_secs(300);

pub struct UsageCache {
    pub state: tokio::sync::Mutex<CacheState>,
    overview_gate: tokio::sync::Mutex<()>,
    detail_gates: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
    probes: Arc<tokio::sync::Semaphore>,
}

impl Default for UsageCache {
    fn default() -> Self {
        Self {
            state: tokio::sync::Mutex::new(CacheState::default()),
            overview_gate: tokio::sync::Mutex::new(()),
            detail_gates: DashMap::new(),
            probes: Arc::new(tokio::sync::Semaphore::new(2)),
        }
    }
}

#[derive(Default)]
pub struct CacheState {
    overview: Option<(Instant, Value)>,
    detail: HashMap<String, (Instant, Value)>,
}

fn error(status: StatusCode, message: &'static str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error":message})))
}

type Reply = Result<Json<Value>, (StatusCode, Json<Value>)>;

async fn authorize(
    app: &Arc<ApiState>,
    session: &Session,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, Json<Value>)> {
    match session_owner(app, session).await {
        Ok(_) => {}
        Err(failure) if failure.0.is::<super::SessionStoreUnavailable>() => {
            return Err(error(
                StatusCode::SERVICE_UNAVAILABLE,
                "session store unavailable",
            ));
        }
        Err(failure) if failure.0.is::<super::AuthLost>() => {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(json!({"error":"discord authorization failed", "code":"auth_lost"})),
            ));
        }
        Err(failure) if failure.0.is::<super::NotAuthenticated>() => {
            return Err(error(StatusCode::UNAUTHORIZED, "sign in required"));
        }
        Err(_) => return Err(error(StatusCode::FORBIDDEN, "bot owner required")),
    }
    if !trusted_web_origin(&app.cfg, headers) {
        return Err(error(StatusCode::FORBIDDEN, "untrusted request origin"));
    }
    Ok(())
}

fn policy(policy: StorePolicy) -> Value {
    match policy {
        StorePolicy::Shared => json!({"kind":"shared"}),
        StorePolicy::Dedicated { guild_id } => {
            json!({"kind":"dedicated","guild_id":guild_id.to_string()})
        }
    }
}

fn measured(
    id: String,
    kind: &str,
    policy: Value,
    usage: Option<DatabaseUsage>,
    stale: bool,
    error_code: Option<&str>,
    runtime: Option<jmodguard::db::CapacityRuntimeStatus>,
) -> Value {
    let allocated = usage.as_ref().map(|u| u.allocated_bytes);
    let status = match allocated {
        None => "unavailable",
        Some(n) if n >= GROWTH_STOP_BYTES => "growth_stopped",
        Some(n) if n >= SNAPSHOT_PAUSE_BYTES => "capture_paused",
        Some(n) if n >= PRESSURE_BYTES => "pressure",
        Some(_) => "normal",
    };
    let status = if stale {
        "stale"
    } else if runtime.as_ref().is_some_and(|r| r.growth_paused) {
        "application_paused"
    } else {
        status
    };
    let health = usage.as_ref().and_then(|u| u.snapshot_health.as_ref());
    let now_ms = chrono::Utc::now().timestamp_millis();
    let heartbeat_age_ms = health
        .and_then(|h| h.updated_at_ms)
        .and_then(|at| (at <= now_ms).then_some(now_ms - at));
    let capture_status = match health {
        Some(h)
            if h.updated_at_ms
                .is_some_and(|at| at <= now_ms && now_ms - at <= 300_000) =>
        {
            if h.capacity_paused {
                "paused"
            } else {
                h.state.as_str()
            }
        }
        Some(_) => "stale",
        None => "unavailable",
    };
    let cleanup_status = match health {
        Some(h)
            if !h
                .updated_at_ms
                .is_some_and(|at| at <= now_ms && now_ms - at <= 300_000) =>
        {
            "stale"
        }
        Some(h) if h.reason.as_deref() == Some("cleanup_failed") => "failed",
        Some(h)
            if h.last_cleanup_at_ms
                .is_some_and(|at| at <= now_ms && now_ms - at <= 900_000) =>
        {
            "recent"
        }
        Some(h) if h.last_cleanup_at_ms.is_some() => "stale",
        _ => "unavailable",
    };
    let last_success_at = usage.as_ref().map(|u| u.sampled_at);
    let allocated_percent = allocated.map(|n| n as f64 * 100.0 / DATABASE_LIMIT_BYTES as f64);
    // Bot admission considers physical attribution and debt that this API
    // cannot verify across processes. Do not present a derived estimate as a
    // checked write allowance.
    let effective_allowance: Option<u64> = None;
    let label = if kind == "core" {
        "Core database"
    } else if policy["kind"] == "shared" {
        "Shared guild database"
    } else {
        "Dedicated guild database"
    };
    json!({
        "id":id,"kind":kind,"role":kind,"label":label,"policy":policy,
        "availability":if usage.is_some() {"available"} else {"unavailable"},
        "status":status,"stale":stale,
        "measurement":{"source":"sqlite_primary_pragma","sampled_at":last_success_at,
            "last_success_at":last_success_at,"stale":stale,
            "capability":if usage.is_some() {"available"} else {"unavailable"},
            "error_code":error_code},
        "provider_usage":{"bytes":null,"sampled_at":null,"source":"unavailable"},
        "limit_bytes":DATABASE_LIMIT_BYTES,
        "policy_limits":{"provider_hard_limit_bytes":DATABASE_LIMIT_BYTES,
            "pressure_bytes":PRESSURE_BYTES,"capture_pause_bytes":SNAPSHOT_PAUSE_BYTES,
            "growth_stop_bytes":GROWTH_STOP_BYTES,"snapshot_budget_bytes":SNAPSHOT_BUDGET_BYTES,
            "effective_snapshot_allowance_bytes":effective_allowance},
        "application_admission":{"api_process_growth_paused":runtime.as_ref().map(|r|r.growth_paused),
            "api_startup_deferred":runtime.as_ref().map(|r|r.startup_deferred),
            "api_reason":runtime.as_ref().and_then(|r|r.reason),
            "bot_cross_process_guard":"unknown","last_successful_check_at":null,
            "last_primary_sample_at":last_success_at,
            "source":"api_process_runtime_state"},
        "snapshot_budget_bytes":SNAPSHOT_BUDGET_BYTES,
        "snapshot_usage_estimate_bytes":health.map(|h| h.estimated_snapshot_bytes),
        "headroom_bytes":allocated.map(|n| DATABASE_LIMIT_BYTES.saturating_sub(n)),
        "percent_of_limit":allocated_percent,
        "usage":usage,
        "capture_status":capture_status,
        "cleanup_status":cleanup_status,
        "capture":{"status":capture_status,"reason":health.and_then(|h|h.reason.as_deref()),
            "heartbeat_age_ms":heartbeat_age_ms,"configured_retention_hours":24,
            "oldest_available_snapshot_at_ms":usage.as_ref().and_then(|u|u.oldest_snapshot_at_ms),
            "cleanup_status":cleanup_status,"epoch":health.map(|h|h.epoch),
            "counters_reset_at_ms":health.and_then(|h|h.counters_reset_at_ms),
            "retained_count":usage.as_ref().and_then(|u|u.snapshot_count),
            "lost_count":health.map(|h|h.lost_count),"expired_count":health.map(|h|h.expired_count),
            "evicted_count":health.map(|h|h.evicted_count)},
        "external_evidence_storage":{"bytes":null,"sampled_at":null,"source":"unavailable"}
    })
}

const PROBE_DEADLINE: Duration = Duration::from_secs(8);
const FLEET_DEADLINE: Duration = Duration::from_secs(25);

async fn probe_core(app: &Arc<ApiState>, details: bool) -> Value {
    let db = app.db.clone();
    let runtime = db.capacity_runtime_status();
    let slots = app.usage_cache.probes.clone();
    let work = tokio::spawn(async move {
        let _permit = tokio::time::timeout(PROBE_DEADLINE, slots.acquire_owned())
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
        measure_database_usage(&db, details, None)
            .await
            .map_err(|_| ())
    });
    let result = tokio::time::timeout(PROBE_DEADLINE, work).await;
    let code = match &result {
        Err(_) => Some("timeout"),
        Ok(Ok(Err(_))) => Some("measurement_failed"),
        Ok(Err(_)) => Some("probe_failed"),
        _ => None,
    };
    measured(
        "core".into(),
        "core",
        Value::Null,
        result.ok().and_then(Result::ok).and_then(Result::ok),
        false,
        code,
        Some(runtime),
    )
}

async fn probe_guild(app: &Arc<ApiState>, id: &str, details: bool) -> Result<Option<Value>, ()> {
    // The selected ID is validated in the primary catalog before any handle opens.
    let tenant = app.tenant.clone();
    let slots = app.usage_cache.probes.clone();
    let selected = id.to_owned();
    let work = tokio::spawn(async move {
        let _permit = tokio::time::timeout(PROBE_DEADLINE, slots.acquire_owned())
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
        let store = tenant.usage_store(&selected).await.map_err(|_| ())?;
        let Some(store) = store else { return Ok(None) };
        let registration = store.registration;
        let (usage, runtime, error_code) = match store.handle {
            Some(handle) => {
                let runtime = handle.capacity_runtime_status();
                match handle.measure_usage(details).await {
                    Ok(usage) => (Some(usage), Some(runtime), None),
                    Err(_) => (None, Some(runtime), Some("measurement_failed")),
                }
            }
            None => (None, None, Some("store_unconfigured_or_unavailable")),
        };
        Ok::<_, ()>(Some((registration, usage, runtime, error_code)))
    });
    let store = tokio::time::timeout(PROBE_DEADLINE, work)
        .await
        .map_err(|_| ())?
        .map_err(|_| ())?
        .map_err(|_| ())?;
    let Some(store) = store else { return Ok(None) };
    Ok(Some(measured(
        id.to_owned(),
        "guild",
        policy(store.0.policy()),
        store.1,
        false,
        store.3,
        store.2,
    )))
}

async fn catalog(app: &Arc<ApiState>) -> Result<Vec<StoreRegistration>, ()> {
    let tenant = app.tenant.clone();
    let slots = app.usage_cache.probes.clone();
    let work = tokio::spawn(async move {
        let _permit = tokio::time::timeout(PROBE_DEADLINE, slots.acquire_owned())
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
        tenant.usage_catalog().await.map_err(|_| ())
    });
    tokio::time::timeout(PROBE_DEADLINE, work)
        .await
        .map_err(|_| ())?
        .map_err(|_| ())?
        .map_err(|_| ())
}

async fn records(app: &Arc<ApiState>, details: bool, only: Option<&str>) -> Result<Vec<Value>, ()> {
    let began = Instant::now();
    if only == Some("core") {
        return Ok(vec![probe_core(app, details).await]);
    }
    if let Some(id) = only {
        return Ok(probe_guild(app, id, details).await?.into_iter().collect());
    }
    let catalog = catalog(app).await?;
    let mut rows = vec![probe_core(app, false).await];
    // At most two physical stores are in flight. The core entry remains usable
    // even when a guild endpoint times out.
    let mut next = 0;
    while next < catalog.len() {
        if began.elapsed() >= FLEET_DEADLINE {
            for registration in &catalog[next..] {
                rows.push(measured(
                    registration.id().as_str().to_owned(),
                    "guild",
                    policy(registration.policy()),
                    None,
                    false,
                    Some("fleet_deadline"),
                    None,
                ));
            }
            break;
        }
        let left = catalog[next].id().as_str().to_owned();
        let right = catalog.get(next + 1).map(|r| r.id().as_str().to_owned());
        let remaining = FLEET_DEADLINE.saturating_sub(began.elapsed());
        let work = async {
            if let Some(right) = right.as_deref() {
                let (a, b) = tokio::join!(
                    probe_guild(app, &left, false),
                    probe_guild(app, right, false)
                );
                (a, Some(b))
            } else {
                (probe_guild(app, &left, false).await, None)
            }
        };
        match tokio::time::timeout(remaining, work).await {
            Ok((a, b)) => {
                rows.push(a.ok().flatten().unwrap_or_else(|| {
                    measured(
                        left,
                        "guild",
                        policy(catalog[next].policy()),
                        None,
                        false,
                        Some("catalog_or_probe_failed"),
                        None,
                    )
                }));
                if let Some(b) = b {
                    rows.push(b.ok().flatten().unwrap_or_else(|| {
                        measured(
                            right.unwrap(),
                            "guild",
                            policy(catalog[next + 1].policy()),
                            None,
                            false,
                            Some("catalog_or_probe_failed"),
                            None,
                        )
                    }));
                }
            }
            Err(_) => {
                rows.push(measured(
                    left,
                    "guild",
                    policy(catalog[next].policy()),
                    None,
                    false,
                    Some("fleet_deadline"),
                    None,
                ));
                if let Some(right) = right {
                    rows.push(measured(
                        right,
                        "guild",
                        policy(catalog[next + 1].policy()),
                        None,
                        false,
                        Some("fleet_deadline"),
                        None,
                    ));
                }
            }
        }
        next += 2;
    }
    Ok(rows)
}

fn stale(mut prior: Value) -> Value {
    prior["stale"] = json!(true);
    prior["status"] = json!("stale");
    prior["capture_status"] = json!("stale");
    prior["cleanup_status"] = json!("stale");
    prior["availability"] = json!("unavailable");
    prior["measurement"]["stale"] = json!(true);
    prior["measurement"]["error_code"] = json!("latest_probe_failed");
    prior["capture"]["status"] = json!("unknown");
    prior["capture"]["cleanup_status"] = json!("unknown");
    prior["capture"]["heartbeat_age_ms"] = prior["usage"]["snapshot_health"]["updated_at_ms"]
        .as_i64()
        .and_then(|at| {
            (at <= chrono::Utc::now().timestamp_millis())
                .then_some(chrono::Utc::now().timestamp_millis() - at)
        })
        .map_or(Value::Null, |age| json!(age));
    prior["application_admission"]["bot_cross_process_guard"] = json!("unknown");
    prior["application_admission"]["api_process_growth_paused"] = Value::Null;
    prior["application_admission"]["api_startup_deferred"] = Value::Null;
    prior
}

fn preserve_failed_samples(current: &mut [Value], prior: &Value) {
    let Some(old) = prior.get("databases").and_then(Value::as_array) else {
        return;
    };
    for record in current {
        if record.get("usage").is_some_and(Value::is_null)
            && let Some(previous) = old.iter().find(|row| row.get("id") == record.get("id"))
            && previous.get("usage").is_some_and(|usage| !usage.is_null())
        {
            *record = stale(previous.clone());
        }
    }
}

/// POST is used for private owner reads, matching the Administration fetch
/// conventions. Origin is checked before the cache, on every request.
pub async fn overview(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
) -> Reply {
    authorize(&app, &session, &headers).await?;
    {
        let cache = app.usage_cache.state.lock().await;
        if let Some((at, value)) = &cache.overview
            && at.elapsed() < OVERVIEW_TTL
        {
            return Ok(Json(value.clone()));
        }
    }
    let _gate = tokio::time::timeout(
        FLEET_DEADLINE + Duration::from_secs(1),
        app.usage_cache.overview_gate.lock(),
    )
    .await
    .map_err(|_| error(StatusCode::SERVICE_UNAVAILABLE, "database overview busy"))?;
    {
        let cache = app.usage_cache.state.lock().await;
        if let Some((at, value)) = &cache.overview
            && at.elapsed() < OVERVIEW_TTL
        {
            return Ok(Json(value.clone()));
        }
    }
    let mut databases = match records(&app, false, None).await {
        Ok(rows) => rows,
        Err(()) => {
            let mut cache = app.usage_cache.state.lock().await;
            let fallback = cache.overview.as_ref().map(|(_, value)| {
                let mut value = value.clone();
                if let Some(rows) = value["databases"].as_array_mut() {
                    for row in rows {
                        *row = stale(row.clone());
                    }
                }
                value["catalog_status"] = json!("unavailable");
                value
            });
            if let Some(value) = fallback {
                cache.overview = Some((Instant::now(), value.clone()));
                return Ok(Json(value));
            }
            drop(cache);
            let core = probe_core(&app, false).await;
            return Ok(Json(
                json!({"databases":[core],"catalog_status":"unavailable",
                "generated_at":chrono::Utc::now().timestamp()}),
            ));
        }
    };
    let mut cache = app.usage_cache.state.lock().await;
    if let Some((_, previous)) = &cache.overview {
        preserve_failed_samples(&mut databases, previous);
    }
    let response = json!({"databases":databases,"catalog_status":"available",
        "generated_at":chrono::Utc::now().timestamp()});
    cache.overview = Some((Instant::now(), response.clone()));
    Ok(Json(response))
}

pub async fn detail(
    State(app): State<Arc<ApiState>>,
    Path(id): Path<String>,
    session: Session,
    headers: HeaderMap,
) -> Reply {
    authorize(&app, &session, &headers).await?;
    if id != "core"
        && (id.len() != 32
            || !id
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)))
    {
        return Err(error(StatusCode::NOT_FOUND, "database not found"));
    }
    {
        let cache = app.usage_cache.state.lock().await;
        if let Some((at, value)) = cache.detail.get(&id)
            && at.elapsed() < DETAIL_TTL
        {
            return Ok(Json(value.clone()));
        }
    }
    let gate = if app.usage_cache.detail_gates.len() < 128
        || app.usage_cache.detail_gates.contains_key(&id)
    {
        app.usage_cache
            .detail_gates
            .entry(id.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    } else {
        Arc::new(tokio::sync::Mutex::new(()))
    };
    let _gate = tokio::time::timeout(PROBE_DEADLINE + Duration::from_secs(1), gate.lock())
        .await
        .map_err(|_| error(StatusCode::SERVICE_UNAVAILABLE, "database detail busy"))?;
    {
        let cache = app.usage_cache.state.lock().await;
        if let Some((at, value)) = cache.detail.get(&id)
            && at.elapsed() < DETAIL_TTL
        {
            return Ok(Json(value.clone()));
        }
    }
    let mut found = match records(&app, true, Some(&id)).await {
        Ok(rows) => rows,
        Err(()) => {
            let mut cache = app.usage_cache.state.lock().await;
            if let Some((_, previous)) = cache.detail.get(&id) {
                let value = stale(previous.clone());
                cache.detail.insert(id, (Instant::now(), value.clone()));
                return Ok(Json(value));
            }
            return Err(error(
                StatusCode::SERVICE_UNAVAILABLE,
                "database catalog unavailable",
            ));
        }
    };
    let Some(response) = found.pop() else {
        return Err(error(StatusCode::NOT_FOUND, "database not found"));
    };
    let mut cache = app.usage_cache.state.lock().await;
    let response = if response.get("usage").is_some_and(Value::is_null) {
        cache
            .detail
            .get(&id)
            .map(|(_, value)| stale(value.clone()))
            .unwrap_or(response)
    } else {
        response
    };
    if cache.detail.len() >= 128 {
        cache.detail.clear();
    }
    cache.detail.insert(id, (Instant::now(), response.clone()));
    Ok(Json(response))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use axum::http::header::ORIGIN;
    use jmodguard::db::Db;
    use tower_sessions::MemoryStore;

    #[test]
    fn failed_probe_keeps_previous_measurement_but_marks_it_stale() {
        let old = json!({"databases":[
            {"id":"core","usage":{"allocated_bytes":8192},"status":"normal","capture_status":"active","cleanup_status":"recent","stale":false},
            {"id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","usage":{"allocated_bytes":4096},"status":"normal","stale":false}
        ]});
        let mut current = vec![
            json!({"id":"core","usage":null,"status":"unavailable"}),
            json!({"id":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","usage":null,"status":"unavailable"}),
        ];
        preserve_failed_samples(&mut current, &old);
        assert_eq!(current[0]["usage"]["allocated_bytes"], 8192);
        assert_eq!(current[0]["status"], "stale");
        assert_eq!(current[0]["capture_status"], "stale");
        assert_eq!(current[0]["measurement"]["stale"], true);
        assert_eq!(
            current[0]["measurement"]["error_code"],
            "latest_probe_failed"
        );
        assert_eq!(current[1]["status"], "unavailable");
    }

    fn fixture() -> (Arc<ApiState>, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_api_usage_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = Db::open(&path, 2).unwrap();
        let mut cfg = jmodguard::config::ApiProcessConfig::default();
        cfg.bot.owner_id = 99;
        cfg.db.path = path.clone();
        (ApiState::new(cfg, db).unwrap(), path)
    }

    async fn signed_in(app: &ApiState, user_id: u64) -> Session {
        let session = Session::new(None, Arc::new(MemoryStore::default()), None);
        session
            .insert(super::super::SESSION_USER, user_id)
            .await
            .unwrap();
        session
            .insert(super::super::SESSION_TOKEN, "test-token")
            .await
            .unwrap();
        let now = Instant::now();
        app.user_cache.insert(
            user_id,
            (
                now,
                super::super::oauth::DiscordUser {
                    id: user_id.to_string(),
                    username: "tester".into(),
                    global_name: None,
                    avatar: None,
                },
            ),
        );
        app.guild_cache.insert(user_id, (now, Vec::new()));
        session
    }

    #[tokio::test]
    async fn owner_read_rejects_anonymous_nonowner_and_untrusted_origin() {
        let (app, path) = fixture();
        let mut headers = HeaderMap::new();
        let origin = reqwest::Url::parse(&app.cfg.web.base_url)
            .unwrap()
            .origin()
            .ascii_serialization();
        headers.insert(ORIGIN, HeaderValue::from_str(&origin).unwrap());
        let anonymous = Session::new(None, Arc::new(MemoryStore::default()), None);
        assert_eq!(
            authorize(&app, &anonymous, &headers).await.unwrap_err().0,
            StatusCode::UNAUTHORIZED
        );
        let nonowner = signed_in(&app, 42).await;
        assert_eq!(
            authorize(&app, &nonowner, &headers).await.unwrap_err().0,
            StatusCode::FORBIDDEN
        );
        let owner = signed_in(&app, 99).await;
        assert!(authorize(&app, &owner, &headers).await.is_ok());
        assert_eq!(
            authorize(&app, &owner, &HeaderMap::new())
                .await
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
        drop((owner, nonowner, anonymous, app));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        jmodguard::tenant::remove_sidecars(&path);
    }

    #[tokio::test]
    async fn authorization_runs_before_cached_owner_data() {
        let (app, path) = fixture();
        app.usage_cache.state.lock().await.overview =
            Some((Instant::now(), json!({"secret":"owner-data"})));
        let session = signed_in(&app, 42).await;
        let mut headers = HeaderMap::new();
        let origin = reqwest::Url::parse(&app.cfg.web.base_url)
            .unwrap()
            .origin()
            .ascii_serialization();
        headers.insert(ORIGIN, HeaderValue::from_str(&origin).unwrap());
        assert_eq!(
            overview(State(app.clone()), session, headers)
                .await
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
        drop(app);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        jmodguard::tenant::remove_sidecars(&path);
    }

    #[tokio::test]
    async fn selected_core_detail_does_not_require_guild_catalog() {
        let (app, path) = fixture();
        app.db
            .with_conn(|conn| {
                conn.execute_batch_bounded("DROP TABLE guild_store_catalog", 64_000_000)?;
                Ok(())
            })
            .await
            .unwrap();
        let core = records(&app, true, Some("core")).await.unwrap();
        assert_eq!(core.len(), 1);
        assert_eq!(core[0]["id"], "core");
        assert_eq!(core[0]["availability"], "available");
        assert!(records(&app, false, None).await.is_err());
        drop(app);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        jmodguard::tenant::remove_sidecars(&path);
    }

    #[tokio::test]
    async fn selected_guild_detail_returns_only_its_catalog_entry() {
        use jmodguard::db::guild_stores::{StoreId, register_store};
        let (app, path) = fixture();
        let selected = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let unrelated = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        register_store(
            &app.db,
            StoreId::parse(selected).unwrap(),
            StorePolicy::Shared,
        )
        .await
        .unwrap();
        register_store(
            &app.db,
            StoreId::parse(unrelated).unwrap(),
            StorePolicy::Shared,
        )
        .await
        .unwrap();
        let rows = records(&app, true, Some(selected)).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], selected);
        assert_eq!(rows[0]["availability"], "unavailable");
        assert_eq!(
            rows[0]["measurement"]["error_code"],
            "store_unconfigured_or_unavailable"
        );
        drop(app);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        jmodguard::tenant::remove_sidecars(&path);
    }

    #[tokio::test]
    async fn cached_overview_stays_available_when_probe_slots_are_busy() {
        let (app, path) = fixture();
        let _one = app
            .usage_cache
            .probes
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        let _two = app
            .usage_cache
            .probes
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        app.usage_cache.state.lock().await.overview =
            Some((Instant::now(), json!({"databases":[{"id":"core"}]})));
        let session = signed_in(&app, 99).await;
        let mut headers = HeaderMap::new();
        let origin = reqwest::Url::parse(&app.cfg.web.base_url)
            .unwrap()
            .origin()
            .ascii_serialization();
        headers.insert(ORIGIN, HeaderValue::from_str(&origin).unwrap());
        let response = tokio::time::timeout(
            Duration::from_millis(200),
            overview(State(app.clone()), session, headers),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.0["databases"][0]["id"], "core");
        drop(app);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        jmodguard::tenant::remove_sidecars(&path);
    }
}
