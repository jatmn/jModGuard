//! Shared/dedicated guild-store routing, case/evidence handles and live erasure.
use crate::config::{EvidenceConfig, GuildStoreConfig, GuildStoresConfig};
use crate::db::guild_data::{
    CaseLimits, EvidenceLimits, EvidenceWork, GuildDataScope, GuildDataStore, GuildStoreOptions,
};
use crate::db::guild_stores::{
    StoreId, StorePolicy, StoreRegistration, assign_initial_placement, clear_erased_placement,
    dedicated_store_registration, guild_placement, register_store, shared_store_registration,
};
use crate::db::{
    self, Db, GuildEnrollment, GuildErasureJob, finish_guild_erasure, next_guild_erasure,
    queue_guild_erasure, record_backup_cleanup_intent, wipe_departed_directory_guild_data,
};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::{HashMap, VecDeque};
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

struct Inner {
    credentials: Vec<ResolvedStore>,
    default_shared: StoreId,
    max_handles: usize,
    handles: Mutex<HandleCache>,
    evidence: EvidenceBackend,
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
            GuildDataStore::initialize(store.options.clone(), store.registration.clone())?;
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
                evidence: open_evidence(core, evidence).await?,
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
        if db::guild_erasure_queued(&self.core, enrollment.guild_id).await? {
            return Ok(None);
        }
        let store_id = self.store_for_guild(enrollment.guild_id)?;
        let placement = assign_initial_placement(&self.core, enrollment.guild_id, store_id).await?;
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
            Err(error)
                if error.downcast_ref::<TenantError>().is_some()
                    || error
                        .downcast_ref::<crate::db::guild_data::TenantDataError>()
                        .is_some() =>
            {
                tracing::warn!(guild_id, "selected case not recorded: {error:#}");
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let id = crate::db::guild_data::CaseId::new();
        match store.record_case(scope, id, data, case_limits()).await {
            Ok(result) => Ok(Some(result)),
            Err(error)
                if error.downcast_ref::<TenantError>().is_some()
                    || error
                        .downcast_ref::<crate::db::guild_data::TenantDataError>()
                        .is_some() =>
            {
                tracing::warn!("selected case not recorded: {error:#}");
                Ok(None)
            }
            Err(error) => Err(error),
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
        let Some(job) = next_guild_erasure(&self.core).await? else {
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
        let handle = self.handle(job.placement.store_id()).await?;
        let scope = GuildDataScope::new(job.placement.clone(), job.enrollment.clone())?;
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
        handle.erase_closed_scope(scope).await?;
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
                    EvidenceWork::Delete,
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
                        let _ = handle
                            .confirm_evidence_deleted(
                                scope.clone(),
                                job.case_id.clone(),
                                job.id.clone(),
                            )
                            .await;
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
        let resolved = self
            .inner
            .credentials
            .iter()
            .find(|store| store.registration.id() == id)
            .ok_or(TenantError::Offline)?;
        let handle = GuildDataStore::open(resolved.options.clone(), resolved.registration.clone())
            .or_else(|_| {
                GuildDataStore::initialize(resolved.options.clone(), resolved.registration.clone())
            })?;
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
                    .read_available(&available.job, &available.sha256)
                    .await?
            }
        };
        Ok(Some((available, bytes)))
    }
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
        "remote core databases require an explicit shared [[guild_stores.stores]] entry"
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
                let id = crate::db::guild_data::CaseId::try_from(
                    db::evidence_storage_id(core, &evidence.storage_id).await?,
                )?;
                let store = crate::evidence_transfer::LocalEvidenceStore::initialize(
                    root.clone(),
                    id.clone(),
                    evidence.buffer_bytes,
                )
                .or_else(|_| {
                    crate::evidence_transfer::LocalEvidenceStore::open(
                        root,
                        id,
                        evidence.buffer_bytes,
                    )
                })?;
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
