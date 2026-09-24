//! Owner-only physical database usage. Cache lives after authorization and
//! coalesces concurrent browsers so inspection does not amplify remote reads.
use super::state::ApiState;
use super::{is_bot_owner, require_auth, trusted_web_origin};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use jmodguard::capacity::{
    DATABASE_LIMIT_BYTES, GROWTH_STOP_BYTES, PRESSURE_BYTES, SNAPSHOT_BUDGET_BYTES,
    SNAPSHOT_PAUSE_BYTES,
};
use jmodguard::db::guild_stores::StorePolicy;
use jmodguard::db::usage::{DatabaseUsage, measure_database_usage};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower_sessions::Session;

const OVERVIEW_TTL: Duration = Duration::from_secs(60);
const DETAIL_TTL: Duration = Duration::from_secs(300);

#[derive(Default)]
pub struct UsageCache {
    pub state: tokio::sync::Mutex<CacheState>,
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
    let auth = require_auth(app, session)
        .await
        .map_err(|_| error(StatusCode::UNAUTHORIZED, "sign in required"))?;
    if !is_bot_owner(app, &auth) {
        return Err(error(StatusCode::FORBIDDEN, "bot owner required"));
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
) -> Value {
    let allocated = usage.as_ref().map(|u| u.allocated_bytes);
    let status = match allocated {
        None => "unavailable",
        Some(n) if n >= GROWTH_STOP_BYTES => "growth_stopped",
        Some(n) if n >= SNAPSHOT_PAUSE_BYTES => "capture_paused",
        Some(n) if n >= PRESSURE_BYTES => "pressure",
        Some(_) => "normal",
    };
    let status = if stale { "stale" } else { status };
    let health = usage.as_ref().and_then(|u| u.snapshot_health.as_ref());
    let now_ms = chrono::Utc::now().timestamp_millis();
    let capture_status = match health {
        Some(h) if h.capacity_paused => "paused",
        Some(h)
            if h.updated_at_ms
                .is_some_and(|at| at <= now_ms && now_ms - at <= 300_000) =>
        {
            h.state.as_str()
        }
        Some(_) => "stale",
        None => "unavailable",
    };
    let cleanup_status = match health {
        Some(h)
            if h.last_cleanup_at_ms
                .is_some_and(|at| at <= now_ms && now_ms - at <= 900_000) =>
        {
            "recent"
        }
        Some(h) if h.last_cleanup_at_ms.is_some() => "stale",
        _ => "unavailable",
    };
    json!({
        "id":id,"kind":kind,"policy":policy,"status":status,"stale":stale,
        "limit_bytes":DATABASE_LIMIT_BYTES,
        "snapshot_budget_bytes":SNAPSHOT_BUDGET_BYTES,
        "snapshot_usage_estimate_bytes":health.map(|h| h.estimated_snapshot_bytes),
        "headroom_bytes":allocated.map(|n| DATABASE_LIMIT_BYTES.saturating_sub(n)),
        "percent_of_limit":allocated.map(|n| n as f64 * 100.0 / DATABASE_LIMIT_BYTES as f64),
        "usage":usage,
        "capture_status":capture_status,
        "cleanup_status":cleanup_status
    })
}

async fn records(app: &ApiState, details: bool, only: Option<&str>) -> Result<Vec<Value>, ()> {
    let mut records = Vec::new();
    if only.is_none() || only == Some("core") {
        let usage = measure_database_usage(&app.db, details, None).await.ok();
        records.push(measured("core".into(), "core", json!(null), usage, false));
    }
    // The catalog, rather than only active handles, supplies the row set.
    // Unconfigured/failed handles remain visible with unavailable measurements.
    let stores = app.tenant.usage_stores().await.map_err(|_| ())?;
    for store in stores {
        let id = store.registration.id().as_str();
        if only.is_some() && only != Some(id) {
            continue;
        }
        let usage = match store.handle {
            Some(handle) => handle.measure_usage(details).await.ok(),
            None => None,
        };
        records.push(measured(
            id.to_owned(),
            "guild",
            policy(store.registration.policy()),
            usage,
            false,
        ));
    }
    Ok(records)
}

fn stale(mut prior: Value) -> Value {
    prior["stale"] = json!(true);
    prior["status"] = json!("stale");
    prior["capture_status"] = json!("stale");
    prior["cleanup_status"] = json!("stale");
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
    let mut cache = app.usage_cache.state.lock().await;
    if let Some((at, value)) = &cache.overview
        && at.elapsed() < OVERVIEW_TTL
    {
        return Ok(Json(value.clone()));
    }
    let mut databases = match records(&app, false, None).await {
        Ok(rows) => rows,
        Err(()) => {
            let fallback = cache.overview.as_ref().map(|(_, value)| {
                let mut value = value.clone();
                if let Some(rows) = value["databases"].as_array_mut() {
                    for row in rows {
                        *row = stale(row.clone());
                    }
                }
                value
            });
            if let Some(value) = fallback {
                cache.overview = Some((Instant::now(), value.clone()));
                return Ok(Json(value));
            }
            return Err(error(
                StatusCode::SERVICE_UNAVAILABLE,
                "database catalog unavailable",
            ));
        }
    };
    if let Some((_, previous)) = &cache.overview {
        preserve_failed_samples(&mut databases, previous);
    }
    let response = json!({"databases":databases,"generated_at":chrono::Utc::now().timestamp()});
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
    if id != "core" && (id.len() != 32 || !id.bytes().all(|c| c.is_ascii_hexdigit())) {
        return Err(error(StatusCode::NOT_FOUND, "database not found"));
    }
    let mut cache = app.usage_cache.state.lock().await;
    if let Some((at, value)) = cache.detail.get(&id)
        && at.elapsed() < DETAIL_TTL
    {
        return Ok(Json(value.clone()));
    }
    let mut found = match records(&app, true, Some(&id)).await {
        Ok(rows) => rows,
        Err(()) => {
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
}
