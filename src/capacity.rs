//! Application-owned database budgets. These never alter provider or SQLite quotas.

use crate::db_compat::SqlExecutor;
use anyhow::{Result, ensure};

pub const DATABASE_LIMIT_BYTES: u64 = 1_000_000_000;
pub const GROWTH_STOP_BYTES: u64 = 900_000_000;
pub const SNAPSHOT_PAUSE_BYTES: u64 = 800_000_000;
pub const PRESSURE_BYTES: u64 = 700_000_000;
pub const SNAPSHOT_BUDGET_BYTES: u64 = 400_000_000;
pub const RECLAIM_STOP_BYTES: u64 = DATABASE_LIMIT_BYTES - 16_000_000;

#[derive(Clone, Copy, Debug)]
pub(crate) struct CapacitySample {
    pub allocated_bytes: u64,
    pub page_size: u64,
}

/// Allocated main-database pages, including reusable pages. Call within the
/// primary transaction: a replica or a cached UI sample cannot admit writes.
pub(crate) fn sample(conn: &impl SqlExecutor) -> Result<CapacitySample> {
    let pages: i64 = conn.query_row("PRAGMA page_count", (), |r| Ok(r.get(0)?))?;
    let page_size: i64 = conn.query_row("PRAGMA page_size", (), |r| Ok(r.get(0)?))?;
    ensure!(
        pages >= 0 && page_size > 0,
        "database capacity measurement unavailable"
    );
    let allocated_bytes = (pages as u64)
        .checked_mul(page_size as u64)
        .ok_or_else(|| anyhow::anyhow!("database capacity measurement overflow"))?;
    Ok(CapacitySample {
        allocated_bytes,
        page_size: page_size as u64,
    })
}

pub(crate) fn allocated_bytes(conn: &impl SqlExecutor) -> Result<u64> {
    Ok(sample(conn)?.allocated_bytes)
}

pub(crate) fn admit_to(allocated: u64, additional: u64, limit: u64) -> Result<()> {
    ensure!(
        allocated
            .checked_add(additional)
            .is_some_and(|n| n <= limit),
        "application database capacity budget exhausted: allocated={allocated} additional={additional} limit={limit}"
    );
    Ok(())
}
