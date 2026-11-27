use super::*;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse as _, Response},
};
use chrono::TimeZone as _;
use jmodguard::config::ApiProcessConfig;
use jmodguard::db::Db;
use serde_json::json;
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
    let (status, delay) = {
        let mut state = state.lock().unwrap();
        state.requests.push(uri.clone());
        (state.status, state.delay)
    };
    tokio::time::sleep(delay).await;
    if let Some(status) = status {
        return (status, format!("reflected request {uri} with {KEY}")).into_response();
    }
    let date = "2026-11-17T10:00:00Z";
    let body = match uri.path() {
        path if path == format!("/mc/apps/{MC}/summary") => {
            json!({"monthlyCost": 2.50, "unused": KEY, "unusedId": MC})
        }
        path if path == format!("/mc/apps/{MC}/statistics/charts") => json!({
            "cpuUsageSplitChart": {"placeholder-pod": {"chart": {date: 0.5}}},
            "ramUsageSplitChart": {"placeholder-pod": {"chart": {date: 67_108_865}}},
            "trafficSplitChart": {"placeholder-pod": {"chart": {date: 500_000_000}}},
            "volumesSplitCapacityChart": {"placeholder-volume": {"chart": {date: 1_000_000_000}}},
            "releases": [{"secret": KEY}],
        }),
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
            json!({"StorageUsedChart": {&from[..10]: 30_000_000_000_u64}, "ignored": KEY})
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
                let expected = 0.01 + 2.0 * 0.0003125 + 0.005 + 0.10 / 720.0;
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
        assert!(source.key.is_sensitive());
        for code in [301, 400, 401, 403, 404, 429, 500, 503] {
            mock.state.lock().unwrap().status = Some(StatusCode::from_u16(code).unwrap());
            let failure = client.mc_projection(source).await.unwrap_err();
            assert_eq!(failure, Failure::Http(code));
            warn(source.service, failure);
        }
        mock.state.lock().unwrap().delay = Duration::from_secs(1);
        let short = mock.client(Duration::from_millis(50));
        let failure = short.mc_projection(source).await.unwrap_err();
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
async fn backfill_survives_api_restart_and_does_not_repeat_completed_requests() {
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

    // Produce the first saved month via the real path and seed the others as
    // completed checkpoints so this test only waits for one paced request.
    let windows = backfill_windows(source.service, now);
    let last_month = windows[0].month;
    refresh_current(&app, &client, source, &mut feed, now)
        .await
        .unwrap();
    for window in &windows[1..] {
        let data = MonthData {
            days: Days::new(),
            month_to_date: None,
            projection: None,
            closed: true,
            partial: true,
        };
        save(&app, source, &mut feed, window.month, now.timestamp(), data)
            .await
            .unwrap();
    }
    backfill(&app, &client, source, &mut feed, now)
        .await
        .unwrap();
    assert_eq!(mock.state.lock().unwrap().requests.len(), 2);
    let expected = serde_json::to_value(app.bunny_costs.view(now).unwrap()).unwrap();
    assert_eq!(
        expected["history"][0]["month"],
        last_month.format("%Y-%m").to_string()
    );
    assert_eq!(expected["history"][0]["storage"]["partial"], true);
    assert!(expected["history"][0]["storage"]["usd"].as_f64().unwrap() > 0.0);
    assert_eq!(expected["cdn"]["status"], "unavailable");
    assert!(!expected.to_string().contains(KEY));
    drop(app);

    let restarted = open();
    restarted.bunny_costs.enable(&cfg);
    let mut feed = Feed::default();
    refresh_current(&restarted, &client, source, &mut feed, now)
        .await
        .unwrap();
    backfill(&restarted, &client, source, &mut feed, now)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(restarted.bunny_costs.view(now).unwrap()).unwrap(),
        expected
    );
    assert_eq!(mock.state.lock().unwrap().requests.len(), 2);

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
    assert_eq!(stale["history"], expected["history"]);
}
