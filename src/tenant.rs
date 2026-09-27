//! Shared/dedicated guild-store routing, case/evidence handles and live erasure.
use crate::config::{EvidenceConfig, GuildStoreConfig, GuildStoresConfig};
use crate::db::guild_data::{
    CaseError, CaseId, CaseLimits, CaseSource, EvidenceLimits, EvidenceOrigin, EvidenceRequest,
    EvidenceState, EvidenceWork, GuildDataScope, GuildDataStore, GuildStoreOptions, LinkCaseSource,
};
use crate::db::guild_stores::{
    GuildWriteFenced, StoreId, StorePolicy, StoreRegistration, admit_tenant_placement,
    clear_erased_placement, dedicated_store_registration, guild_is_write_fenced, guild_placement,
    register_store, shared_store_registration,
};
use crate::db::{
    self, Db, GuildEnrollment, GuildErasureJob, SqlExecutor, finish_guild_erasure,
    next_guild_erasure_after, queue_guild_erasure, record_backup_cleanup_intent,
    wipe_departed_directory_guild_data,
};
use crate::db_compat::OptionalExtension;
use anyhow::{Context, Result, anyhow, bail};
use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum TenantError {
    #[error("this server is not assigned a guild store")]
    Unplaced,
    #[error("guild store is unavailable")]
    Offline,
    #[error("guild membership changed")]
    Membership,
}

#[derive(Clone)]
pub struct TenantRuntime {
    core: Db,
    inner: Arc<Inner>,
}

/// Catalog entry and its locally reachable handle for owner usage inspection.
pub struct UsageStore {
    pub registration: StoreRegistration,
    pub handle: Option<GuildDataStore>,
}

struct Inner {
    credentials: Vec<ResolvedStore>,
    default_shared: StoreId,
    max_handles: usize,
    handles: Mutex<HandleCache>,
    handle_open_locks: parking_lot::Mutex<HashMap<String, Arc<Mutex<()>>>>,
    evidence: EvidenceBackend,
    erasure_cursor: Mutex<Option<i64>>,
    locks: parking_lot::Mutex<HashMap<i64, Arc<Mutex<()>>>>,
}

struct HandleCache {
    order: VecDeque<String>,
    map: HashMap<String, GuildDataStore>,
}

struct ResolvedStore {
    registration: StoreRegistration,
    options: GuildStoreOptions,
}

enum EvidenceBackend {
    None,
    #[cfg(unix)]
    Local(crate::evidence_transfer::LocalEvidenceStore),
    #[cfg(unix)]
    Bunny(Box<crate::bunny_evidence::BunnyEvidenceStore>),
}

pub fn case_limits() -> CaseLimits {
    CaseLimits::default()
}

pub fn evidence_limits() -> EvidenceLimits {
    EvidenceLimits {
        max_items_per_case: 8,
        max_file_bytes: 8_388_608,
        max_case_bytes: 33_554_432,
        max_page_size: 32,
    }
}

/// Admission plan for sequential case-evidence reservations matching
/// `GuildDataStore::reserve_case_evidence` item, per-file, and per-case byte checks.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EvidenceCapturePlan {
    pub admitted: Vec<usize>,
    pub skipped_invalid: u32,
    pub skipped_item_limit: u32,
    pub skipped_byte_limit: u32,
}

pub fn plan_case_evidence_captures(
    reserved_bytes: &[u64],
    limits: EvidenceLimits,
) -> EvidenceCapturePlan {
    let _ = limits.validate();
    let max_items = limits.max_items_per_case as usize;
    let mut plan = EvidenceCapturePlan::default();
    let mut case_bytes = 0u64;
    for (index, bytes) in reserved_bytes.iter().enumerate() {
        if *bytes == 0 || *bytes > limits.max_file_bytes {
            plan.skipped_invalid += 1;
            continue;
        }
        if plan.admitted.len() >= max_items {
            plan.skipped_item_limit += 1;
            continue;
        }
        let Some(sum) = case_bytes.checked_add(*bytes) else {
            plan.skipped_byte_limit += 1;
            continue;
        };
        if sum > limits.max_case_bytes {
            plan.skipped_byte_limit += 1;
            continue;
        }
        case_bytes = sum;
        plan.admitted.push(index);
    }
    plan
}

/// Stable evidence identity for one Discord attachment origin. Retries of the
/// same channel/message/attachment reuse this ID; it is not a random CaseId.
pub fn attachment_evidence_id(origin: &EvidenceOrigin) -> Result<CaseId> {
    anyhow::ensure!(
        origin.channel_id > 0 && origin.message_id > 0 && origin.attachment_id > 0,
        "attachment origin is incomplete"
    );
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    digest.update(b"jmodguard-discord-attachment-1");
    digest.update(&origin.channel_id.to_le_bytes());
    digest.update(&origin.message_id.to_le_bytes());
    digest.update(&origin.attachment_id.to_le_bytes());
    let digest = digest.finish();
    let mut hex = String::with_capacity(32);
    for byte in &digest.as_ref()[..16] {
        write!(&mut hex, "{byte:02x}").expect("hex");
    }
    CaseId::try_from(hex)
}

fn skippable_live(error: &anyhow::Error) -> bool {
    error.downcast_ref::<TenantError>().is_some()
        || error
            .downcast_ref::<crate::db::guild_data::TenantDataError>()
            .is_some()
        || error
            .downcast_ref::<crate::db::guild_stores::GuildWriteFenced>()
            .is_some()
}

pub fn history_limits() -> crate::db::guild_data::CaseHistoryLimits {
    crate::db::guild_data::CaseHistoryLimits {
        entry: CaseLimits::default(),
        max_entries: 64,
        max_total_bytes: 65_536,
    }
}

fn block_on<T: Send>(fut: impl std::future::Future<Output = Result<T>> + Send) -> Result<T> {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("tenant runtime")?
                    .block_on(fut)
            })
            .join()
            .map_err(|_| anyhow!("tenant runtime thread panicked"))?
    })
}

impl TenantRuntime {
    /// Resolve one catalog ID without opening or enumerating unrelated stores.
    pub async fn usage_store(&self, id: &str) -> Result<Option<UsageStore>> {
        let id = StoreId::parse(id)?;
        let lookup = id.as_str().to_owned();
        let registration = self
            .core
            .with_conn(move |conn| {
                let tx = conn.transaction_primary_read()?;
                let row = tx
                    .query_row(
                        "SELECT dedicated_guild_id FROM guild_store_catalog WHERE store_id=?1",
                        crate::db::params![lookup],
                        |row| Ok(row.get::<Option<i64>>(0)?),
                    )
                    .optional()?;
                tx.finish_read()?;
                Ok(row)
            })
            .await?;
        let Some(owner) = registration else {
            return Ok(None);
        };
        let policy = match owner {
            None => StorePolicy::Shared,
            Some(guild_id) if guild_id > 0 => StorePolicy::Dedicated { guild_id },
            _ => bail!("invalid guild store catalog owner"),
        };
        let registration = StoreRegistration::from_parts(id, policy);
        let handle = if self
            .inner
            .credentials
            .iter()
            .any(|known| known.registration == registration)
        {
            self.handle(registration.id()).await.ok()
        } else {
            None
        };
        Ok(Some(UsageStore {
            registration,
            handle,
        }))
    }

    pub async fn usage_catalog(&self) -> Result<Vec<StoreRegistration>> {
        let catalog: Vec<StoreRegistration> = self
            .core
            .with_conn(|conn| {
                let tx = conn.transaction_primary_read()?;
                let mut stmt = tx.prepare(
                    "SELECT store_id,dedicated_guild_id FROM guild_store_catalog ORDER BY store_id",
                )?;
                let rows = stmt.query_map((), |row| {
                    Ok((row.get::<String>(0)?, row.get::<Option<i64>>(1)?))
                })?;
                let mut catalog = Vec::new();
                for row in rows {
                    let (id, owner) = row?;
                    let id = StoreId::parse(&id)?;
                    let policy = match owner {
                        None => StorePolicy::Shared,
                        Some(guild_id) if guild_id > 0 => StorePolicy::Dedicated { guild_id },
                        _ => anyhow::bail!("invalid guild store catalog owner"),
                    };
                    catalog.push(StoreRegistration::from_parts(id, policy));
                }
                drop(stmt);
                tx.finish_read()?;
                Ok(catalog)
            })
            .await?;
        Ok(catalog)
    }

    pub async fn usage_stores(&self) -> Result<Vec<UsageStore>> {
        let catalog = self.usage_catalog().await?;
        let mut result = Vec::with_capacity(catalog.len());
        for registration in catalog {
            let handle = if self
                .inner
                .credentials
                .iter()
                .any(|known| known.registration == registration)
            {
                self.handle(registration.id()).await.ok()
            } else {
                None
            };
            result.push(UsageStore {
                registration,
                handle,
            });
        }
        Ok(result)
    }
    pub fn open(
        core: &Db,
        db: &crate::config::DbConfig,
        stores: &GuildStoresConfig,
        evidence: &EvidenceConfig,
    ) -> Result<Self> {
        block_on(Self::open_async(core, db, stores, evidence))
    }

    async fn open_async(
        core: &Db,
        db: &crate::config::DbConfig,
        stores: &GuildStoresConfig,
        evidence: &EvidenceConfig,
    ) -> Result<Self> {
        stores.validate()?;
        evidence.validate()?;
        let pool = db.pool_size.max(2);
        let mut credentials = Vec::new();
        for configured in &stores.stores {
            credentials.push(resolve_configured(core, configured, pool).await?);
        }
        if !credentials
            .iter()
            .any(|store| matches!(store.registration.policy(), StorePolicy::Shared))
        {
            credentials.push(default_shared(core, db, pool).await?);
        }
        let mut default_shared = None;
        for store in &credentials {
            let registered = register_store(
                core,
                store.registration.id().clone(),
                store.registration.policy(),
            )
            .await?;
            anyhow::ensure!(
                registered == store.registration,
                "configured guild store ownership conflicts with the catalog"
            );
            open_runtime_store(core, store).await?;
            if matches!(store.registration.policy(), StorePolicy::Shared) {
                default_shared.get_or_insert_with(|| store.registration.id().clone());
            }
        }
        let default_shared =
            default_shared.ok_or_else(|| anyhow!("no shared guild store is configured"))?;
        Ok(Self {
            core: core.clone(),
            inner: Arc::new(Inner {
                credentials,
                default_shared,
                max_handles: stores.max_handles as usize,
                handles: Mutex::new(HandleCache {
                    order: VecDeque::new(),
                    map: HashMap::new(),
                }),
                handle_open_locks: parking_lot::Mutex::new(HashMap::new()),
                evidence: open_evidence(core, evidence).await?,
                erasure_cursor: Mutex::new(None),
                locks: parking_lot::Mutex::new(HashMap::new()),
            }),
        })
    }

    fn lock(&self, guild: i64) -> Arc<Mutex<()>> {
        self.inner
            .locks
            .lock()
            .entry(guild)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    pub async fn activate(&self, enrollment: GuildEnrollment) -> Result<Option<GuildDataScope>> {
        let lock = self.lock(enrollment.guild_id);
        let _guard = lock.lock().await;
        // Catalog placement is the live assignment after a placement change.
        // Config (dedicated-wins) is only the initial-placement hint.
        let store_id = match guild_placement(&self.core, enrollment.guild_id).await? {
            Some(existing) => existing.store_id().clone(),
            None => self.store_for_guild(enrollment.guild_id)?,
        };
        let Some(placement) =
            admit_tenant_placement(&self.core, enrollment.clone(), store_id).await?
        else {
            return Ok(None);
        };
        let handle = self.handle(placement.store_id()).await?;
        handle.bind_initial_placement(placement.clone()).await?;
        if let Some((existing, _active)) = handle.stored_membership(placement.clone()).await? {
            if existing.id != enrollment.id || existing.joined_at_ns != enrollment.joined_at_ns {
                queue_guild_erasure(&self.core, existing, placement).await?;
                return Ok(None);
            }
            return GuildDataScope::new(placement, enrollment).map(Some);
        }
        let scope = GuildDataScope::new(placement, enrollment)?;
        handle.bind_initial_membership(scope.clone()).await?;
        Ok(Some(scope))
    }

    pub async fn scope(&self, enrollment: GuildEnrollment) -> Result<GuildDataScope> {
        let placement = guild_placement(&self.core, enrollment.guild_id)
            .await?
            .ok_or(TenantError::Unplaced)?;
        GuildDataScope::new(placement, enrollment).map_err(|_| TenantError::Membership.into())
    }

    pub async fn store(&self, scope: &GuildDataScope) -> Result<GuildDataStore> {
        self.handle(scope.placement().store_id())
            .await
            .map_err(|_| TenantError::Offline.into())
    }

    pub async fn live(
        &self,
        enrollment: GuildEnrollment,
    ) -> Result<(GuildDataStore, GuildDataScope)> {
        if guild_is_write_fenced(&self.core, enrollment.guild_id).await? {
            return Err(GuildWriteFenced.into());
        }
        let scope = self.scope(enrollment).await?;
        let store = self.store(&scope).await?;
        match store.stored_membership(scope.placement().clone()).await {
            Ok(Some((current, true)))
                if current.id == scope.enrollment().id
                    && current.joined_at_ns == scope.enrollment().joined_at_ns => {}
            Ok(_) => return Err(TenantError::Membership.into()),
            Err(_) => return Err(TenantError::Offline.into()),
        }
        Ok((store, scope))
    }

    pub async fn record_selected_case(
        &self,
        enrollment: GuildEnrollment,
        data: crate::db::guild_data::CaseData,
    ) -> Result<Option<crate::db::guild_data::RecordCase>> {
        let guild_id = enrollment.guild_id;
        let (store, scope) = match self.live(enrollment).await {
            Ok(live) => live,
            Err(error) if skippable_live(&error) => {
                tracing::warn!(guild_id, "selected case not recorded: {error:#}");
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let id = CaseId::new();
        match store.record_case(scope, id, data, case_limits()).await {
            Ok(result) => Ok(Some(result)),
            Err(error) if skippable_live(&error) => {
                tracing::warn!("selected case not recorded: {error:#}");
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// Bind a Discord audit entry to an already-recorded application case.
    /// Callers must supply the dispatch's own source identity; this does not
    /// search cases by subject or time.
    pub async fn link_selected_source(
        &self,
        enrollment: GuildEnrollment,
        case_id: CaseId,
        anchor: CaseSource,
        source: CaseSource,
    ) -> Result<Option<LinkCaseSource>> {
        let guild_id = enrollment.guild_id;
        let (store, scope) = match self.live(enrollment).await {
            Ok(live) => live,
            Err(error) if skippable_live(&error) => {
                tracing::warn!(guild_id, "selected source not linked: {error:#}");
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        match store.link_case_source(scope, case_id, anchor, source).await {
            Ok(result) => Ok(Some(result)),
            Err(error) if skippable_live(&error) => {
                tracing::warn!(guild_id, "selected source not linked: {error:#}");
                Ok(None)
            }
            Err(error) if error.downcast_ref::<CaseError>().is_some() => {
                tracing::warn!(guild_id, "selected source not linked: {error:#}");
                Err(error.context("selected source link conflict"))
            }
            Err(error) => Err(error),
        }
    }

    #[cfg(unix)]
    async fn reconcile_attachment_evidence(
        &self,
        store: GuildDataStore,
        target: crate::evidence_transfer::EvidenceTarget,
    ) -> Result<EvidenceState> {
        match &self.inner.evidence {
            EvidenceBackend::None => bail!("evidence backend is not configured"),
            EvidenceBackend::Local(backend) => backend.reconcile(store, target).await,
            EvidenceBackend::Bunny(backend) => backend.reconcile(store, target).await,
        }
    }

    #[cfg(unix)]
    async fn cleanup_attachment_evidence(
        &self,
        store: GuildDataStore,
        target: crate::evidence_transfer::EvidenceTarget,
    ) -> Result<()> {
        match &self.inner.evidence {
            EvidenceBackend::None => bail!("evidence backend is not configured"),
            EvidenceBackend::Local(backend) => {
                backend.cleanup(store, target).await?;
            }
            EvidenceBackend::Bunny(backend) => {
                backend.cleanup(store, target).await?;
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    async fn prepare_attachment_evidence_capture(
        &self,
        store: GuildDataStore,
        target: crate::evidence_transfer::EvidenceTarget,
    ) -> Result<bool> {
        let Some(job) = store
            .find_evidence_transfer_job(
                target.scope.clone(),
                target.case_id.clone(),
                target.evidence_id.clone(),
            )
            .await?
        else {
            return Ok(true);
        };
        match job.state {
            EvidenceState::Available => Ok(false),
            EvidenceState::Capturing => {
                let state = self
                    .reconcile_attachment_evidence(store.clone(), target.clone())
                    .await?;
                self.after_attachment_reconcile(store, target, state).await
            }
            EvidenceState::CleanupPending => {
                self.cleanup_attachment_evidence(store, target).await?;
                Err(anyhow!(
                    "attachment evidence origin was discarded and cannot be recaptured"
                ))
            }
            EvidenceState::Erased => Err(anyhow!(
                "attachment evidence origin was discarded and cannot be recaptured"
            )),
        }
    }

    #[cfg(unix)]
    async fn after_attachment_reconcile(
        &self,
        store: GuildDataStore,
        target: crate::evidence_transfer::EvidenceTarget,
        state: EvidenceState,
    ) -> Result<bool> {
        match state {
            EvidenceState::Available => Ok(false),
            EvidenceState::CleanupPending => {
                self.cleanup_attachment_evidence(store, target).await?;
                Err(anyhow!(
                    "attachment evidence origin was discarded and cannot be recaptured"
                ))
            }
            EvidenceState::Capturing => {
                bail!("attachment capture remains in flight after reconciliation")
            }
            EvidenceState::Erased => Err(anyhow!(
                "attachment evidence origin was discarded and cannot be recaptured"
            )),
        }
    }

    /// Stream a proving Discord attachment into the configured evidence backend.
    /// Completed objects are not re-uploaded. A disabled backend is a no-op.
    pub async fn capture_selected_attachment(
        &self,
        enrollment: GuildEnrollment,
        case_id: CaseId,
        origin: EvidenceOrigin,
        bytes: u64,
        source: PathBuf,
    ) -> Result<Option<EvidenceState>> {
        #[cfg(not(unix))]
        {
            let _ = (enrollment, case_id, origin, bytes, source);
            return Ok(None);
        }
        #[cfg(unix)]
        {
            if matches!(self.inner.evidence, EvidenceBackend::None) {
                return Ok(None);
            }
            let guild_id = enrollment.guild_id;
            let (store, scope) = match self.live(enrollment).await {
                Ok(live) => live,
                Err(error) if skippable_live(&error) => {
                    tracing::warn!(guild_id, "selected attachment not captured: {error:#}");
                    return Ok(None);
                }
                Err(error) => return Err(error),
            };
            let Some(storage_id) = self.evidence_storage_id() else {
                return Ok(None);
            };
            let evidence_id = attachment_evidence_id(&origin)?;
            let request = EvidenceRequest {
                storage_id,
                bytes,
                origin,
            };
            let target = crate::evidence_transfer::EvidenceTarget {
                scope: scope.clone(),
                case_id: case_id.clone(),
                evidence_id: evidence_id.clone(),
            };
            if !self
                .prepare_attachment_evidence_capture(store.clone(), target.clone())
                .await?
            {
                return Ok(None);
            }
            let upload = crate::evidence_transfer::EvidenceUpload {
                target,
                request,
                limits: evidence_limits(),
            };
            let state = match &self.inner.evidence {
                EvidenceBackend::None => return Ok(None),
                EvidenceBackend::Local(backend) => {
                    let file = std::fs::File::open(&source)
                        .context("attachment capture source is missing")?;
                    backend.capture(store, upload, file).await?
                }
                EvidenceBackend::Bunny(backend) => {
                    let file = tokio::fs::File::open(&source)
                        .await
                        .context("attachment capture source is missing")?;
                    backend.capture(store, upload, file).await?
                }
            };
            Ok(Some(state))
        }
    }

    #[cfg(unix)]
    fn evidence_storage_id(&self) -> Option<CaseId> {
        match &self.inner.evidence {
            EvidenceBackend::None => None,
            EvidenceBackend::Local(store) => Some(store.storage_id().clone()),
            EvidenceBackend::Bunny(store) => Some(store.storage_id().clone()),
        }
    }

    pub async fn schedule_erasure(&self, enrollment: GuildEnrollment) -> Result<()> {
        let Some(placement) = guild_placement(&self.core, enrollment.guild_id).await? else {
            return Ok(());
        };
        queue_guild_erasure(&self.core, enrollment, placement).await?;
        Ok(())
    }

    pub async fn drive_erasure(&self) -> Result<bool> {
        let job = {
            let mut cursor = self.inner.erasure_cursor.lock().await;
            let job = next_guild_erasure_after(&self.core, *cursor).await?;
            // Advance before I/O so a failed guild cannot monopolize future ticks.
            *cursor = job.as_ref().map(|job| job.guild_id);
            job
        };
        let Some(job) = job else {
            return Ok(false);
        };
        let guild_id = job.guild_id;
        {
            let lock = self.lock(guild_id);
            let _guard = lock.lock().await;
            self.erase_job(&job).await?;
        }
        let mut locks = self.inner.locks.lock();
        if locks
            .get(&guild_id)
            .is_some_and(|lock| Arc::strong_count(lock) == 1)
        {
            locks.remove(&guild_id);
        }
        Ok(true)
    }

    async fn erase_job(&self, job: &GuildErasureJob) -> Result<()> {
        if guild_is_write_fenced(&self.core, job.guild_id).await? {
            return Ok(());
        }
        let handle = self.handle(job.placement.store_id()).await?;
        let scope = GuildDataScope::new(job.placement.clone(), job.enrollment.clone())?;
        if !handle.finish_empty_erasure(scope.clone()).await? {
            handle.close_membership(scope.clone()).await?;
            let limits = evidence_limits();
            while handle
                .cancel_closed_evidence(scope.clone(), limits.max_page_size, limits)
                .await?
                > 0
            {}
            self.cleanup_evidence(&handle, &scope).await?;
            while handle
                .purge_closed_cases(scope.clone(), case_limits().max_page_size, case_limits())
                .await?
                > 0
            {}
            while !handle.erase_closed_scope(scope.clone()).await? {}
        }
        record_backup_cleanup_intent(
            &self.core,
            "guild",
            job.guild_id,
            Some(&job.enrollment.id),
            None,
            None,
            None,
        )
        .await?;
        if !wipe_departed_directory_guild_data(&self.core, job.guild_id).await? {
            tracing::info!(
                guild_id = job.guild_id,
                departed_enrollment = %job.enrollment.id,
                "preserved directory data for a newer active guild enrollment"
            );
        }
        clear_erased_placement(&self.core, job.guild_id, &job.placement).await?;
        finish_guild_erasure(&self.core, job).await
    }

    pub async fn cleanup_owned_evidence(
        &self,
        handle: &GuildDataStore,
        scope: &GuildDataScope,
    ) -> Result<()> {
        self.cleanup_evidence(handle, scope).await
    }

    async fn cleanup_evidence(
        &self,
        handle: &GuildDataStore,
        scope: &GuildDataScope,
    ) -> Result<()> {
        let limits = evidence_limits();
        let mut after = None;
        loop {
            let jobs = handle
                .evidence_jobs(
                    scope.clone(),
                    EvidenceWork::Cleanup,
                    after.clone(),
                    limits.max_page_size,
                    limits,
                )
                .await?;
            if jobs.is_empty() {
                break;
            }
            for job in &jobs {
                match &self.inner.evidence {
                    EvidenceBackend::None => {
                        anyhow::bail!("private evidence cleanup requires its configured backend");
                    }
                    #[cfg(unix)]
                    EvidenceBackend::Local(store) => {
                        store
                            .cleanup(
                                handle.clone(),
                                crate::evidence_transfer::EvidenceTarget {
                                    scope: scope.clone(),
                                    case_id: job.case_id.clone(),
                                    evidence_id: job.id.clone(),
                                },
                            )
                            .await?;
                    }
                    #[cfg(unix)]
                    EvidenceBackend::Bunny(store) => {
                        store
                            .cleanup(
                                handle.clone(),
                                crate::evidence_transfer::EvidenceTarget {
                                    scope: scope.clone(),
                                    case_id: job.case_id.clone(),
                                    evidence_id: job.id.clone(),
                                },
                            )
                            .await?;
                    }
                }
                after = Some(job.id.clone());
            }
            if jobs.len() < limits.max_page_size as usize {
                break;
            }
        }
        Ok(())
    }

    pub async fn record_case_cleanup(
        &self,
        scope: &GuildDataScope,
        case: &crate::db::guild_data::CaseId,
    ) -> Result<()> {
        record_backup_cleanup_intent(
            &self.core,
            "case",
            scope.guild_id(),
            Some(&scope.enrollment().id),
            Some(case.as_str()),
            None,
            None,
        )
        .await
    }

    fn store_for_guild(&self, guild: i64) -> Result<StoreId> {
        for store in &self.inner.credentials {
            if let StorePolicy::Dedicated { guild_id } = store.registration.policy()
                && guild_id == guild
            {
                return Ok(store.registration.id().clone());
            }
        }
        Ok(self.inner.default_shared.clone())
    }

    async fn handle(&self, id: &StoreId) -> Result<GuildDataStore> {
        let mut cache = self.inner.handles.lock().await;
        if let Some(handle) = cache.map.get(id.as_str()) {
            let handle = handle.clone();
            cache.order.retain(|stored| stored != id.as_str());
            cache.order.push_back(id.as_str().to_owned());
            return Ok(handle);
        }
        drop(cache);
        let open_lock = self
            .inner
            .handle_open_locks
            .lock()
            .entry(id.as_str().to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _opening = open_lock.lock().await;
        let cache = self.inner.handles.lock().await;
        if let Some(handle) = cache.map.get(id.as_str()).cloned() {
            return Ok(handle);
        }
        drop(cache);
        let resolved = self
            .inner
            .credentials
            .iter()
            .find(|store| store.registration.id() == id)
            .ok_or(TenantError::Offline)?;
        let handle = open_runtime_store(&self.core, resolved).await?;
        let mut cache = self.inner.handles.lock().await;
        if let Some(existing) = cache.map.get(id.as_str()).cloned() {
            return Ok(existing);
        }
        cache.order.retain(|stored| stored != id.as_str());
        cache.order.push_back(id.as_str().to_owned());
        cache.map.insert(id.as_str().to_owned(), handle.clone());
        while cache.map.len() > self.inner.max_handles {
            if let Some(old) = cache.order.pop_front()
                && old != id.as_str()
            {
                cache.map.remove(&old);
            } else if cache.order.front().map(String::as_str) == Some(id.as_str()) {
                break;
            }
        }
        Ok(handle)
    }

    #[cfg(test)]
    async fn cached_handle_count(&self) -> usize {
        self.inner.handles.lock().await.map.len()
    }

    pub fn evidence_enabled(&self) -> bool {
        !matches!(self.inner.evidence, EvidenceBackend::None)
    }

    #[cfg(unix)]
    pub fn local_evidence(&self) -> Option<&crate::evidence_transfer::LocalEvidenceStore> {
        match &self.inner.evidence {
            EvidenceBackend::Local(store) => Some(store),
            _ => None,
        }
    }

    #[cfg(unix)]
    pub fn bunny_evidence(&self) -> Option<&crate::bunny_evidence::BunnyEvidenceStore> {
        match &self.inner.evidence {
            EvidenceBackend::Bunny(store) => Some(store.as_ref()),
            _ => None,
        }
    }

    pub async fn read_available_evidence(
        &self,
        enrollment: GuildEnrollment,
        case: crate::db::guild_data::CaseId,
        evidence: crate::db::guild_data::CaseId,
    ) -> Result<Option<(crate::db::guild_data::AvailableEvidence, Vec<u8>)>> {
        let (store, scope) = self.live(enrollment).await?;
        #[cfg(unix)]
        let target = crate::evidence_transfer::EvidenceTarget {
            scope: scope.clone(),
            case_id: case.clone(),
            evidence_id: evidence.clone(),
        };
        let Some(available) = store.available_case_evidence(scope, case, evidence).await? else {
            return Ok(None);
        };
        let bytes = match &self.inner.evidence {
            EvidenceBackend::None => {
                anyhow::bail!("evidence backend is not configured")
            }
            #[cfg(unix)]
            EvidenceBackend::Local(backend) => {
                let job = available.job.clone();
                let sha = available.sha256.clone();
                let backend = backend.clone();
                tokio::task::spawn_blocking(move || backend.read_available(&job, &sha)).await??
            }
            #[cfg(unix)]
            EvidenceBackend::Bunny(backend) => {
                backend
                    .read_available(&store, &target, &available.job, &available.sha256)
                    .await?
            }
        };
        Ok(Some((available, bytes)))
    }
}

async fn open_runtime_store(core: &Db, store: &ResolvedStore) -> Result<GuildDataStore> {
    if GuildDataStore::existing_registration(store.options.clone())?.is_none() {
        use crate::db::SqlExecutor;
        let id = store.registration.id().as_str().to_owned();
        let owned: bool = core.with_conn(move |conn| {
            let tx = conn.transaction_primary_read()?;
            let owned = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM guild_store_placements WHERE store_id=?1) OR EXISTS(SELECT 1 FROM guild_erasure_jobs WHERE store_id=?1)",
                crate::db::params![id], |row| Ok(row.get(0)?),
            )?;
            tx.finish_read()?;
            Ok(owned)
        }).await?;
        anyhow::ensure!(
            !owned,
            "owned guild store is missing or empty; restore its original database before retrying"
        );
    }
    Ok(
        GuildDataStore::initialize(store.options.clone(), store.registration.clone())?
            .with_core_write_fence(core.clone()),
    )
}

async fn resolve_configured(
    core: &Db,
    configured: &GuildStoreConfig,
    pool: u32,
) -> Result<ResolvedStore> {
    configured.validate()?;
    let policy = if configured.dedicated_guild_id > 0 {
        StorePolicy::Dedicated {
            guild_id: configured.dedicated_guild_id,
        }
    } else {
        StorePolicy::Shared
    };
    anyhow::ensure!(
        !configured.path.as_os_str().is_empty() || !configured.url.trim().is_empty(),
        "guild store path or url is required"
    );
    let options = GuildStoreOptions {
        path: configured.path.clone(),
        url: configured.url.clone(),
        token: configured.token.clone(),
        pool_size: pool,
    };
    let id = if configured.id.is_empty() {
        discover_store_id(core, &options, policy).await?
    } else {
        StoreId::parse(&configured.id)?
    };
    Ok(ResolvedStore {
        registration: StoreRegistration::from_parts(id, policy),
        options,
    })
}

async fn default_shared(
    core: &Db,
    db: &crate::config::DbConfig,
    pool: u32,
) -> Result<ResolvedStore> {
    anyhow::ensure!(
        db.url.trim().is_empty(),
        "remote core databases require shared guild-store configuration (TOML or JMODGUARD_GUILD_STORE_URL/TOKEN)"
    );
    let path = core
        .sqlite_path()
        .map(|path| {
            let mut store = path.as_os_str().to_owned();
            store.push(".guild.db");
            PathBuf::from(store)
        })
        .ok_or_else(|| anyhow!("missing local core database path"))?;
    let options = GuildStoreOptions {
        path,
        url: String::new(),
        token: String::new(),
        pool_size: pool,
    };
    let id = discover_store_id(core, &options, StorePolicy::Shared).await?;
    Ok(ResolvedStore {
        registration: StoreRegistration::from_parts(id, StorePolicy::Shared),
        options,
    })
}

async fn discover_store_id(
    core: &Db,
    options: &GuildStoreOptions,
    policy: StorePolicy,
) -> Result<StoreId> {
    if let Some(stamped) = GuildDataStore::existing_registration(options.clone())? {
        anyhow::ensure!(
            stamped.policy() == policy,
            "configured guild store ownership conflicts with the store identity"
        );
        return Ok(stamped.id().clone());
    }
    let catalog = match policy {
        StorePolicy::Shared => shared_store_registration(core).await?,
        StorePolicy::Dedicated { guild_id } => dedicated_store_registration(core, guild_id).await?,
    };
    Ok(catalog
        .map(|registration| registration.id().clone())
        .unwrap_or_else(StoreId::new))
}

pub fn remove_sidecars(core: &std::path::Path) {
    let mut guild = core.as_os_str().to_owned();
    guild.push(".guild.db");
    let guild = PathBuf::from(guild);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", guild.display()));
    }
    let mut evidence = core.as_os_str().to_owned();
    evidence.push(".evidence");
    let _ = std::fs::remove_dir_all(PathBuf::from(evidence));
}

fn evidence_root(core: &Db, evidence: &EvidenceConfig) -> Result<PathBuf> {
    let root = if evidence.root.as_os_str().is_empty() {
        let path = core
            .sqlite_path()
            .context("local evidence requires a filesystem core database or evidence.root")?;
        let mut root = path.as_os_str().to_owned();
        root.push(".evidence");
        PathBuf::from(root)
    } else {
        evidence.root.clone()
    };
    if root.is_absolute() {
        Ok(root)
    } else {
        Ok(std::env::current_dir()?.join(root))
    }
}

async fn open_evidence(core: &Db, evidence: &EvidenceConfig) -> Result<EvidenceBackend> {
    match evidence.backend.as_str() {
        "none" => Ok(EvidenceBackend::None),
        "local" => {
            #[cfg(unix)]
            {
                let root = evidence_root(core, evidence)?;
                let first_install = db::existing_evidence_storage_id(core, &evidence.storage_id)
                    .await?
                    .is_none();
                let id = crate::db::guild_data::CaseId::try_from(
                    db::evidence_storage_id(core, &evidence.storage_id).await?,
                )?;
                let store = if first_install {
                    crate::evidence_transfer::LocalEvidenceStore::initialize(
                        root,
                        id,
                        evidence.buffer_bytes,
                    )?
                } else {
                    crate::evidence_transfer::LocalEvidenceStore::open(
                        root,
                        id,
                        evidence.buffer_bytes,
                    )
                    .context(
                        "owned local evidence resource is missing or unavailable; restore the original evidence root before retrying",
                    )?
                };
                Ok(EvidenceBackend::Local(store))
            }
            #[cfg(not(unix))]
            {
                let _ = core;
                Ok(EvidenceBackend::None)
            }
        }
        "bunny" => {
            #[cfg(unix)]
            {
                let id = crate::db::guild_data::CaseId::try_from(
                    db::evidence_storage_id(core, &evidence.storage_id).await?,
                )?;
                Ok(EvidenceBackend::Bunny(Box::new(
                    crate::bunny_evidence::BunnyEvidenceStore::new(
                        id,
                        &evidence.bunny.hostname,
                        &evidence.bunny.zone,
                        &evidence.bunny.access_key,
                        crate::bunny_evidence::BunnyTransferLimits {
                            buffer_bytes: evidence.buffer_bytes,
                            request_timeout: std::time::Duration::from_secs(
                                evidence.bunny.timeout_secs.max(1),
                            ),
                        },
                    )?,
                )))
            }
            #[cfg(not(unix))]
            {
                let _ = core;
                bail!("bunny evidence requires unix")
            }
        }
        _ => bail!("unsupported evidence backend"),
    }
}

#[cfg(test)]
mod tests;
