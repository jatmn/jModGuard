//! Owner-only estimates from Bunny statistics. HTTP and database work run in
//! the background; Bot Resources renders a sanitized, in-memory snapshot.

mod client;

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Datelike as _, Days as CalendarDays, Months, NaiveDate, Utc};
use jmodguard::db::bunny_costs::{
    self as db, BunnyCostSnapshot, BunnyCpuAverages, BunnyDayCost as Cost,
    BunnyMonthData as MonthData, BunnyService as Service, CostDays as Days, SourceDigest,
};
use jmodguard::db::{ControlOp, ControlReply};
use reqwest::header::HeaderValue;
use serde::Serialize;

use super::state::ApiState;
use client::Client;

const REFRESH: Duration = Duration::from_hours(6);
const RETRY: Duration = Duration::from_mins(15);
const DEADLINE: Duration = Duration::from_secs(5);
const HISTORY_MONTHS: Months = Months::new(12);

fn retention(service: Service) -> CalendarDays {
    CalendarDays::new(match service {
        Service::CoreDatabase | Service::GuildDatabase => 180,
        Service::MagicContainers | Service::Storage => 365,
    })
}

struct Source {
    service: Service,
    // Neither this type nor the outbound client implements Debug.
    id: String,
    key: HeaderValue,
}

impl Source {
    fn new(service: Service, id: String, key: &str) -> Option<Self> {
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
            || key.is_empty()
            || key.len() > 256
        {
            return None;
        }
        let mut key = HeaderValue::from_str(key).ok()?;
        key.set_sensitive(true);
        Some(Self { service, id, key })
    }

    fn digest(&self) -> SourceDigest {
        let digest = ring::digest::digest(&ring::digest::SHA256, self.id.as_bytes());
        let mut bytes = [0; 32];
        bytes.copy_from_slice(digest.as_ref());
        bytes
    }
}

struct Config {
    sources: Vec<Source>,
}

impl Config {
    fn load(get: impl Fn(&str) -> Option<String>) -> Self {
        let mc_key = get("JMODGUARD_BUNNY_MC_STATS_KEY");
        let data_key = get("JMODGUARD_BUNNY_DATA_STATS_KEY");
        let mc = get("JMODGUARD_BUNNY_MC_APP_ID");
        let core = get("JMODGUARD_BUNNY_DB_CORE_ID");
        let guild = get("JMODGUARD_BUNNY_DB_GUILD_ID");
        let storage = get("JMODGUARD_BUNNY_STORAGE_ZONE_ID");
        let guild = guild.filter(|id| Some(id) != core.as_ref());
        let sources = [
            (Service::MagicContainers, mc, mc_key.as_deref()),
            (Service::CoreDatabase, core, data_key.as_deref()),
            (Service::GuildDatabase, guild, data_key.as_deref()),
            (Service::Storage, storage, data_key.as_deref()),
        ]
        .into_iter()
        .filter_map(|(service, id, key)| Source::new(service, id?, key?))
        .collect();
        Self { sources }
    }
}

/// Only fixed categories and bounded numeric status codes cross the HTTP
/// boundary. In particular, neither the response body nor the request URL
/// can reach logs or the cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    Configuration,
    Transport,
    Http(u16),
    Timeout,
    Response,
    Persistence,
}

fn warn(service: Service, failure: Failure) {
    tracing::warn!(
        service = service.name(),
        ?failure,
        "Bunny statistics refresh failed"
    );
}

#[derive(Clone, Copy)]
struct Window {
    month: NaiveDate,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
}

impl Window {
    fn query_dates(self) -> (String, String) {
        // Use half-open windows internally and an inclusive upper query bound.
        (
            client::query_time(self.from),
            client::query_time(self.to - chrono::TimeDelta::milliseconds(1)),
        )
    }

    fn day(self, time: DateTime<Utc>) -> Option<NaiveDate> {
        (self.from <= time && time < self.to).then(|| time.date_naive())
    }

    fn month_days(self) -> f64 {
        (shift_month(self.month, 1) - self.month).num_days() as f64
    }
}

fn midnight(date: NaiveDate) -> DateTime<Utc> {
    date.and_time(chrono::NaiveTime::MIN).and_utc()
}

fn month_start(date: NaiveDate) -> NaiveDate {
    date.with_day(1).expect("day one exists in every month")
}

fn shift_month(month: NaiveDate, offset: i32) -> NaiveDate {
    if offset < 0 {
        month - Months::new(offset.unsigned_abs())
    } else {
        month + Months::new(offset.unsigned_abs())
    }
}

fn backfill_windows(service: Service, now: DateTime<Utc>) -> Vec<Window> {
    // Round forward to a full UTC day so every request stays within the
    // provider's retention even when the worker starts late in the day.
    let earliest = midnight(now.date_naive() - retention(service) + CalendarDays::new(1));
    let month = month_start(now.date_naive());
    (1..=12)
        .map(|offset| shift_month(month, -offset))
        .map(|previous| Window {
            month: previous,
            from: midnight(previous).max(earliest),
            to: midnight(shift_month(previous, 1)),
        })
        // Let the provider finish publishing the last UTC day's data.
        .filter(|window| {
            window.from < window.to && (now - window.to).num_seconds() >= REFRESH.as_secs() as i64
        })
        .collect()
}

async fn collect(
    client: &Client,
    source: &Source,
    window: Window,
    historical: bool,
) -> Result<MonthData, Failure> {
    let mut data = MonthData::default();
    match source.service {
        Service::MagicContainers => {
            if historical {
                client.mc_days(source, window, false, &mut data).await?;
            } else {
                client.mc_summary(source, window, &mut data).await?;
                // Use the most recent six full days plus today's closed hours.
                let recent = midnight(window.to.date_naive() - CalendarDays::new(6));
                let hourly_from = window.from.max(recent);
                client
                    .mc_days(
                        source,
                        Window {
                            to: hourly_from,
                            ..window
                        },
                        false,
                        &mut data,
                    )
                    .await?;
                client
                    .mc_days(
                        source,
                        Window {
                            from: hourly_from,
                            ..window
                        },
                        true,
                        &mut data,
                    )
                    .await?;
            }
            data.magic_cpu_from_current_hours = true;
            data.magic_cpu_percent_converted = true;
        }
        Service::CoreDatabase | Service::GuildDatabase => {
            data.days = client.database_days(source, window, !historical).await?;
        }
        Service::Storage => data.days = client.storage_days(source, window).await?,
    }
    // Allow the same publication lag for a day at the end of a live
    // window as for the final day at month rollover.
    let complete_to = if historical {
        window.to
    } else {
        window.to - REFRESH
    };
    for (date, day) in &mut data.days {
        day.complete = *date < complete_to.date_naive();
    }
    data.summarize(source.service, window.month, window.to);
    Ok(data)
}

#[derive(Clone, Default)]
struct Feed {
    months: BTreeMap<NaiveDate, BunnyCostSnapshot>,
    failed: bool,
}

#[derive(Default)]
pub(super) struct Cache(RwLock<Option<BTreeMap<Service, Feed>>>);

impl Cache {
    fn enable(&self, config: &Config) {
        if let Ok(mut state) = self.0.write() {
            *state = (!config.sources.is_empty()).then(|| {
                config
                    .sources
                    .iter()
                    .map(|source| (source.service, Feed::default()))
                    .collect()
            });
        }
    }

    #[cfg(test)]
    pub(super) fn configure_for_test(&self, get: impl Fn(&str) -> Option<String>) {
        self.enable(&Config::load(get));
    }

    fn publish(&self, service: Service, feed: &Feed) {
        if let Ok(mut cache) = self.0.write()
            && let Some(slot) = cache
                .as_mut()
                .and_then(|services| services.get_mut(&service))
        {
            *slot = feed.clone();
        }
    }

    pub(super) fn view(&self, now: DateTime<Utc>) -> Option<Report> {
        let cache = self.0.read().ok()?;
        let feeds = cache.as_ref()?;
        let month = month_start(now.date_naive());
        let magic: Vec<_> = feeds.get(&Service::MagicContainers).into_iter().collect();
        let storage: Vec<_> = feeds.get(&Service::Storage).into_iter().collect();
        let databases: Vec<_> = [Service::CoreDatabase, Service::GuildDatabase]
            .iter()
            .filter_map(|service| feeds.get(service))
            .collect();
        let mut history = Vec::new();
        for offset in (1..=12).rev() {
            let previous = shift_month(month, -offset);
            let magic_containers = historic(&magic, previous);
            let database = historic(&databases, previous);
            let storage = historic(&storage, previous);
            if magic_containers.is_some() || database.is_some() || storage.is_some() {
                history.push(HistoryMonth {
                    month: previous.format("%Y-%m").to_string(),
                    magic_containers,
                    database,
                    storage,
                });
            }
        }
        let mc_month = magic.first().and_then(|feed| feed.months.get(&month));
        let breakdown = mc_month.map(|row| {
            let days = row.data.days.values();
            let egress_rate_unknown = row.data.egress_rate_unknown();
            MagicBreakdown {
                cpu: days
                    .clone()
                    .filter_map(|day| day.hourly_cpu_cost)
                    .reduce(|a, b| a + b),
                ram: (!row.data.days.is_empty()).then(|| days.clone().map(|day| day.ram).sum()),
                egress: (!row.data.days.is_empty() && !egress_rate_unknown)
                    .then(|| days.clone().map(|day| day.egress).sum()),
                egress_rate_unknown,
                volume: row.data.allocated_volume_cost,
            }
        });
        Some(Report {
            status: "configured",
            month: month.format("%Y-%m").to_string(),
            magic_containers: current(&magic, month),
            magic_breakdown: breakdown,
            magic_cpu_hourly_averages: mc_month.and_then(|row| row.data.cpu_hourly_averages),
            database: current(&databases, month),
            storage: current(&storage, month),
            cdn: Current::default(),
            history,
        })
    }
}

#[derive(Serialize)]
pub(super) struct Report {
    status: &'static str,
    month: String,
    magic_containers: Current,
    magic_breakdown: Option<MagicBreakdown>,
    magic_cpu_hourly_averages: Option<BunnyCpuAverages>,
    database: Current,
    storage: Current,
    cdn: Current,
    history: Vec<HistoryMonth>,
}

#[derive(Serialize)]
struct MagicBreakdown {
    cpu: Option<f64>,
    ram: Option<f64>,
    egress: Option<f64>,
    egress_rate_unknown: bool,
    volume: Option<f64>,
}

#[derive(Serialize)]
struct Current {
    status: &'static str,
    configured_resources: usize,
    month_to_date_usd: Option<f64>,
    projected_usd: Option<f64>,
    last_updated_secs: Option<i64>,
    partial: bool,
}

impl Default for Current {
    fn default() -> Self {
        Self {
            status: "unavailable",
            configured_resources: 0,
            month_to_date_usd: None,
            projected_usd: None,
            last_updated_secs: None,
            partial: false,
        }
    }
}

fn current(feeds: &[&Feed], month: NaiveDate) -> Current {
    let mut view = Current {
        configured_resources: feeds.len(),
        ..Current::default()
    };
    if feeds.is_empty() {
        return view;
    }
    let rows: Option<Vec<_>> = feeds.iter().map(|feed| feed.months.get(&month)).collect();
    let Some(rows) = rows else { return view };
    view.month_to_date_usd = rows.iter().map(|row| row.data.month_to_date).sum();
    view.projected_usd = rows.iter().map(|row| row.data.projection).sum();
    view.last_updated_secs = rows.iter().map(|row| row.fetched_at).min();
    view.partial = rows.iter().any(|row| row.data.partial);
    if !feeds.iter().any(|feed| feed.failed)
        && (view.month_to_date_usd.is_some() || view.projected_usd.is_some())
    {
        view.status = "available";
    }
    view
}

#[derive(Serialize)]
struct HistoricalCost {
    usd: Option<f64>,
    partial: bool,
    egress_rate_unknown: bool,
}

#[derive(Serialize)]
struct HistoryMonth {
    month: String,
    magic_containers: Option<HistoricalCost>,
    database: Option<HistoricalCost>,
    storage: Option<HistoricalCost>,
}

fn historic(feeds: &[&Feed], month: NaiveDate) -> Option<HistoricalCost> {
    if feeds.is_empty() {
        return None;
    }
    let rows: Option<Vec<_>> = feeds.iter().map(|feed| feed.months.get(&month)).collect();
    let rows = rows?;
    let egress_rate_unknown = rows.iter().any(|row| row.data.egress_rate_unknown());
    let usd = rows
        .iter()
        .map(|row| row.data.month_to_date)
        .sum::<Option<f64>>();
    let end = shift_month(month, 1);
    (usd.is_some() || egress_rate_unknown).then(|| HistoricalCost {
        usd,
        partial: rows.iter().any(|row| !row.data.covers(month, end)),
        egress_rate_unknown,
    })
}

async fn save(
    app: &ApiState,
    source: &Source,
    feed: &mut Feed,
    month: NaiveDate,
    fetched_at: i64,
    data: MonthData,
) -> Result<(), Failure> {
    let snapshot = BunnyCostSnapshot {
        month,
        fetched_at,
        data,
    };
    let reply = app
        .commit(ControlOp::StoreBunnyCostSnapshot {
            service: source.service,
            source_digest: source.digest(),
            snapshot,
        })
        .await
        .map_err(|_| Failure::Persistence)?;
    let ControlReply::Saved { body } = reply else {
        return Err(Failure::Persistence);
    };
    let stored = serde_json::from_value(body).map_err(|_| Failure::Persistence)?;
    feed.months.insert(month, stored);
    app.bunny_costs.publish(source.service, feed);
    Ok(())
}

async fn refresh_current(
    app: &ApiState,
    client: &Client,
    source: &Source,
    feed: &mut Feed,
    now: DateTime<Utc>,
) -> Result<(), Failure> {
    let month = month_start(now.date_naive());
    let first = month - HISTORY_MONTHS;
    let stored = db::bunny_cost_snapshots(&app.db, source.service, source.digest(), first, month)
        .await
        .map_err(|_| Failure::Persistence)?;
    feed.months = stored.into_iter().map(|row| (row.month, row)).collect();
    app.bunny_costs.publish(source.service, feed);

    let from = midnight(month);
    if now <= from {
        return Ok(());
    }
    if !feed.failed
        && feed.months.get(&month).is_some_and(|row| {
            (source.service != Service::MagicContainers || row.data.magic_cpu_from_current_hours)
                && (0..REFRESH.as_secs() as i64).contains(&(now.timestamp() - row.fetched_at))
        })
    {
        return Ok(());
    }

    let data = collect(
        client,
        source,
        Window {
            month,
            from,
            to: now,
        },
        false,
    )
    .await?;
    feed.failed = false;
    save(app, source, feed, month, now.timestamp(), data).await
}

async fn backfill(
    app: &ApiState,
    client: &Client,
    source: &Source,
    feed: &mut Feed,
    now: DateTime<Utc>,
) -> Result<(), Failure> {
    for window in backfill_windows(source.service, now) {
        if feed.months.get(&window.month).is_some_and(|row| {
            (source.service != Service::MagicContainers
                || (row.data.magic_cpu_from_current_hours
                    && row.fetched_at >= window.to.timestamp()))
                && row
                    .data
                    .covers(window.from.date_naive(), window.to.date_naive())
        }) {
            continue;
        }
        // Pace historical requests and prioritize the current cycle.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let data = collect(client, source, window, true).await?;
        save(app, source, feed, window.month, now.timestamp(), data).await?;
    }
    Ok(())
}

async fn run(app: Arc<ApiState>, client: Arc<Client>, source: Source) {
    let mut feed = Feed::default();
    let mut retry = RETRY;
    loop {
        let now = Utc::now();
        let current = refresh_current(&app, &client, &source, &mut feed, now).await;
        if let Err(failure) = current {
            feed.failed = true;
            app.bunny_costs.publish(source.service, &feed);
            warn(source.service, failure);
        }
        let history = if current.is_ok() {
            backfill(&app, &client, &source, &mut feed, now).await
        } else {
            current
        };
        if let Err(failure) = history {
            if current.is_ok() {
                warn(source.service, failure);
            }
            tokio::time::sleep(retry).await;
            retry = (retry * 2).min(REFRESH);
        } else {
            retry = RETRY;
            tokio::time::sleep(REFRESH).await;
        }
    }
}

pub fn spawn(app: &Arc<ApiState>) {
    let config = Config::load(|name| std::env::var(name).ok());
    spawn_with_config(app, config, "https://api.bunny.net");
}

fn spawn_with_config(app: &Arc<ApiState>, config: Config, base: &str) {
    if config.sources.is_empty() {
        return;
    }
    let Ok(client) = Client::new(base, DEADLINE) else {
        tracing::warn!("Bunny statistics client could not start");
        return;
    };
    app.bunny_costs.enable(&config);
    let client = Arc::new(client);
    for source in config.sources {
        tokio::spawn(run(Arc::clone(app), Arc::clone(&client), source));
    }
}

#[cfg(test)]
mod tests;
