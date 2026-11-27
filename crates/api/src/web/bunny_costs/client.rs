use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use reqwest::header::HeaderValue;
use serde::{Deserialize, de::DeserializeOwned};

use super::{BunnyCpuAverages, Cost, Days, Failure, MonthData, Source, Window};

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const GB: f64 = 1_000_000_000.0;
const RAM_BLOCK: f64 = 64.0 * 1024.0 * 1024.0;
// List prices checked 2026-11-26. See docs/CONFIGURATION.md for the sources
// and assumptions, including the Storage tier.
const RAM_BLOCK_HOUR: f64 = 0.0003125;
const CPU_CORE_HOUR: f64 = 0.02;
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

    pub(super) async fn mc_summary(
        &self,
        source: &Source,
        window: Window,
        data: &mut MonthData,
    ) -> Result<(), Failure> {
        let summary: McSummary = self.get(source, &["mc", "apps"], &["summary"], &[]).await?;
        data.projection = Some(nonnegative(summary.monthly_cost)?);
        let elapsed_days =
            (window.to - super::midnight(window.month)).num_seconds() as f64 / 86_400.0;
        data.allocated_volume_cost = summary
            .total_volume_size_in_gb
            .and_then(|gb| nonnegative(gb).ok())
            .map(|gb| gb * VOLUME_GB_MONTH * elapsed_days / window.month_days());
        Ok(())
    }

    pub(super) async fn mc_days(
        &self,
        source: &Source,
        window: Window,
        hourly: bool,
        data: &mut MonthData,
    ) -> Result<(), Failure> {
        let hours = if hourly { 1 } else { 24 };
        let seconds = 3600 * hours;
        // Bunny can return buckets past toDate, including the open bucket.
        // Restrict queries and pricing to complete hours or UTC days.
        let to = DateTime::from_timestamp(window.to.timestamp().div_euclid(seconds) * seconds, 0)
            .ok_or(Failure::Response)?;
        if window.from >= to {
            return Ok(());
        }
        let (from_date, to_date) = Window { to, ..window }.query_dates();
        let granularity = if hourly { "Hourly" } else { "Daily" };
        let charts: McCharts = self
            .get(
                source,
                &["mc", "apps"],
                &["statistics", "charts"],
                &[
                    ("fromDate", &from_date),
                    ("toDate", &to_date),
                    ("granularity", granularity),
                ],
            )
            .await?;
        let range = window.from..to;
        let hours = hours as f64;
        // A daily CPU mean has no duration. Only closed hourly CPU points
        // contribute to the per-day sum of core-hours.
        if hourly {
            for series in charts.cpu.into_values() {
                let mut samples = series.chart.range(range.clone());
                for (&time, &value) in samples.clone() {
                    let day = data.days.entry(time.date_naive()).or_default();
                    *day.hourly_cpu_cost.get_or_insert(0.0) += nonnegative(value)? * CPU_CORE_HOUR;
                }
                if let Some((&sampled_at, &value)) = samples.next_back() {
                    match &mut data.cpu_hourly_averages {
                        Some(cpu) if sampled_at < cpu.sampled_at => {}
                        Some(cpu) if sampled_at == cpu.sampled_at => {
                            cpu.min = cpu.min.min(value);
                            cpu.max = cpu.max.max(value);
                        }
                        _ => {
                            data.cpu_hourly_averages = Some(BunnyCpuAverages {
                                sampled_at,
                                min: value,
                                max: value,
                            });
                        }
                    }
                }
            }
        }
        // Daily RAM points average their raw samples. Traffic points are
        // byte totals for their bucket at either granularity.
        let days = &mut data.days;
        for series in charts.ram.into_values() {
            for (&time, &value) in series.chart.range(range.clone()) {
                let blocks = (nonnegative(value)? / RAM_BLOCK).ceil().max(1.0);
                days.entry(time.date_naive()).or_default().ram += blocks * hours * RAM_BLOCK_HOUR;
            }
        }
        for series in charts.traffic.into_values() {
            let rate = series.region.as_deref().and_then(egress_usd_per_gb);
            for (&time, &value) in series.chart.range(range.clone()) {
                let bytes = nonnegative(value)?;
                let cost = days.entry(time.date_naive()).or_default();
                if let Some(rate) = rate {
                    cost.egress += bytes / GB * rate;
                } else if bytes > 0.0 {
                    cost.egress_rate_unknown = true;
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

// Magic Containers list prices in USD/GB, also listed for Standard CDN:
// https://bunny.net/docs/magic-containers/pricing#network-traffic-usage
// https://bunny.net/pricing/
// Match published city names, documented MC CLI IDs, and Bunny's geography codes:
// https://bunny.net/docs/magic-containers/regions
// https://bunny.net/docs/cli/commands/sandbox
// https://bunny.net/docs/api-reference/core/pull-zone/add-pull-zone
fn egress_usd_per_gb(region: &str) -> Option<f64> {
    match region.trim().to_lowercase().as_str() {
        "eu"
        | "europe"
        | "us"
        | "north america"
        | "europe & north america"
        | "ams"
        | "ny"
        | "la"
        | "ashburn"
        | "atlanta"
        | "boston"
        | "chicago"
        | "dallas"
        | "denver"
        | "los angeles"
        | "miami"
        | "new york city"
        | "san jose"
        | "seattle"
        | "toronto"
        | "amsterdam"
        | "athens"
        | "bucharest"
        | "copenhagen"
        | "frankfurt"
        | "london"
        | "madrid"
        | "milan"
        | "paris"
        | "prague"
        | "stockholm"
        | "vienna"
        | "warsaw"
        | "zagreb" => Some(0.01),
        "sa" | "south america" | "bogota" | "bogotá" | "mexico city" | "sao paulo"
        | "são paulo" => Some(0.045),
        "asia" | "oceania" | "asia & oceania" | "bangkok" | "hong kong" | "jakarta"
        | "kuala lumpur" | "manila" | "singapore" | "tokyo" | "sydney" => Some(0.03),
        "af" | "africa" | "middle east" | "middle east & africa" | "johannesburg" | "lagos" => {
            Some(0.06)
        }
        _ => None,
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
    total_volume_size_in_gb: Option<f64>,
}

#[derive(Deserialize)]
struct McCharts {
    #[serde(rename = "cpuUsageSplitChart", default)]
    cpu: BTreeMap<String, McSeries>,
    #[serde(rename = "ramUsageSplitChart")]
    ram: BTreeMap<String, McSeries>,
    #[serde(rename = "trafficSplitChart")]
    traffic: BTreeMap<String, McSeries>,
}

#[derive(Deserialize)]
struct McSeries {
    region: Option<String>,
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
