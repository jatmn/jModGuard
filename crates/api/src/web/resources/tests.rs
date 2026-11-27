use super::*;
use axum::body::Body;
use axum::http::HeaderValue;
use axum::http::header::ORIGIN;
use jmodguard::db::{Db, ResourceProcess};
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
    fixture_with_estimate(HostingEstimateConfig::default())
}

fn fixture_with_estimate(estimate: HostingEstimateConfig) -> Fixture {
    let path = std::env::temp_dir().join(format!(
        "jmodguard_api_resources_{:032x}.db",
        rand::random::<u128>()
    ));
    let db = Db::open(&path, 2).unwrap();
    let mut cfg = jmodguard::config::ApiProcessConfig::default();
    cfg.bot.owner_id = 99;
    cfg.db.path = path.clone();
    cfg.web.hosting_estimate = estimate;
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
    app.guild_cache.insert(user_id, (now, Arc::from([])));
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

async fn response_json(response: Response) -> Value {
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "application/json");
    let bytes = axum::body::to_bytes(response.into_body(), 1_048_576)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
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
    let value = response_json(
        overview(State(f.app.clone()), owner, headers, range("1h"))
            .await
            .unwrap(),
    )
    .await;
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
async fn memory_billing_returns_rates_only_when_enabled() {
    for enabled in [false, true] {
        let f = fixture_with_estimate(HostingEstimateConfig {
            enabled,
            memory_usd_per_block_hour: 0.001,
            cpu_usd_per_core_hour: 0.04,
            ..HostingEstimateConfig::default()
        });
        let owner = signed_in(&f.app, 99).await;
        let value = response_json(
            overview(State(f.app.clone()), owner, origin(&f.app), range("1h"))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(value["memory_billing"]["block_mib"], 64);
        assert_eq!(
            value["memory_billing"]["rates"],
            if enabled {
                json!({"memory_usd_per_block_hour": 0.001, "cpu_usd_per_core_hour": 0.04})
            } else {
                Value::Null
            }
        );
    }
}

#[tokio::test]
async fn resources_compare_stored_and_live_scopes_without_exposing_identity() {
    let f = fixture();
    let now_secs = resources::unix_ms_now() / 1000;
    let status = |scopes| ProcessStatus {
        sampled_at_ms: Some(now_secs * 1000),
        interval_ms: 60_000,
        cgroup_scopes: serde_json::from_value(scopes).unwrap(),
        ..ProcessStatus::default()
    };
    let bot = status(json!({"memory": [7, 1], "cpu": [8, 2]}));
    db::record_resource_rollups(
        &f.app.db,
        Vec::new(),
        Some((ResourceProcess::Bot, bot)),
        now_secs,
    )
    .await
    .unwrap();
    let (prepared, history_status) = history(&f.app, ResourceRange::Hour, now_secs).await;
    for assume_disjoint_cgroups in [false, true] {
        let estimate = HostingEstimateConfig {
            enabled: true,
            assume_disjoint_cgroups,
            ..HostingEstimateConfig::default()
        };
        let private_relation = if assume_disjoint_cgroups {
            "operator_declared_disjoint"
        } else {
            "unverified"
        };
        for (scopes, memory, cpu) in [
            (json!({"memory": [7], "cpu": [8]}), "shared", "shared"),
            (
                json!({"memory": [3, 1], "cpu": [4, 2]}),
                "disjoint",
                "disjoint",
            ),
            (
                json!({"memory": [3], "cpu": [4]}),
                private_relation,
                private_relation,
            ),
            (json!({"memory": [], "cpu": []}), "unknown", "unknown"),
            (
                json!({"memory": [1], "cpu": [2]}),
                "overlapping",
                "overlapping",
            ),
            (
                json!({"memory": [1], "cpu": [4]}),
                "overlapping",
                private_relation,
            ),
        ] {
            let value = response_json(build_response(
                ResourceRange::Hour,
                now_secs * 1000,
                prepared.as_deref(),
                history_status,
                &status(scopes),
                &estimate,
            ))
            .await;
            assert_eq!(
                value["cgroup_relation"],
                json!({"memory": memory, "cpu": cpu}),
                "assume_disjoint_cgroups={assume_disjoint_cgroups}"
            );
            for role in ["bot", "api"] {
                assert!(
                    value["processes"][role]["current"]
                        .get("cgroup_scopes")
                        .is_none()
                );
            }
        }
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
    let value = response_json(
        overview(State(f.app.clone()), owner, origin(&f.app), range("1h"))
            .await
            .unwrap(),
    )
    .await;
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
    assert_eq!(
        value["cgroup_relation"],
        json!({"memory": "unknown", "cpu": "unknown"})
    );
    assert_eq!(
        value["combined_memory_peak"],
        json!({"peak_bytes": 2300 + 2050, "periods": 1, "total_periods": 59}),
        "only the minute both processes recorded"
    );
}

async fn paired_peak_at(f: &Fixture, range: ResourceRange, now_secs: i64) -> Value {
    let (prepared, status) = history(&f.app, range, now_secs).await;
    assert_eq!(status, "available");
    response_json(build_response(
        range,
        now_secs * 1000,
        prepared.as_deref(),
        status,
        &ProcessStatus::default(),
        &HostingEstimateConfig::default(),
    ))
    .await["combined_memory_peak"]
        .clone()
}

#[tokio::test]
async fn paired_minute_coverage_excludes_open_and_future_minutes() {
    let f = fixture();
    let now = 1_800_000_000;
    let rows = [
        (now - 60, 100),
        (now + 60, 200),
        (now + 120, 300),
        (now + 180, 400),
    ]
    .into_iter()
    .flat_map(|(start, cpu)| {
        [ResourceProcess::Bot, ResourceProcess::Api]
            .map(|process| (process, minute(start, cpu, 0, 1)))
    })
    .collect();
    // The store can accept a minute from a writer whose clock is ahead.
    db::record_resource_rollups(&f.app.db, rows, None, now + 150)
        .await
        .unwrap();

    assert_eq!(
        paired_peak_at(&f, ResourceRange::Day, now + 150).await,
        json!({"peak_bytes": 4400, "periods": 2, "total_periods": 1432})
    );
    // Eligibility advances inside the same ten-minute chart bucket.
    assert_eq!(
        paired_peak_at(&f, ResourceRange::Day, now + 180).await,
        json!({"peak_bytes": 4600, "periods": 3, "total_periods": 1433})
    );
}

#[tokio::test]
async fn failed_refresh_keeps_complete_paired_coverage_from_the_cached_snapshot() {
    let f = fixture();
    let range = ResourceRange::Day;
    let now = 1_800_000_150;
    let window = range.window(now);
    let rows: Vec<_> = (window.from_secs..window.eligible_to_secs)
        .step_by(60)
        .map(|start| minute(start, 100, 0, 1))
        .collect();
    let records = ResourceRecords {
        bot: rows.clone(),
        api: rows,
        ..ResourceRecords::default()
    };
    let snapshot = Arc::new(PreparedHistory::new(records, &window).unwrap());
    assert_eq!(history(&f.app, range, now).await.1, "available");
    f.app
        .resource_cache
        .entries
        .lock()
        .await
        .get_mut(&range)
        .unwrap()
        .prepared = Some(snapshot.clone());
    let complete = json!({"peak_bytes": 4200, "periods": 1432, "total_periods": 1432});
    assert_eq!(paired_peak_at(&f, range, now).await, complete);

    f.app
        .db
        .with_conn(|conn| {
            conn.execute_batch_bounded(
                "ALTER TABLE resource_rollups RENAME TO offline_rollups",
                64 * 1024 * 1024,
            )
        })
        .await
        .unwrap();

    // A request can arrive at the cache after a later eligibility cutoff
    // was already attempted. Reuse depends on the successful snapshot.
    for request_secs in [now + 30, now + 90, now + 30] {
        let (prepared, status) = history(&f.app, range, request_secs).await;
        let prepared = prepared.unwrap();
        assert!(Arc::ptr_eq(&snapshot, &prepared));
        let value = response_json(build_response(
            range,
            request_secs * 1000,
            Some(&prepared),
            status,
            &ProcessStatus::default(),
            &HostingEstimateConfig::default(),
        ))
        .await;
        assert_eq!(value["history_status"], "stale");
        assert_eq!(value["generated_at_ms"], request_secs * 1000);
        assert_eq!(value["combined_memory_peak"], complete);
    }
}

#[tokio::test]
async fn paired_hour_coverage_includes_the_open_hour_after_its_first_minute() {
    let f = fixture();
    let now = 1_800_000_000;
    let record_pair = async |start, cpu| {
        let rows = [ResourceProcess::Bot, ResourceProcess::Api]
            .map(|process| (process, minute(start, cpu, 0, 1)));
        db::record_resource_rollups(&f.app.db, rows.into(), None, start + MINUTE_SECS)
            .await
            .unwrap();
    };
    record_pair(now - MINUTE_SECS, 100).await;
    let ranges = [(ResourceRange::Week, 167), (ResourceRange::Month, 716)];
    for (range, closed_hours) in ranges {
        assert_eq!(
            paired_peak_at(&f, range, now + 59).await,
            json!({"peak_bytes": 4200, "periods": 1, "total_periods": closed_hours})
        );
    }
    record_pair(now, 200).await;
    for (range, closed_hours) in ranges {
        assert_eq!(
            paired_peak_at(&f, range, now + 60).await,
            json!({"peak_bytes": 4400, "periods": 2, "total_periods": closed_hours + 1})
        );
    }
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
fn range_breakdown_ranks_a_late_busy_label_by_count() {
    // Each early minute is full of quiet paths; the busiest path only appears
    // in the last minute of the range.
    let mut rows: Vec<Rollup> = (0..30)
        .map(|m| {
            let mut row = minute(600 + m * 60, 100, 1, 1);
            row.data.labelled.insert(
                "db_primary_paths".into(),
                (0..jmodguard::resources::MAX_LABELS)
                    .map(|i| (format!("quiet{m:02}-{i:02}"), 2))
                    .collect(),
            );
            row
        })
        .collect();
    let mut last = minute(600 + 30 * 60, 100, 1, 1);
    last.data
        .labelled
        .insert("db_primary_paths".into(), [("busy".to_owned(), 90)].into());
    rows.push(last);
    let value = history_json(&rows, 600, 600 + 3_600, 600);
    let paths = value["breakdowns"]["db_primary_paths"].as_array().unwrap();
    assert_eq!(paths[0], serde_json::json!(["busy", 90]));
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
    let value = response_json(build_response(
        ResourceRange::Week,
        1_800_000_000_000,
        None,
        "unavailable",
        &api,
        &HostingEstimateConfig::default(),
    ))
    .await;
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
async fn cached_history_reuses_payload_and_refreshes_on_expiry_or_bucket_change() {
    let f = fixture();
    let now = 1_800_000_000;
    let hour = ResourceRange::Hour;
    let mut bot = ProcessStatus {
        sampled_at_ms: Some((now - 30) * 1000),
        interval_ms: 60_000,
        ..ProcessStatus::default()
    };
    bot.info.started_at_ms = (now - 600) * 1000;
    db::record_resource_rollups(
        &f.app.db,
        vec![(ResourceProcess::Api, minute(now - 60, 50, 30, 7))],
        Some((ResourceProcess::Bot, bot)),
        now,
    )
    .await
    .unwrap();

    let request = async |range, now_secs, api_gauge| {
        let (prepared, status) = history(&f.app, range, now_secs).await;
        let mut api = ProcessStatus {
            sampled_at_ms: Some(now_secs * 1000),
            ..ProcessStatus::default()
        };
        api.gauges.insert("api_sessions".into(), api_gauge);
        let value = response_json(build_response(
            range,
            now_secs * 1000,
            prepared.as_deref(),
            status,
            &api,
            &HostingEstimateConfig::default(),
        ))
        .await;
        assert_eq!(value["history_status"], "available");
        (prepared.unwrap(), value)
    };
    let (initial, first) = request(hour, now, 3).await;
    db::record_resource_rollups(
        &f.app.db,
        vec![
            (ResourceProcess::Api, minute(now, 60, 40, 7)),
            (ResourceProcess::Api, minute(now + 60, 70, 50, 7)),
        ],
        None,
        now,
    )
    .await
    .unwrap();

    let (warm, later) = request(hour, now + 15, 5).await;
    assert!(Arc::ptr_eq(&initial, &warm));
    assert_eq!(later["generated_at_ms"], (now + 15) * 1000);
    for role in ["api", "bot"] {
        assert_eq!(
            later["processes"][role]["history"],
            first["processes"][role]["history"],
        );
    }
    assert_eq!(
        later["processes"]["api"]["current"]["gauges"]["api_sessions"],
        5
    );
    assert_eq!(later["processes"]["bot"]["current"]["age_ms"], 45_000);
    assert_eq!(later["processes"]["bot"]["current"]["uptime_ms"], 615_000);

    let (_, day) = request(ResourceRange::Day, now + 15, 5).await;
    assert_eq!(day["bucket_secs"], 600);
    assert_eq!(
        day["processes"]["api"]["history"]["totals"]["api_requests"],
        120
    );
    assert!(Arc::ptr_eq(&warm, &request(hour, now + 15, 5).await.0));

    f.app
        .resource_cache
        .entries
        .lock()
        .await
        .get_mut(&hour)
        .unwrap()
        .checked_at -= HISTORY_TTL;
    let (refreshed, same_bucket) = request(hour, now + 15, 5).await;
    assert!(!Arc::ptr_eq(&warm, &refreshed));
    assert_eq!(same_bucket["to_secs"], now + 60);
    assert_eq!(
        same_bucket["processes"]["api"]["history"]["totals"]["api_requests"],
        70
    );

    let (rolled, new_bucket) = request(hour, now + 60, 5).await;
    assert!(!Arc::ptr_eq(&refreshed, &rolled));
    assert_eq!(new_bucket["to_secs"], now + 120);
    assert_eq!(
        new_bucket["processes"]["api"]["history"]["totals"]["api_requests"],
        120
    );
}

#[tokio::test]
async fn failed_history_reads_are_reused_until_the_next_refresh() {
    let f = fixture();
    let now = 1_800_000_000;
    let warm = ResourceRange::Day;
    let cold = ResourceRange::Week;
    let (snapshot, status) = history(&f.app, warm, now).await;
    assert_eq!(status, "available");
    let snapshot = snapshot.unwrap();
    assert_eq!(
        history(&f.app, ResourceRange::Hour, now).await.1,
        "available"
    );
    f.app
        .resource_cache
        .entries
        .lock()
        .await
        .get_mut(&warm)
        .unwrap()
        .checked_at -= HISTORY_TTL;

    let rename = |sql: &'static str| {
        f.app
            .db
            .with_conn(move |conn| conn.execute_batch_bounded(sql, 64 * 1024 * 1024))
    };
    rename("ALTER TABLE resource_rollups RENAME TO offline_rollups")
        .await
        .unwrap();
    let (fallback, status) = history(&f.app, warm, now).await;
    assert_eq!(status, "stale");
    assert!(Arc::ptr_eq(&snapshot, &fallback.unwrap()));
    assert_eq!(history(&f.app, cold, now).await.1, "unavailable");
    let (snapshot, status) = history(&f.app, ResourceRange::Hour, now + 60).await;
    assert_eq!(status, "unavailable");
    assert!(snapshot.is_none());
    rename("ALTER TABLE offline_rollups RENAME TO resource_rollups")
        .await
        .unwrap();

    // Even with the database healthy, concurrent callers reuse the failed attempt.
    for (range, status, has_snapshot) in [(warm, "stale", true), (cold, "unavailable", false)] {
        let (first, second) =
            tokio::join!(history(&f.app, range, now), history(&f.app, range, now));
        for (records, actual) in [first, second] {
            assert_eq!(actual, status);
            assert_eq!(records.is_some(), has_snapshot);
        }
        f.app
            .resource_cache
            .entries
            .lock()
            .await
            .get_mut(&range)
            .unwrap()
            .checked_at -= HISTORY_TTL;
        assert_eq!(history(&f.app, range, now).await.1, "available");
    }
}

#[tokio::test]
async fn session_load_counts_toward_active_sessions() {
    let inner = MemoryStore::default();
    let store = CountedStore(inner.clone());
    let mut record = tower_sessions::session::Record {
        id: tower_sessions::session::Id::default(),
        data: std::collections::HashMap::new(),
        expiry_date: time::Date::MAX.midnight().assume_utc(),
    };
    inner.create(&mut record).await.unwrap();
    // Other routers in this process also track sessions. Read only this fixture's ID.
    let tracked_expiry = || {
        tracked_sessions()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&record.id)
            .copied()
    };
    assert_eq!(tracked_expiry(), None);
    assert!(store.load(&record.id).await.unwrap().is_some());
    assert_eq!(tracked_expiry(), Some(record.expiry_date));
    store.delete(&record.id).await.unwrap();
    assert_eq!(tracked_expiry(), None);
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
