//! Separate guild-store identity, tenant data and coordinated live erasure.
use super::guild_stores::{GuildPlacement, StoreId, StorePolicy, StoreRegistration};
use super::write_budget::BoundedWrites;
use super::*;

const FAMILY: &str = "jmodguard-guild-1";
const VERSION: i64 = include!("../../migrations/guild/current_version.rs");
const SCHEMA: &str = include_str!("../../migrations/guild/0001_identity.sql");

const TENANT_SCHEMA: &str = include_str!("../../migrations/guild/0002_tenant_log_channels.sql");
const CASE_SCHEMA: &str = include_str!("../../migrations/guild/0003_cases.sql");
const HISTORY_SCHEMA: &str = include_str!("../../migrations/guild/0004_case_history.sql");
const SOURCE_SCHEMA: &str = include_str!("../../migrations/guild/0005_case_sources.sql");
const EVIDENCE_SCHEMA: &str = include_str!("../../migrations/guild/0006_case_evidence.sql");
const BUNNY_SCHEMA: &str = include_str!("../../migrations/guild/0007_bunny_evidence.sql");
const ERASURE_SCHEMA: &str = include_str!("../../migrations/guild/0008_erasure.sql");
const SNAPSHOT_SCHEMA: &str = include_str!("../../migrations/guild/0009_message_snapshots.sql");
const MEMBER_SCHEMA: &str = include_str!("../../migrations/guild/0010_member_directory.sql");
const MEMBER_STATUS_SCHEMA: &str =
    include_str!("../../migrations/guild/0011_member_directory_status.sql");
const MEMBER_PROGRESS_SCHEMA: &str =
    include_str!("../../migrations/guild/0012_member_sync_progress.sql");
const MEMBER_UNRESOLVED_AUDITS_SCHEMA: &str =
    include_str!("../../migrations/guild/0013_member_unresolved_audits.sql");
const MEMBER_CASE_RETAIN_REVISION_SCHEMA: &str =
    include_str!("../../migrations/guild/0014_member_case_retain_revision.sql");
mod bunny_transfer;
#[cfg(unix)]
pub(crate) use bunny_transfer::*;
mod case_evidence;
pub use case_evidence::*;
mod case_sources;
pub use case_sources::*;
mod case_history;
pub use case_history::*;
mod cases;
pub use cases::*;
mod erasure;
mod snapshots;
pub use snapshots::*;
mod member_directory;
pub use member_directory::*;
mod tenant_logs;
pub use tenant_logs::{GuildDataScope, LogChannelSnapshot, TenantDataError};
mod tenant_settings;
pub use tenant_settings::TenantSettings;

/// Trusted installation connection settings. Never accept these from a guild request.
/// Deliberately has no Debug/Serialize implementation: token and URL may be sensitive.
#[derive(Clone)]
pub struct GuildStoreOptions {
    pub path: PathBuf,
    pub url: String,
    pub token: String,
    pub pool_size: u32,
}

/// Keeps the raw pool private so core schema operations cannot accidentally target it.
#[derive(Clone)]
pub struct GuildDataStore {
    db: Db,
    expected: StoreRegistration,
    /// When set, tenant mutations consult core `guild_write_fences` before writing.
    core: Option<Db>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreCheckError {
    #[error("unsupported guild-store schema")]
    Schema,
    #[error("guild-store identity or ownership does not match")]
    Identity,
    #[error("guild placement does not match this store")]
    Placement,
    #[error("guild has no binding in this store")]
    Unbound,
}

fn owner(registration: &StoreRegistration) -> Option<i64> {
    match registration.policy() {
        StorePolicy::Shared => None,
        StorePolicy::Dedicated { guild_id } => Some(guild_id),
    }
}

fn read_existing_registration(conn: &impl SqlExecutor) -> Result<Option<StoreRegistration>> {
    let version: i64 = conn.query_row("PRAGMA user_version", (), |r| Ok(r.get(0)?))?;
    let objects: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE name NOT GLOB 'sqlite_*'",
        (),
        |r| Ok(r.get(0)?),
    )?;
    if version == 0 && objects == 0 {
        return Ok(None);
    }
    let identity = conn
        .query_row(
            "SELECT schema_family,store_id,dedicated_guild_id FROM store_identity WHERE singleton=1",
            (),
            |r| Ok((r.get::<String>(0)?, r.get::<String>(1)?, r.get::<Option<i64>>(2)?)),
        )
        .optional();
    let Some((family, id, dedicated)) = identity.ok().flatten() else {
        return Err(StoreCheckError::Schema.into());
    };
    anyhow::ensure!(family == FAMILY, StoreCheckError::Schema);
    let policy = match dedicated {
        None => StorePolicy::Shared,
        Some(guild_id) if guild_id > 0 => StorePolicy::Dedicated { guild_id },
        Some(_) => return Err(StoreCheckError::Identity.into()),
    };
    Ok(Some(StoreRegistration::from_parts(
        StoreId::parse(&id)?,
        policy,
    )))
}

fn verify_identity(conn: &impl SqlExecutor, expected: &StoreRegistration) -> Result<()> {
    verify_identity_version(conn, expected, VERSION)
}

fn verify_identity_version(
    conn: &impl SqlExecutor,
    expected: &StoreRegistration,
    wanted: i64,
) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", (), |r| Ok(r.get(0)?))?;
    anyhow::ensure!(version == wanted, StoreCheckError::Schema);
    let binding_delete = if wanted >= 8 {
        "guild_binding_delete"
    } else {
        "guild_binding_no_delete"
    };
    let tables: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE (type='table' AND name IN ('store_identity','guild_bindings','mutation_receipts')) OR (type='trigger' AND name IN ('store_identity_no_replace','store_identity_no_update','store_identity_no_delete','guild_binding_owner','guild_binding_no_replace','guild_binding_no_update')) OR (type='trigger' AND name=?1)",
        params![binding_delete],
        |r| Ok(r.get(0)?),
    )?;
    anyhow::ensure!(tables == 10, StoreCheckError::Schema);
    if wanted >= 2 {
        let membership_delete = if wanted >= 8 {
            "guild_membership_delete"
        } else {
            "guild_membership_no_delete"
        };
        let objects: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE (type='table' AND name IN ('guild_memberships','guild_log_channels')) OR (type='trigger' AND name IN ('guild_membership_binding','guild_membership_no_replace','guild_membership_update','guild_log_channel_insert','guild_log_channel_update')) OR (type='trigger' AND name=?1)",
            params![membership_delete],
            |r| Ok(r.get(0)?),
        )?;
        anyhow::ensure!(objects == 8, StoreCheckError::Schema);
    }
    if wanted >= 3 {
        let objects: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE (type='table' AND name='guild_cases') OR (type='trigger' AND name IN ('guild_case_insert','guild_case_update','guild_case_delete')) OR (type='index' AND name='guild_cases_visible')", (), |r| Ok(r.get(0)?))?;
        anyhow::ensure!(objects == 5, StoreCheckError::Schema);
    }
    if wanted >= 4 {
        let objects: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE (type='table' AND name='guild_case_history') OR (type='trigger' AND name IN ('guild_case_history_insert','guild_case_history_update','guild_case_history_delete','guild_case_remove_history','guild_case_purge_history'))", (), |r| Ok(r.get(0)?))?;
        anyhow::ensure!(objects == 6, StoreCheckError::Schema);
    }
    if wanted >= 5 {
        let objects: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE (type='table' AND name='guild_case_sources') OR (type='trigger' AND name IN ('guild_case_source_insert','guild_case_source_update','guild_case_source_delete','guild_case_original_source','guild_case_purge_sources'))", (), |r| Ok(r.get(0)?))?;
        anyhow::ensure!(objects == 6, StoreCheckError::Schema);
    }
    if wanted >= 6 {
        let objects:i64=conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE (type='table' AND name='guild_case_evidence') OR (type='index' AND name IN ('guild_evidence_parent','guild_evidence_work','guild_evidence_cancel')) OR (type='trigger' AND name IN ('guild_evidence_insert','guild_evidence_update','guild_evidence_delete','guild_case_cancel_evidence','guild_case_purge_evidence'))",(),|r|Ok(r.get(0)?))?;
        anyhow::ensure!(objects == 9, StoreCheckError::Schema);
    }
    if wanted >= 7 {
        let objects: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE (type='table' AND name='guild_bunny_evidence') OR (type='index' AND name='guild_bunny_resource') OR (type='trigger' AND name IN ('guild_bunny_insert','guild_bunny_update','guild_bunny_delete','guild_bunny_settlement','guild_bunny_purge','guild_bunny_forget'))", (), |r| Ok(r.get(0)?))?;
        anyhow::ensure!(objects == 8, StoreCheckError::Schema);
    }
    if wanted >= 8 {
        let objects: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE (type='table' AND name='guild_tenant_settings') OR (type='trigger' AND name IN ('guild_binding_delete','guild_tenant_settings_insert','guild_tenant_settings_no_replace','guild_tenant_settings_update'))", (), |r| Ok(r.get(0)?))?;
        anyhow::ensure!(objects == 5, StoreCheckError::Schema);
    }
    if wanted >= 9 {
        let objects: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE (type='table' AND name IN ('guild_message_snapshots','guild_snapshot_health')) OR (type='index' AND name='guild_snapshot_expiry') OR (type='trigger' AND name IN ('guild_snapshot_insert','guild_snapshot_update','guild_snapshot_account_insert','guild_snapshot_account_update','guild_snapshot_account_delete','guild_membership_delete'))", (), |r| Ok(r.get(0)?))?;
        anyhow::ensure!(objects == 9, StoreCheckError::Schema);
    }
    if wanted >= 10 {
        let objects: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('guild_member_feature','guild_member_seed','guild_member_sync','guild_member_directory','guild_member_stays','guild_member_case_index','guild_member_pending_bans','guild_member_state_observations','guild_member_target_checks','guild_member_audit_recovery')", (), |r| Ok(r.get(0)?))?;
        anyhow::ensure!(objects == 10, StoreCheckError::Schema);
    }
    if wanted >= 13 {
        let objects: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE (type='table' AND name='guild_member_unresolved_audits') OR (type='index' AND name='guild_member_unresolved_audits_user') OR (type='trigger' AND name IN ('guild_member_unresolved_audit_insert','guild_member_unresolved_audit_update','guild_member_unresolved_audit_delete'))", (), |r| Ok(r.get(0)?))?;
        anyhow::ensure!(objects == 5, StoreCheckError::Schema);
    }
    let identity = conn.query_row("SELECT schema_family,store_id,dedicated_guild_id FROM store_identity WHERE singleton=1", (), |r| Ok((r.get::<String>(0)?,r.get::<String>(1)?,r.get::<Option<i64>>(2)?))).optional()?;
    let Some((family, id, dedicated)) = identity else {
        return Err(StoreCheckError::Schema.into());
    };
    anyhow::ensure!(family == FAMILY, StoreCheckError::Schema);
    anyhow::ensure!(
        id == expected.id().as_str() && dedicated == owner(expected),
        StoreCheckError::Identity
    );
    Ok(())
}

fn initialize_store(
    conn: &mut Connection,
    expected: &StoreRegistration,
    create: bool,
) -> Result<()> {
    let mut tx = conn.transaction_primary_read()?;
    let version: i64 = tx.query_row("PRAGMA user_version", (), |r| Ok(r.get(0)?))?;
    let objects: i64 = tx.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE name NOT GLOB 'sqlite_*'",
        (),
        |r| Ok(r.get(0)?),
    )?;
    if create && (1..VERSION).contains(&version) {
        // Verify the old family and expected identity before extending it. No core migration.
        verify_identity_version(&tx, expected, version)?;
        tx.ensure_mutation_receipt()?;
        if version < 2 {
            tx.execute_batch_bounded(TENANT_SCHEMA, 32_000_000)?;
        }
        if version < 3 {
            tx.execute_batch_bounded(CASE_SCHEMA, 32_000_000)?;
        }
        if version < 4 {
            tx.execute_batch_bounded(HISTORY_SCHEMA, 32_000_000)?;
        }
        if version < 5 {
            let source_bound = crate::capacity::allocated_bytes(&tx)?
                .saturating_mul(2)
                .saturating_add(32_000_000);
            tx.execute_batch_bounded(SOURCE_SCHEMA, source_bound)?;
        }
        if version < 6 {
            tx.execute_batch_bounded(EVIDENCE_SCHEMA, 32_000_000)?;
        }
        if version < 7 {
            tx.execute_batch_bounded(BUNNY_SCHEMA, 32_000_000)?;
        }
        if version < 8 {
            tx.execute_batch_bounded(ERASURE_SCHEMA, 32_000_000)?;
        }
        if version < 9 {
            tx.execute_batch_bounded(SNAPSHOT_SCHEMA, 32_000_000)?;
        }
        if version < 10 {
            tx.execute_batch_bounded(MEMBER_SCHEMA, 32_000_000)?;
        }
        if version < 11 {
            tx.execute_batch_bounded(MEMBER_STATUS_SCHEMA, 32_000_000)?;
        }
        if version < 12 {
            tx.execute_batch_bounded(MEMBER_PROGRESS_SCHEMA, 32_000_000)?;
        }
        if version < 13 {
            tx.execute_batch_bounded(MEMBER_UNRESOLVED_AUDITS_SCHEMA, 32_000_000)?;
        }
        if version < 14 {
            tx.execute_batch_bounded(MEMBER_CASE_RETAIN_REVISION_SCHEMA, 32_000_000)?;
        }
        tx.pragma_update(None, "user_version", VERSION)?;
        return tx.commit();
    }
    if !create || version != 0 || objects != 0 {
        verify_identity(&tx, expected)?;
        tx.finish_read()?;
        return Ok(());
    }
    tx.execute_batch_bounded(SCHEMA, 32_000_000)?;
    tx.execute_batch_bounded(TENANT_SCHEMA, 32_000_000)?;
    tx.execute_batch_bounded(CASE_SCHEMA, 32_000_000)?;
    tx.execute_batch_bounded(HISTORY_SCHEMA, 32_000_000)?;
    tx.execute_batch_bounded(SOURCE_SCHEMA, 32_000_000)?;
    tx.execute_batch_bounded(EVIDENCE_SCHEMA, 32_000_000)?;
    tx.execute_batch_bounded(BUNNY_SCHEMA, 32_000_000)?;
    tx.execute_batch_bounded(ERASURE_SCHEMA, 32_000_000)?;
    tx.execute_batch_bounded(SNAPSHOT_SCHEMA, 32_000_000)?;
    tx.execute_batch_bounded(MEMBER_SCHEMA, 32_000_000)?;
    tx.execute_batch_bounded(MEMBER_STATUS_SCHEMA, 32_000_000)?;
    tx.execute_batch_bounded(MEMBER_PROGRESS_SCHEMA, 32_000_000)?;
    tx.execute_batch_bounded(MEMBER_UNRESOLVED_AUDITS_SCHEMA, 32_000_000)?;
    tx.execute_batch_bounded(MEMBER_CASE_RETAIN_REVISION_SCHEMA, 32_000_000)?;
    tx.ensure_mutation_receipt()?;
    tx.execute_rows(
        "INSERT INTO store_identity(singleton,schema_family,store_id,dedicated_guild_id) VALUES(1,?1,?2,?3)",
        params![FAMILY, expected.id().as_str(), owner(expected)],
        1,
    )?;
    tx.pragma_update(None, "user_version", VERSION)?;
    // The receipt table did not exist at transaction start. Verify the complete
    // identity/schema marker on primary if initialization's COMMIT response is lost.
    match tx.commit() {
        Ok(()) => Ok(()),
        Err(error) if is_database_commit_indeterminate(&error) => {
            let check = conn.transaction_primary_read()?;
            verify_identity(&check, expected)?;
            check.finish_read()?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

impl GuildDataStore {
    pub fn capacity_runtime_status(&self) -> crate::db::CapacityRuntimeStatus {
        self.db.capacity_runtime_status()
    }

    /// Whole-store usage, measured on the store primary with identity verification.
    pub async fn measure_usage(&self, details: bool) -> Result<crate::db::usage::DatabaseUsage> {
        crate::db::usage::measure_database_usage(&self.db, details, Some(self.expected.clone()))
            .await
    }

    /// Initialize only an empty store, or reopen this exact recognized identity.
    /// Synchronous like Db::open_with; callers must use a startup/blocking worker.
    pub fn initialize(options: GuildStoreOptions, expected: StoreRegistration) -> Result<Self> {
        Self::connect(options, expected, true)
    }
    /// Verify an existing store without creating its schema or bindings.
    pub fn open(options: GuildStoreOptions, expected: StoreRegistration) -> Result<Self> {
        Self::connect(options, expected, false)
    }

    /// Read a stamped installation identity without creating or upgrading schema.
    /// Missing local files and empty databases return `None`.
    pub fn existing_registration(options: GuildStoreOptions) -> Result<Option<StoreRegistration>> {
        if options.url.trim().is_empty() && !options.path.exists() {
            return Ok(None);
        }
        let mut found = None;
        let opts = DbOpenOptions {
            path: options.path,
            url: options.url,
            token: options.token,
            pool_size: options.pool_size,
            migrate: false,
            default_prefix: String::new(),
            retention_days: 0,
        };
        let _db = Db::open_with_bootstrap(
            &opts,
            |conn| {
                found = read_existing_registration(conn)?;
                Ok(())
            },
            |_| Ok(()),
        )?;
        Ok(found)
    }
    fn connect(
        options: GuildStoreOptions,
        expected: StoreRegistration,
        create: bool,
    ) -> Result<Self> {
        let opts = DbOpenOptions {
            path: options.path,
            url: options.url,
            token: options.token,
            pool_size: options.pool_size,
            // Not consumed by this schema's bootstrap. No core defaults are seeded.
            migrate: false,
            default_prefix: String::new(),
            retention_days: 0,
        };
        let db = Db::open_with_bootstrap(
            &opts,
            |conn| initialize_store(conn, &expected, create),
            |_| Ok(()),
        )?;
        Ok(Self {
            db,
            expected,
            core: None,
        })
    }

    /// Bot/API tenant runtimes attach the core database so move fences fail closed on
    /// guild-store mutations, not only at `TenantRuntime::live` handle acquisition.
    pub fn with_core_write_fence(self, core: Db) -> Self {
        Self {
            core: Some(core),
            ..self
        }
    }

    pub(super) async fn refuse_fenced_guild_write(&self, guild: i64) -> Result<()> {
        if let Some(core) = &self.core
            && super::guild_stores::guild_is_write_fenced(core, guild).await?
        {
            return Err(super::guild_stores::GuildWriteFenced.into());
        }
        Ok(())
    }

    fn check_target(&self, placement: &GuildPlacement) -> Result<()> {
        anyhow::ensure!(
            placement.guild_id() > 0
                && placement.store_id() == self.expected.id()
                && owner(&self.expected).is_none_or(|guild| guild == placement.guild_id()),
            StoreCheckError::Placement
        );
        Ok(())
    }

    /// Install only the initial catalog revision. An existing different revision
    /// cannot be overwritten; this is not a move or stale-writer fencing protocol.
    pub async fn bind_initial_placement(&self, placement: GuildPlacement) -> Result<()> {
        self.check_target(&placement)?;
        self.refuse_fenced_guild_write(placement.guild_id()).await?;
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let mut tx = conn.transaction_primary_read()?;
                verify_identity(&tx, &expected)?;
                let current = binding_revision(&tx, placement.guild_id())?;
                if let Some(current) = current {
                    anyhow::ensure!(current == placement.revision(), StoreCheckError::Placement);
                    tx.finish_read()?;
                    return Ok(());
                }
                tx.ensure_mutation_receipt()?;
                tx.execute_rows(
                    "INSERT INTO guild_bindings(guild_id,revision) VALUES(?1,?2)",
                    params![placement.guild_id(), placement.revision()],
                    1,
                )?;
                tx.commit()
            })
            .await
    }

    /// Check identity and binding in one primary snapshot. Success only describes
    /// that snapshot: callers cannot use it as authorization for a later data write.
    pub async fn verify_placement(&self, placement: GuildPlacement) -> Result<()> {
        self.check_target(&placement)?;
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let tx = conn.transaction_primary_read()?;
                verify_bound_placement(&tx, &expected, &placement)?;
                tx.finish_read()
            })
            .await
    }
}

fn verify_bound_placement(
    conn: &impl SqlExecutor,
    expected: &StoreRegistration,
    placement: &GuildPlacement,
) -> Result<()> {
    verify_identity(conn, expected)?;
    let current = binding_revision(conn, placement.guild_id())?.ok_or(StoreCheckError::Unbound)?;
    anyhow::ensure!(current == placement.revision(), StoreCheckError::Placement);
    Ok(())
}

fn binding_revision(conn: &impl SqlExecutor, guild: i64) -> Result<Option<String>> {
    conn.query_row(
        "SELECT revision FROM guild_bindings WHERE guild_id=?1",
        params![guild],
        |r| Ok(r.get(0)?),
    )
    .optional()
}

#[cfg(test)]
mod tests;
