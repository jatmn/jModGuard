use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use reqwest::header::HeaderValue;
use serde::{Deserialize, de::DeserializeOwned};

use super::{Cost, Days, Failure, Source, Window};

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const GB: f64 = 1_000_000_000.0;
const RAM_BLOCK: f64 = 64.0 * 1024.0 * 1024.0;
// List prices checked 2026-11-26. See docs/CONFIGURATION.md for the sources
// and assumptions, including the region/tier used for storage and traffic.
const RAM_BLOCK_HOUR: f64 = 0.0003125;
const CORE_HOUR: f64 = 0.02;
const EGRESS_GB: f64 = 0.01;
const VOLUME_GB_MONTH: f64 = 0.10;
const STORAGE_GB_MONTH: f64 = 0.01;

pub(super) struct Client {
    http: reqwest::Client,
    base: reqwest::Url,
}

impl Client {
    pub(super) fn new(base: &str, timeout: Duration) -> Result<Self, Failure> {
        let base = reqwest::Url::parse(base).map_err(|_| Failure::Configuration)?;
        let http = jmodguard_updater::http_client_builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .http1_only()
            .build()
            .map_err(|_| Failure::Configuration)?;
        Ok(Self { http, base })
    }

    async fn get<T: DeserializeOwned>(
        &self,
        source: &Source,
        prefix: &[&str],
        suffix: &[&str],
        query: &[(&str, &str)],
    ) -> Result<T, Failure> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| Failure::Configuration)?
            .extend(prefix)
            .push(&source.id)
            .extend(suffix);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        let key: HeaderValue = source.key.clone();
        // Drop third-party error objects at the boundary. They can contain the
        // URL or response body. HTTP/1 and the default quiet connection keep
        // HTTP/2 frames and raw wire data out of transport trace output.
        let mut response = self
            .http
            .get(url)
            .header("accesskey", key)
            .send()
            .await
            .map_err(transport_failure)?;
        if !response.status().is_success() {
            return Err(Failure::Http(response.status().as_u16()));
        }

        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport_failure)? {
            if chunk.len() > MAX_RESPONSE_BYTES - body.len() {
                return Err(Failure::Response);
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|_| Failure::Response)
    }

    pub(super) async fn mc_projection(&self, source: &Source) -> Result<f64, Failure> {
        let summary: McSummary = self.get(source, &["mc", "apps"], &["summary"], &[]).await?;
        nonnegative(summary.monthly_cost)
    }

    pub(super) async fn mc_days(
        &self,
        source: &Source,
        window: Window,
        hourly: bool,
        days: &mut Days,
    ) -> Result<(), Failure> {
        if window.from >= window.to {
            return Ok(());
        }
        let (from, to) = window.query_dates();
        let granularity = if hourly { "Hourly" } else { "Daily" };
        let charts: McCharts = self
            .get(
                source,
                &["mc", "apps"],
                &["statistics", "charts"],
                &[
                    ("fromDate", &from),
                    ("toDate", &to),
                    ("granularity", granularity),
                ],
            )
            .await?;
        let hours = if hourly { 1.0 } else { 24.0 };
        let month_hours = window.month_days() * 24.0;
        for series in charts.cpu.into_values() {
            for (time, value) in series.chart {
                if let Some(day) = window.day(time) {
                    days.entry(day).or_default().cpu += nonnegative(value)? * hours * CORE_HOUR;
                }
            }
        }
        for series in charts.ram.into_values() {
            for (time, value) in series.chart {
                if let Some(day) = window.day(time) {
                    let blocks = (nonnegative(value)? / RAM_BLOCK).ceil().max(1.0);
                    days.entry(day).or_default().ram += blocks * hours * RAM_BLOCK_HOUR;
                }
            }
        }
        for series in charts.traffic.into_values() {
            for (time, value) in series.chart {
                if let Some(day) = window.day(time) {
                    days.entry(day).or_default().egress += nonnegative(value)? / GB * EGRESS_GB;
                }
            }
        }
        for series in charts.volumes.into_values() {
            for (time, value) in series.chart {
                if let Some(day) = window.day(time) {
                    days.entry(day).or_default().volume +=
                        nonnegative(value)? / GB * hours / month_hours * VOLUME_GB_MONTH;
                }
            }
        }
        Ok(())
    }

    pub(super) async fn database_days(
        &self,
        source: &Source,
        window: Window,
        current: bool,
    ) -> Result<Days, Failure> {
        let (from, to) = window.query_dates();
        let query = [("from", from.as_str()), ("to", to.as_str())];
        let prefix = ["database", "v2", "databases"];
        let stats: DatabaseStats = self.get(source, &prefix, &["statistics"], &query).await?;
        let mut days = Days::new();
        for (time, value) in stats.reads.data.into_iter().chain(stats.writes.data) {
            if let Some(day) = window.day(time) {
                nonnegative(value)?;
                // Bunny does not charge for Database during public preview.
                days.entry(day).or_default();
            }
        }
        if current {
            let _: DatabaseUsage = self.get(source, &prefix, &["usage"], &query).await?;
        }
        Ok(days)
    }

    pub(super) async fn storage_days(
        &self,
        source: &Source,
        window: Window,
    ) -> Result<Days, Failure> {
        let (from, to) = window.query_dates();
        let stats: StorageStats = self
            .get(
                source,
                &["storagezone"],
                &["statistics"],
                &[("dateFrom", &from), ("dateTo", &to)],
            )
            .await?;
        let mut days = Days::new();
        for (date, bytes) in stats.used {
            let day = parse_storage_date(&date).ok_or(Failure::Response)?;
            let start = super::midnight(day);
            if window.from <= start && start < window.to {
                let fraction = ((window.to - start).num_seconds() as f64 / 86_400.0).min(1.0);
                days.insert(
                    day,
                    Cost {
                        storage: nonnegative(bytes)? / GB * STORAGE_GB_MONTH * fraction
                            / window.month_days(),
                        ..Cost::default()
                    },
                );
            }
        }
        Ok(days)
    }
}

fn transport_failure(error: reqwest::Error) -> Failure {
    if error.is_timeout() {
        Failure::Timeout
    } else {
        Failure::Transport
    }
}

fn nonnegative(value: f64) -> Result<f64, Failure> {
    if (0.0..=1e18).contains(&value) {
        Ok(value)
    } else {
        Err(Failure::Response)
    }
}

pub(super) fn query_time(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn parse_storage_date(date: &str) -> Option<NaiveDate> {
    if let Ok(day) = NaiveDate::parse_from_str(date, "%Y-%m-%d") {
        Some(day)
    } else {
        date.parse::<DateTime<Utc>>()
            .ok()
            .map(|time| time.date_naive())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct McSummary {
    monthly_cost: f64,
}

#[derive(Deserialize)]
struct McCharts {
    #[serde(rename = "cpuUsageSplitChart")]
    cpu: BTreeMap<String, McSeries>,
    #[serde(rename = "ramUsageSplitChart")]
    ram: BTreeMap<String, McSeries>,
    #[serde(rename = "trafficSplitChart")]
    traffic: BTreeMap<String, McSeries>,
    #[serde(rename = "volumesSplitCapacityChart", default)]
    volumes: BTreeMap<String, McSeries>,
}

#[derive(Deserialize)]
struct McSeries {
    chart: BTreeMap<DateTime<Utc>, f64>,
}

#[derive(Deserialize)]
struct DatabaseStats {
    #[serde(rename = "RowReadCount", alias = "rowReadCount")]
    reads: DatabaseChart,
    #[serde(rename = "RowWriteCount", alias = "rowWriteCount")]
    writes: DatabaseChart,
}

#[derive(Deserialize)]
struct DatabaseChart {
    #[serde(rename = "Data", alias = "data")]
    data: Vec<(DateTime<Utc>, f64)>,
}

#[derive(Deserialize)]
struct DatabaseUsage {
    #[serde(rename = "RowsRead", alias = "rowsRead")]
    _rows_read: u64,
    #[serde(rename = "RowsWritten", alias = "rowsWritten")]
    _rows_written: u64,
}

#[derive(Deserialize)]
struct StorageStats {
    #[serde(rename = "StorageUsedChart")]
    used: BTreeMap<String, f64>,
}
