//! Typed, bounded persistence for the API's sanitized Bunny estimates.

use std::collections::BTreeMap;

use chrono::{Datelike as _, NaiveDate};
use serde::{Deserialize, Serialize};

use super::{BoundedWrites as _, Db, Result, SqlExecutor, params};

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
    pub cpu: f64,
    pub ram: f64,
    pub egress: f64,
    pub volume: f64,
    pub storage: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BunnyMonthData {
    pub days: CostDays,
    pub month_to_date: Option<f64>,
    pub projection: Option<f64>,
    pub closed: bool,
    pub partial: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BunnyCostSnapshot {
    pub month: NaiveDate,
    pub fetched_at: i64,
    pub data: BunnyMonthData,
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
        Ok(serde_json::from_str(&rows)?)
    })
    .await
}

/// The bot writes a typed source/month snapshot on the fenced control link.
/// Replays replace the same row and cannot overwrite a newer observation.
pub(super) async fn store_bunny_cost_snapshot(
    db: &Db,
    service: BunnyService,
    digest: SourceDigest,
    row: BunnyCostSnapshot,
) -> Result<()> {
    let month = midnight_secs(row.month);
    let observed = chrono::DateTime::from_timestamp(row.fetched_at, 0);
    let valid_amount = |amount: f64| (0.0..=1e18).contains(&amount);
    anyhow::ensure!(
        row.month.day() == 1
            && observed.is_some_and(|time| {
                row.fetched_at >= month && (time.date_naive() - row.month).num_days() <= 400
            })
            && row.data.days.len() <= 31
            && row.data.days.iter().all(|(date, cost)| {
                date.year() == row.month.year()
                    && date.month() == row.month.month()
                    && [cost.cpu, cost.ram, cost.egress, cost.volume, cost.storage]
                        .into_iter()
                        .all(valid_amount)
            })
            && row.data.month_to_date.is_none_or(valid_amount)
            && row.data.projection.is_none_or(valid_amount),
        "invalid Bunny cost snapshot"
    );
    let data = serde_json::to_string(&row)?;
    anyhow::ensure!(data.len() <= 16_384, "Bunny cost snapshot too large");
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_write()?;
        tx.ensure_mutation_receipt()?;
        tx.execute_rows(
            "INSERT INTO bunny_cost_snapshots (service, month, source_digest, fetched_at, data) \
             VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(service, month) DO UPDATE SET \
             source_digest=excluded.source_digest, fetched_at=excluded.fetched_at, data=excluded.data \
             WHERE excluded.fetched_at >= bunny_cost_snapshots.fetched_at",
            params![service.name(), month, digest.to_vec(), row.fetched_at, data],
            1,
        )?;
        tx.commit()
    })
    .await
}
