//! Owner Bot Resources storage: bounded per-process rollups and the bot's
//! latest status. Only the bot writes. Each new minute is merged into its
//! hour row in the same transaction, so 7- and 30-day ranges read hour rows
//! and never scan a month of minutes. Every write prunes expired rows through
//! the primary-key range, without scanning the table.

use super::write_budget::BoundedWrites;
use super::{Db, OptionalExtension, SqlExecutor, params};
use crate::resources::{HOUR_SECS, MINUTE_SECS, ProcessStatus, Rollup};
use anyhow::Result;

/// Minute rows serve the 1-hour and 24-hour ranges.
pub const RESOURCE_MINUTE_RETENTION_SECS: i64 = 48 * 3600;
/// Hour rows serve the 7-day and 30-day ranges.
pub const RESOURCE_HOUR_RETENTION_SECS: i64 = 31 * 86_400;
/// Committed minute keys (including existing rows) and the number newly inserted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResourceRollupWrite {
    pub minute_rows_inserted: usize,
    pub bot_minute_starts: Vec<i64>,
    pub api_minute_starts: Vec<i64>,
}

/// Bytes of one stored rollup or status, matching the schema CHECK.
const MAX_ROW_BYTES: usize = 16_384;
/// Minute rows accepted in one write (the API keeps 30 recent minutes).
const MAX_MINUTES_PER_WRITE: usize = 64;
/// Rows one history read returns per process: a full day of minutes, or a
/// month of hours.
pub const RESOURCE_HISTORY_ROW_LIMIT: i64 = 1500;

/// Expired rows of one span, through the primary-key range.
const PRUNE_SQL: &str = "DELETE FROM resource_rollups \
     WHERE process IN ('bot', 'api') AND span_secs=?1 AND start_secs<?2";

/// One process's newest rows in a window, through the primary-key range.
fn history_sql(process: ResourceProcess) -> String {
    format!(
        "SELECT data FROM resource_rollups WHERE process='{}' AND span_secs=?1 \
         AND start_secs>=?2 AND start_secs<?3 ORDER BY start_secs DESC LIMIT ?4",
        process.as_str()
    )
}

/// Which process a rollup describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ResourceProcess {
    Bot,
    Api,
}

impl ResourceProcess {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bot => "bot",
            Self::Api => "api",
        }
    }
}

/// Stored rollups and statuses for one history window, from one snapshot.
#[derive(Clone, Debug, Default)]
pub struct ResourceRecords {
    pub bot: Vec<Rollup>,
    pub api: Vec<Rollup>,
    /// `(process, updated_at_ms, status)` for each process with a status row.
    pub statuses: Vec<(String, i64, ProcessStatus)>,
}

/// Serialized row data within the stored bound. Label breakdowns are the
/// only unbounded-looking part; they are dropped before the totals are.
fn bounded_json<T: serde::Serialize>(value: &T) -> Result<String> {
    let text = serde_json::to_string(value)?;
    anyhow::ensure!(text.len() <= MAX_ROW_BYTES, "resource row too large");
    Ok(text)
}

fn rollup_json(rollup: &Rollup) -> Result<String> {
    match bounded_json(rollup) {
        Ok(text) => Ok(text),
        Err(_) => {
            let mut trimmed = rollup.clone();
            trimmed.data.labelled.clear();
            bounded_json(&trimmed)
        }
    }
}

/// Saves completed minutes (new ones only; a repeated minute is ignored),
/// merges each new minute into its hour, replaces `status`, and prunes rows
/// past retention. Acknowledges accepted minutes only after commit.
pub async fn record_resource_rollups(
    db: &Db,
    minutes: Vec<(ResourceProcess, Rollup)>,
    status: Option<(ResourceProcess, ProcessStatus)>,
    now_secs: i64,
) -> Result<ResourceRollupWrite> {
    let oldest = now_secs - RESOURCE_MINUTE_RETENTION_SECS;
    let latest = now_secs + MINUTE_SECS;
    let mut accepted = Vec::new();
    for (process, rollup) in minutes.into_iter().take(MAX_MINUTES_PER_WRITE) {
        if rollup.span_secs != MINUTE_SECS
            || rollup.start_secs <= 0
            || rollup.start_secs % MINUTE_SECS != 0
            || rollup.start_secs < oldest
            || rollup.start_secs > latest
        {
            continue;
        }
        let text = rollup_json(&rollup)?;
        accepted.push((process, rollup, text));
    }
    let status = match status {
        Some((process, status)) => Some((
            process,
            status.sampled_at_ms.unwrap_or(now_secs * 1000).max(1),
            bounded_json(&status)?,
        )),
        None => None,
    };
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_write()?;
        tx.ensure_mutation_receipt()?;
        let mut write = ResourceRollupWrite::default();
        for (process, rollup, text) in accepted {
            let inserted = tx.execute_rows(
                "INSERT INTO resource_rollups (process, span_secs, start_secs, data) \
                 VALUES (?1, ?2, ?3, ?4) ON CONFLICT(process, span_secs, start_secs) DO NOTHING",
                params![process.as_str(), MINUTE_SECS, rollup.start_secs, text],
                1,
            )?;
            match process {
                ResourceProcess::Bot => write.bot_minute_starts.push(rollup.start_secs),
                ResourceProcess::Api => write.api_minute_starts.push(rollup.start_secs),
            }
            if inserted == 0 {
                continue;
            }
            write.minute_rows_inserted += 1;
            let hour_start = rollup.start_secs - rollup.start_secs.rem_euclid(HOUR_SECS);
            let existing: Option<String> = tx
                .query_row(
                    "SELECT data FROM resource_rollups \
                     WHERE process=?1 AND span_secs=?2 AND start_secs=?3",
                    params![process.as_str(), HOUR_SECS, hour_start],
                    |row| Ok(row.get(0)?),
                )
                .optional()?;
            let mut hour = existing
                .and_then(|text| serde_json::from_str::<Rollup>(&text).ok())
                .filter(|hour| hour.start_secs == hour_start && hour.span_secs == HOUR_SECS)
                .unwrap_or_else(|| Rollup::empty(hour_start, HOUR_SECS));
            hour.absorb(&rollup);
            tx.execute_rows(
                "INSERT INTO resource_rollups (process, span_secs, start_secs, data) \
                 VALUES (?1, ?2, ?3, ?4) ON CONFLICT(process, span_secs, start_secs) \
                 DO UPDATE SET data=excluded.data",
                params![process.as_str(), HOUR_SECS, hour_start, rollup_json(&hour)?],
                1,
            )?;
        }
        if let Some((process, updated_at_ms, text)) = status {
            tx.execute_rows(
                "INSERT INTO resource_status (process, updated_at_ms, data) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(process) DO UPDATE \
                 SET updated_at_ms=excluded.updated_at_ms, data=excluded.data",
                params![process.as_str(), updated_at_ms, text],
                1,
            )?;
        }
        for (span, retention) in [
            (MINUTE_SECS, RESOURCE_MINUTE_RETENTION_SECS),
            (HOUR_SECS, RESOURCE_HOUR_RETENTION_SECS),
        ] {
            tx.execute_reclaim(PRUNE_SQL, params![span, now_secs - retention - span])?;
        }
        tx.commit()?;
        Ok(write)
    })
    .await
}

/// Rollups of `span_secs` starting in `[from_secs, to_secs)` for both
/// processes, plus every stored status, from one primary snapshot and one
/// statement. Owner-only data: callers must authorize the session first.
pub async fn resource_records(
    db: &Db,
    span_secs: i64,
    from_secs: i64,
    to_secs: i64,
) -> Result<ResourceRecords> {
    anyhow::ensure!(
        span_secs == MINUTE_SECS || span_secs == HOUR_SECS,
        "unsupported resource rollup span"
    );
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let history = |process| {
            format!(
                "(SELECT json_group_array(json(data)) FROM ({}))",
                history_sql(process)
            )
        };
        let (statuses, bot, api): (String, String, String) = tx.query_row(
            &format!(
                "SELECT (SELECT json_group_array(json_array(process, updated_at_ms, json(data))) \
                 FROM resource_status), {}, {}",
                history(ResourceProcess::Bot),
                history(ResourceProcess::Api)
            ),
            params![span_secs, from_secs, to_secs, RESOURCE_HISTORY_ROW_LIMIT],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        tx.finish_read()?;
        let parse = |text: &str| -> Vec<Rollup> {
            let mut rows: Vec<Rollup> = serde_json::from_str::<Vec<serde_json::Value>>(text)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|value| serde_json::from_value(value).ok())
                .filter(|rollup: &Rollup| rollup.span_secs == span_secs)
                .collect();
            rows.sort_by_key(|rollup| rollup.start_secs);
            rows
        };
        let statuses = serde_json::from_str::<Vec<(String, i64, serde_json::Value)>>(&statuses)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(process, at, value)| {
                Some((process, at, serde_json::from_value(value).ok()?))
            })
            .collect();
        Ok(ResourceRecords {
            bot: parse(&bot),
            api: parse(&api),
            statuses,
        })
    })
    .await
}

#[cfg(test)]
mod tests;
