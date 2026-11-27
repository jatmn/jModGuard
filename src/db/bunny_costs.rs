//! Typed, bounded persistence for the API's sanitized Bunny estimates.

use std::collections::BTreeMap;

use anyhow::Context as _;
use chrono::{DateTime, Datelike as _, Months, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use super::{BoundedWrites as _, Db, OptionalExtension as _, Result, SqlExecutor, params};

pub type SourceDigest = [u8; 32];
pub type CostDays = BTreeMap<NaiveDate, BunnyDayCost>;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BunnyService {
    MagicContainers,
    CoreDatabase,
    GuildDatabase,
    Storage,
}

impl BunnyService {
    pub fn name(self) -> &'static str {
        match self {
            Self::MagicContainers => "magic_containers",
            Self::CoreDatabase => "core_database",
            Self::GuildDatabase => "guild_database",
            Self::Storage => "storage",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct BunnyDayCost {
    // Older `cpu` and `cpu_core_cost` fields may include unverified charges
    // or daily rollups without usage duration. Only read hourly prices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hourly_cpu_cost: Option<f64>,
    pub ram: f64,
    pub egress: f64,
    // Retain a zero on the writer wire for older peers and discard legacy
    // chart-derived volume costs when reading.
    #[serde(skip_deserializing)]
    pub volume: f64,
    pub storage: f64,
    #[serde(default)]
    pub complete: bool,
    #[serde(default)]
    pub egress_rate_unknown: bool,
}

impl BunnyDayCost {
    fn has_only_hourly_cpu(self) -> bool {
        self.hourly_cpu_cost.is_some()
            && self.ram == 0.0
            && self.egress == 0.0
            && !self.egress_rate_unknown
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct BunnyCpuAverages {
    pub sampled_at: DateTime<Utc>,
    pub min: f64,
    pub max: f64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct BunnyMonthData {
    pub days: CostDays,
    #[serde(default)]
    pub magic_cpu_from_current_hours: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_hourly_averages: Option<BunnyCpuAverages>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocated_volume_cost: Option<f64>,
    pub month_to_date: Option<f64>,
    pub projection: Option<f64>,
    pub closed: bool,
    pub partial: bool,
}

impl BunnyMonthData {
    /// Coverage is based on completed observations for each UTC day.
    pub fn covers(&self, from: NaiveDate, to: NaiveDate) -> bool {
        from.iter_days()
            .take_while(|day| *day < to)
            .all(|day| self.days.get(&day).is_some_and(|cost| cost.complete))
    }

    pub fn egress_rate_unknown(&self) -> bool {
        self.days.values().any(|day| day.egress_rate_unknown)
    }

    fn magic_subtotal(&self) -> Option<f64> {
        if self.days.is_empty() {
            return self.allocated_volume_cost;
        }
        (!self.egress_rate_unknown()).then(|| {
            self.days
                .values()
                .map(|day| day.hourly_cpu_cost.unwrap_or_default() + day.ram + day.egress)
                .sum::<f64>()
                + self.allocated_volume_cost.unwrap_or_default()
        })
    }

    fn merge_days(&mut self, previous: CostDays, close_out_current: bool) {
        for (date, mut saved) in previous {
            // CPU-only days still need a daily observation at close-out.
            if close_out_current && saved.has_only_hourly_cpu() {
                saved.complete = false;
            }
            self.days
                .entry(date)
                .and_modify(|day| {
                    // Keep recorded rates and amounts from complete days.
                    if saved.complete {
                        let hourly_cpu_cost = if day.complete {
                            saved.hourly_cpu_cost.or(day.hourly_cpu_cost)
                        } else {
                            saved.hourly_cpu_cost
                        };
                        *day = BunnyDayCost {
                            hourly_cpu_cost,
                            ..saved
                        };
                    } else {
                        // Keep any saved closed-hour prices when a later
                        // daily rollup has no hourly coverage.
                        day.hourly_cpu_cost = day.hourly_cpu_cost.or(saved.hourly_cpu_cost);
                    }
                })
                .or_insert(saved);
        }
    }

    /// Recompute billing fields after assembling the day's observations.
    pub fn summarize(&mut self, service: BunnyService, month: NaiveDate, observed: DateTime<Utc>) {
        let next = month + Months::new(1);
        let completed_to = observed.date_naive().min(next);
        let volume_only = service == BunnyService::MagicContainers
            && self.days.is_empty()
            && self.allocated_volume_cost.is_some();
        self.partial = volume_only || !self.covers(month, completed_to);
        self.closed = !self.partial && completed_to == next;
        self.month_to_date = match service {
            BunnyService::MagicContainers => self.magic_subtotal(),
            BunnyService::Storage if !self.days.is_empty() => {
                Some(self.days.values().map(|day| day.storage).sum())
            }
            BunnyService::CoreDatabase | BunnyService::GuildDatabase
                if completed_to < next || !self.days.is_empty() =>
            {
                Some(0.0)
            }
            _ => None,
        };
        if completed_to == next {
            self.projection = None;
        } else if service != BunnyService::MagicContainers {
            let elapsed = (observed.timestamp() - midnight_secs(month)) as f64;
            self.projection = self
                .month_to_date
                .filter(|_| elapsed > 0.0)
                .map(|cost| cost * (midnight_secs(next) - midnight_secs(month)) as f64 / elapsed);
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BunnyCostSnapshot {
    pub month: NaiveDate,
    pub fetched_at: i64,
    pub data: BunnyMonthData,
}

impl BunnyCostSnapshot {
    fn refresh_stored_total(&mut self, service: BunnyService) {
        if service != BunnyService::MagicContainers {
            return;
        }
        if !self.data.magic_cpu_from_current_hours
            && let Some(end) = self.month.checked_add_months(Months::new(1))
            && self.fetched_at >= midnight_secs(end)
            && let Some(observed) = DateTime::from_timestamp(self.fetched_at, 0)
        {
            // Older history may include supplemental CPU with no daily
            // observation. Discard ambiguous CPU charges and coverage.
            self.data.days.retain(|_, day| {
                let keep_usage = !day.has_only_hourly_cpu();
                day.hourly_cpu_cost = None;
                keep_usage
            });
            self.data.summarize(service, self.month, observed);
        } else {
            self.data.month_to_date = self.data.magic_subtotal();
        }
    }
}

fn midnight_secs(date: NaiveDate) -> i64 {
    date.and_time(chrono::NaiveTime::MIN).and_utc().timestamp()
}

/// A single primary read loads the current month and at most 12 prior months.
pub async fn bunny_cost_snapshots(
    db: &Db,
    service: BunnyService,
    digest: SourceDigest,
    first_month: NaiveDate,
    current_month: NaiveDate,
) -> Result<Vec<BunnyCostSnapshot>> {
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let rows: String = tx.query_row(
            "SELECT json_group_array(json(data)) FROM (\
             SELECT data FROM bunny_cost_snapshots \
             WHERE service=?1 AND source_digest=?2 AND month BETWEEN ?3 AND ?4 \
             ORDER BY month DESC LIMIT 13)",
            params![
                service.name(),
                digest.to_vec(),
                midnight_secs(first_month),
                midnight_secs(current_month)
            ],
            |row| Ok(row.get(0)?),
        )?;
        tx.finish_read()?;
        let mut rows: Vec<BunnyCostSnapshot> = serde_json::from_str(&rows)?;
        for row in &mut rows {
            row.refresh_stored_total(service);
        }
        Ok(rows)
    })
    .await
}

/// The bot writes a typed source/month snapshot on the fenced control link.
/// Merge observed days under the write lock before computing completeness.
pub(super) async fn store_bunny_cost_snapshot(
    db: &Db,
    service: BunnyService,
    digest: SourceDigest,
    mut row: BunnyCostSnapshot,
) -> Result<BunnyCostSnapshot> {
    let month = midnight_secs(row.month);
    let observed =
        DateTime::from_timestamp(row.fetched_at, 0).context("invalid Bunny snapshot time")?;
    let valid_amount = |amount: f64| (0.0..=1e18).contains(&amount);
    anyhow::ensure!(
        row.month.day() == 1
            && row.month.checked_add_months(Months::new(1)).is_some()
            && row.fetched_at >= month
            && (observed.date_naive() - row.month).num_days() <= 400
            && row.data.days.len() <= 31
            && row.data.days.iter().all(|(date, cost)| {
                date.year() == row.month.year()
                    && date.month() == row.month.month()
                    && *date <= observed.date_naive()
                    && (!cost.complete || *date < observed.date_naive())
                    && cost.hourly_cpu_cost.is_none_or(valid_amount)
                    && [cost.ram, cost.egress, cost.storage]
                        .into_iter()
                        .all(valid_amount)
            })
            && row.data.allocated_volume_cost.is_none_or(valid_amount)
            && row.data.cpu_hourly_averages.is_none_or(|cpu| {
                service == BunnyService::MagicContainers
                    && cpu.sampled_at.year() == row.month.year()
                    && cpu.sampled_at.month() == row.month.month()
                    && cpu.sampled_at < observed
                    && cpu.min.is_finite()
                    && cpu.max.is_finite()
                    && cpu.min <= cpu.max
            })
            && row.data.month_to_date.is_none_or(valid_amount)
            && row.data.projection.is_none_or(valid_amount),
        "invalid Bunny cost snapshot"
    );
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_write()?;
        let digest = digest.to_vec();
        let previous = tx
            .query_row(
                "SELECT data FROM bunny_cost_snapshots WHERE service=?1 AND source_digest=?2 AND month=?3",
                params![service.name(), digest.clone(), month],
                |stored| Ok(stored.get::<String>(0)?),
            )
            .optional()?;
        if let Some(previous) = previous {
            let mut previous: BunnyCostSnapshot = serde_json::from_str(&previous)?;
            previous.refresh_stored_total(service);
            if previous.fetched_at > row.fetched_at {
                tx.finish_read()?;
                return Ok(previous);
            }
            let month_end = midnight_secs(row.month + Months::new(1));
            if row.fetched_at >= month_end {
                // Backfill has no allocation history. Keep the volume cost
                // estimated by the last observation within the month.
                row.data.allocated_volume_cost = previous.data.allocated_volume_cost;
            }
            let close_out_current = service == BunnyService::MagicContainers
                && previous.fetched_at < month_end
                && row.fetched_at >= month_end;
            row.data.merge_days(previous.data.days, close_out_current);
        }
        row.data.summarize(service, row.month, observed);
        anyhow::ensure!(
            row.data.month_to_date.is_none_or(valid_amount) && row.data.projection.is_none_or(valid_amount),
            "invalid merged Bunny cost snapshot"
        );
        let data = serde_json::to_string(&row)?;
        anyhow::ensure!(data.len() <= 16_384, "Bunny cost snapshot too large");
        tx.ensure_mutation_receipt()?;
        tx.execute_rows(
            "INSERT INTO bunny_cost_snapshots (service, month, source_digest, fetched_at, data) \
             VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(service, source_digest, month) DO UPDATE SET \
             fetched_at=excluded.fetched_at, data=excluded.data",
            params![service.name(), month, digest, row.fetched_at, data],
            1,
        )?;
        tx.commit()?;
        Ok(row)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_subtotal_includes_known_volume_with_empty_charts() {
        let month = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
        let observed = DateTime::from_timestamp(midnight_secs(month) + 30 * 60, 0).unwrap();
        for cost in [None, Some(0.0), Some(0.125)] {
            let mut data = BunnyMonthData {
                allocated_volume_cost: cost,
                ..BunnyMonthData::default()
            };
            data.summarize(BunnyService::MagicContainers, month, observed);
            assert_eq!(data.month_to_date, cost);
            assert_eq!(data.partial, cost.is_some());

            data.days.insert(
                month,
                BunnyDayCost {
                    egress_rate_unknown: true,
                    ..BunnyDayCost::default()
                },
            );
            data.summarize(BunnyService::MagicContainers, month, observed);
            assert_eq!(data.month_to_date, None);
        }
    }

    #[test]
    fn old_magic_history_does_not_keep_supplemental_cpu_coverage() {
        let month = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
        let end = month + Months::new(1);
        let missing_day = month.succ_opt().unwrap();
        let cpu_only = BunnyDayCost {
            hourly_cpu_cost: Some(0.01),
            complete: true,
            ..BunnyDayCost::default()
        };
        let daily_usage = BunnyDayCost {
            ram: 0.125,
            egress: 0.0625,
            ..cpu_only
        };
        let mut row = BunnyCostSnapshot {
            month,
            fetched_at: midnight_secs(end),
            data: BunnyMonthData {
                days: [(month, daily_usage), (missing_day, cpu_only)].into(),
                allocated_volume_cost: Some(0.25),
                ..BunnyMonthData::default()
            },
        };
        row.refresh_stored_total(BunnyService::MagicContainers);
        assert!(row.data.days[&month].complete);
        assert_eq!(row.data.days[&month].hourly_cpu_cost, None);
        assert!(!row.data.days.contains_key(&missing_day));
        assert_eq!(row.data.month_to_date, Some(0.4375));
    }
}
