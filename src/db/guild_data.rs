//! Separate guild-store identity, tenant data and coordinated live erasure.
use super::guild_stores::{GuildPlacement, StoreId, StorePolicy, StoreRegistration};
use super::write_budget::BoundedWrites;
use super::*;

const FAMILY: &str = "jmodguard-guild-1";
pub const SCHEMA_VERSION: i64 = include!("../../migrations/guild/current_version.rs");
const VERSION: i64 = SCHEMA_VERSION;
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
const LOCAL_WRITE_FENCE_SCHEMA: &str =
    include_str!("../../migrations/guild/0015_local_write_fence.sql");
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
    /// Readers never take the writer lock. The API sets this when its core pool is read-only.
    pub read_only: bool,
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

fn verify_identity_row(
    conn: &impl SqlExecutor,
    expected_store_id: &str,
    expected_dedicated: Option<i64>,
) -> Result<()> {
    let identity = conn
        .query_row(
            "SELECT schema_family,store_id,dedicated_guild_id FROM store_identity WHERE singleton=1",
            (),
            |r| Ok((r.get::<String>(0)?, r.get::<String>(1)?, r.get::<Option<i64>>(2)?)),
        )
        .optional()?;
    let Some((family, id, dedicated)) = identity else {
        return Err(StoreCheckError::Schema.into());
    };
    anyhow::ensure!(family == FAMILY, StoreCheckError::Schema);
    anyhow::ensure!(
        id == expected_store_id && dedicated == expected_dedicated,
        StoreCheckError::Identity
    );
    Ok(())
}

fn clear_identity_memo_for_versions(
    slot: &std::sync::Arc<std::sync::Mutex<Option<crate::db_compat::SchemaVerification>>>,
    probe_schema_version: i64,
    probe_user_version: i64,
) {
    let mut guard = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if matches!(
        guard.as_ref(),
        Some(memo) if memo.schema_version == probe_schema_version
            && memo.user_version == probe_user_version
    ) {
        *guard = None;
    }
}

fn verify_identity_version(
    conn: &impl SqlExecutor,
    expected: &StoreRegistration,
    wanted: i64,
) -> Result<()> {
    const PROBE_SQL: &str = "SELECT (SELECT schema_version FROM pragma_schema_version), (SELECT user_version FROM pragma_user_version)";
    const CATALOG_SQL: &str = "SELECT (SELECT user_version FROM pragma_user_version), (SELECT schema_version FROM pragma_schema_version), json_group_array(json_array(type,name)) FROM sqlite_master";

    let (probe_schema_version, probe_user_version): (i64, i64) =
        conn.query_row(PROBE_SQL, (), |r| Ok((r.get(0)?, r.get(1)?)))?;
    let slot = conn.schema_verification_slot();
    let cached = slot
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let expected_store_id = expected.id().as_str();
    let expected_dedicated = owner(expected);
    if let Some(memo) = cached
        && memo.schema_version == probe_schema_version
        && memo.user_version == probe_user_version
        && memo.user_version == wanted
        && memo.store_id == expected_store_id
        && memo.dedicated_guild_id == expected_dedicated
    {
        let hit = verify_identity_row(conn, expected_store_id, expected_dedicated);
        if hit.is_err() {
            clear_identity_memo_for_versions(&slot, probe_schema_version, probe_user_version);
        }
        return hit;
    }

    let full = (|| -> Result<()> {
        let (version, schema_version, objects): (i64, i64, String) =
            conn.query_row(CATALOG_SQL, (), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        anyhow::ensure!(version == wanted, StoreCheckError::Schema);
        let objects: std::collections::HashSet<(String, String)> = serde_json::from_str(&objects)?;
        let has = |kind: &str, name: &str| objects.contains(&(kind.to_owned(), name.to_owned()));
        let required: &[(i64, &str, &str)] = &[
            (
                1,
                "table",
                "store_identity guild_bindings mutation_receipts",
            ),
            (
                1,
                "trigger",
                "store_identity_no_replace store_identity_no_update store_identity_no_delete guild_binding_owner guild_binding_no_replace guild_binding_no_update",
            ),
            (2, "table", "guild_memberships guild_log_channels"),
            (
                2,
                "trigger",
                "guild_membership_binding guild_membership_no_replace guild_membership_update guild_log_channel_insert guild_log_channel_update",
            ),
            (3, "table", "guild_cases"),
            (
                3,
                "trigger",
                "guild_case_insert guild_case_update guild_case_delete",
            ),
            (3, "index", "guild_cases_visible"),
            (4, "table", "guild_case_history"),
            (
                4,
                "trigger",
                "guild_case_history_insert guild_case_history_update guild_case_history_delete guild_case_remove_history guild_case_purge_history",
            ),
            (5, "table", "guild_case_sources"),
            (
                5,
                "trigger",
                "guild_case_source_insert guild_case_source_update guild_case_source_delete guild_case_original_source guild_case_purge_sources",
            ),
            (6, "table", "guild_case_evidence"),
            (
                6,
                "index",
                "guild_evidence_parent guild_evidence_work guild_evidence_cancel",
            ),
            (
                6,
                "trigger",
                "guild_evidence_insert guild_evidence_update guild_evidence_delete guild_case_cancel_evidence guild_case_purge_evidence",
            ),
            (7, "table", "guild_bunny_evidence"),
            (7, "index", "guild_bunny_resource"),
            (
                7,
                "trigger",
                "guild_bunny_insert guild_bunny_update guild_bunny_delete guild_bunny_settlement guild_bunny_purge guild_bunny_forget",
            ),
            (8, "table", "guild_tenant_settings"),
            (
                8,
                "trigger",
                "guild_binding_delete guild_tenant_settings_insert guild_tenant_settings_no_replace guild_tenant_settings_update",
            ),
            (9, "table", "guild_message_snapshots guild_snapshot_health"),
            (9, "index", "guild_snapshot_expiry"),
            (
                9,
                "trigger",
                "guild_snapshot_insert guild_snapshot_update guild_snapshot_account_insert guild_snapshot_account_update guild_snapshot_account_delete guild_membership_delete",
            ),
            (
                10,
                "table",
                "guild_member_feature guild_member_seed guild_member_sync guild_member_directory guild_member_stays guild_member_case_index guild_member_pending_bans guild_member_state_observations guild_member_target_checks guild_member_audit_recovery",
            ),
            (13, "table", "guild_member_unresolved_audits"),
            (13, "index", "guild_member_unresolved_audits_user"),
            (
                13,
                "trigger",
                "guild_member_unresolved_audit_insert guild_member_unresolved_audit_update guild_member_unresolved_audit_delete",
            ),
            (15, "table", "guild_local_write_fence guild_writer_epoch"),
            (15, "trigger", "guild_case_settle_retain"),
        ];
        for (version, kind, names) in required {
            if wanted >= *version {
                anyhow::ensure!(
                    names.split_whitespace().all(|name| has(kind, name)),
                    StoreCheckError::Schema
                );
            }
        }
        anyhow::ensure!(
            has(
                "trigger",
                if wanted >= 8 {
                    "guild_binding_delete"
                } else {
                    "guild_binding_no_delete"
                }
            ),
            StoreCheckError::Schema
        );
        if wanted >= 2 {
            anyhow::ensure!(
                has(
                    "trigger",
                    if wanted >= 8 {
                        "guild_membership_delete"
                    } else {
                        "guild_membership_no_delete"
                    }
                ),
                StoreCheckError::Schema
            );
        }
        verify_identity_row(conn, expected_store_id, expected_dedicated)?;
        *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(crate::db_compat::SchemaVerification {
                schema_version,
                user_version: wanted,
                store_id: expected_store_id.to_owned(),
                dedicated_guild_id: expected_dedicated,
            });
        Ok(())
    })();
    if full.is_err() {
        clear_identity_memo_for_versions(&slot, probe_schema_version, probe_user_version);
    }
    full
}

fn initialize_store(
    conn: &mut Connection,
    expected: &StoreRegistration,
    create: bool,
) -> Result<()> {
    {
        let tx = conn.transaction_primary_snapshot()?;
        let version: i64 = tx.query_row("PRAGMA user_version", (), |r| Ok(r.get(0)?))?;
        let objects: i64 = tx.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name NOT GLOB 'sqlite_*'",
            (),
            |r| Ok(r.get(0)?),
        )?;
        let needs_write =
            create && ((1..VERSION).contains(&version) || (version == 0 && objects == 0));
        if !needs_write {
            if !create && (1..VERSION).contains(&version) {
                verify_identity_version(&tx, expected, version)?;
                tx.finish_read()?;
                return Err(SchemaNotReady(format!(
                    "guild store schema {version} is older than {VERSION}; waiting for the bot to migrate it"
                ))
                .into());
            }
            verify_identity(&tx, expected)?;
            tx.finish_read()?;
            return Ok(());
        }
        tx.finish_read()?;
    }
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
        if version < 15 {
            tx.execute_batch_bounded(LOCAL_WRITE_FENCE_SCHEMA, 32_000_000)?;
        }
        // Version 16 adds the optional immutable ban_attempt_tag to case JSON.
        // No DDL or rewrite of legacy cases: their absent tags must remain absent.
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
    tx.execute_batch_bounded(LOCAL_WRITE_FENCE_SCHEMA, 32_000_000)?;
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

    /// Build a store with the real guild migrations through `version` and stop there.
    /// The writable initializer is what advances it later.
    #[cfg(any(test, feature = "test-support"))]
    pub fn create_historical_store(
        options: GuildStoreOptions,
        expected: StoreRegistration,
        version: i64,
    ) -> Result<()> {
        anyhow::ensure!(
            (1..VERSION).contains(&version),
            "historical guild schema is outside 1..{VERSION}"
        );
        let opts = DbOpenOptions {
            path: options.path,
            url: options.url,
            token: options.token,
            pool_size: options.pool_size,
            migrate: false,
            read_only: false,
            default_prefix: String::new(),
            retention_days: 0,
        };
        Db::open_with_bootstrap(
            &opts,
            |conn| {
                let mut tx = conn.transaction_immediate()?;
                let batches: &[(&str, i64)] = &[
                    (SCHEMA, 1),
                    (TENANT_SCHEMA, 2),
                    (CASE_SCHEMA, 3),
                    (HISTORY_SCHEMA, 4),
                    (SOURCE_SCHEMA, 5),
                    (EVIDENCE_SCHEMA, 6),
                    (BUNNY_SCHEMA, 7),
                    (ERASURE_SCHEMA, 8),
                    (SNAPSHOT_SCHEMA, 9),
                    (MEMBER_SCHEMA, 10),
                    (MEMBER_STATUS_SCHEMA, 11),
                    (MEMBER_PROGRESS_SCHEMA, 12),
                    (MEMBER_UNRESOLVED_AUDITS_SCHEMA, 13),
                    (MEMBER_CASE_RETAIN_REVISION_SCHEMA, 14),
                    (LOCAL_WRITE_FENCE_SCHEMA, 15),
                ];
                for (sql, at) in batches {
                    if *at <= version {
                        tx.execute_batch_bounded(sql, 32_000_000)?;
                    }
                }
                tx.ensure_mutation_receipt()?;
                tx.execute_rows(
                    "INSERT INTO store_identity(singleton,schema_family,store_id,dedicated_guild_id) VALUES(1,?1,?2,?3)",
                    params![FAMILY, expected.id().as_str(), owner(&expected)],
                    1,
                )?;
                tx.pragma_update(None, "user_version", version)?;
                tx.commit()?;
                Ok(())
            },
            |_| Ok(()),
        )?;
        Ok(())
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
            read_only: options.read_only,
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
            read_only: options.read_only,
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

    /// Bot/API tenant runtimes attach the core database so persisted write fences fail closed on
    /// guild-store mutations, not only at `TenantRuntime::live` handle acquisition.
    ///
    /// When the core pool has an armed writer, every guild mutation commit re-reads the core
    /// `active_bot_lease` row in fresh primary snapshots, releasing core before
    /// the guild commit. The guild transaction also requires `guild_writer_epoch` to equal the
    /// generation snapshotted for that commit. An empty owner slot does not query and does
    /// not change commits.
    pub fn with_core_write_fence(self, core: Db) -> Self {
        self.db.follow_writer_generation(&core);
        let probe_core = core.clone();
        self.db
            .install_commit_lease_probe(std::sync::Arc::new(move |commit| {
                Self::confirm_core_writer_lease(&probe_core, commit)
            }));
        Self {
            core: Some(core),
            ..self
        }
    }

    /// Raise this store's epoch to `generation` when the stored value is absent or lower.
    /// A higher stored value is left unchanged and reported as a lost lease.
    pub async fn raise_writer_epoch(&self, generation: i64) -> Result<()> {
        anyhow::ensure!(generation > 0, "active bot generation is corrupt");
        self.db
            .with_conn(move |conn| {
                let mut tx = conn.transaction_primary_read()?;
                let stored: Option<i64> = tx
                    .query_row(
                        "SELECT generation FROM guild_writer_epoch WHERE singleton=1",
                        (),
                        |row| Ok(row.get(0)?),
                    )
                    .optional()?;
                if stored == Some(generation) {
                    tx.finish_read()?;
                    return Ok(());
                }
                if stored.is_some_and(|value| value > generation) {
                    tx.finish_read()?;
                    return Err(crate::db_compat::ActiveBotLeaseRejected.into());
                }
                tx.ensure_mutation_receipt()?;
                tx.execute_rows(
                    "INSERT INTO guild_writer_epoch(singleton, generation) VALUES(1, ?1)
                     ON CONFLICT(singleton) DO UPDATE SET generation = excluded.generation
                     WHERE guild_writer_epoch.generation < excluded.generation",
                    params![generation],
                    1,
                )?;
                let raised: Option<i64> = tx
                    .query_row(
                        "SELECT generation FROM guild_writer_epoch WHERE singleton=1",
                        (),
                        |row| Ok(row.get(0)?),
                    )
                    .optional()?;
                if raised != Some(generation) {
                    tx.finish_read()?;
                    return Err(crate::db_compat::ActiveBotLeaseRejected.into());
                }
                tx.commit()?;
                Ok(())
            })
            .await
    }

    /// Re-read the core lease without a writer lock, then release core before guild `COMMIT`.
    /// An empty owner slot does not query. The guild epoch check inside the guild
    /// transaction is what rejects a stale holder. Renewal of the same owner does not
    /// take a new generation and does not wait on the guild commit.
    fn confirm_core_writer_lease(core: &Db, commit: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        let Some(expected) = core.leased_owner() else {
            return commit();
        };
        Self::read_core_lease(core, &expected)?;
        #[cfg(test)]
        if let Some(hook) = core.take_before_final_lease_reread() {
            hook();
        }
        Self::read_core_lease(core, &expected)?;
        // The guild transaction retains its writer lock and checks its epoch
        // before COMMIT. An empty core snapshot adds no fencing; release core
        // so slow guild commits cannot occupy its pool or a provider stream.
        #[cfg(test)]
        if let Some(hook) = core.take_during_leased_guild_commit() {
            hook();
        }
        commit()
    }

    fn read_core_lease(core: &Db, expected: &str) -> Result<()> {
        core.with_conn_sync(|conn| {
            let tx = conn.transaction_primary_snapshot()?;
            let value = Self::lease_row(&tx)?;
            tx.finish_read()?;
            crate::db_compat::reject_active_bot_lease(value.as_deref(), expected)
        })
    }

    fn lease_row(tx: &crate::db_compat::Transaction) -> Result<Option<String>> {
        tx.query_row(
            "SELECT value FROM meta WHERE key='active_bot_lease'",
            (),
            |row| Ok(row.get(0)?),
        )
        .optional()
    }

    pub(super) fn confirm_captured_enrollment(
        core: &Option<Db>,
        scope: &GuildDataScope,
    ) -> Result<()> {
        let Some(core) = core else {
            return Ok(());
        };
        core.with_conn_sync(|conn| {
            super::enrollments::verify_enrollment_conn(conn, scope.enrollment())
        })
    }

    pub async fn confirm_active_enrollment(&self, scope: GuildDataScope) -> Result<()> {
        let core = self.core.clone();
        self.db
            .with_conn(move |conn| {
                let tx = conn.transaction_primary_read()?;
                if let Err(error) = Self::confirm_captured_enrollment(&core, &scope) {
                    tx.finish_read()?;
                    return Err(error);
                }
                tx.finish_read()?;
                Ok(())
            })
            .await
    }

    /// Record the guild-local fence first, then the core fence row.
    pub async fn publish_guild_write_fence(
        &self,
        guild: i64,
        fenced_at_ms: i64,
        generation: &str,
        destination_store_id: &str,
        destination_revision: &str,
    ) -> Result<()> {
        self.record_local_write_fence(guild, fenced_at_ms).await?;
        let Some(core) = self.core.clone() else {
            anyhow::bail!("guild write fence requires the core database");
        };
        let generation = generation.to_owned();
        let destination_store_id = destination_store_id.to_owned();
        let destination_revision = destination_revision.to_owned();
        core.with_conn(move |conn| {
            let mut tx = conn.transaction_primary_read()?;
            tx.ensure_mutation_receipt()?;
            tx.execute_bounded(
                "INSERT INTO guild_write_fences(
                    guild_id, generation, phase, destination_store_id, destination_revision, created_at
                 ) VALUES(?1, ?2, 'freeze', ?3, ?4, ?5)",
                params![
                    guild,
                    generation,
                    destination_store_id,
                    destination_revision,
                    fenced_at_ms
                ],
                8192,
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
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
                let tx = conn.transaction_primary_snapshot()?;
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
