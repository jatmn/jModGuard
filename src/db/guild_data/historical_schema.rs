//! Test-only replay of the SQL that shipped before the baseline was squashed.
use super::*;

pub(super) fn apply(tx: &crate::db_compat::Transaction, from: i64, to: i64) -> Result<()> {
    let steps: &[(i64, &str)] = &[
        (
            1,
            include_str!("../test_fixtures/v0.2.0/guild/0001_identity.sql"),
        ),
        (
            2,
            include_str!("../test_fixtures/v0.2.0/guild/0002_tenant_log_channels.sql"),
        ),
        (
            3,
            include_str!("../test_fixtures/v0.2.0/guild/0003_cases.sql"),
        ),
        (
            4,
            include_str!("../test_fixtures/v0.2.0/guild/0004_case_history.sql"),
        ),
        (
            5,
            include_str!("../test_fixtures/v0.2.0/guild/0005_case_sources.sql"),
        ),
        (
            6,
            include_str!("../test_fixtures/v0.2.0/guild/0006_case_evidence.sql"),
        ),
        (
            7,
            include_str!("../test_fixtures/v0.2.0/guild/0007_bunny_evidence.sql"),
        ),
        (
            8,
            include_str!("../test_fixtures/v0.2.0/guild/0008_erasure.sql"),
        ),
        (
            9,
            include_str!("../test_fixtures/v0.2.0/guild/0009_message_snapshots.sql"),
        ),
        (
            10,
            include_str!("../test_fixtures/v0.2.0/guild/0010_member_directory.sql"),
        ),
        (
            11,
            include_str!("../test_fixtures/v0.2.0/guild/0011_member_directory_status.sql"),
        ),
        (
            12,
            include_str!("../test_fixtures/v0.2.0/guild/0012_member_sync_progress.sql"),
        ),
        (
            13,
            include_str!("../test_fixtures/v0.2.0/guild/0013_member_unresolved_audits.sql"),
        ),
        (
            14,
            include_str!("../test_fixtures/v0.2.0/guild/0014_member_case_retain_revision.sql"),
        ),
        (
            15,
            include_str!("../test_fixtures/v0.2.0/guild/0015_local_write_fence.sql"),
        ),
        (
            17,
            include_str!("../test_fixtures/v0.2.0/guild/0016_member_pending_rejoin_notes.sql"),
        ),
        (18, MEMBER_GAP_AUTO_CLEAR_SCHEMA),
        (19, ACCESS_ROLES_SCHEMA),
        (20, SERVER_NAMES_SCHEMA),
        (21, CASE_LIST_SCHEMA),
        (22, MEMBER_LIST_SCHEMA),
        (23, MEMBER_NAME_SCHEMA),
        (24, ONBOARDING_TRAP_SCHEMA),
    ];
    for &(version, sql) in steps {
        if (from + 1..=to).contains(&version) {
            tx.execute_batch_bounded(sql, 32_000_000)?;
        }
    }
    Ok(())
}
