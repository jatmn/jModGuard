//! Read-only physical allocation and optional object attribution for owner diagnostics.
//! No database names, connection strings, row payloads or provider errors leave this module.
use super::guild_stores::{StorePolicy, StoreRegistration};
use super::{Db, OptionalExtension, SqlExecutor};
use anyhow::{Result, ensure};
use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BreakdownQuality {
    Measured,
    Estimated,
    Unavailable,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageCategories {
    pub snapshot_data_bytes: Option<u64>,
    pub other_data_bytes: Option<u64>,
    pub snapshot_index_bytes: Option<u64>,
    pub other_index_bytes: Option<u64>,
    pub reusable_bytes: u64,
    pub other_allocation_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DatabaseUsage {
    pub sampled_at: i64,
    pub allocated_bytes: u64,
    pub page_count: u64,
    pub page_size: u64,
    pub freelist_count: u64,
    pub reusable_bytes: u64,
    pub snapshot_count: Option<u64>,
    pub snapshot_logical_bytes: Option<u64>,
    pub snapshot_health: Option<SnapshotHealth>,
    pub breakdown_quality: BreakdownQuality,
    pub categories: UsageCategories,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotHealth {
    pub state: String,
    pub capacity_paused: bool,
    pub last_observed_at_ms: Option<i64>,
    pub last_cleanup_at_ms: Option<i64>,
    pub lost_count: u64,
    pub estimated_snapshot_bytes: u64,
    pub updated_at_ms: Option<i64>,
}

fn natural(value: i64) -> Result<u64> {
    ensure!(value >= 0, "negative database accounting value");
    Ok(value as u64)
}

fn pragma(tx: &impl SqlExecutor, name: &str) -> Result<u64> {
    // Only fixed, audited PRAGMA names are accepted; no caller-controlled SQL.
    ensure!(matches!(
        name,
        "page_count" | "page_size" | "freelist_count"
    ));
    natural(tx.query_row(&format!("PRAGMA {name}"), (), |row| Ok(row.get::<i64>(0)?))?)
}

fn verify_guild_identity(tx: &impl SqlExecutor, expected: &StoreRegistration) -> Result<()> {
    let actual = tx.query_row(
        "SELECT schema_family,store_id,dedicated_guild_id FROM store_identity WHERE singleton=1",
        (),
        |row| {
            Ok((
                row.get::<String>(0)?,
                row.get::<String>(1)?,
                row.get::<Option<i64>>(2)?,
            ))
        },
    )?;
    let dedicated = match expected.policy() {
        StorePolicy::Shared => None,
        StorePolicy::Dedicated { guild_id } => Some(guild_id),
    };
    ensure!(
        actual.0 == "jmodguard-guild-1"
            && actual.1 == expected.id().as_str()
            && actual.2 == dedicated,
        "guild store identity mismatch"
    );
    Ok(())
}

/// Primary read of physical allocation. `dbstat` is optional; its absence must
/// never turn an unknown category into a measured zero. Every category is
/// disjoint when measured: table pages, index pages, freelist, then remainder.
pub async fn measure_database_usage(
    db: &Db,
    details: bool,
    expected: Option<StoreRegistration>,
) -> Result<DatabaseUsage> {
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_read()?;
        if let Some(expected) = &expected {
            verify_guild_identity(&tx, expected)?;
        }
        let page_count = pragma(&tx, "page_count")?;
        let page_size = pragma(&tx, "page_size")?;
        let freelist_count = pragma(&tx, "freelist_count")?;
        ensure!(page_size > 0 && freelist_count <= page_count, "invalid database accounting");
        let allocated_bytes = page_count.checked_mul(page_size).ok_or_else(|| anyhow::anyhow!("database allocation overflow"))?;
        let reusable_bytes = freelist_count.checked_mul(page_size).ok_or_else(|| anyhow::anyhow!("freelist overflow"))?;
        let mut result = DatabaseUsage {
            sampled_at: chrono::Utc::now().timestamp(), allocated_bytes, page_count, page_size,
            freelist_count, reusable_bytes, snapshot_count: None, snapshot_logical_bytes: None,
            snapshot_health: None,
            breakdown_quality: BreakdownQuality::Unavailable,
            categories: UsageCategories { snapshot_data_bytes: None, other_data_bytes: None,
                snapshot_index_bytes: None, other_index_bytes: None,
                reusable_bytes, other_allocation_bytes: None },
        };
        if expected.is_some() {
            let health_exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='guild_snapshot_health')",
                (), |row| Ok(row.get::<i64>(0)? != 0))?;
            if health_exists {
                let health = tx.query_row(
                    "SELECT state,last_observed_at_ms,last_cleanup_at_ms,lost_count,updated_at_ms,estimated_snapshot_bytes,capacity_paused \
                     FROM guild_snapshot_health WHERE singleton=1",
                    (), |row| Ok((row.get::<String>(0)?, row.get::<Option<i64>>(1)?,
                        row.get::<Option<i64>>(2)?, row.get::<i64>(3)?, row.get::<Option<i64>>(4)?, row.get::<i64>(5)?, row.get::<i64>(6)?)),
                ).optional()?;
                if let Some((state, observed, cleanup, lost, updated, estimated_snapshot_bytes, capacity_paused)) = health {
                    result.snapshot_health = Some(SnapshotHealth {
                        // Only documented state codes cross this boundary.
                        state: match state.as_str() {
                            "active" | "paused" | "unavailable" | "disabled" | "unknown" => state,
                            _ => "unknown".into(),
                        },
                        capacity_paused: capacity_paused != 0,
                        last_observed_at_ms: observed,
                        last_cleanup_at_ms: cleanup,
                        lost_count: natural(lost)?, estimated_snapshot_bytes: natural(estimated_snapshot_bytes)?,
                        updated_at_ms: updated,
                    });
                }
            }
        }
        if details {
            let snapshot_exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='guild_message_snapshots')",
                (), |row| Ok(row.get::<i64>(0)? != 0))?;
            if snapshot_exists {
                let (count, logical): (i64, i64) = tx.query_row(
                    "SELECT COUNT(*),COALESCE(SUM(encoded_bytes),0) FROM guild_message_snapshots",
                    (), |row| Ok((row.get(0)?, row.get(1)?)))?;
                result.snapshot_count = Some(natural(count)?);
                result.snapshot_logical_bytes = Some(natural(logical)?);
            }
            // dbstat can be unavailable on a remote provider or a SQLite build.
            // One row per B-tree means the optional probe is bounded by schema size.
            let distribution = (|| -> Result<(u64, u64, u64, u64)> {
                let mut stmt = tx.prepare(
                    "SELECT d.name,COALESCE(m.type,''),COALESCE(m.tbl_name,''),SUM(d.pgsize) \
                     FROM dbstat AS d LEFT JOIN sqlite_master AS m ON m.name=d.name \
                     GROUP BY d.name,m.type,m.tbl_name",
                )?;
                let rows = stmt.query_map((), |row| Ok((
                    row.get::<String>(0)?, row.get::<String>(1)?,
                    row.get::<String>(2)?, row.get::<i64>(3)?)))?;
                let mut snapshot = 0u64;
                let mut other = 0u64;
                let mut snapshot_indexes = 0u64;
                let mut other_indexes = 0u64;
                let mut objects = 0u64;
                for row in rows {
                    objects += 1;
                    let (name, kind, table, bytes) = row?;
                    let bytes = natural(bytes)?;
                    if kind == "index" || name.starts_with("sqlite_autoindex_") {
                        let target = if table == "guild_message_snapshots" {
                            &mut snapshot_indexes
                        } else {
                            &mut other_indexes
                        };
                        *target = target.checked_add(bytes).ok_or_else(|| anyhow::anyhow!("index size overflow"))?;
                    } else if name == "guild_message_snapshots" {
                        snapshot = snapshot.checked_add(bytes).ok_or_else(|| anyhow::anyhow!("snapshot size overflow"))?;
                    } else {
                        other = other.checked_add(bytes).ok_or_else(|| anyhow::anyhow!("table size overflow"))?;
                    }
                }
                ensure!(page_count == 0 || objects > 0, "dbstat returned no objects");
                Ok((snapshot, other, snapshot_indexes, other_indexes))
            })();
            if let Ok((snapshot, other, snapshot_indexes, other_indexes)) = distribution {
                if let Some(remainder) = allocated_bytes
                    .checked_sub(reusable_bytes)
                    .and_then(|x| x.checked_sub(snapshot))
                    .and_then(|x| x.checked_sub(other))
                    .and_then(|x| x.checked_sub(snapshot_indexes))
                    .and_then(|x| x.checked_sub(other_indexes)) {
                    result.breakdown_quality = BreakdownQuality::Measured;
                    result.categories.snapshot_data_bytes = snapshot_exists.then_some(snapshot);
                    result.categories.other_data_bytes = Some(other);
                    result.categories.snapshot_index_bytes = snapshot_exists.then_some(snapshot_indexes);
                    result.categories.other_index_bytes = Some(other_indexes);
                    result.categories.other_allocation_bytes = Some(remainder);
                }
            } else if result.snapshot_logical_bytes.is_some() {
                result.breakdown_quality = BreakdownQuality::Estimated;
                // Logical encoded payload bytes are useful context, but cannot
                // partition physical allocation; leave physical categories unknown.
            }
        }
        tx.finish_read()?;
        Ok(result)
    }).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn physical_parts_reconcile_without_counting_indexes_twice() {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_usage_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = Db::open(&path, 2).unwrap();
        db.with_conn(|conn| {
            conn.execute_batch_bounded("CREATE TABLE guild_message_snapshots (id INTEGER PRIMARY KEY, encoded_bytes INTEGER NOT NULL, body TEXT); CREATE INDEX test_usage_snapshot_index ON guild_message_snapshots(body); INSERT INTO guild_message_snapshots(encoded_bytes,body) VALUES(20,'sample');", 64_000_000)?;
            Ok(())
        }).await.unwrap();
        let overview = measure_database_usage(&db, false, None).await.unwrap();
        let detail = measure_database_usage(&db, true, None).await.unwrap();
        assert_eq!(
            overview.allocated_bytes,
            overview.page_count * overview.page_size
        );
        assert_eq!(detail.snapshot_count, Some(1));
        assert_eq!(detail.snapshot_logical_bytes, Some(20));
        if detail.breakdown_quality == BreakdownQuality::Measured {
            let parts = &detail.categories;
            let total = parts.snapshot_data_bytes.unwrap_or(0)
                + parts.other_data_bytes.unwrap()
                + parts.snapshot_index_bytes.unwrap_or(0)
                + parts.other_index_bytes.unwrap()
                + parts.reusable_bytes
                + parts.other_allocation_bytes.unwrap();
            assert_eq!(total, detail.allocated_bytes);
            assert!(parts.snapshot_data_bytes.unwrap() >= detail.page_size);
            assert!(parts.snapshot_index_bytes.unwrap() >= detail.page_size);
        } else {
            assert_eq!(detail.breakdown_quality, BreakdownQuality::Estimated);
            assert!(detail.categories.other_data_bytes.is_none());
            assert!(detail.categories.snapshot_index_bytes.is_none());
        }
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
}
