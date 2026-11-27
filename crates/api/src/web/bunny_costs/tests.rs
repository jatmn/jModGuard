use super::*;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse as _, Response},
};
use chrono::TimeZone as _;
use jmodguard::config::ApiProcessConfig;
use jmodguard::db::{Db, SqlExecutor as _, params};
use serde_json::{Value, json};
use std::{
    io::{self, Write},
    path::PathBuf,
    sync::Mutex,
};
use tracing::instrument::WithSubscriber as _;

const KEY: &str = "placeholder-stats-key";
const MC: &str = "Placeholder-App-MiXeD";
const CORE: &str = "PLACEHOLDER-CORE-DB";
const GUILD: &str = "PLACEHOLDER-GUILD-DB";
const STORAGE: &str = "0000";

fn instant() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 17, 12, 30, 0).unwrap()
}

fn config() -> Config {
    Config::load(|name| {
        Some(
            match name {
                "JMODGUARD_BUNNY_MC_STATS_KEY" | "JMODGUARD_BUNNY_DATA_STATS_KEY" => KEY,
                "JMODGUARD_BUNNY_MC_APP_ID" => MC,
                "JMODGUARD_BUNNY_DB_CORE_ID" => CORE,
                "JMODGUARD_BUNNY_DB_GUILD_ID" => GUILD,
                "JMODGUARD_BUNNY_STORAGE_ZONE_ID" => STORAGE,
                _ => return None,
            }
            .into(),
        )
    })
}

#[derive(Default)]
struct MockData {
    status: Option<StatusCode>,
    delay: Duration,
    requests: Vec<Uri>,
    charts: Option<Value>,
    monthly_cost: Option<f64>,
    allocated_volume_gb: Option<f64>,
    traffic: Option<Value>,
    storage: Option<BTreeMap<NaiveDate, u64>>,
}

struct Mock {
    state: Arc<Mutex<MockData>>,
    url: String,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Mock {
    async fn start() -> Self {
        let state = Arc::new(Mutex::new(MockData::default()));
        let router = Router::new()
            .fallback(mock_reply)
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { state, url, server }
    }

    fn client(&self, timeout: Duration) -> Client {
        Client::new(&self.url, timeout).unwrap()
    }
}

async fn mock_reply(
    State(state): State<Arc<Mutex<MockData>>>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    assert_eq!(headers.get("accesskey").unwrap(), KEY);
    let (status, delay, charts, monthly_cost, allocated_volume_gb, traffic, storage) = {
        let mut state = state.lock().unwrap();
        state.requests.push(uri.clone());
        (
            state.status,
            state.delay,
            state.charts.clone(),
            state.monthly_cost.unwrap_or(2.50),
            state.allocated_volume_gb,
            state.traffic.clone(),
            state.storage.clone(),
        )
    };
    tokio::time::sleep(delay).await;
    if let Some(status) = status {
        return (status, format!("reflected request {uri} with {KEY}")).into_response();
    }
    let date = "2026-11-17T00:00:00Z";
    let body = match uri.path() {
        path if path == format!("/mc/apps/{MC}/summary") => {
            let mut summary = json!({"monthlyCost": monthly_cost, "unused": KEY, "unusedId": MC});
            if let Some(gb) = allocated_volume_gb {
                summary["totalVolumeSizeInGb"] = json!(gb);
            }
            summary
        }
        path if path == format!("/mc/apps/{MC}/statistics/charts") => charts.unwrap_or_else(|| json!({
            "cpuUsageSplitChart": {"placeholder-pod": {"chart": {date: 0.5}}},
            "ramUsageSplitChart": {"placeholder-pod": {"chart": {date: 67_108_865}}},
            "trafficSplitChart": traffic.unwrap_or_else(|| json!({
                "placeholder-pod": {"region": "EU", "chart": {date: 500_000_000}}
            })),
            "volumesSplitCapacityChart": {"placeholder-volume": {"chart": {date: 1_000_000_000}}},
            "releases": [{"secret": KEY}],
        })),
        path if [CORE, GUILD]
            .iter()
            .any(|id| path == format!("/database/v2/databases/{id}/statistics")) =>
        {
            json!({"RowReadCount": {"Data": [[date, 42]]}, "RowWriteCount": {"Data": [[date, 2]]}})
        }
        path if [CORE, GUILD]
            .iter()
            .any(|id| path == format!("/database/v2/databases/{id}/usage")) =>
        {
            json!({"RowsRead": 42, "RowsWritten": 2})
        }
        path if path == format!("/storagezone/{STORAGE}/statistics") => {
            let from = reqwest::Url::parse(&format!("http://localhost{uri}"))
                .unwrap()
                .query_pairs()
                .find(|(key, _)| key == "dateFrom")
                .unwrap()
                .1
                .into_owned();
            let storage = storage.unwrap_or_else(|| {
                [(
                    NaiveDate::parse_from_str(&from[..10], "%Y-%m-%d").unwrap(),
                    30_000_000_000,
                )]
                .into()
            });
            json!({"StorageUsedChart": storage, "ignored": KEY})
        }
        _ => panic!("unexpected mock route {uri}"),
    };
    Json(body).into_response()
}

#[test]
fn unset_and_partial_config_only_enable_complete_pairs() {
    let cache = Cache::default();
    cache.enable(&Config::load(|_| None));
    assert!(cache.view(instant()).is_none());

    for (variable, value) in [
        ("JMODGUARD_BUNNY_MC_APP_ID", MC),
        ("JMODGUARD_BUNNY_MC_STATS_KEY", KEY),
    ] {
        let cfg = Config::load(|name| (name == variable).then(|| value.to_owned()));
        assert!(cfg.sources.is_empty());
        cache.enable(&cfg);
        assert!(cache.0.read().unwrap().is_none(), "{variable}");
        assert!(cache.view(instant()).is_none(), "{variable}");
    }

    let cfg = Config::load(|name| match name {
        "JMODGUARD_BUNNY_DATA_STATS_KEY" => Some(KEY.into()),
        "JMODGUARD_BUNNY_DB_GUILD_ID" => Some(GUILD.into()),
        "JMODGUARD_BUNNY_MC_APP_ID" => Some(MC.into()),
        _ => None,
    });
    assert_eq!(cfg.sources.len(), 1);
    assert_eq!(cfg.sources[0].id, GUILD);
    cache.enable(&cfg);
    let view = serde_json::to_value(cache.view(instant()).unwrap()).unwrap();
    assert_eq!(view["database"]["configured_resources"], 1);
    for service in ["magic_containers", "database", "storage", "cdn"] {
        assert_eq!(view[service]["status"], "unavailable");
        assert!(view[service]["month_to_date_usd"].is_null());
    }
    assert!(!view.to_string().contains(KEY));
    assert!(!view.to_string().contains(GUILD));
    assert!(!view.to_string().contains(MC));
}

#[tokio::test]
async fn allowed_routes_keep_identifier_case_and_send_required_query_windows() {
    let mock = Mock::start().await;
    let client = mock.client(Duration::from_secs(2));
    let now = instant();
    let month = month_start(now.date_naive());
    let window = Window {
        month,
        from: midnight(month),
        to: now,
    };
    let cfg = config();
    for source in &cfg.sources {
        let result = collect(&client, source, window, false).await.unwrap();
        match source.service {
            Service::MagicContainers => {
                let expected = 0.5 * 0.02 + 2.0 * 0.0003125 + 0.005;
                assert!((result.month_to_date.unwrap() - expected).abs() < 1e-10);
                assert_eq!(result.projection, Some(2.50));
            }
            Service::CoreDatabase | Service::GuildDatabase => {
                assert_eq!(result.month_to_date, Some(0.0));
                assert_eq!(result.projection, Some(0.0));
            }
            Service::Storage => {
                assert_eq!(result.month_to_date, Some(0.01));
                let expected = 0.01 * 30.0 / (16.0 + 12.5 / 24.0);
                assert!((result.projection.unwrap() - expected).abs() < 1e-10);
            }
        }
    }

    let requests = &mock.state.lock().unwrap().requests;
    assert_eq!(requests.len(), 8);
    for uri in requests {
        let url = reqwest::Url::parse(&format!("http://localhost{uri}")).unwrap();
        let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        match uri.path().split('/').next_back().unwrap() {
            "summary" => assert!(query.is_empty()),
            "charts" => {
                let age = query["toDate"].parse::<DateTime<Utc>>().unwrap()
                    - query["fromDate"].parse::<DateTime<Utc>>().unwrap();
                if query["granularity"] == "Hourly" {
                    assert!(age.num_days() < 7);
                } else {
                    assert_eq!(query["granularity"], "Daily");
                    assert_eq!(query["fromDate"], "2026-11-01T00:00:00.000Z");
                }
            }
            _ if uri.path().starts_with("/database/") => {
                assert_eq!(query["from"], "2026-11-01T00:00:00.000Z");
                assert_eq!(query["to"], "2026-11-17T12:29:59.999Z");
            }
            _ => {
                assert_eq!(query["dateFrom"], "2026-11-01T00:00:00.000Z");
                assert_eq!(query["dateTo"], "2026-11-17T12:29:59.999Z");
            }
        }
    }
}

#[tokio::test]
async fn magic_container_chart_fixtures_price_closed_hourly_and_daily_buckets() {
    let mock = Mock::start().await;
    let hourly = include_str!("fixtures/magic_containers_hourly_representative.json");
    {
        let mut state = mock.state.lock().unwrap();
        state.charts = Some(serde_json::from_str(hourly).unwrap());
        state.monthly_cost = Some(2.38);
        state.allocated_volume_gb = Some(6.0);
    }
    let client = mock.client(Duration::from_secs(2));
    let cfg = config();
    let source = &cfg.sources[0];
    let observed = Utc.with_ymd_and_hms(2026, 11, 2, 0, 30, 0).unwrap();
    let month = month_start(observed.date_naive());
    let window = Window {
        month,
        from: midnight(month),
        to: observed,
    };
    let data = collect(&client, source, window, false).await.unwrap();
    assert_eq!(data.days.len(), 1);
    let costs = data.days[&month];

    // 232, 248, 272 and 304 MiB round to 4, 4, 5 and 5 blocks.
    let ram = 0.005625;
    // 0.875 + 1 + 1.125 + 1.25 yields 4.25 core-hours.
    let cpu = 0.085;
    // Traffic sums to 20,000,000 bytes (4.8 + 6.2 + 5.4 + 3.6 MB).
    let egress = 0.0002;
    // The summary's 6 GB are prorated over 24.5 hours of a 30-day month.
    let volume = 0.60 * 24.5 / 720.0;
    // The response also has buckets before fromDate and at/after toDate.
    for (component, actual, expected) in [
        ("CPU", costs.cpu_core_cost.unwrap(), cpu),
        ("RAM", costs.ram, ram),
        ("egress", costs.egress, egress),
        ("volume", data.allocated_volume_cost.unwrap(), volume),
    ] {
        assert!(
            (actual - expected).abs() < 1e-12,
            "{component}: {actual} != {expected}"
        );
    }
    let subtotal = cpu + ram + egress + volume;
    assert!((data.month_to_date.unwrap() - subtotal).abs() < 1e-12);
    assert_eq!(data.projection, Some(2.38));

    let cache = Cache::default();
    cache.enable(&cfg);
    let feed = Feed {
        months: [(
            month,
            BunnyCostSnapshot {
                month,
                fetched_at: observed.timestamp(),
                data,
            },
        )]
        .into(),
        failed: false,
    };
    cache.publish(source.service, &feed);
    let report = serde_json::to_value(cache.view(observed).unwrap()).unwrap();
    let cpu_report = report["magic_breakdown"]["cpu"].as_f64().unwrap();
    assert!((cpu_report - cpu).abs() < 1e-12);
    let volume_report = report["magic_breakdown"]["volume"].as_f64().unwrap();
    assert!((volume_report - volume).abs() < 1e-12);
    assert_eq!(
        report["magic_cpu_hourly_averages"],
        json!({"sampled_at": "2026-11-01T23:00:00Z", "min": 1.25, "max": 1.25})
    );

    let daily = include_str!("fixtures/magic_containers_daily_representative.json");
    mock.state.lock().unwrap().charts = Some(serde_json::from_str(daily).unwrap());
    let daily = collect(&client, source, window, true).await.unwrap();
    assert_eq!(
        daily.days.len(),
        1,
        "drop the partial day and earlier bucket"
    );
    let costs = daily.days[&month];
    assert!(costs.complete);
    // The provider's daily mean is weighted by sample counts (see fixture README).
    // 250 MiB takes 4 blocks, while an unweighted 264 MiB would take 5.
    assert!((costs.ram - 0.03).abs() < 1e-12);
    // Mean 0.984375 cores × 24 h × $0.02, instead of mean 1.0625 cores.
    assert!((costs.cpu_core_cost.unwrap() - 0.4725).abs() < 1e-12);
    assert!((costs.egress - egress).abs() < 1e-12);
    assert!(daily.cpu_hourly_averages.is_none());
    assert!(daily.allocated_volume_cost.is_none());
    assert!((daily.month_to_date.unwrap() - 0.5027).abs() < 1e-12);
}

#[tokio::test]
async fn magic_container_cpu_range_uses_pods_from_the_latest_hour() {
    let mock = Mock::start().await;
    let earlier = "2026-11-17T10:00:00Z";
    let latest = "2026-11-17T11:00:00Z";
    mock.state.lock().unwrap().charts = Some(json!({
        "cpuUsageSplitChart": {
            "pod-example-a": {"chart": {earlier: 0.95, latest: 0.98}},
            "pod-example-b": {"chart": {latest: 1.04}},
            "pod-example-c": {"chart": {earlier: 1.09}},
        },
        "ramUsageSplitChart": {},
        "trafficSplitChart": {},
    }));
    let client = mock.client(Duration::from_secs(2));
    let cfg = config();
    let now = instant();
    let month = month_start(now.date_naive());
    let window = Window {
        month,
        from: midnight(now.date_naive()),
        to: now,
    };
    let data = collect(&client, &cfg.sources[0], window, false)
        .await
        .unwrap();
    let cpu = data.cpu_hourly_averages.unwrap();
    assert_eq!(cpu.sampled_at, latest.parse::<DateTime<Utc>>().unwrap());
    assert_eq!((cpu.min, cpu.max), (0.98, 1.04));
    let cpu_cost = data.days[&now.date_naive()].cpu_core_cost.unwrap();
    assert!((cpu_cost - 0.0812).abs() < 1e-12);
}

#[tokio::test]
async fn magic_container_estimates_apply_regional_egress_rates() {
    let mock = Mock::start().await;
    let client = mock.client(Duration::from_secs(2));
    let cfg = config();
    let source = &cfg.sources[0];
    let date = "2026-11-17T00:00:00Z";
    let day = instant().date_naive();
    let month = month_start(day);
    let window = Window {
        month,
        from: midnight(month),
        to: instant(),
    };

    // Each traffic series reports one decimal GB in a single bucket.
    let priced = json!({
        "placeholder-eu": {"region": "AMS", "chart": {date: 1_000_000_000}},
        "placeholder-sa": {"region": "SA", "chart": {date: 1_000_000_000}},
        "placeholder-asia": {"region": "asia", "chart": {date: 1_000_000_000}},
        "placeholder-af": {"region": "Africa", "chart": {date: 1_000_000_000}},
    });
    mock.state.lock().unwrap().traffic = Some(priced.clone());
    let close_enough = |actual: f64, expected: f64| assert!((actual - expected).abs() < 1e-10);
    let current = collect(&client, source, window, false).await.unwrap();
    let costs = current.days[&day];
    let cpu = current.cpu_hourly_averages.unwrap();
    assert_eq!((cpu.min, cpu.max), (0.5, 0.5));
    close_enough(costs.ram, 2.0 * 0.0003125);
    assert_eq!(current.allocated_volume_cost, None);
    close_enough(costs.egress, 0.01 + 0.045 + 0.03 + 0.06);
    assert!(!current.egress_rate_unknown());
    assert_eq!(current.projection, Some(2.50));

    let historical = Window {
        to: midnight(shift_month(month, 1)),
        ..window
    };
    let daily = collect(&client, source, historical, true).await.unwrap();
    assert!(daily.cpu_hourly_averages.is_none());
    close_enough(daily.days[&day].ram, costs.ram * 24.0);
    assert_eq!(daily.allocated_volume_cost, None);
    close_enough(daily.days[&day].egress, costs.egress);

    let mut idle = priced.clone();
    idle["placeholder-unmapped-idle"] =
        json!({"region": "placeholder-unpriced", "chart": {date: 0}});
    idle["placeholder-regionless-idle"] = json!({"chart": {date: 0}});
    mock.state.lock().unwrap().traffic = Some(idle);
    for (window, historical) in [(window, false), (historical, true)] {
        let data = collect(&client, source, window, historical).await.unwrap();
        assert!(!data.egress_rate_unknown());
        assert!(data.month_to_date.is_some());
        close_enough(data.days[&day].egress, costs.egress);
    }

    let mut unpriced = priced;
    unpriced["placeholder-unknown"] =
        json!({"region": "placeholder-unpriced", "chart": {date: 1_000_000_000}});
    mock.state.lock().unwrap().traffic = Some(unpriced);
    let current = collect(&client, source, window, false).await.unwrap();
    assert!(current.egress_rate_unknown());
    assert_eq!(current.month_to_date, None);
    assert_eq!(current.projection, Some(2.50));

    let prev = month;
    let cache = Cache::default();
    cache.enable(&cfg);
    let daily = collect(&client, source, historical, true).await.unwrap();
    let feed = Feed {
        months: [(
            prev,
            BunnyCostSnapshot {
                month: prev,
                fetched_at: historical.to.timestamp(),
                data: daily,
            },
        )]
        .into(),
        failed: false,
    };
    cache.publish(source.service, &feed);
    let value = serde_json::to_value(cache.view(historical.to).unwrap()).unwrap();
    let history = &value["history"][0]["magic_containers"];
    assert_eq!(history["egress_rate_unknown"], true);
    assert!(history["usd"].is_null());
    assert!(!value.to_string().contains("placeholder-unpriced"));
}

#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl Write for Log {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn provider_errors_and_timeouts_are_bounded_and_redacted() {
    let mock = Mock::start().await;
    let log = Log::default();
    let output = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || output.clone())
        .finish();
    async {
        let client = mock.client(Duration::from_secs(2));
        let source = &config().sources[0];
        let now = instant();
        let month = month_start(now.date_naive());
        let window = Window {
            month,
            from: midnight(month),
            to: now,
        };
        assert!(source.key.is_sensitive());
        for code in [301, 400, 401, 403, 404, 429, 500, 503] {
            mock.state.lock().unwrap().status = Some(StatusCode::from_u16(code).unwrap());
            let failure = client
                .mc_summary(source, window, &mut MonthData::default())
                .await
                .unwrap_err();
            assert_eq!(failure, Failure::Http(code));
            warn(source.service, failure);
        }
        mock.state.lock().unwrap().delay = Duration::from_secs(1);
        let short = mock.client(Duration::from_millis(50));
        let failure = short
            .mc_summary(source, window, &mut MonthData::default())
            .await
            .unwrap_err();
        assert_eq!(failure, Failure::Timeout);
        warn(source.service, failure);
    }
    .with_subscriber(subscriber)
    .await;
    let logs = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("Http(403)"));
    assert!(logs.contains("Timeout"));
    assert!(!logs.contains(KEY));
    assert!(!logs.contains(MC));
    assert!(!logs.contains("/mc/apps/"));
    assert!(!logs.contains("reflected request"));
}

#[test]
fn backfill_windows_respect_retention_and_close_after_publication_lag() {
    let now = instant();
    for (service, days) in [
        (Service::MagicContainers, 365),
        (Service::CoreDatabase, 180),
        (Service::Storage, 365),
    ] {
        let windows = backfill_windows(service, now);
        assert_eq!(windows.len(), if days == 180 { 6 } else { 12 });
        let oldest = windows.last().unwrap();
        assert!((now - oldest.from).num_hours() <= days * 24);
        assert!((now - oldest.from).num_hours() > (days - 2) * 24);
        for window in &windows {
            assert!((window.to - window.from).num_days() <= 31);
            assert!(window.from < window.to);
        }
    }
    let rollover = Utc.with_ymd_and_hms(2026, 11, 1, 2, 0, 0).unwrap();
    assert_eq!(
        backfill_windows(Service::Storage, rollover)[0]
            .month
            .month(),
        9
    );
    assert_eq!(
        backfill_windows(Service::Storage, rollover + chrono::TimeDelta::hours(4))[0]
            .month
            .month(),
        10
    );
}

struct DbFile(PathBuf);

impl Drop for DbFile {
    fn drop(&mut self) {
        for suffix in [
            "",
            "-wal",
            "-shm",
            ".guild.db",
            ".guild.db-wal",
            ".guild.db-shm",
        ] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
        jmodguard::tenant::remove_sidecars(&self.0);
    }
}

#[tokio::test]
async fn stored_magic_costs_discard_legacy_charges_and_fill_in_verified_cpu() {
    let file =
        DbFile(std::env::temp_dir().join(format!("bunny_costs_{}.db", rand::random::<u64>())));
    let mut api_cfg = ApiProcessConfig::default();
    api_cfg.db.path = file.0.clone();
    let app = ApiState::new(api_cfg, Db::open(&file.0, 2).unwrap()).unwrap();
    let cfg = config();
    let source = &cfg.sources[0];
    app.bunny_costs.enable(&cfg);
    let now = instant();
    let month = month_start(now.date_naive());
    let prior = shift_month(month, -1);
    let fetched_at = now.timestamp();
    let digest = source.digest();
    app.db
        .with_conn(move |conn| {
            for (date, projection) in [(month, Some(3.50)), (prior, None)] {
                let row = json!({
                    "month": date,
                    "fetched_at": fetched_at,
                    "data": {
                        "days": { (date.to_string()): {
                            "cpu": 1.25, "ram": 0.125, "egress": 0.0625,
                            "volume": 0.0625, "storage": 0.0, "complete": true
                        }},
                        "month_to_date": 1.50, "projection": projection,
                        "closed": false, "partial": true
                    }
                });
                conn.execute_bounded(
                    "INSERT INTO bunny_cost_snapshots (service, month, source_digest, fetched_at, data) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        Service::MagicContainers.name(),
                        midnight(date).timestamp(),
                        digest.to_vec(),
                        fetched_at,
                        row.to_string(),
                    ],
                    1024 * 1024,
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();

    let mock = Mock::start().await;
    let client = mock.client(Duration::from_secs(2));
    let stored = db::bunny_cost_snapshots(&app.db, source.service, digest, prior, month)
        .await
        .unwrap();
    let mut feed = Feed {
        months: stored.into_iter().map(|row| (row.month, row)).collect(),
        ..Feed::default()
    };
    app.bunny_costs.publish(source.service, &feed);
    let report = serde_json::to_value(app.bunny_costs.view(now).unwrap()).unwrap();
    assert_eq!(report["magic_containers"]["month_to_date_usd"], 0.1875);
    assert_eq!(report["magic_containers"]["projected_usd"], 3.50);
    assert_eq!(report["magic_breakdown"]["cpu"], Value::Null);
    assert_eq!(report["magic_breakdown"]["volume"], Value::Null);
    assert!(report["magic_cpu_hourly_averages"].is_null());
    assert_eq!(report["history"][0]["magic_containers"]["usd"], 0.1875);
    assert!(mock.state.lock().unwrap().requests.is_empty());

    save(
        &app,
        source,
        &mut feed,
        month,
        fetched_at - 1,
        MonthData::default(),
    )
    .await
    .unwrap();
    assert_eq!(feed.months[&month].data.month_to_date, Some(0.1875));

    let day = "2026-11-01T00:00:00Z";
    let hour = "2026-11-17T00:00:00Z";
    mock.state.lock().unwrap().charts = Some(json!({
        "cpuUsageSplitChart": {"pod-example": {"chart": {day: 0.25, hour: 0.5}}},
        "ramUsageSplitChart": {"pod-example": {"chart": {day: 268_435_456}}},
        "trafficSplitChart": {},
    }));
    // A fresh legacy snapshot normally suppresses polling for six hours.
    // Refresh it now to fill in CPU from the daily bucket without replacing saved RAM.
    refresh_current(&app, &client, source, &mut feed, now)
        .await
        .unwrap();
    refresh_current(&app, &client, source, &mut feed, now)
        .await
        .unwrap();
    assert_eq!(mock.state.lock().unwrap().requests.len(), 3);
    let report = serde_json::to_value(app.bunny_costs.view(now).unwrap()).unwrap();
    assert_eq!(report["magic_cpu_hourly_averages"]["min"], 0.5);
    assert_eq!(report["magic_cpu_hourly_averages"]["max"], 0.5);
    assert_eq!(report["magic_containers"]["projected_usd"], 2.50);
    let breakdown = &report["magic_breakdown"];
    assert!((breakdown["cpu"].as_f64().unwrap() - 0.13).abs() < 1e-12);
    assert_eq!(breakdown["ram"], 0.125);
    assert_eq!(breakdown["volume"], Value::Null);
    let subtotal = report["magic_containers"]["month_to_date_usd"]
        .as_f64()
        .unwrap();
    assert!((subtotal - 0.3175).abs() < 1e-12);

    let mut current = feed.months[&month].data.clone();
    current.allocated_volume_cost = Some(0.25);
    save(&app, source, &mut feed, month, now.timestamp(), current)
        .await
        .unwrap();
    let closing = midnight(shift_month(month, 1));
    let historical = collect(
        &client,
        source,
        Window {
            month,
            from: midnight(month),
            to: closing,
        },
        true,
    )
    .await
    .unwrap();
    assert!(historical.allocated_volume_cost.is_none());
    save(
        &app,
        source,
        &mut feed,
        month,
        closing.timestamp(),
        historical,
    )
    .await
    .unwrap();
    assert_eq!(feed.months[&month].data.allocated_volume_cost, Some(0.25));
}

#[tokio::test]
async fn startup_leaves_bunny_costs_unconfigured_when_client_creation_fails() {
    let file =
        DbFile(std::env::temp_dir().join(format!("bunny_costs_{}.db", rand::random::<u64>())));
    let mut api_cfg = ApiProcessConfig::default();
    api_cfg.db.path = file.0.clone();
    let app = ApiState::new(api_cfg, Db::open(&file.0, 2).unwrap()).unwrap();

    spawn_with_config(&app, config(), "invalid-url");

    assert!(app.bunny_costs.0.read().unwrap().is_none());
    assert!(app.bunny_costs.view(instant()).is_none());
}

#[test]
fn history_stays_partial_until_each_resource_covers_the_final_day() {
    let cfg = config();
    let cache = Cache::default();
    cache.enable(&cfg);
    let month = month_start(instant().date_naive());
    let end = shift_month(month, 1);
    let last_day = end.pred_opt().unwrap();
    let late_snapshot = midnight(last_day) + chrono::TimeDelta::hours(12);
    let after_rollover = midnight(end) + chrono::TimeDelta::hours(2);
    let after_close_out = midnight(end) + chrono::TimeDelta::hours(6);
    let completed_day = Cost {
        complete: true,
        ..Cost::default()
    };
    let mut data = MonthData {
        days: month
            .iter_days()
            .take_while(|day| *day < last_day)
            .map(|day| (day, completed_day))
            .collect(),
        ..MonthData::default()
    };
    let mut core_data = data.clone();
    core_data.days.insert(last_day, completed_day);
    core_data.summarize(Service::CoreDatabase, month, after_close_out);
    let closed_core = BunnyCostSnapshot {
        month,
        fetched_at: after_close_out.timestamp(),
        data: core_data,
    };
    for final_day in [None, Some(Cost::default()), Some(completed_day)] {
        let complete = final_day.is_some_and(|cost| cost.complete);
        let observed = if complete {
            after_close_out
        } else {
            late_snapshot
        };
        if let Some(cost) = final_day {
            data.days.insert(last_day, cost);
        }
        data.summarize(Service::GuildDatabase, month, observed);
        assert!(!data.partial, "the snapshot covers all its elapsed days");
        let guild = BunnyCostSnapshot {
            month,
            fetched_at: observed.timestamp(),
            data: data.clone(),
        };
        let (core, viewed_at) = if final_day.is_none() {
            (guild.clone(), after_rollover)
        } else {
            (closed_core.clone(), after_close_out)
        };
        cache.publish(
            Service::CoreDatabase,
            &Feed {
                months: [(month, core)].into(),
                ..Feed::default()
            },
        );
        cache.publish(
            Service::GuildDatabase,
            &Feed {
                months: [(month, guild)].into(),
                ..Feed::default()
            },
        );
        let report = cache.view(viewed_at).unwrap();
        let database = report.history[0].database.as_ref().unwrap();
        assert_eq!(database.usd, Some(0.0));
        assert_eq!(database.partial, !complete);
    }
}

#[tokio::test]
async fn merged_snapshot_uses_pricing_status_of_the_selected_day() {
    let file =
        DbFile(std::env::temp_dir().join(format!("bunny_costs_{}.db", rand::random::<u64>())));
    let mut api_cfg = ApiProcessConfig::default();
    api_cfg.db.path = file.0.clone();
    let app = ApiState::new(api_cfg, Db::open(&file.0, 2).unwrap()).unwrap();
    let cfg = config();
    let source = &cfg.sources[0];
    let mut feed = Feed::default();
    let month = month_start(instant().date_naive());
    let closing = midnight(shift_month(month, 1));
    let final_day = closing.date_naive().pred_opt().unwrap();
    let other_day = final_day.pred_opt().unwrap();
    let priced = Cost {
        egress: 0.25,
        complete: true,
        ..Cost::default()
    };
    let unpriced = Cost {
        egress_rate_unknown: true,
        ..Cost::default()
    };
    let complete_unpriced = Cost {
        complete: true,
        ..unpriced
    };

    for (offset, date, cost, expected) in [
        (-1, final_day, unpriced, None),
        (0, final_day, priced, Some(0.25)),
        (1, final_day, complete_unpriced, Some(0.25)),
        (2, other_day, complete_unpriced, None),
        (3, final_day, priced, None),
    ] {
        save(
            &app,
            source,
            &mut feed,
            month,
            closing.timestamp() + offset,
            MonthData {
                days: [(date, cost)].into(),
                ..MonthData::default()
            },
        )
        .await
        .unwrap();
        let stored =
            db::bunny_cost_snapshots(&app.db, source.service, source.digest(), month, month)
                .await
                .unwrap();
        assert_eq!(stored[0].data.month_to_date, expected, "refresh {offset}");
        assert_eq!(stored[0].data.egress_rate_unknown(), expected.is_none());
    }
}

#[tokio::test]
async fn close_out_preserves_saved_days_and_retries_gaps_after_restart() {
    let file =
        DbFile(std::env::temp_dir().join(format!("bunny_costs_{}.db", rand::random::<u64>())));
    let mut api_cfg = ApiProcessConfig::default();
    api_cfg.db.path = file.0.clone();
    let open = || {
        // The production read-only pool and private writer are covered in
        // tests/writer_link.rs. Here we exercise the same typed control op
        // and persistence against a fresh file using the in-process writer.
        let db = Db::open(&file.0, 2).unwrap();
        ApiState::new(api_cfg.clone(), db).unwrap()
    };
    let cfg = Config::load(|name| match name {
        "JMODGUARD_BUNNY_DATA_STATS_KEY" => Some(KEY.into()),
        "JMODGUARD_BUNNY_STORAGE_ZONE_ID" => Some(STORAGE.into()),
        _ => None,
    });
    let source = &cfg.sources[0];
    let now = instant();
    let app = open();
    app.bunny_costs.enable(&cfg);
    let mock = Mock::start().await;
    let client = mock.client(Duration::from_secs(2));
    let mut feed = Feed::default();

    let windows = backfill_windows(source.service, now);
    let last_month = windows[0].month;
    let last_day = windows[0].to.date_naive().pred_opt().unwrap();
    refresh_current(&app, &client, source, &mut feed, now)
        .await
        .unwrap();

    // Roll over with a full day and a provisional last day from a previous
    // current-month refresh. Seed older months with the days still retained.
    let prior = MonthData {
        days: [
            (
                last_month,
                Cost {
                    storage: 0.25,
                    complete: true,
                    ..Cost::default()
                },
            ),
            (
                last_day,
                Cost {
                    storage: 0.125,
                    ..Cost::default()
                },
            ),
        ]
        .into(),
        ..MonthData::default()
    };
    let before_rollover = midnight(last_day) + chrono::TimeDelta::hours(12);
    save(
        &app,
        source,
        &mut feed,
        last_month,
        before_rollover.timestamp(),
        prior,
    )
    .await
    .unwrap();
    for window in &windows[1..] {
        let days = window
            .from
            .date_naive()
            .iter_days()
            .take_while(|day| *day < window.to.date_naive())
            .map(|day| {
                (
                    day,
                    Cost {
                        complete: true,
                        ..Cost::default()
                    },
                )
            })
            .collect();
        let data = MonthData {
            days,
            ..MonthData::default()
        };
        save(&app, source, &mut feed, window.month, now.timestamp(), data)
            .await
            .unwrap();
    }

    // Older snapshots could be marked closed even with missing days.
    feed.months.get_mut(&last_month).unwrap().data.closed = true;
    // The response omits the already recorded first and last days.
    let new_day = last_month.succ_opt().unwrap();
    let bytes = 31_000_000_000;
    mock.state.lock().unwrap().storage = Some([(new_day, bytes)].into());
    backfill(&app, &client, source, &mut feed, now)
        .await
        .unwrap();
    assert_eq!(mock.state.lock().unwrap().requests.len(), 2);
    let partial = &feed.months[&last_month].data;
    assert!(!partial.closed && partial.partial);
    assert!((partial.month_to_date.unwrap() - 0.385).abs() < 1e-10);
    assert!(!partial.days[&last_day].complete);
    let expected = serde_json::to_value(app.bunny_costs.view(now).unwrap()).unwrap();
    assert_eq!(
        expected["history"].as_array().unwrap().last().unwrap()["storage"]["partial"],
        true
    );
    assert_eq!(expected["cdn"]["status"], "unavailable");
    assert!(!expected.to_string().contains(KEY));
    drop(app);

    let restarted = open();
    restarted.bunny_costs.enable(&cfg);
    let mut feed = Feed::default();
    refresh_current(&restarted, &client, source, &mut feed, now)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(restarted.bunny_costs.view(now).unwrap()).unwrap(),
        expected
    );
    assert_eq!(mock.state.lock().unwrap().requests.len(), 2);

    // A later result covers the remaining days, including the previously
    // provisional last day, and tries to replace the already completed first day.
    mock.state.lock().unwrap().storage = Some(
        last_month
            .iter_days()
            .take_while(|day| *day <= last_day)
            .map(|day| (day, bytes))
            .collect(),
    );
    backfill(&restarted, &client, source, &mut feed, now)
        .await
        .unwrap();
    let complete = &feed.months[&last_month].data;
    assert!(complete.closed && !complete.partial);
    assert!((complete.month_to_date.unwrap() - 0.55).abs() < 1e-10);
    assert!(complete.days[&last_day].complete);
    backfill(&restarted, &client, source, &mut feed, now)
        .await
        .unwrap();
    assert_eq!(
        mock.state.lock().unwrap().requests.len(),
        3,
        "completed and retention-truncated months are settled"
    );
    let complete_view = serde_json::to_value(restarted.bunny_costs.view(now).unwrap()).unwrap();
    assert_eq!(
        complete_view["history"].as_array().unwrap().last().unwrap()["storage"]["partial"],
        false
    );

    mock.state.lock().unwrap().status = Some(StatusCode::FORBIDDEN);
    let later = now + chrono::TimeDelta::hours(7);
    let error = refresh_current(&restarted, &client, source, &mut feed, later)
        .await
        .unwrap_err();
    assert_eq!(error, Failure::Http(403));
    feed.failed = true;
    restarted.bunny_costs.publish(source.service, &feed);
    let stale = serde_json::to_value(restarted.bunny_costs.view(later).unwrap()).unwrap();
    assert_eq!(stale["storage"]["status"], "unavailable");
    assert_eq!(stale["storage"]["last_updated_secs"], now.timestamp());
    assert_eq!(stale["history"], complete_view["history"]);
}
