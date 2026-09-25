//! Read-only physical allocation and optional object attribution for owner diagnostics.
//! No database names, connection strings, row payloads or provider errors leave this module.
use super::guild_stores::{StorePolicy, StoreRegistration};
use super::{Db, OptionalExtension, SqlExecutor, params};
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
    pub moderation_evidence_data_bytes: Option<u64>,
    pub configuration_data_bytes: Option<u64>,
    pub operational_data_bytes: Option<u64>,
    pub index_bytes: Option<u64>,
    pub reusable_bytes: u64,
    pub system_unattributed_bytes: Option<u64>,
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
    pub oldest_snapshot_at_ms: Option<i64>,
    pub breakdown_quality: BreakdownQuality,
    pub breakdown_error_code: Option<&'static str>,
    pub category_sampled_at: Option<i64>,
    pub categories: UsageCategories,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotHealth {
    pub epoch: u64,
    pub state: String,
    pub reason: Option<String>,
    pub capacity_paused: bool,
    pub last_observed_at_ms: Option<i64>,
    pub last_cleanup_at_ms: Option<i64>,
    pub lost_count: u64,
    pub expired_count: u64,
    pub evicted_count: u64,
    pub counters_reset_at_ms: Option<i64>,
    pub estimated_snapshot_bytes: u64,
    pub updated_at_ms: Option<i64>,
}

fn data_group(name: &str) -> usize {
    if name == "guild_message_snapshots" {
        0
    } else if matches!(
        name,
        "guild_cases"
            | "guild_case_evidence"
            | "guild_case_history"
            | "guild_case_sources"
            | "guild_bunny_evidence"
            | "events"
            | "warns"
            | "bans"
    ) {
        1
    } else if matches!(
        name,
        "guild_tenant_settings"
            | "guild_log_channels"
            | "guild_memberships"
            | "guild_bindings"
            | "store_identity"
            | "guild_settings"
            | "commands"
            | "rules"
            | "templates"
            | "log_channels"
            | "badwords"
            | "log_filters"
            | "meta"
            | "guild_enrollments"
            | "guild_store_catalog"
            | "guild_store_placements"
    ) || name.starts_with("config_")
    {
        2
    } else {
        3
    }
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
            snapshot_health: None, oldest_snapshot_at_ms: None,
            breakdown_quality: BreakdownQuality::Unavailable,
            breakdown_error_code: None,
            category_sampled_at: None,
            categories: UsageCategories { snapshot_data_bytes: None, moderation_evidence_data_bytes: None,
                configuration_data_bytes: None, operational_data_bytes: None,
                index_bytes: None, reusable_bytes, system_unattributed_bytes: None },
        };
        if expected.is_some() {
            let health_exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='guild_snapshot_health')",
                (), |row| Ok(row.get::<i64>(0)? != 0))?;
            if health_exists {
                let health = tx.query_row(
                    "SELECT epoch,state,reason,last_observed_at_ms,last_cleanup_at_ms,lost_count,updated_at_ms,estimated_snapshot_bytes,capacity_paused,snapshot_count,snapshot_logical_bytes,expired_count,evicted_count,counters_reset_at_ms \
                     FROM guild_snapshot_health WHERE singleton=1",
                    (), |row| Ok((row.get::<i64>(0)?, row.get::<String>(1)?, row.get::<Option<String>>(2)?,
                        row.get::<Option<i64>>(3)?, row.get::<Option<i64>>(4)?, row.get::<i64>(5)?, row.get::<Option<i64>>(6)?, row.get::<i64>(7)?, row.get::<i64>(8)?,
                        row.get::<i64>(9)?, row.get::<i64>(10)?, row.get::<i64>(11)?, row.get::<i64>(12)?, row.get::<Option<i64>>(13)?)),
                ).optional()?;
                if let Some((epoch, state, reason, observed, cleanup, lost, updated, estimated_snapshot_bytes, capacity_paused, count, logical, expired, evicted, reset)) = health {
                    result.snapshot_count = Some(natural(count)?);
                    result.snapshot_logical_bytes = Some(natural(logical)?);
                    result.snapshot_health = Some(SnapshotHealth {
                        epoch: natural(epoch)?,
                        // Only documented state codes cross this boundary.
                        state: match state.as_str() {
                            "active" | "paused" | "unavailable" | "disabled" | "unknown" => state,
                            _ => "unknown".into(),
                        },
                        reason: reason.filter(|s| matches!(s.as_str(), "capacity" | "content_access" | "schema" | "scope" | "queue_overflow" | "database" | "gateway_gap" | "disabled" | "cleanup" | "cleanup_failed" | "measurement_failed" | "startup_deferred")),
                        capacity_paused: capacity_paused != 0,
                        last_observed_at_ms: observed,
                        last_cleanup_at_ms: cleanup,
                        lost_count: natural(lost)?, expired_count: natural(expired)?, evicted_count: natural(evicted)?, counters_reset_at_ms: reset,
                        estimated_snapshot_bytes: natural(estimated_snapshot_bytes)?,
                        updated_at_ms: updated,
                    });
                }
                // The expiry index gives the oldest retained creation time without a body scan.
                result.oldest_snapshot_at_ms = tx.query_row(
                    "SELECT expires_at_ms - 86400000 FROM guild_message_snapshots INDEXED BY guild_snapshot_expiry WHERE expires_at_ms>?1 ORDER BY expires_at_ms LIMIT 1",
                    params![chrono::Utc::now().timestamp_millis()], |row| Ok(row.get::<i64>(0)?)).optional()?;
            }
        }
        if details {
            // dbstat can be unavailable on a remote provider or a SQLite build.
            // Aggregate output is bounded by schema size, but the physical scan
            // visits database pages; the API caches this optional detail for five minutes.
            let distribution = (|| -> Result<([u64; 4], u64)> {
                let mut stmt = tx.prepare(
                    "SELECT d.name,COALESCE(m.type,''),COALESCE(m.tbl_name,''),SUM(d.pgsize) \
                     FROM dbstat AS d LEFT JOIN sqlite_master AS m ON m.name=d.name \
                     GROUP BY d.name,m.type,m.tbl_name",
                )?;
                let rows = stmt.query_map((), |row| Ok((
                    row.get::<String>(0)?, row.get::<String>(1)?,
                    row.get::<String>(2)?, row.get::<i64>(3)?)))?;
                let mut data = [0u64; 4];
                let mut indexes = 0u64;
                let mut objects = 0u64;
                for row in rows {
                    objects += 1;
                    let (name, kind, table, bytes) = row?;
                    let bytes = natural(bytes)?;
                    if kind == "index" || name.starts_with("sqlite_autoindex_") {
                        let _ = table;
                        indexes = indexes.checked_add(bytes).ok_or_else(|| anyhow::anyhow!("index size overflow"))?;
                    } else {
                        let group = data_group(&name);
                        data[group] = data[group].checked_add(bytes).ok_or_else(|| anyhow::anyhow!("table size overflow"))?;
                    }
                }
                ensure!(page_count == 0 || objects > 0, "dbstat returned no objects");
                Ok((data, indexes))
            })();
            if distribution.is_err() {
                result.breakdown_error_code = Some("dbstat_probe_failed");
            }
            if let Ok((data, indexes)) = distribution {
                if let Some(remainder) = allocated_bytes
                    .checked_sub(reusable_bytes)
                    .and_then(|x| data.iter().try_fold(x, |remaining, part| remaining.checked_sub(*part)))
                    .and_then(|x| x.checked_sub(indexes)) {
                    result.breakdown_quality = BreakdownQuality::Measured;
                    result.category_sampled_at = Some(result.sampled_at);
                    result.categories.snapshot_data_bytes = Some(data[0]);
                    result.categories.moderation_evidence_data_bytes = Some(data[1]);
                    result.categories.configuration_data_bytes = Some(data[2]);
                    result.categories.operational_data_bytes = Some(data[3]);
                    result.categories.index_bytes = Some(indexes);
                    result.categories.system_unattributed_bytes = Some(remainder);
                } else {
                    result.breakdown_error_code = Some("inconsistent_allocation_sample");
                }
            } else if result.snapshot_logical_bytes.is_some() {
                result.breakdown_quality = BreakdownQuality::Estimated;
                result.category_sampled_at = Some(result.sampled_at);
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

    #[test]
    fn table_groups_cover_both_database_families_without_overlap() {
        assert_eq!(data_group("guild_message_snapshots"), 0);
        for name in [
            "guild_cases",
            "guild_case_evidence",
            "guild_bunny_evidence",
            "warns",
            "bans",
            "events",
        ] {
            assert_eq!(data_group(name), 1);
        }
        for name in [
            "guild_tenant_settings",
            "guild_log_channels",
            "guild_bindings",
            "guild_settings",
            "rules",
            "commands",
            "templates",
            "log_filters",
            "badwords",
            "guild_store_catalog",
        ] {
            assert_eq!(data_group(name), 2);
        }
        for name in [
            "guild_snapshot_health",
            "bot_actions",
            "mutation_receipts",
            "sqlite_schema",
        ] {
            assert_eq!(data_group(name), 3);
        }
    }

    #[tokio::test]
    async fn physical_parts_reconcile_without_counting_indexes_twice() {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_usage_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = Db::open(&path, 2).unwrap();
        let core_only = measure_database_usage(&db, true, None).await.unwrap();
        if core_only.breakdown_quality == BreakdownQuality::Measured {
            assert_eq!(core_only.categories.snapshot_data_bytes, Some(0));
        }
        db.with_conn(|conn| {
            conn.execute_batch_bounded("CREATE TABLE guild_message_snapshots (id INTEGER PRIMARY KEY, encoded_bytes INTEGER NOT NULL, body TEXT); CREATE INDEX test_usage_snapshot_index ON guild_message_snapshots(body); INSERT INTO guild_message_snapshots(encoded_bytes,body) VALUES(20,'sample'); CREATE TABLE guild_cases(id INTEGER PRIMARY KEY, body TEXT); INSERT INTO guild_cases(body) VALUES('case');", 64_000_000)?;
            Ok(())
        }).await.unwrap();
        let overview = measure_database_usage(&db, false, None).await.unwrap();
        let detail = measure_database_usage(&db, true, None).await.unwrap();
        assert_eq!(
            overview.allocated_bytes,
            overview.page_count * overview.page_size
        );
        // Standalone fixture has no bounded health singleton. A detail probe
        // must never fall back to a full COUNT/SUM over snapshot bodies.
        assert_eq!(detail.snapshot_count, None);
        assert_eq!(detail.snapshot_logical_bytes, None);
        if detail.breakdown_quality == BreakdownQuality::Measured {
            let parts = &detail.categories;
            let total = parts.snapshot_data_bytes.unwrap_or(0)
                + parts.moderation_evidence_data_bytes.unwrap()
                + parts.configuration_data_bytes.unwrap()
                + parts.operational_data_bytes.unwrap()
                + parts.index_bytes.unwrap()
                + parts.reusable_bytes
                + parts.system_unattributed_bytes.unwrap();
            assert_eq!(total, detail.allocated_bytes);
            assert!(parts.snapshot_data_bytes.unwrap() >= detail.page_size);
            assert!(parts.moderation_evidence_data_bytes.unwrap() >= detail.page_size);
            assert!(parts.configuration_data_bytes.unwrap() >= detail.page_size);
            assert!(parts.index_bytes.unwrap() >= detail.page_size);
        } else {
            assert_eq!(detail.breakdown_quality, BreakdownQuality::Unavailable);
            assert!(detail.categories.operational_data_bytes.is_none());
            assert!(detail.categories.index_bytes.is_none());
        }
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
}
