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
    // The legacy `cpu` field used unverified chart units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_core_cost: Option<f64>,
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
    pub magic_cpu_priced: bool,
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
        (!self.days.is_empty() && !self.egress_rate_unknown()).then(|| {
            self.days
                .values()
                .map(|day| day.cpu_core_cost.unwrap_or_default() + day.ram + day.egress)
                .sum::<f64>()
                + self.allocated_volume_cost.unwrap_or_default()
        })
    }

    fn merge_days(&mut self, previous: CostDays) {
        for (date, saved) in previous {
            self.days
                .entry(date)
                .and_modify(|day| {
                    // Keep a collected full day, including its original
                    // hourly detail, when a later rollup revisits that date.
                    if saved.complete {
                        // Fill in verified CPU pricing when a legacy snapshot
                        // already has a full day of RAM and traffic.
                        let cpu_core_cost = if day.complete {
                            saved.cpu_core_cost.or(day.cpu_core_cost)
                        } else {
                            saved.cpu_core_cost
                        };
                        *day = BunnyDayCost {
                            cpu_core_cost,
                            ..saved
                        };
                    }
                })
                .or_insert(saved);
        }
    }

    /// Recompute billing fields after assembling the day's observations.
    pub fn summarize(&mut self, service: BunnyService, month: NaiveDate, observed: DateTime<Utc>) {
        let next = month + Months::new(1);
        let completed_to = observed.date_naive().min(next);
        self.partial = !self.covers(month, completed_to);
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
        if service == BunnyService::MagicContainers {
            // Earlier snapshots included unverified chart charges in the total.
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
                    && cost.cpu_core_cost.is_none_or(valid_amount)
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
            if previous.fetched_at > row.fetched_at {
                tx.finish_read()?;
                previous.refresh_stored_total(service);
                return Ok(previous);
            }
            if observed.date_naive() >= row.month + Months::new(1) {
                // Backfill has no allocation history. Keep the volume cost
                // estimated by the last observation within the month.
                row.data.allocated_volume_cost = previous.data.allocated_volume_cost;
            }
            row.data.merge_days(previous.data.days);
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
