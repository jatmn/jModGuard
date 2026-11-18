use super::*;
use axum::body::Body;
use axum::http::HeaderValue;
use axum::http::header::ORIGIN;
use jmodguard::db::{Db, ResourceProcess, ResourceRecords};
use tower::ServiceExt;
use tower_sessions::Session;

struct Fixture {
    app: Arc<ApiState>,
    path: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
        }
        jmodguard::tenant::remove_sidecars(&self.path);
    }
}

fn fixture() -> Fixture {
    let path = std::env::temp_dir().join(format!(
        "jmodguard_api_resources_{:032x}.db",
        rand::random::<u128>()
    ));
    let db = Db::open(&path, 2).unwrap();
    let mut cfg = jmodguard::config::ApiProcessConfig::default();
    cfg.bot.owner_id = 99;
    cfg.db.path = path.clone();
    Fixture {
        app: ApiState::new(cfg, db).unwrap(),
        path,
    }
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

fn origin(app: &ApiState) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let origin = reqwest::Url::parse(&app.cfg.web.base_url)
        .unwrap()
        .origin()
        .ascii_serialization();
    headers.insert(ORIGIN, HeaderValue::from_str(&origin).unwrap());
    headers
}

fn range(raw: &str) -> Query<ResourcesQuery> {
    Query(ResourcesQuery {
        range: Some(raw.into()),
    })
}

fn minute(start_secs: i64, cpu: u32, requests: u64, started_ms: i64) -> Rollup {
    let mut rollup = Rollup::empty(start_secs, MINUTE_SECS);
    rollup.cpu_avg_tenths = Some(cpu);
    rollup.cpu_max_tenths = Some(cpu);
    rollup.memory_avg_bytes = Some(1000);
    rollup.memory_max_bytes = Some(2000 + u64::from(cpu));
    rollup.data.cpu_samples = 12;
    rollup.data.memory_samples = 12;
    rollup.data.minutes = 1;
    rollup.data.starts = vec![started_ms];
    rollup.data.counters.insert("api_requests".into(), requests);
    rollup
}

#[tokio::test]
async fn resources_are_owner_only_and_reject_unknown_ranges() {
    let f = fixture();
    let headers = origin(&f.app);
    let anonymous = Session::new(None, Arc::new(MemoryStore::default()), None);
    let denied = |result: Reply| result.unwrap_err().0;
    assert_eq!(
        denied(
            overview(
                State(f.app.clone()),
                anonymous,
                headers.clone(),
                range("24h")
            )
            .await
        ),
        StatusCode::UNAUTHORIZED
    );
    let nonowner = signed_in(&f.app, 42).await;
    assert_eq!(
        denied(
            overview(
                State(f.app.clone()),
                nonowner,
                headers.clone(),
                range("24h")
            )
            .await
        ),
        StatusCode::FORBIDDEN
    );
    let owner = signed_in(&f.app, 99).await;
    assert_eq!(
        denied(overview(State(f.app.clone()), owner, headers.clone(), range("90d")).await),
        StatusCode::BAD_REQUEST
    );
    let owner = signed_in(&f.app, 99).await;
    let value = overview(State(f.app.clone()), owner, headers, range("1h"))
        .await
        .unwrap()
        .0;
    assert_eq!(value["range"], "1h");
    assert_eq!(value["history_status"], "available");
    assert_eq!(
        value["processes"]["bot"]["current"]["availability"],
        "unavailable"
    );
    assert_eq!(
        value["processes"]["api"]["history"]["points"]
            .as_array()
            .unwrap()
            .len(),
        60
    );

    // The route is mounted for GET and POST behind the same owner check.
    let router = super::super::router(f.app.clone());
    for method in ["GET", "POST"] {
        let response = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method(method)
                    .uri("/api/owner/resources?range=24h")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{method}");
    }
}

#[tokio::test]
async fn saved_bot_history_and_status_reach_the_owner_response() {
    let f = fixture();
    let now_secs = resources::unix_ms_now() / 1000;
    let last = now_secs - now_secs.rem_euclid(60) - 60;
    let mut status = ProcessStatus {
        sampled_at_ms: Some(now_secs * 1000),
        interval_ms: 60_000,
        ..ProcessStatus::default()
    };
    status.info.started_at_ms = (now_secs - 600) * 1000;
    status.gauges.insert("guilds".into(), 3);
    db::record_resource_rollups(
        &f.app.db,
        vec![
            (ResourceProcess::Bot, minute(last - 60, 100, 0, 5)),
            (
                ResourceProcess::Bot,
                minute(last, 300, 0, (now_secs - 600) * 1000),
            ),
            (ResourceProcess::Api, minute(last, 50, 30, 7)),
        ],
        Some((ResourceProcess::Bot, status)),
        now_secs,
    )
    .await
    .unwrap();
    let owner = signed_in(&f.app, 99).await;
    let value = overview(State(f.app.clone()), owner, origin(&f.app), range("1h"))
        .await
        .unwrap()
        .0;
    let bot = &value["processes"]["bot"];
    assert_eq!(bot["current"]["availability"], "available");
    assert_eq!(bot["current"]["stale"], false);
    assert_eq!(bot["current"]["gauges"]["guilds"], 3);
    assert_eq!(bot["history"]["cpu_peak_tenths"], 300);
    assert_eq!(bot["history"]["cpu_avg_tenths"], 200);
    assert_eq!(bot["history"]["memory_peak_bytes"], 2300);
    assert_eq!(
        bot["history"]["restarts"], 1,
        "only the start inside the range"
    );
    let api = &value["processes"]["api"];
    assert_eq!(api["history"]["totals"]["api_requests"], 30);
    assert_eq!(api["history"]["per_minute"]["api_requests"], 30.0);
    assert_eq!(api["current"]["info"]["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn history_downsamples_onto_a_fixed_grid_with_gaps_not_zeros() {
    let rows = vec![
        minute(600, 100, 1, 1),
        minute(660, 300, 3, 1),
        minute(1_800, 200, 2, 1),
    ];
    let value = history_json(&rows, 600, 600 + 3_600, 600);
    let points = value["points"].as_array().unwrap();
    assert_eq!(points.len(), 6);
    assert_eq!(points[0]["minutes"], 2);
    assert_eq!(points[0]["cpu_avg_tenths"], 200);
    assert_eq!(points[0]["cpu_max_tenths"], 300);
    assert_eq!(points[0]["rates"]["api_requests"], 2.0);
    assert_eq!(points[1]["minutes"], 0);
    assert!(
        points[1].get("cpu_avg_tenths").is_none(),
        "an empty bucket has no value"
    );
    assert_eq!(points[2]["minutes"], 1);
    assert_eq!(value["minutes"], 3);
    assert_eq!(value["coverage"], 0.05);
    assert_eq!(value["totals"]["api_requests"], 6);
    assert!(
        value["p95_ms"].is_null(),
        "no requests means no latency, not 0 ms"
    );
}

#[test]
fn freshness_marks_missing_status_unavailable_and_old_status_stale() {
    let now = 1_800_000_000_000;
    assert_eq!(current(None, now)["availability"], "unavailable");
    let never = ProcessStatus {
        interval_ms: 5_000,
        ..ProcessStatus::default()
    };
    let value = current(Some(&never), now);
    assert_eq!(value["availability"], "unavailable");
    assert_eq!(value["stale"], true);
    let fresh = ProcessStatus {
        sampled_at_ms: Some(now - 9_000),
        interval_ms: 5_000,
        ..ProcessStatus::default()
    };
    assert_eq!(current(Some(&fresh), now)["stale"], false);
    let old = ProcessStatus {
        sampled_at_ms: Some(now - 16_000),
        ..fresh
    };
    let value = current(Some(&old), now);
    assert_eq!(value["stale"], true);
    assert_eq!(value["stale_after_ms"], 15_000);
    assert!(
        value["cpu_tenths"].is_null(),
        "a missing reading stays null"
    );
}

#[tokio::test]
async fn unavailable_history_still_returns_live_api_values() {
    let api = ProcessStatus::current(api_interval_ms());
    let value = build_response(
        ResourceRange::Week,
        1_800_000_000_000,
        None,
        "unavailable",
        &api,
    );
    assert_eq!(value["history_status"], "unavailable");
    assert!(value["processes"]["api"]["history"].is_null());
    assert!(value["processes"]["bot"]["history"].is_null());
    assert_eq!(
        value["processes"]["bot"]["current"]["availability"],
        "unavailable"
    );
    assert_eq!(value["rows_read"]["status"], "unavailable");
    assert_eq!(value["provider"]["status"], "not_configured");
}

#[tokio::test]
async fn degraded_history_cache_stays_stale_within_ttl() {
    let f = fixture();
    let picked = ResourceRange::Day;
    let now_secs = resources::unix_ms_now() / 1000;
    let (_, _, bucket) = picked.shape();
    let to_secs = now_secs - now_secs.rem_euclid(bucket) + bucket;
    {
        let mut cache = f.app.resource_cache.entries.lock().await;
        cache.insert(
            picked,
            super::CachedHistory {
                read_at: Instant::now(),
                to_secs,
                records: ResourceRecords::default(),
                stale: true,
            },
        );
    }
    let owner = signed_in(&f.app, 99).await;
    let value = overview(State(f.app.clone()), owner, origin(&f.app), range("24h"))
        .await
        .unwrap()
        .0;
    assert_eq!(value["history_status"], "stale");
}

#[tokio::test]
async fn session_load_counts_toward_active_sessions() {
    use tower_sessions::cookie::time::Duration as CookieDuration;
    let inner = MemoryStore::default();
    let store = CountedStore(inner.clone());
    let mut record = tower_sessions::session::Record {
        id: tower_sessions::session::Id::default(),
        data: std::collections::HashMap::new(),
        expiry_date: tower_sessions::cookie::time::OffsetDateTime::now_utc()
            + CookieDuration::hours(1),
    };
    inner.create(&mut record).await.unwrap();
    assert_eq!(super::active_sessions(), 0);
    store.load(&record.id).await.unwrap();
    assert_eq!(super::active_sessions(), 1);
}

#[tokio::test]
async fn request_counter_skips_probes_and_counts_errors() {
    let router = axum::Router::new()
        .route("/health", axum::routing::get(|| async { "ok" }))
        .route(
            "/missing",
            axum::routing::get(|| async { (StatusCode::NOT_FOUND, "gone") }),
        )
        .layer(axum::middleware::from_fn(count_request));
    let requests = resources::peek_counter("api_requests");
    let client_errors = resources::peek_counter("api_4xx");
    let bytes = resources::peek_counter("api_bytes_out");
    for uri in ["/health", "/missing"] {
        router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
    }
    // Other tests in this binary also make requests; these only grow.
    assert!(resources::peek_counter("api_requests") > requests);
    assert!(resources::peek_counter("api_4xx") > client_errors);
    assert!(resources::peek_counter("api_bytes_out") >= bytes + 4);
}
