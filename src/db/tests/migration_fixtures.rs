//! Test-only replay of the original released core SQL.
use super::*;

pub(super) const HISTORICAL_CORE: &[(i64, &str)] = &[
    (
        1,
        include_str!("../test_fixtures/v0.2.0/core/0001_init.sql"),
    ),
    (
        2,
        include_str!("../test_fixtures/v0.2.0/core/0002_config_version.sql"),
    ),
    (
        3,
        include_str!("../test_fixtures/v0.2.0/core/0003_archive_global_log_channels.sql"),
    ),
    (
        4,
        include_str!("../test_fixtures/v0.2.0/core/0004_nickname_reconcile.sql"),
    ),
    (
        5,
        include_str!("../test_fixtures/v0.2.0/core/0005_mutation_receipts_nickname_jobs.sql"),
    ),
    (
        6,
        include_str!("../test_fixtures/v0.2.0/core/0006_bot_actions.sql"),
    ),
    (
        7,
        include_str!("../test_fixtures/v0.2.0/core/0007_guild_enrollments.sql"),
    ),
    (
        8,
        include_str!("../test_fixtures/v0.2.0/core/0008_action_enrollments.sql"),
    ),
    (
        9,
        include_str!("../test_fixtures/v0.2.0/core/0009_nickname_enrollments.sql"),
    ),
    (
        10,
        include_str!("../test_fixtures/v0.2.0/core/0010_guild_store_catalog.sql"),
    ),
    (
        11,
        include_str!("../test_fixtures/v0.2.0/core/0011_tenant_runtime.sql"),
    ),
    (
        12,
        include_str!("../test_fixtures/v0.2.0/core/0012_guild_moves.sql"),
    ),
    (
        13,
        include_str!("../test_fixtures/v0.2.0/core/0013_log_filters.sql"),
    ),
    (
        14,
        include_str!("../test_fixtures/v0.2.0/core/0014_ban_display_identity.sql"),
    ),
    (
        15,
        include_str!("../test_fixtures/v0.2.0/core/0015_component_updates.sql"),
    ),
    (
        16,
        include_str!("../test_fixtures/v0.2.0/core/0016_member_sync_pacing.sql"),
    ),
    (
        17,
        include_str!("../test_fixtures/v0.2.0/core/0017_hot_query_indexes.sql"),
    ),
    (
        18,
        include_str!("../test_fixtures/v0.2.0/core/0018_one_active_ban.sql"),
    ),
    (
        19,
        include_str!("../test_fixtures/v0.2.0/core/0019_known_bad_images.sql"),
    ),
    (
        20,
        include_str!("../test_fixtures/v0.2.0/core/0020_gateway_ban_deliveries.sql"),
    ),
    (
        21,
        include_str!("../test_fixtures/v0.2.0/core/0021_clear_builtin_command_access.sql"),
    ),
    (
        22,
        include_str!("../test_fixtures/v0.2.0/core/0022_clear_userinfo_command_rows.sql"),
    ),
    (
        23,
        include_str!("../test_fixtures/v0.2.0/core/0023_ban_expiry.sql"),
    ),
];

/// A private migration prefix or an isolated copy used for a recovery case.
pub(in crate::db) struct CoreMigrationFile(PathBuf);

impl CoreMigrationFile {
    pub(in crate::db) fn new(label: &str) -> Self {
        Self(tmp_db_path(label))
    }

    pub(in crate::db) fn path(&self) -> &Path {
        &self.0
    }

    pub(in crate::db) fn copy_closed(&self, label: &str) -> Self {
        let copy = Self::new(label);
        copy_closed_db(self.path(), copy.path());
        copy
    }
}

impl Drop for CoreMigrationFile {
    fn drop(&mut self) {
        cleanup_db(self.path());
    }
}

pub(super) fn apply_core_steps(conn: &Connection, steps: &[(i64, &str)]) {
    for &(version, sql) in steps {
        let tx = conn.transaction_primary_write().unwrap();
        tx.execute_batch_bounded(sql, 64 * 1024 * 1024).unwrap();
        tx.pragma_update(None, "user_version", version).unwrap();
        tx.commit().unwrap();
    }
}

pub(super) fn apply_historical_core(conn: &Connection, from: i64, to: i64) {
    assert!((0..=CORE_BASELINE_VERSION).contains(&from));
    assert!((from..=CORE_BASELINE_VERSION).contains(&to));
    apply_core_steps(conn, &HISTORICAL_CORE[from as usize..to as usize]);
}

// The stored DDL includes column types, defaults, constraints, generated columns,
// index expressions and predicates, views and trigger bodies. Autoindexes also
// appear as separate rows with NULL SQL. Physical page numbers are omitted.
type SchemaObject = (String, String, String, Option<String>);

pub(in crate::db) fn schema_dump(conn: &Connection) -> Vec<SchemaObject> {
    conn.prepare("SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY type, name")
        .unwrap()
        .query_map((), |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap()
}

pub(in crate::db) fn user_version(conn: &Connection) -> i64 {
    conn.query_row("PRAGMA user_version", (), |row| Ok(row.get(0)?))
        .unwrap()
}
