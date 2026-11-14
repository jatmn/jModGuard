//! Bounded, ordered admission for routine message observations. Serenity spawns
//! each high-level callback, so this serializes callback ingress before SQL
//! awaits. Timestamps and tombstones reject stale callbacks; coverage remains
//! explicitly last-observed rather than claiming a complete Gateway history.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serenity::client::Context;
use serenity::gateway::CollectorCallback;
use serenity::model::channel::Message;
use serenity::model::event::{Event, MessageUpdateEvent};
use serenity::model::id::{ChannelId, GuildId, MessageId};
use tokio::sync::mpsc;

use crate::db::guild_data::{
    GuildDataScope, GuildDataStore, MessageSnapshot, SNAPSHOT_CAPTURE_BATCH, SnapshotAttachment,
    SnapshotError, SnapshotFields, SnapshotHealthState, SnapshotHealthUpdate, SnapshotObservation,
    SnapshotOutcome, SnapshotTransition,
};
use crate::logging::{self, MessageReport};
use crate::state::BotState;

const MAX_PENDING: usize = 4096;
const MAX_PENDING_BYTES: usize = 16 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 32 * 1024;
const MAX_ATTACHMENTS_BYTES: usize = 16 * 1024;
const MAX_KNOWN: usize = 4096;
const TOMBSTONE_KEEP: Duration = Duration::from_secs(86_400);
const MAX_RECENT: usize = 8192;
const MAX_RECENT_BYTES: usize = 16 * 1024 * 1024;
const RECENT_LIFETIME_MS: i64 = 86_400_000;

type Key = (u64, u64, u64);

pub(crate) fn unique_ids(ids: &[MessageId]) -> Vec<MessageId> {
    let mut seen = HashSet::with_capacity(ids.len());
    ids.iter().copied().filter(|id| seen.insert(*id)).collect()
}

/// Fixed-memory reminder of deleted identities after the exact tombstone map
/// reaches its 4096-entry limit. False positives leave a marked capture gap;
/// they cannot recreate a deleted row.
struct DeletionBloom {
    started: Instant,
    epochs: [u64; 24],
    bits: Vec<Vec<u8>>,
}

impl DeletionBloom {
    const BUCKET_BYTES: usize = 64 * 1024;

    fn new() -> Self {
        Self {
            started: Instant::now(),
            epochs: [u64::MAX; 24],
            bits: vec![vec![0; Self::BUCKET_BYTES]; 24],
        }
    }

    fn hour(&self) -> u64 {
        self.started.elapsed().as_secs() / 3600
    }

    fn positions(key: Key) -> [usize; 5] {
        use std::hash::{Hash, Hasher};
        std::array::from_fn(|seed| {
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            seed.hash(&mut hash);
            key.hash(&mut hash);
            hash.finish() as usize % (Self::BUCKET_BYTES * 8)
        })
    }

    fn insert(&mut self, key: Key) {
        let hour = self.hour();
        let bucket = hour as usize % self.bits.len();
        if self.epochs[bucket] != hour {
            self.bits[bucket].fill(0);
            self.epochs[bucket] = hour;
        }
        for position in Self::positions(key) {
            self.bits[bucket][position / 8] |= 1 << (position % 8);
        }
    }

    fn contains(&self, key: Key) -> bool {
        let hour = self.hour();
        let positions = Self::positions(key);
        self.bits.iter().enumerate().any(|(index, bits)| {
            hour.checked_sub(self.epochs[index])
                .is_some_and(|age| age < 24)
                && positions
                    .iter()
                    .all(|position| bits[position / 8] & (1 << (position % 8)) != 0)
        })
    }
}

#[derive(Default)]
struct KnownHumans {
    entries: HashSet<Key>,
    order: VecDeque<(Key, Instant)>,
}

struct DeletedIdentities {
    exact: HashMap<Key, Instant>,
    order: VecDeque<(Key, Instant)>,
    bloom: DeletionBloom,
}

impl Default for DeletedIdentities {
    fn default() -> Self {
        Self {
            exact: HashMap::new(),
            order: VecDeque::new(),
            bloom: DeletionBloom::new(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DeletionKnowledge {
    Unseen,
    Exact,
    Possible,
}

impl DeletionKnowledge {
    fn blocks_capture(self) -> bool {
        !matches!(self, Self::Unseen)
    }
}

impl DeletedIdentities {
    fn knowledge(&self, key: Key) -> DeletionKnowledge {
        if self.exact.contains_key(&key) {
            DeletionKnowledge::Exact
        } else if self.bloom.contains(key) {
            DeletionKnowledge::Possible
        } else {
            DeletionKnowledge::Unseen
        }
    }
}

/// Process-only copy of the latest observation of recent eligible messages.
/// Delete and edit reports use it when the guild store cannot supply a row, so
/// a store outage does not drop text this process already observed. It follows
/// snapshot eligibility and expiry, is bounded by count and bytes, drops deleted
/// identities and is lost on restart. Each row keeps the captured scope it was
/// observed under and is only used by work with that same scope.
#[derive(Default)]
struct RecentMessages {
    entries: HashMap<Key, RecentEntry>,
    order: VecDeque<Key>,
    bytes: usize,
}

struct RecentEntry {
    row: MessageSnapshot,
    scope: GuildDataScope,
    size: usize,
}

fn same_scope(a: &GuildDataScope, b: &GuildDataScope) -> bool {
    a.placement() == b.placement() && a.enrollment() == b.enrollment()
}

impl RecentMessages {
    fn row(guild: u64, observation: &SnapshotObservation) -> MessageSnapshot {
        MessageSnapshot {
            guild_id: guild as i64,
            channel_id: observation.channel_id,
            message_id: observation.message_id,
            fields: observation.fields.clone(),
            created_at_ms: observation.created_at_ms,
            first_observed_at_ms: observation.observed_at_ms,
            last_observed_at_ms: observation.observed_at_ms,
            edit_at_ms: observation.edit_at_ms,
            expires_at_ms: observation.created_at_ms.saturating_add(RECENT_LIFETIME_MS),
            generation: logging::MEMORY_CACHE_GENERATION,
            coverage_epoch: 0,
            continuous: false,
            content_supplied: observation.fields.content.is_some(),
            attachments_supplied: observation.fields.attachments.is_some(),
        }
    }

    /// Records an observation and returns the cached state that precedes it:
    /// a row from an earlier revision (the create or an older edit). Fields an
    /// edit does not supply keep their earlier value; an older edit is ignored.
    /// As in the guild store, a field already supplied at one edit timestamp
    /// keeps its first value at that timestamp, and neither a newer nor an
    /// equal revision is returned as this observation's prior.
    fn observe(
        &mut self,
        scope: &GuildDataScope,
        observation: &SnapshotObservation,
        allow_insert: bool,
        now_ms: i64,
    ) -> Option<MessageSnapshot> {
        let guild = scope.guild_id() as u64;
        let key = (
            guild,
            observation.channel_id as u64,
            observation.message_id as u64,
        );
        let cached = self.get(key, scope, now_ms);
        let before = cached
            .clone()
            .filter(|row| row.edit_at_ms < observation.edit_at_ms);
        let row = match cached {
            Some(old) if old.edit_at_ms > observation.edit_at_ms => return None,
            Some(mut row) => {
                if row.edit_at_ms != observation.edit_at_ms {
                    row.content_supplied = false;
                    row.attachments_supplied = false;
                }
                let fields = &observation.fields;
                if fields.content.is_some() && !row.content_supplied {
                    row.fields.content = fields.content.clone();
                    row.fields.content_truncated = fields.content_truncated;
                    row.fields.content_original_bytes = fields.content_original_bytes;
                    row.content_supplied = true;
                }
                if fields.attachments.is_some() && !row.attachments_supplied {
                    row.fields.attachments = fields.attachments.clone();
                    row.fields.attachments_truncated = fields.attachments_truncated;
                    row.fields.attachments_omitted = fields.attachments_omitted;
                    row.attachments_supplied = true;
                }
                if fields.author_id.is_some() {
                    row.fields.author_id = fields.author_id;
                }
                if fields.author_name.is_some() {
                    row.fields.author_name = fields.author_name.clone();
                }
                row.last_observed_at_ms = observation.observed_at_ms;
                row.edit_at_ms = observation.edit_at_ms.or(row.edit_at_ms);
                row
            }
            None if allow_insert => Self::row(guild, observation),
            None => return None,
        };
        if row.expires_at_ms <= now_ms {
            return before;
        }
        let size = fields_bytes(&row.fields);
        let entry = RecentEntry {
            row,
            scope: scope.clone(),
            size,
        };
        if let Some(old) = self.entries.insert(key, entry) {
            self.bytes -= old.size;
        } else {
            self.order.push_back(key);
        }
        self.bytes += size;
        self.trim(now_ms);
        before
    }

    fn get(&self, key: Key, scope: &GuildDataScope, now_ms: i64) -> Option<MessageSnapshot> {
        self.entries
            .get(&key)
            .filter(|entry| entry.row.expires_at_ms > now_ms && same_scope(&entry.scope, scope))
            .map(|entry| entry.row.clone())
    }

    /// Removes the cached copy. It is returned only to work of the same scope.
    fn take(&mut self, key: Key, scope: &GuildDataScope, now_ms: i64) -> Option<MessageSnapshot> {
        let entry = self.entries.remove(&key)?;
        self.bytes -= entry.size;
        (entry.row.expires_at_ms > now_ms && same_scope(&entry.scope, scope)).then_some(entry.row)
    }

    fn remove(&mut self, key: Key) {
        if let Some(entry) = self.entries.remove(&key) {
            self.bytes -= entry.size;
        }
    }

    fn clear_guild(&mut self, guild: u64) {
        self.entries.retain(|(g, _, _), _| *g != guild);
        self.order.retain(|(g, _, _)| *g != guild);
        self.bytes = self.entries.values().map(|entry| entry.size).sum();
    }

    /// Releases every expired entry. Creation time can differ from insertion
    /// order, so `trim` alone can leave expired entries behind a live one.
    fn expire(&mut self, now_ms: i64) {
        self.entries
            .retain(|_, entry| entry.row.expires_at_ms > now_ms);
        let entries = &self.entries;
        self.order.retain(|key| entries.contains_key(key));
        self.bytes = self.entries.values().map(|entry| entry.size).sum();
    }

    /// Oldest observations leave first. Keys already taken or removed are
    /// dropped from the order lazily and compacted once they dominate it.
    fn trim(&mut self, now_ms: i64) {
        if self.order.len() > MAX_RECENT.saturating_mul(2) {
            let entries = &self.entries;
            self.order.retain(|key| entries.contains_key(key));
        }
        while let Some(&oldest) = self.order.front() {
            let over = self.entries.len() > MAX_RECENT || self.bytes > MAX_RECENT_BYTES;
            let expired = self
                .entries
                .get(&oldest)
                .is_none_or(|entry| entry.row.expires_at_ms <= now_ms);
            if !over && !expired {
                break;
            }
            self.order.pop_front();
            self.remove(oldest);
        }
    }
}

struct CoverageEpoch(AtomicU64);

impl CoverageEpoch {
    fn current(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }

    fn mark_gap(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ByteAdmission {
    Reserved,
    Refused,
}

enum CaptureWorker {
    Ready(mpsc::Receiver<Work>),
    Started,
}

impl CaptureWorker {
    fn start(&mut self) -> Option<mpsc::Receiver<Work>> {
        match std::mem::replace(self, Self::Started) {
            Self::Ready(receiver) => Some(receiver),
            Self::Started => None,
        }
    }
}

struct Work {
    scope: GuildDataScope,
    context: Context,
    body: Body,
    bytes: usize,
    epoch: u64,
}

enum Body {
    Create(SnapshotObservation),
    Edit(SnapshotObservation, Option<MessageReport>, bool),
    Delete {
        guild: GuildId,
        channel: ChannelId,
        ids: Vec<MessageId>,
        bulk: bool,
    },
}

impl Body {
    fn pending_bytes(&self) -> usize {
        // The edit report owns another copy of supplied text and metadata.
        const ITEM_OVERHEAD: usize = 1024;
        match self {
            Self::Create(observation) => ITEM_OVERHEAD.saturating_add(estimated_bytes(observation)),
            Self::Edit(observation, report, _) => ITEM_OVERHEAD
                .saturating_add(estimated_bytes(observation))
                .saturating_add(report.as_ref().map_or(0, |report| {
                    report
                        .vars
                        .iter()
                        .map(|(key, value)| {
                            key.len().saturating_add(value.len()).saturating_add(32)
                        })
                        .sum()
                })),
            Self::Delete { ids, .. } => ITEM_OVERHEAD.saturating_add(ids.len().saturating_mul(16)),
        }
    }
}

pub struct SnapshotIngress {
    sender: mpsc::Sender<Work>,
    receiver: Mutex<CaptureWorker>,
    bytes: AtomicUsize,
    epoch: CoverageEpoch,
    started_at_ms: i64,
    /// Loss is charged only to the physical store whose scoped work failed.
    /// Unknown gateway/catalog gaps still advance the observation epoch.
    lost_pending: Mutex<HashMap<String, u64>>,
    scopes: Mutex<HashMap<u64, GuildDataScope>>,
    known_humans: Mutex<KnownHumans>,
    deleted: Mutex<DeletedIdentities>,
    recent: Mutex<RecentMessages>,
    content_access: Mutex<HashSet<u64>>,
    contexts: Mutex<HashMap<u64, (u64, Context)>>,
    collectors: Mutex<HashMap<u64, u64>>,
    cleanup_paused: Mutex<HashSet<String>>,
    health_initialized: Mutex<HashSet<String>>,
    /// Report tasks are bounded separately from snapshot work.
    report_slots: Arc<tokio::sync::Semaphore>,
}

impl Default for SnapshotIngress {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel(MAX_PENDING);
        Self {
            sender,
            receiver: Mutex::new(CaptureWorker::Ready(receiver)),
            bytes: AtomicUsize::new(0),
            epoch: CoverageEpoch(AtomicU64::new(chrono::Utc::now().timestamp_micros() as u64)),
            started_at_ms: chrono::Utc::now().timestamp_millis(),
            lost_pending: Mutex::new(HashMap::new()),
            scopes: Mutex::new(HashMap::new()),
            known_humans: Mutex::new(KnownHumans::default()),
            deleted: Mutex::new(DeletedIdentities::default()),
            recent: Mutex::new(RecentMessages::default()),
            content_access: Mutex::new(HashSet::new()),
            contexts: Mutex::new(HashMap::new()),
            collectors: Mutex::new(HashMap::new()),
            cleanup_paused: Mutex::new(HashSet::new()),
            health_initialized: Mutex::new(HashSet::new()),
            report_slots: Arc::new(tokio::sync::Semaphore::new(32)),
        }
    }
}

impl SnapshotIngress {
    /// Called before the first await in READY/RESUME. Serenity invokes this
    /// callback on the shard runner before spawning either event handler.
    pub fn install_collector(&self, app: &Arc<BotState>, context: &Context) {
        let shard = context.shard_id.get() as u64;
        let generation = {
            let collectors = self.collectors.lock();
            collectors.get(&shard).copied().unwrap_or(0).wrapping_add(1)
        };
        let weak = Arc::downgrade(app);
        context
            .shard
            .add_collector(CollectorCallback(Box::new(move |event| {
                let Some(app) = weak.upgrade() else {
                    return false;
                };
                if app.snapshots.collectors.lock().get(&shard).copied() != Some(generation) {
                    return false;
                }
                if !matches!(
                    event,
                    Event::MessageCreate(_)
                        | Event::MessageUpdate(_)
                        | Event::MessageDelete(_)
                        | Event::MessageDeleteBulk(_)
                ) {
                    return true;
                }
                let context = app
                    .snapshots
                    .contexts
                    .lock()
                    .get(&shard)
                    .filter(|(current, _)| *current == generation)
                    .map(|(_, context)| context.clone());
                if let Some(context) = context {
                    app.snapshots.on_gateway_event(&app, context, event);
                } else {
                    return false;
                }
                true
            })));
        let mut collectors = self.collectors.lock();
        collectors.insert(shard, generation);
        self.contexts
            .lock()
            .insert(shard, (generation, context.clone()));
    }

    pub fn collector_active(&self, context: &Context) -> bool {
        let shard = context.shard_id.get() as u64;
        let collectors = self.collectors.lock();
        let contexts = self.contexts.lock();
        matches!(
            (collectors.get(&shard), contexts.get(&shard)),
            (Some(generation), Some((current, _))) if generation == current
        )
    }

    pub fn unhooked_delete(&self, guild: GuildId, channel: ChannelId, ids: &[MessageId]) {
        self.gap();
        self.mark_deleted(guild, channel, ids);
    }

    fn on_gateway_event(&self, app: &Arc<BotState>, context: Context, event: &Event) {
        match event {
            Event::MessageCreate(event) => self.create(app, context, &event.message),
            Event::MessageUpdate(event) => {
                let known = event.guild_id.and_then(|guild| {
                    crate::logging::known_message_author(
                        app,
                        guild.get(),
                        event.channel_id.get(),
                        event.id.get(),
                    )
                });
                let report = crate::logging::message_update_report(
                    event,
                    known,
                    Some(context.cache.current_user().id.get()),
                )
                .filter(|_| app.edit_reports.should_report(event));
                let queued = self.edit(app, context.clone(), event, report.clone());
                if !queued && let Some(report) = report {
                    submit_report(app, &context, report);
                }
            }
            Event::MessageDelete(event) => {
                if let Some(guild) = event.guild_id {
                    let queued = self.delete(
                        app,
                        context.clone(),
                        guild,
                        event.channel_id,
                        vec![event.message_id],
                        false,
                    );
                    if !queued
                        && let Some(report) = logging::observed_delete_report(
                            app,
                            guild,
                            event.channel_id,
                            event.message_id,
                            None,
                        )
                    {
                        submit_report(app, &context, report);
                    }
                }
            }
            Event::MessageDeleteBulk(event) => {
                if let Some(guild) = event.guild_id {
                    let ids = unique_ids(&event.ids);
                    let queued = self.delete(
                        app,
                        context.clone(),
                        guild,
                        event.channel_id,
                        ids.clone(),
                        true,
                    );
                    if !queued {
                        // No collector queue. Admit one report task when a slot
                        // is free. When it is not, observe inline: do not spawn
                        // a task that waits on configuration.
                        schedule_unqueued_bulk_delete(app, context, guild, event.channel_id, ids);
                    }
                }
            }
            _ => {}
        }
    }

    pub async fn maintain(&self, app: &BotState) -> std::time::Duration {
        self.maintain_at(app, None).await
    }

    async fn maintain_at(&self, app: &BotState, now_override: Option<i64>) -> std::time::Duration {
        self.recent
            .lock()
            .expire(now_override.unwrap_or_else(|| chrono::Utc::now().timestamp_millis()));
        let stores = match app.tenant.usage_stores().await {
            Ok(stores) => stores,
            Err(error) => {
                tracing::warn!("snapshot maintenance catalog unavailable: {error:#}");
                self.gap();
                return std::time::Duration::from_secs(30);
            }
        };
        let mut idle = true;
        let mut active = vec![true; stores.len()];
        // One batch per store per round. A known backlog gets prompt follow-up,
        // with a scheduler yield so gateway and scope refresh keep running.
        while active.iter().any(|pending| *pending) {
            for (index, entry) in stores.iter().enumerate() {
                if !active[index] {
                    continue;
                }
                let store_id = entry.registration.id().as_str();
                let Some(store) = entry.handle.as_ref() else {
                    self.cleanup_paused.lock().insert(store_id.to_owned());
                    idle = false;
                    active[index] = false;
                    self.gap_store(store_id);
                    continue;
                };
                let now = now_override.unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
                if !self.health_initialized.lock().contains(store_id) {
                    let lost = self.take_loss(store_id);
                    let initialized = store
                        .update_snapshot_health(SnapshotHealthUpdate {
                            epoch: self.epoch.current(),
                            state: SnapshotHealthState::Paused,
                            reason: Some("gateway_gap"),
                            observed_at_ms: None,
                            lost_delta: lost,
                            now_ms: now,
                        })
                        .await
                        .is_ok();
                    if initialized {
                        self.health_initialized.lock().insert(store_id.to_owned());
                    } else {
                        idle = false;
                        self.restore_loss(store_id, lost);
                        self.cleanup_paused.lock().insert(store_id.to_owned());
                    }
                }
                let expired = match store.expire_snapshots(100, now).await {
                    Ok(count) => count,
                    Err(error) => {
                        tracing::warn!(store_id, "snapshot cleanup unavailable: {error:#}");
                        self.cleanup_paused.lock().insert(store_id.to_owned());
                        idle = false;
                        active[index] = false;
                        self.gap_store(store_id);
                        self.write_maintenance_pause(store_id, store, "cleanup_failed", now)
                            .await;
                        continue;
                    }
                };
                let usage = match store.measure_usage(false).await {
                    Ok(usage) => usage,
                    Err(error) => {
                        tracing::warn!(
                            store_id,
                            "snapshot usage measurement unavailable: {error:#}"
                        );
                        self.cleanup_paused.lock().insert(store_id.to_owned());
                        idle = false;
                        active[index] = false;
                        self.gap_store(store_id);
                        self.write_maintenance_pause(store_id, store, "measurement_failed", now)
                            .await;
                        continue;
                    }
                };
                let evicted = if pressure_cleanup_due(usage.allocated_bytes, usage.snapshot_count) {
                    match store.evict_snapshots(100, now).await {
                        Ok(count) => count,
                        Err(error) => {
                            tracing::warn!(
                                store_id,
                                "snapshot pressure cleanup unavailable: {error:#}"
                            );
                            self.cleanup_paused.lock().insert(store_id.to_owned());
                            idle = false;
                            active[index] = false;
                            self.gap_store(store_id);
                            self.write_maintenance_pause(store_id, store, "cleanup_failed", now)
                                .await;
                            continue;
                        }
                    }
                } else {
                    0
                };
                if usage.allocated_bytes < jmodguard::capacity::SNAPSHOT_PAUSE_BYTES
                    && !app.db.capacity_runtime_status().startup_deferred
                    && self.health_initialized.lock().contains(store_id)
                {
                    self.cleanup_paused.lock().remove(store_id);
                }
                idle &= expired == 0 && evicted == 0;
                active[index] = expired == 100 || evicted == 100;
                let verified_content = {
                    let access = self.content_access.lock();
                    self.scopes.lock().iter().any(|(guild, candidate)| {
                        candidate.placement().store_id().as_str() == store_id
                            && access.contains(guild)
                    })
                };
                let status = if !app.cfg.bot.message_capture_enabled {
                    (SnapshotHealthState::Disabled, Some("disabled"))
                } else if app.db.capacity_runtime_status().startup_deferred {
                    (SnapshotHealthState::Paused, Some("startup_deferred"))
                } else if usage.allocated_bytes >= jmodguard::capacity::SNAPSHOT_PAUSE_BYTES
                    || usage
                        .snapshot_health
                        .as_ref()
                        .is_some_and(|health| health.capacity_paused)
                {
                    (SnapshotHealthState::Paused, Some("capacity"))
                } else if self.cleanup_paused.lock().contains(store_id) {
                    (SnapshotHealthState::Paused, Some("cleanup_failed"))
                } else if !verified_content {
                    (SnapshotHealthState::Unavailable, Some("content_access"))
                } else {
                    (SnapshotHealthState::Active, None)
                };
                let (state, reason) = status;
                let lost = self.take_loss(store_id);
                if store
                    .update_snapshot_health(SnapshotHealthUpdate {
                        epoch: self.epoch.current(),
                        state,
                        reason,
                        observed_at_ms: None,
                        lost_delta: lost,
                        now_ms: now,
                    })
                    .await
                    .is_err()
                {
                    idle = false;
                    self.restore_loss(store_id, lost);
                }
            }
            tokio::task::yield_now().await;
        }
        std::time::Duration::from_secs(if idle { 120 } else { 30 })
    }

    async fn write_maintenance_pause(
        &self,
        store_id: &str,
        store: &GuildDataStore,
        reason: &'static str,
        now_ms: i64,
    ) {
        let lost = self.take_loss(store_id);
        if store
            .update_snapshot_health(SnapshotHealthUpdate {
                epoch: self.epoch.current(),
                state: SnapshotHealthState::Paused,
                reason: Some(reason),
                observed_at_ms: None,
                lost_delta: lost,
                now_ms,
            })
            .await
            .is_err()
        {
            self.restore_loss(store_id, lost);
        }
    }

    pub async fn refresh_scope(&self, app: &BotState, guild: u64) {
        let Ok(guild_id) = i64::try_from(guild) else {
            self.gap();
            return;
        };
        let enrollment = match crate::db::guild_enrollment(&app.db, guild_id).await {
            Ok(Some(enrollment)) => enrollment,
            Ok(None) => {
                let had_scope = self.scopes.lock().remove(&guild).is_some();
                if had_scope {
                    self.gap();
                }
                return;
            }
            Err(error) => {
                tracing::warn!(
                    guild_id,
                    "snapshot scope refresh enrollment unavailable: {error:#}"
                );
                return;
            }
        };
        match app.tenant.scope(enrollment).await {
            Ok(scope) => {
                self.scopes.lock().insert(guild, scope);
            }
            Err(error) => {
                tracing::warn!(
                    guild_id,
                    "snapshot scope refresh tenant scope unavailable: {error:#}"
                );
            }
        }
    }

    pub fn clear_guild(&self, guild: u64) {
        self.scopes.lock().remove(&guild);
        {
            // `mark_deleted` takes this lock while holding `deleted`, and
            // `remember_recent` takes `deleted` while holding `recent`. Release
            // it before `recent` so the three cannot form a cycle.
            let mut known = self.known_humans.lock();
            known.entries.retain(|(g, _, _)| *g != guild);
            known.order.retain(|((g, _, _), _)| *g != guild);
        }
        self.content_access.lock().remove(&guild);
        self.recent.lock().clear_guild(guild);
        self.gap();
    }

    pub fn gap(&self) {
        self.epoch.mark_gap();
    }

    fn gap_scope(&self, scope: &GuildDataScope) {
        self.gap_scope_messages(scope, 1);
    }

    /// One coverage discontinuity for a failed shared write, with `messages`
    /// added to the store's pending loss. A single message stays on `gap_scope`.
    fn gap_scope_messages(&self, scope: &GuildDataScope, messages: u64) {
        if messages == 0 {
            return;
        }
        if messages == 1 {
            self.gap_store(scope.placement().store_id().as_str());
            return;
        }
        self.gap();
        let key = scope.placement().store_id().as_str().to_owned();
        let mut pending = self.lost_pending.lock();
        let count = pending.entry(key).or_default();
        *count = count.saturating_add(messages);
    }

    fn gap_store(&self, store_id: &str) {
        self.gap();
        let key = store_id.to_owned();
        let mut pending = self.lost_pending.lock();
        let count = pending.entry(key).or_default();
        *count = count.saturating_add(1);
    }

    fn take_loss(&self, store_id: &str) -> u64 {
        self.lost_pending.lock().remove(store_id).unwrap_or(0)
    }

    fn restore_loss(&self, store_id: &str, lost: u64) {
        if lost == 0 {
            return;
        }
        let mut pending = self.lost_pending.lock();
        let count = pending.entry(store_id.to_owned()).or_default();
        *count = count.saturating_add(lost);
    }

    fn capture_paused(&self, scope: &GuildDataScope) -> bool {
        self.cleanup_paused
            .lock()
            .contains(scope.placement().store_id().as_str())
    }

    fn scope(&self, guild: u64) -> Option<GuildDataScope> {
        self.scopes.lock().get(&guild).cloned()
    }

    fn remember_human(&self, key: Key) {
        let mut known = self.known_humans.lock();
        if known.entries.contains(&key) {
            return;
        }
        while known
            .order
            .front()
            .is_some_and(|(_, at)| at.elapsed() >= Duration::from_secs(86400))
            || known.order.len() >= MAX_KNOWN
        {
            let Some((oldest, at)) = known.order.pop_front() else {
                break;
            };
            if known.entries.remove(&oldest) && at.elapsed() < Duration::from_secs(86400) {
                self.gap();
            }
        }
        known.entries.insert(key);
        known.order.push_back((key, Instant::now()));
    }

    fn known_human(&self, key: Key) -> bool {
        self.known_humans.lock().entries.contains(&key)
    }

    fn deleted(&self, key: Key) -> bool {
        let deleted = self.deleted.lock();
        deleted.knowledge(key).blocks_capture()
    }

    pub(crate) fn deletion_recorded(
        &self,
        guild: GuildId,
        channel: ChannelId,
        id: MessageId,
    ) -> bool {
        self.deleted((guild.get(), channel.get(), id.get()))
    }

    pub(crate) fn mark_deleted(&self, guild: GuildId, channel: ChannelId, ids: &[MessageId]) {
        let mut deleted = self.deleted.lock();
        for id in ids {
            let key = (guild.get(), channel.get(), id.get());
            deleted.bloom.insert(key);
            if let std::collections::hash_map::Entry::Vacant(entry) = deleted.exact.entry(key) {
                let at = Instant::now();
                entry.insert(at);
                deleted.order.push_back((key, at));
            }
        }
        while deleted
            .order
            .front()
            .is_some_and(|(_, at)| at.elapsed() >= TOMBSTONE_KEEP)
            || deleted.order.len() > MAX_KNOWN
        {
            let Some((key, at)) = deleted.order.pop_front() else {
                break;
            };
            if deleted
                .exact
                .get(&key)
                .is_some_and(|current| *current == at)
            {
                deleted.exact.remove(&key);
            }
        }
        let mut known = self.known_humans.lock();
        for id in ids {
            known
                .entries
                .remove(&(guild.get(), channel.get(), id.get()));
        }
        drop(known);
        drop(deleted);
        // `remember_recent` checks deletion while holding this lock, so a
        // concurrent observation cannot leave a deleted message cached.
        let mut recent = self.recent.lock();
        for id in ids {
            recent.remove((guild.get(), channel.get(), id.get()));
        }
    }

    fn remember_recent(
        &self,
        scope: &GuildDataScope,
        observation: &SnapshotObservation,
        allow_insert: bool,
    ) -> Option<MessageSnapshot> {
        let guild = scope.guild_id() as u64;
        let key = (
            guild,
            observation.channel_id as u64,
            observation.message_id as u64,
        );
        let mut recent = self.recent.lock();
        // `clear_guild` removes the captured scope before it clears this cache,
        // and scope refresh replaces it. Checking it under this lock keeps work
        // queued under an old membership from restoring or reading old rows.
        if !self.scope_is_current(scope) || self.deleted.lock().knowledge(key).blocks_capture() {
            return None;
        }
        recent.observe(
            scope,
            observation,
            allow_insert,
            chrono::Utc::now().timestamp_millis(),
        )
    }

    fn scope_is_current(&self, scope: &GuildDataScope) -> bool {
        self.scopes
            .lock()
            .get(&(scope.guild_id() as u64))
            .is_some_and(|current| same_scope(current, scope))
    }

    /// Always removes the cached copy; it is returned only while the deleting
    /// work's scope is still the captured one.
    fn take_recent(
        &self,
        scope: &GuildDataScope,
        channel: ChannelId,
        id: MessageId,
    ) -> Option<MessageSnapshot> {
        let mut recent = self.recent.lock();
        let row = recent.take(
            (scope.guild_id() as u64, channel.get(), id.get()),
            scope,
            chrono::Utc::now().timestamp_millis(),
        )?;
        self.scope_is_current(scope).then_some(row)
    }

    fn enqueue(
        &self,
        app: &Arc<BotState>,
        context: Context,
        scope: GuildDataScope,
        body: Body,
    ) -> bool {
        let bytes = body.pending_bytes();
        if bytes > MAX_PENDING_BYTES {
            self.gap_scope(&scope);
            return false;
        }
        if self.reserve_bytes(bytes) == ByteAdmission::Refused {
            self.gap_scope(&scope);
            return false;
        }
        self.start_worker(app);
        let item = Work {
            scope: scope.clone(),
            context,
            body,
            bytes,
            epoch: self.epoch.current(),
        };
        if self.sender.try_send(item).is_err() {
            self.bytes.fetch_sub(bytes, Ordering::AcqRel);
            self.gap_scope(&scope);
            return false;
        }
        true
    }

    fn reserve_bytes(&self, bytes: usize) -> ByteAdmission {
        if self
            .bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                old.checked_add(bytes)
                    .filter(|next| *next <= MAX_PENDING_BYTES)
            })
            .is_ok()
        {
            ByteAdmission::Reserved
        } else {
            ByteAdmission::Refused
        }
    }

    fn start_worker(&self, app: &Arc<BotState>) {
        let receiver = self.receiver.lock().start();
        if let Some(receiver) = receiver {
            let app = Arc::clone(app);
            tokio::spawn(async move { run(app, receiver).await });
        }
    }

    pub fn create(&self, app: &Arc<BotState>, context: Context, message: &Message) {
        if !app.cfg.bot.message_capture_enabled
            || app.db.capacity_runtime_status().startup_deferred
            || message.author.bot
            || message.webhook_id.is_some()
        {
            return;
        }
        let Some(guild) = message.guild_id else {
            return;
        };
        if message.id.created_at().timestamp_millis() < self.started_at_ms {
            self.gap();
            return;
        }
        // A nonempty ordinary human message that does not mention this bot is
        // our observable proof that the privileged content field is delivered.
        // Discord can supply mention messages without the content intent.
        let bot_id = context.cache.current_user().id;
        if !message.content.is_empty() && !message.mentions.iter().any(|user| user.id == bot_id) {
            self.content_access.lock().insert(guild.get());
        }
        if !self.content_access.lock().contains(&guild.get()) {
            self.gap();
            return;
        }
        let Some(scope) = self.scope(guild.get()) else {
            self.gap();
            return;
        };
        let key = (guild.get(), message.channel_id.get(), message.id.get());
        self.remember_human(key);
        let observation = SnapshotObservation {
            channel_id: message.channel_id.get() as i64,
            message_id: message.id.get() as i64,
            created_at_ms: message.id.created_at().timestamp_millis(),
            observed_at_ms: chrono::Utc::now().timestamp_millis(),
            edit_at_ms: None,
            coverage_epoch: 0,
            fields: fields(
                Some(message.content.as_str()),
                Some(message.attachments.as_slice()),
                Some(message.author.id.get()),
                Some(message.author.name.as_str()),
            ),
        };
        let _ = self.enqueue(app, context, scope, Body::Create(observation));
    }

    pub(crate) fn edit(
        &self,
        app: &Arc<BotState>,
        context: Context,
        event: &MessageUpdateEvent,
        report: Option<MessageReport>,
    ) -> bool {
        if !app.cfg.bot.message_capture_enabled
            || app.db.capacity_runtime_status().startup_deferred
            || event.edited_timestamp.is_none()
            || (event.content.is_none() && event.attachments.is_none())
            || event.author.as_ref().is_some_and(|author| author.bot)
            || event.webhook_id.flatten().is_some()
        {
            return false;
        }
        let Some(guild) = event.guild_id else {
            return false;
        };
        if !self.content_access.lock().contains(&guild.get()) {
            self.gap();
            return false;
        }
        let key = (guild.get(), event.channel_id.get(), event.id.get());
        let known_human = self.known_human(key);
        let explicit_non_webhook = event.webhook_id == Some(None);
        let allow_insert = event.author.as_ref().is_some_and(|author| !author.bot)
            && (known_human || explicit_non_webhook);
        if allow_insert {
            self.remember_human(key);
        }
        let Some(scope) = self.scope(guild.get()) else {
            self.gap();
            return false;
        };
        let observation = SnapshotObservation {
            channel_id: event.channel_id.get() as i64,
            message_id: event.id.get() as i64,
            created_at_ms: event.id.created_at().timestamp_millis(),
            observed_at_ms: chrono::Utc::now().timestamp_millis(),
            edit_at_ms: event.edited_timestamp.map(|at| at.timestamp_millis()),
            coverage_epoch: 0,
            fields: fields(
                event.content.as_deref(),
                event.attachments.as_deref(),
                event.author.as_ref().map(|author| author.id.get()),
                event.author.as_ref().map(|author| author.name.as_str()),
            ),
        };
        self.enqueue(
            app,
            context,
            scope,
            Body::Edit(observation, report, allow_insert),
        )
    }

    pub fn delete(
        &self,
        app: &Arc<BotState>,
        context: Context,
        guild: GuildId,
        channel: ChannelId,
        ids: Vec<MessageId>,
        bulk: bool,
    ) -> bool {
        let ids = unique_ids(&ids);
        let Some(scope) = self.scope(guild.get()) else {
            self.gap();
            self.mark_deleted(guild, channel, &ids);
            return false;
        };
        let queued = self.enqueue(
            app,
            context,
            scope,
            Body::Delete {
                guild,
                channel,
                ids: ids.clone(),
                bulk,
            },
        );
        if !queued {
            self.mark_deleted(guild, channel, &ids);
        }
        queued
    }
}

fn pressure_cleanup_due(allocated_bytes: u64, snapshot_count: Option<u64>) -> bool {
    allocated_bytes >= jmodguard::capacity::PRESSURE_BYTES && snapshot_count != Some(0)
}

fn clip_bytes(value: &str, limit: usize) -> (String, bool) {
    if value.len() <= limit {
        return (value.to_string(), false);
    }
    let mut cut = limit;
    while !value.is_char_boundary(cut) {
        cut -= 1;
    }
    (value[..cut].to_string(), true)
}

fn fields(
    content: Option<&str>,
    attachments: Option<&[serenity::model::channel::Attachment]>,
    author_id: Option<u64>,
    author_name: Option<&str>,
) -> SnapshotFields {
    let (content, content_truncated, content_original_bytes) = match content {
        Some(value) => {
            let (clipped, truncated) = clip_bytes(value, MAX_TEXT_BYTES);
            (Some(clipped), truncated, Some(value.len() as i64))
        }
        None => (None, false, None),
    };
    let (attachments, attachments_truncated, attachments_omitted) = match attachments {
        Some(items) => {
            let mut out = Vec::new();
            let mut bytes = 0usize;
            let mut metadata_clipped = false;
            for attachment in items {
                let (filename, filename_clipped) = clip_bytes(&attachment.filename, 256);
                let (url, url_clipped) = clip_bytes(&attachment.url, 1024);
                let (content_type, type_clipped) = match attachment.content_type.as_deref() {
                    Some(value) => {
                        let (clipped, truncated) = clip_bytes(value, 128);
                        (Some(clipped), truncated)
                    }
                    None => (None, false),
                };
                let entry = SnapshotAttachment {
                    id: attachment.id.get() as i64,
                    filename,
                    size: u64::from(attachment.size),
                    content_type,
                    url,
                };
                let encoded =
                    serde_json::to_vec(&entry).map_or(MAX_ATTACHMENTS_BYTES + 1, |v| v.len());
                if bytes.saturating_add(encoded) > MAX_ATTACHMENTS_BYTES {
                    break;
                }
                bytes += encoded;
                metadata_clipped |= filename_clipped || url_clipped || type_clipped;
                out.push(entry);
            }
            let omitted = items.len().saturating_sub(out.len());
            (
                Some(out),
                metadata_clipped || omitted > 0,
                Some(omitted as i64),
            )
        }
        None => (None, false, None),
    };
    SnapshotFields {
        content,
        content_truncated,
        content_original_bytes,
        attachments,
        attachments_truncated,
        attachments_omitted,
        author_id: author_id.map(|id| id as i64),
        author_name: author_name.map(|name| clip_bytes(name, 256).0),
    }
}

fn estimated_bytes(observation: &SnapshotObservation) -> usize {
    fields_bytes(&observation.fields)
}

fn fields_bytes(fields: &SnapshotFields) -> usize {
    256usize
        .saturating_add(fields.content.as_ref().map_or(0, String::len))
        .saturating_add(fields.author_name.as_ref().map_or(0, String::len))
        .saturating_add(fields.attachments.as_ref().map_or(0, |items| {
            items
                .iter()
                .map(|item| {
                    item.filename.len()
                        + item.url.len()
                        + item.content_type.as_ref().map_or(0, String::len)
                        + 64
                })
                .sum()
        }))
}

fn capture_enabled(config: &crate::state::CachedGuildConfig) -> bool {
    [
        "message_edit",
        "message_delete",
        "message_bulk_delete",
        "automod_delete",
    ]
    .iter()
    .any(|event| config.log_channels.contains_key(*event))
}

// Admission stays next to the write for both a drain and each pressure-split
// item. Recheck cleanup pause and deletion identity after the configuration
// await: a deletion can arrive while a preceding split item finishes eviction.
async fn observe_configured_creates(
    app: &BotState,
    store: &GuildDataStore,
    scope: &GuildDataScope,
    observations: Vec<SnapshotObservation>,
) -> anyhow::Result<Vec<Option<SnapshotOutcome>>> {
    let mut outcomes = vec![None; observations.len()];
    let mut admitted = Vec::with_capacity(observations.len());
    let mut indices = Vec::with_capacity(observations.len());
    for (index, observation) in observations.into_iter().enumerate() {
        let guild = scope.guild_id() as u64;
        match app.configs.get(&app.db, guild).await {
            Ok(config) if capture_enabled(&config) => {
                // The memory copy follows this per-row decision, so a drained
                // create is not cached under an earlier item's configuration.
                app.snapshots.remember_recent(scope, &observation, true);
                let key = (
                    scope.guild_id() as u64,
                    observation.channel_id as u64,
                    observation.message_id as u64,
                );
                if app.snapshots.capture_paused(scope) || app.snapshots.deleted(key) {
                    app.snapshots.gap_scope(scope);
                    continue;
                }
                indices.push(index);
                admitted.push(observation);
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!("message snapshot config unavailable: {error:#}");
                app.snapshots.gap_scope(scope);
                if app
                    .configs
                    .last_known(guild)
                    .is_some_and(|cfg| capture_enabled(&cfg))
                {
                    app.snapshots.remember_recent(scope, &observation, true);
                }
            }
        }
    }
    if admitted.is_empty() {
        return Ok(outcomes);
    }
    // Configuration loads are the awaits in this function. Yield once so a
    // cleanup pause or deletion that arrived during those loads is visible,
    // then re-check every row that is about to share one write.
    tokio::task::yield_now().await;
    if app.snapshots.capture_paused(scope) {
        for _ in &admitted {
            app.snapshots.gap_scope(scope);
        }
        return Ok(outcomes);
    }
    let mut ready = Vec::with_capacity(admitted.len());
    let mut ready_indices = Vec::with_capacity(admitted.len());
    for (index, observation) in indices.into_iter().zip(admitted) {
        let key = (
            scope.guild_id() as u64,
            observation.channel_id as u64,
            observation.message_id as u64,
        );
        if app.snapshots.deleted(key) {
            app.snapshots.gap_scope(scope);
            continue;
        }
        ready_indices.push(index);
        ready.push(observation);
    }
    if ready.is_empty() {
        return Ok(outcomes);
    }
    let stored = match store.observe_snapshots(scope.clone(), ready).await {
        Ok(stored) => stored,
        Err(error) => {
            // Capacity splits roll back and are retried one row at a time.
            // Any other failure is one store call: one discontinuity, and one
            // lost message per row that call was responsible for.
            if !matches!(
                error.downcast_ref::<SnapshotError>(),
                Some(SnapshotError::BatchCapacity)
            ) {
                app.snapshots
                    .gap_scope_messages(scope, ready_indices.len() as u64);
            }
            return Err(error);
        }
    };
    for (index, outcome) in ready_indices.into_iter().zip(stored) {
        outcomes[index] = Some(outcome);
    }
    Ok(outcomes)
}

async fn run(app: Arc<BotState>, mut receiver: mpsc::Receiver<Work>) {
    let mut health: HashMap<String, (Instant, SnapshotHealthState, Option<&'static str>)> =
        HashMap::new();
    let mut pending = None;
    while let Some(mut work) = match pending.take() {
        Some(work) => Some(work),
        None => receiver.recv().await,
    } {
        app.snapshots.bytes.fetch_sub(work.bytes, Ordering::AcqRel);
        let guild = work.scope.guild_id() as u64;
        let (config, config_load_failed) = match app.configs.get(&app.db, guild).await {
            Ok(cfg) => (Some(cfg), false),
            Err(error) => {
                tracing::warn!("message snapshot config unavailable: {error:#}");
                app.snapshots.gap_scope(&work.scope);
                (None, true)
            }
        };
        let configured = config.as_ref().is_some_and(|cfg| capture_enabled(cfg));
        // The memory cache keeps the last loaded eligibility while configuration
        // reads fail, so a database stall does not also empty it.
        let cache_configured = configured
            || (config_load_failed
                && app
                    .configs
                    .last_known(guild)
                    .is_some_and(|cfg| capture_enabled(&cfg)));
        let cached_before = match &work.body {
            Body::Create(observation) if cache_configured => {
                app.snapshots
                    .remember_recent(&work.scope, observation, true);
                None
            }
            Body::Edit(observation, _, allow_insert) if cache_configured => app
                .snapshots
                .remember_recent(&work.scope, observation, *allow_insert),
            _ => None,
        };
        let store = match app.tenant.store(&work.scope).await {
            Ok(store) => store,
            Err(error) => {
                tracing::warn!("message snapshot store unavailable: {error:#}");
                // A failed configuration read already charged this item.
                if !config_load_failed {
                    app.snapshots.gap_scope(&work.scope);
                }
                match work.body {
                    Body::Edit(_, Some(mut report), _) => {
                        if cached_before.is_some() {
                            logging::apply_retained_edit(&mut report, cached_before.as_ref(), None);
                        } else {
                            report
                                .vars
                                .insert("coverage".into(), "snapshot storage unavailable".into());
                        }
                        submit_report(&app, &work.context, report);
                    }
                    Body::Delete {
                        guild,
                        channel,
                        ids,
                        bulk: false,
                    } => {
                        let cached = ids
                            .first()
                            .and_then(|id| app.snapshots.take_recent(&work.scope, channel, *id));
                        app.snapshots.mark_deleted(guild, channel, &ids);
                        if let Some(id) = ids.first()
                            && let Some(mut report) = logging::observed_delete_report(
                                &app,
                                guild,
                                channel,
                                *id,
                                cached.as_ref(),
                            )
                        {
                            if cached.is_none() {
                                report.vars.insert(
                                    "coverage".into(),
                                    "snapshot storage unavailable".into(),
                                );
                            }
                            submit_report(&app, &work.context, report);
                        }
                    }
                    Body::Delete {
                        guild,
                        channel,
                        ids,
                        bulk: true,
                    } => {
                        let cached = ids
                            .iter()
                            .map(|id| app.snapshots.take_recent(&work.scope, channel, *id))
                            .collect();
                        app.snapshots.mark_deleted(guild, channel, &ids);
                        submit_peeled_bulk_deletes(
                            &app,
                            &work.context,
                            guild,
                            channel,
                            ids,
                            cached,
                        )
                        .await;
                    }
                    _ => {}
                }
                continue;
            }
        };
        match &mut work.body {
            Body::Create(observation) => {
                if !configured {
                    continue;
                }
                if app.snapshots.capture_paused(&work.scope) {
                    app.snapshots.gap_scope(&work.scope);
                    continue;
                }
                let key = (
                    guild,
                    observation.channel_id as u64,
                    observation.message_id as u64,
                );
                if app.snapshots.deleted(key) {
                    app.snapshots.gap_scope(&work.scope);
                    continue;
                }
                observation.coverage_epoch = work.epoch;
                let mut observations = vec![observation.clone()];
                for _ in 1..SNAPSHOT_CAPTURE_BATCH {
                    let Ok(mut next) = receiver.try_recv() else {
                        break;
                    };
                    // Stop at any event/scope boundary. In particular, never
                    // move a create across a queued edit or deletion.
                    let same_scope = next.scope.placement() == work.scope.placement()
                        && next.scope.enrollment() == work.scope.enrollment();
                    if !same_scope || !matches!(next.body, Body::Create(_)) {
                        pending = Some(next);
                        break;
                    }
                    app.snapshots.bytes.fetch_sub(next.bytes, Ordering::AcqRel);
                    let Body::Create(next_observation) = &mut next.body else {
                        unreachable!()
                    };
                    let key = (
                        guild,
                        next_observation.channel_id as u64,
                        next_observation.message_id as u64,
                    );
                    if app.snapshots.deleted(key) {
                        app.snapshots.gap_scope(&next.scope);
                        continue;
                    }
                    next_observation.coverage_epoch = next.epoch;
                    observations.push(next_observation.clone());
                }
                let count = observations.len();
                let outcomes =
                    observe_configured_creates(&app, &store, &work.scope, observations.clone())
                        .await;
                let split = matches!(
                    outcomes
                        .as_ref()
                        .err()
                        .and_then(|e| e.downcast_ref::<SnapshotError>()),
                    Some(SnapshotError::BatchCapacity)
                );
                // A shared store error is one call, not one outcome per index.
                // `observe_configured_creates` already recorded its loss.
                if !split && let Err(error) = &outcomes {
                    tracing::warn!("message snapshot create unavailable: {error:#}");
                    update_health(
                        &app,
                        &store,
                        &work.scope,
                        &mut health,
                        SnapshotHealthState::Unavailable,
                        Some("database"),
                    )
                    .await;
                } else {
                    for index in 0..count {
                        // A split batch made no writes. Preserve the complete
                        // per-event sequence, including eviction and cleanup pause.
                        let single;
                        let outcome = if split {
                            if app.snapshots.capture_paused(&work.scope) {
                                app.snapshots.gap_scope(&work.scope);
                                continue;
                            }
                            single = observe_configured_creates(
                                &app,
                                &store,
                                &work.scope,
                                vec![observations[index].clone()],
                            )
                            .await;
                            single.as_ref().map(|outcomes| outcomes[0])
                        } else {
                            outcomes.as_ref().map(|outcomes| outcomes[index])
                        };
                        match outcome {
                            Ok(Some(SnapshotOutcome::Stored)) => {
                                update_health(
                                    &app,
                                    &store,
                                    &work.scope,
                                    &mut health,
                                    SnapshotHealthState::Active,
                                    None,
                                )
                                .await;
                            }
                            Ok(Some(SnapshotOutcome::Duplicate | SnapshotOutcome::Stale)) => {}
                            Ok(Some(SnapshotOutcome::CapacityRefused)) => {
                                app.snapshots.gap_scope(&work.scope);
                                update_health(
                                    &app,
                                    &store,
                                    &work.scope,
                                    &mut health,
                                    SnapshotHealthState::Paused,
                                    Some("capacity"),
                                )
                                .await;
                                if let Err(error) = store
                                    .evict_snapshots(100, chrono::Utc::now().timestamp_millis())
                                    .await
                                {
                                    tracing::warn!(
                                        "snapshot pressure eviction unavailable: {error:#}"
                                    );
                                    app.snapshots.cleanup_paused.lock().insert(
                                        work.scope.placement().store_id().as_str().to_owned(),
                                    );
                                    update_health(
                                        &app,
                                        &store,
                                        &work.scope,
                                        &mut health,
                                        SnapshotHealthState::Paused,
                                        Some("cleanup_failed"),
                                    )
                                    .await;
                                }
                                tokio::task::yield_now().await;
                            }
                            Ok(None) => {}
                            Ok(Some(_)) => app.snapshots.gap(),
                            Err(error) => {
                                tracing::warn!("message snapshot create unavailable: {error:#}");
                                update_health(
                                    &app,
                                    &store,
                                    &work.scope,
                                    &mut health,
                                    SnapshotHealthState::Unavailable,
                                    Some("database"),
                                )
                                .await;
                            }
                        }
                    }
                }
            }
            Body::Edit(observation, report, allow_insert) => {
                if app.snapshots.capture_paused(&work.scope) {
                    app.snapshots.gap_scope(&work.scope);
                    if let Some(mut report) = report.take() {
                        // The pause stops durable growth only; an eligible prior
                        // this process already observed still describes the edit.
                        let coverage = if cached_before.is_some() {
                            logging::apply_retained_edit(&mut report, cached_before.as_ref(), None);
                            "from memory cache; capture paused after cleanup failure"
                        } else {
                            "capture paused after cleanup failure"
                        };
                        report.vars.insert("coverage".into(), coverage.into());
                        submit_report(&app, &work.context, report);
                    }
                    continue;
                }
                let key = (
                    guild,
                    observation.channel_id as u64,
                    observation.message_id as u64,
                );
                if app.snapshots.deleted(key) {
                    app.snapshots.gap_scope(&work.scope);
                    if let Some(mut report) = report.take() {
                        report
                            .vars
                            .insert("before".into(), "unavailable (capture gap)".into());
                        report.vars.insert(
                            "coverage".into(),
                            "deletion identity saturation; capture unavailable".into(),
                        );
                        submit_report(&app, &work.context, report);
                    }
                    continue;
                }
                if !configured {
                    if config_load_failed && let Some(mut report) = report.take() {
                        if cached_before.is_some() {
                            logging::apply_retained_edit(&mut report, cached_before.as_ref(), None);
                        } else {
                            report
                                .vars
                                .insert("coverage".into(), "configuration unavailable".into());
                        }
                        submit_report(&app, &work.context, report);
                    }
                    continue;
                }
                observation.coverage_epoch = work.epoch;
                let result = if *allow_insert {
                    store
                        .apply_snapshot_edit(work.scope.clone(), observation.clone())
                        .await
                } else {
                    store
                        .apply_snapshot_edit_existing(work.scope.clone(), observation.clone())
                        .await
                };
                match result {
                    Ok(transition) => {
                        if matches!(
                            transition.outcome,
                            SnapshotOutcome::Duplicate | SnapshotOutcome::Stale
                        ) {
                            continue;
                        }
                        if !matches!(transition.outcome, SnapshotOutcome::Stored) {
                            app.snapshots.gap_scope(&work.scope);
                        }
                        match transition.outcome {
                            SnapshotOutcome::Stored => {
                                update_health(
                                    &app,
                                    &store,
                                    &work.scope,
                                    &mut health,
                                    SnapshotHealthState::Active,
                                    None,
                                )
                                .await
                            }
                            SnapshotOutcome::CapacityRefused => {
                                update_health(
                                    &app,
                                    &store,
                                    &work.scope,
                                    &mut health,
                                    SnapshotHealthState::Paused,
                                    Some("capacity"),
                                )
                                .await;
                                if let Err(error) = store
                                    .evict_snapshots(100, chrono::Utc::now().timestamp_millis())
                                    .await
                                {
                                    tracing::warn!(
                                        "snapshot pressure eviction unavailable: {error:#}"
                                    );
                                    app.snapshots.cleanup_paused.lock().insert(
                                        work.scope.placement().store_id().as_str().to_owned(),
                                    );
                                    update_health(
                                        &app,
                                        &store,
                                        &work.scope,
                                        &mut health,
                                        SnapshotHealthState::Paused,
                                        Some("cleanup_failed"),
                                    )
                                    .await;
                                }
                                tokio::task::yield_now().await;
                            }
                            _ => {}
                        }
                        if let Some(mut report) = report.take() {
                            apply_transition_report(&mut report, &transition);
                            apply_cached_edit_prior(
                                &mut report,
                                &transition,
                                cached_before.as_ref(),
                            );
                            submit_report(&app, &work.context, report);
                        }
                    }
                    Err(error) => {
                        tracing::warn!("message snapshot edit unavailable: {error:#}");
                        app.snapshots.gap_scope(&work.scope);
                        update_health(
                            &app,
                            &store,
                            &work.scope,
                            &mut health,
                            SnapshotHealthState::Unavailable,
                            Some("database"),
                        )
                        .await;
                        if let Some(mut report) = report.take() {
                            if cached_before.is_some() {
                                logging::apply_retained_edit(
                                    &mut report,
                                    cached_before.as_ref(),
                                    None,
                                );
                            } else {
                                report.vars.insert(
                                    "coverage".into(),
                                    "snapshot storage unavailable".into(),
                                );
                            }
                            submit_report(&app, &work.context, report);
                        }
                    }
                }
            }
            Body::Delete {
                guild,
                channel,
                ids,
                bulk,
            } => {
                if *bulk {
                    ids.retain(|id| !app.snapshots.deletion_recorded(*guild, *channel, *id));
                    if ids.is_empty() {
                        continue;
                    }
                }
                if !*bulk
                    && ids
                        .first()
                        .is_some_and(|id| app.snapshots.deletion_recorded(*guild, *channel, *id))
                {
                    continue;
                }
                let now = chrono::Utc::now().timestamp_millis();
                let mut transcript = logging::BulkTranscript::new(ids.len());
                let mut peeled_reports = Vec::new();
                let ignored_authors = bulk_ignored_authors(config.as_deref());
                let automod_individual = config
                    .as_ref()
                    .is_some_and(|cfg| logging::channel_for(cfg, "automod_delete").is_some());
                // Always consume the cached copy; it fills in only for a missing
                // or unreadable stored row.
                let cached: Vec<_> = ids
                    .iter()
                    .map(|id| app.snapshots.take_recent(&work.scope, *channel, *id))
                    .collect();
                let mut single = None;
                let mut single_unknown = false;
                for (chunk, cached_chunk) in ids.chunks(100).zip(cached.chunks(100)) {
                    let values = chunk.iter().map(|id| id.get() as i64).collect();
                    match store
                        .take_snapshots_at_epoch(
                            work.scope.clone(),
                            channel.get() as i64,
                            values,
                            now,
                            app.snapshots.epoch.current(),
                        )
                        .await
                    {
                        Ok(rows) => {
                            for ((id, row), cached) in chunk.iter().zip(rows).zip(cached_chunk) {
                                let row = row.or_else(|| cached.clone());
                                if *bulk {
                                    let placement = logging::place_bulk_delete(
                                        &app,
                                        *guild,
                                        *channel,
                                        *id,
                                        row.as_ref(),
                                        !automod_individual,
                                    );
                                    let author_filtered = row
                                        .as_ref()
                                        .and_then(|row| row.fields.author_id)
                                        .is_some_and(|author| ignored_authors.contains(&author));
                                    if let Some(report) = record_observed_bulk_member(
                                        &mut transcript,
                                        *id,
                                        row.as_ref(),
                                        placement,
                                        automod_individual,
                                        author_filtered,
                                        false,
                                    ) {
                                        peeled_reports.push(report);
                                    }
                                } else {
                                    single = row;
                                }
                            }
                        }
                        Err(error) => {
                            tracing::warn!("message snapshot delete unavailable: {error:#}");
                            app.snapshots.gap_scope(&work.scope);
                            if *bulk {
                                for (id, cached) in chunk.iter().zip(cached_chunk) {
                                    peeled_reports.extend(record_unread_bulk_member(
                                        &app,
                                        *guild,
                                        *channel,
                                        *id,
                                        cached.as_ref(),
                                        &mut transcript,
                                        automod_individual,
                                        &ignored_authors,
                                    ));
                                }
                            } else {
                                single = cached_chunk.first().cloned().flatten();
                                single_unknown = single.is_none();
                            }
                        }
                    }
                }
                app.snapshots.mark_deleted(*guild, *channel, ids);
                for report in peeled_reports {
                    dispatch_observed_report(&app, &work.context, report);
                }
                if *bulk {
                    let pending = transcript.pending_automod.clone();
                    if transcript.worth_sending() {
                        submit_bulk_report(&app, &work.context, *guild, *channel, transcript).await;
                    } else if !pending.is_empty() {
                        logging::settle_bulk_delivery(
                            &app,
                            work.context.http.as_ref(),
                            *guild,
                            *channel,
                            &pending,
                            logging::BulkDeliveryOutcome::Failed,
                        )
                        .await;
                    }
                } else if let Some(id) = ids.first()
                    && let Some(mut report) = logging::observed_delete_report(
                        &app,
                        *guild,
                        *channel,
                        *id,
                        single.as_ref(),
                    )
                {
                    if single_unknown {
                        report
                            .vars
                            .insert("coverage".into(), "snapshot storage unavailable".into());
                    }
                    submit_report(&app, &work.context, report);
                }
            }
        }
    }
}

fn apply_transition_report(report: &mut MessageReport, transition: &SnapshotTransition) {
    match transition.outcome {
        SnapshotOutcome::Stored => logging::apply_retained_edit(
            report,
            transition.before.as_ref(),
            transition.after.as_ref(),
        ),
        SnapshotOutcome::CapacityRefused => {
            // Storage returns the unchanged old row in `after` on refusal.
            // It can describe a prior retained observation, never this edit's
            // accepted after-state. Keep the update's supplied text/attachments.
            logging::apply_retained_edit(report, transition.after.as_ref(), None);
            report.vars.insert(
                "coverage".into(),
                if transition.after.is_some() {
                    "last retained observation; current edit not retained (capacity); observation gap"
                } else {
                    "prior observation unavailable; current edit not retained (capacity); observation gap"
                }
                .into(),
            );
        }
        outcome => {
            logging::apply_retained_edit(report, None, None);
            report.vars.insert(
                "coverage".into(),
                format!("retention unavailable: {outcome:?}; current edit not retained"),
            );
        }
    }
}

/// The store had no earlier row, e.g. its create write was lost while the
/// store was unavailable. Use the memory copy as the edit's prior observation;
/// the event's own fields stay the after-state.
fn apply_cached_edit_prior(
    report: &mut MessageReport,
    transition: &SnapshotTransition,
    cached_before: Option<&MessageSnapshot>,
) {
    let prior_absent = match transition.outcome {
        SnapshotOutcome::Stored => transition.before.is_none(),
        SnapshotOutcome::Missing => true,
        SnapshotOutcome::CapacityRefused => transition.after.is_none(),
        _ => false,
    };
    if !prior_absent || cached_before.is_none() {
        return;
    }
    logging::apply_retained_edit(report, cached_before, None);
    if transition.outcome == SnapshotOutcome::CapacityRefused {
        report.vars.insert(
            "coverage".into(),
            "from memory cache; current edit not retained (capacity); observation gap".into(),
        );
    }
}

async fn update_health(
    app: &BotState,
    store: &GuildDataStore,
    scope: &GuildDataScope,
    recent: &mut HashMap<String, (Instant, SnapshotHealthState, Option<&'static str>)>,
    state: SnapshotHealthState,
    reason: Option<&'static str>,
) {
    let key = scope.placement().store_id().as_str().to_owned();
    if recent.get(&key).is_some_and(|(at, old, old_reason)| {
        *old == state && *old_reason == reason && at.elapsed() < Duration::from_secs(30)
    }) {
        return;
    }
    let now = chrono::Utc::now().timestamp_millis();
    let lost = app.snapshots.take_loss(&key);
    let update = SnapshotHealthUpdate {
        epoch: app.snapshots.epoch.current(),
        state,
        reason,
        observed_at_ms: (state == SnapshotHealthState::Active).then_some(now),
        lost_delta: lost,
        now_ms: now,
    };
    if store.update_snapshot_health(update).await.is_ok() {
        recent.insert(key, (Instant::now(), state, reason));
    } else {
        app.snapshots.restore_loss(&key, lost);
    }
}

pub(crate) fn acquire_report_slot(app: &BotState) -> Option<tokio::sync::OwnedSemaphorePermit> {
    match Arc::clone(&app.snapshots.report_slots).try_acquire_owned() {
        Ok(permit) => Some(permit),
        Err(_) => {
            logging::record_delivery_failure(app);
            None
        }
    }
}

fn schedule_unqueued_bulk_delete(
    app: &Arc<BotState>,
    context: Context,
    guild: GuildId,
    channel: ChannelId,
    ids: Vec<MessageId>,
) {
    if let Some(permit) = acquire_report_slot(app) {
        let app = Arc::clone(app);
        let context = context.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let cached = vec![None; ids.len()];
            submit_peeled_bulk_deletes_admitted(&app, &context, guild, channel, ids, cached).await;
        });
    } else {
        observe_unadmitted_bulk_delete(app, &context, guild, channel, &ids);
    }
}

/// Report slots are full. Observe on this caller. A confirmed automod report
/// is spawned only after a delivery permit is already held.
fn observe_unadmitted_bulk_delete(
    app: &Arc<BotState>,
    context: &Context,
    guild: GuildId,
    channel: ChannelId,
    ids: &[MessageId],
) {
    for id in ids {
        let placement = logging::place_bulk_delete(app, guild, channel, *id, None, false);
        if let Some(report) = placement
            .report
            .filter(|report| report.event == "automod_delete")
        {
            dispatch_delivery_admitted_report(app, context, report);
        }
    }
}

fn dispatch_observed_report(app: &Arc<BotState>, context: &Context, report: MessageReport) {
    if let Ok(permit) = Arc::clone(&app.snapshots.report_slots).try_acquire_owned() {
        let app = Arc::clone(app);
        let context = context.clone();
        tokio::spawn(async move {
            let _permit = permit;
            logging::emit_message_report(&app, context.http.as_ref(), report, None).await;
        });
        return;
    }
    dispatch_delivery_admitted_report(app, context, report);
}

fn dispatch_delivery_admitted_report(
    app: &Arc<BotState>,
    context: &Context,
    report: MessageReport,
) {
    let Some(reservation) = logging::reserve_log_delivery(app) else {
        return;
    };
    let app = Arc::clone(app);
    let context = context.clone();
    tokio::spawn(async move {
        logging::emit_message_report(&app, context.http.as_ref(), report, Some(reservation)).await;
    });
}

#[allow(clippy::too_many_arguments)]
fn record_observed_bulk_member(
    transcript: &mut logging::BulkTranscript,
    id: MessageId,
    row: Option<&crate::db::guild_data::MessageSnapshot>,
    placement: logging::BulkDeletePlacement,
    automod_individual: bool,
    author_filtered: bool,
    storage_unknown: bool,
) -> Option<MessageReport> {
    let report = placement.report;
    if logging::peel_automod_from_bulk(&report, automod_individual) {
        transcript.peel_automod_delete();
        return report;
    }
    // Cap miss or a concurrent REST replay: the id has no bulk row, so it must
    // not stay in a transcript that can still be sent.
    if report.is_none() && !placement.reserved_pending {
        transcript.drop_unreserved();
        return None;
    }
    if placement.reserved_pending {
        transcript.keep_pending_automod(id);
    }
    if author_filtered {
        transcript.push_filtered();
    } else if storage_unknown {
        transcript.push_unknown(id);
    } else {
        transcript.push(id, row);
    }
    None
}

pub(crate) fn peel_automod_bulk_chunk(
    app: &Arc<BotState>,
    guild: GuildId,
    channel: ChannelId,
    ids: &[MessageId],
    transcript: &mut logging::BulkTranscript,
    automod_individual: bool,
) -> Vec<MessageReport> {
    let mut peeled = Vec::new();
    for id in ids {
        let placement =
            logging::place_bulk_delete(app, guild, channel, *id, None, !automod_individual);
        if let Some(report) = record_observed_bulk_member(
            transcript,
            *id,
            None,
            placement,
            automod_individual,
            false,
            true,
        ) {
            peeled.push(report);
        }
    }
    peeled
}

fn bulk_ignored_authors(config: Option<&crate::state::CachedGuildConfig>) -> HashSet<i64> {
    config
        .and_then(|cfg| cfg.log_filters.get("message_bulk_delete"))
        .map(|filter| {
            filter
                .users
                .iter()
                .filter_map(|id| id.parse::<i64>().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// A bulk member the guild store could not read. A memory-cache row is
/// recorded like a retained row; without one the member stays storage unknown.
#[allow(clippy::too_many_arguments)]
fn record_unread_bulk_member(
    app: &Arc<BotState>,
    guild: GuildId,
    channel: ChannelId,
    id: MessageId,
    cached: Option<&MessageSnapshot>,
    transcript: &mut logging::BulkTranscript,
    automod_individual: bool,
    ignored_authors: &HashSet<i64>,
) -> Option<MessageReport> {
    let Some(row) = cached else {
        return peel_automod_bulk_chunk(app, guild, channel, &[id], transcript, automod_individual)
            .pop();
    };
    let placement =
        logging::place_bulk_delete(app, guild, channel, id, Some(row), !automod_individual);
    let author_filtered = row
        .fields
        .author_id
        .is_some_and(|author| ignored_authors.contains(&author));
    record_observed_bulk_member(
        transcript,
        id,
        Some(row),
        placement,
        automod_individual,
        author_filtered,
        false,
    )
}

async fn submit_peeled_bulk_deletes(
    app: &Arc<BotState>,
    context: &Context,
    guild: GuildId,
    channel: ChannelId,
    ids: Vec<MessageId>,
    cached: Vec<Option<MessageSnapshot>>,
) {
    let Some(permit) = acquire_report_slot(app) else {
        // The transcript cannot be sent. Observe pending ids on this worker
        // instead of spawning a task that waits on configuration.
        observe_unadmitted_bulk_delete(app, context, guild, channel, &ids);
        return;
    };
    let app = Arc::clone(app);
    let context = context.clone();
    tokio::spawn(async move {
        let _permit = permit;
        submit_peeled_bulk_deletes_admitted(&app, &context, guild, channel, ids, cached).await;
    });
}

async fn submit_peeled_bulk_deletes_admitted(
    app: &Arc<BotState>,
    context: &Context,
    guild: GuildId,
    channel: ChannelId,
    ids: Vec<MessageId>,
    cached: Vec<Option<MessageSnapshot>>,
) {
    let cfg = app.configs.get(&app.db, guild.get()).await.ok();
    let automod_individual = cfg
        .as_ref()
        .is_some_and(|cfg| logging::channel_for(cfg, "automod_delete").is_some());
    let ignored_authors = bulk_ignored_authors(cfg.as_deref());
    let mut transcript = logging::BulkTranscript::new(ids.len());
    let mut peeled = Vec::new();
    for (id, cached) in ids.iter().zip(&cached) {
        peeled.extend(record_unread_bulk_member(
            app,
            guild,
            channel,
            *id,
            cached.as_ref(),
            &mut transcript,
            automod_individual,
            &ignored_authors,
        ));
    }
    // This task already holds the report permit. Emit peeled automod reports
    // here instead of acquiring a second report slot.
    for report in peeled {
        logging::emit_message_report(app, context.http.as_ref(), report, None).await;
    }
    let pending = transcript.pending_automod.clone();
    if !transcript.worth_sending() {
        if !pending.is_empty() {
            logging::settle_bulk_delivery(
                app,
                context.http.as_ref(),
                guild,
                channel,
                &pending,
                logging::BulkDeliveryOutcome::Failed,
            )
            .await;
        }
        return;
    }
    if !pending.is_empty() {
        logging::begin_bulk_delivery(app, guild, channel, &pending);
    }
    let outcome = logging::emit_retained_bulk(app, context, guild, channel, transcript).await;
    if !pending.is_empty() {
        logging::settle_bulk_delivery(
            app,
            context.http.as_ref(),
            guild,
            channel,
            &pending,
            outcome,
        )
        .await;
    }
}

async fn submit_bulk_report(
    app: &Arc<BotState>,
    context: &Context,
    guild: GuildId,
    channel: ChannelId,
    transcript: logging::BulkTranscript,
) {
    let pending = transcript.pending_automod.clone();
    if !pending.is_empty() {
        logging::begin_bulk_delivery(app, guild, channel, &pending);
    }
    let Some(permit) = acquire_report_slot(app) else {
        if !pending.is_empty() {
            logging::settle_bulk_delivery(
                app,
                context.http.as_ref(),
                guild,
                channel,
                &pending,
                logging::BulkDeliveryOutcome::Failed,
            )
            .await;
        }
        return;
    };
    let app = Arc::clone(app);
    let context = context.clone();
    tokio::spawn(async move {
        let _permit = permit;
        let outcome = logging::emit_retained_bulk(&app, &context, guild, channel, transcript).await;
        if !pending.is_empty() {
            logging::settle_bulk_delivery(
                &app,
                context.http.as_ref(),
                guild,
                channel,
                &pending,
                outcome,
            )
            .await;
        }
    });
}

fn submit_report(app: &Arc<BotState>, context: &Context, report: MessageReport) {
    // Report slot first. If those are saturated, a free delivery slot still
    // admits the send. Neither branch spawns before its permit is held.
    dispatch_observed_report(app, context, report);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pressure_split_rechecks_deletions_before_each_admission() {
        use std::future::Future;

        let path = std::env::temp_dir().join(format!(
            "snapshot_capture_deleted_{:032x}.db",
            rand::random::<u128>()
        ));
        let mut cfg = crate::config::BotProcessConfig::default();
        cfg.db.path = path.clone();
        cfg.db.pool_size = 1;
        let db = crate::db::Db::open(&path, 1).unwrap();
        let app = BotState::new(cfg, db).unwrap();
        let enrollment = crate::db::enroll_guild(&app.db, 789, 100)
            .await
            .unwrap()
            .unwrap();
        let scope = app.tenant.activate(enrollment).await.unwrap().unwrap();
        let store = app.tenant.store(&scope).await.unwrap();
        crate::db::upsert_log_channel(&app.db, 789, "message_delete", 456)
            .await
            .unwrap();
        app.invalidate_configs();
        let now = chrono::Utc::now().timestamp_millis();
        let observation = |id| SnapshotObservation {
            channel_id: 456,
            message_id: id,
            created_at_ms: now,
            observed_at_ms: now,
            edit_at_ms: None,
            coverage_epoch: 1,
            fields: SnapshotFields {
                content: Some("captured".into()),
                author_id: Some(42),
                ..Default::default()
            },
        };
        store
            .observe_snapshot(scope.clone(), observation(100))
            .await
            .unwrap();
        let raw = libsql::Builder::new_local(format!("{}.guild.db", path.display()))
            .build()
            .await
            .unwrap();
        let conn = raw.connect().unwrap();
        conn.execute(
            "UPDATE guild_snapshot_health SET estimated_snapshot_bytes=?1 WHERE singleton=1",
            libsql::params![jmodguard::capacity::SNAPSHOT_BUDGET_BYTES as i64 - 2000],
        )
        .await
        .unwrap();
        // All creates pass the drain's deletion check before the batch refuses.
        for id in 1..=4 {
            assert!(!app.snapshots.deleted((789, 456, id)));
        }
        let error =
            observe_configured_creates(&app, &store, &scope, (1..=4).map(observation).collect())
                .await
                .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<SnapshotError>(),
            Some(SnapshotError::BatchCapacity)
        ));
        assert_eq!(
            observe_configured_creates(&app, &store, &scope, vec![observation(1)])
                .await
                .unwrap(),
            vec![Some(SnapshotOutcome::CapacityRefused)]
        );
        // Drive the real deletion owner while the preceding split item has yet
        // to finish eviction. There is no queued deletion to remove a late row.
        app.snapshots
            .unhooked_delete(GuildId::new(789), ChannelId::new(456), &[MessageId::new(2)]);
        assert_eq!(store.evict_snapshots(100, now).await.unwrap(), 1);
        let store_id = scope.placement().store_id().as_str();
        app.snapshots.take_loss(store_id);
        for id in [2, 3] {
            // For item 3 the deletion arrives during its own config reload.
            let outcomes = if id == 3 {
                app.invalidate_configs();
                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                let (release_tx, release_rx) = std::sync::mpsc::channel();
                let occupied_db = app.db.clone();
                let occupied = tokio::spawn(async move {
                    occupied_db
                        .with_conn(move |_| {
                            entered_tx.send(()).unwrap();
                            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                            Ok(())
                        })
                        .await
                        .unwrap();
                });
                entered_rx.await.unwrap();
                let mut write = Box::pin(observe_configured_creates(
                    &app,
                    &store,
                    &scope,
                    vec![observation(id)],
                ));
                std::future::poll_fn(|cx| match write.as_mut().poll(cx) {
                    std::task::Poll::Pending => std::task::Poll::Ready(()),
                    std::task::Poll::Ready(_) => panic!("configuration reload did not wait"),
                })
                .await;
                app.snapshots.unhooked_delete(
                    GuildId::new(789),
                    ChannelId::new(456),
                    &[MessageId::new(id as u64)],
                );
                release_tx.send(()).unwrap();
                occupied.await.unwrap();
                write.await.unwrap()
            } else {
                observe_configured_creates(&app, &store, &scope, vec![observation(id)])
                    .await
                    .unwrap()
            };
            let retained = store
                .take_snapshot(scope.clone(), 456, id, now + 1)
                .await
                .unwrap();
            assert!(
                retained.is_none(),
                "deleted message {id} persisted: {outcomes:?}"
            );
            assert_eq!(outcomes, vec![None]);
        }
        assert_eq!(app.snapshots.take_loss(store_id), 2);
        // An undeleted retry still writes and retains the original payload.
        assert_eq!(
            observe_configured_creates(&app, &store, &scope, vec![observation(4)])
                .await
                .unwrap(),
            vec![Some(SnapshotOutcome::Stored)]
        );
        let retained = store
            .take_snapshot(scope, 456, 4, now + 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(retained.fields.content.as_deref(), Some("captured"));
        drop(conn);
        drop(raw);
        drop(store);
        drop(app);
        for suffix in [
            "",
            "-wal",
            "-shm",
            ".guild.db",
            ".guild.db-wal",
            ".guild.db-shm",
        ] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn capture_rechecks_cleanup_pause_after_configuration_reload() {
        use std::future::Future;

        let path = std::env::temp_dir().join(format!(
            "snapshot_capture_pause_{:032x}.db",
            rand::random::<u128>()
        ));
        let mut cfg = crate::config::BotProcessConfig::default();
        cfg.db.path = path.clone();
        cfg.db.pool_size = 1;
        let db = crate::db::Db::open(&path, 1).unwrap();
        let app = BotState::new(cfg, db).unwrap();
        let enrollment = crate::db::enroll_guild(&app.db, 789, 100)
            .await
            .unwrap()
            .unwrap();
        let scope = app.tenant.activate(enrollment).await.unwrap().unwrap();
        let store = app.tenant.store(&scope).await.unwrap();
        crate::db::upsert_log_channel(&app.db, 789, "message_delete", 456)
            .await
            .unwrap();
        app.invalidate_configs();
        let now = chrono::Utc::now().timestamp_millis();
        let observation = |id| SnapshotObservation {
            channel_id: 456,
            message_id: id,
            created_at_ms: now,
            observed_at_ms: now,
            edit_at_ms: None,
            coverage_epoch: 1,
            fields: SnapshotFields {
                content: Some("captured".into()),
                author_id: Some(42),
                ..Default::default()
            },
        };
        // Exercise both a drained batch and the single-item pressure fallback.
        for ids in [vec![1, 2], vec![3]] {
            let config = app.configs.get(&app.db, 789).await.unwrap();
            assert!(capture_enabled(&config));
            assert!(!app.snapshots.capture_paused(&scope));
            app.invalidate_configs();
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let occupied_db = app.db.clone();
            let occupied = tokio::spawn(async move {
                occupied_db
                    .with_conn(move |_| {
                        entered_tx.send(()).unwrap();
                        release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                        Ok(())
                    })
                    .await
                    .unwrap();
            });
            entered_rx.await.unwrap();
            let observations: Vec<_> = ids.iter().copied().map(observation).collect();
            let mut writes = Box::pin(observe_configured_creates(
                &app,
                &store,
                &scope,
                observations.clone(),
            ));
            std::future::poll_fn(|cx| match writes.as_mut().poll(cx) {
                std::task::Poll::Pending => std::task::Poll::Ready(()),
                std::task::Poll::Ready(_) => panic!("configuration reload did not wait"),
            })
            .await;
            release_tx.send(()).unwrap();
            occupied.await.unwrap();
            // Before resuming admission, use the real maintenance failure path.
            // An invalid cleanup time is rejected by the store, as in the
            // maintenance recovery test below; no pause flag is set by the test.
            app.snapshots.maintain_at(&app, Some(-1)).await;
            assert!(app.snapshots.capture_paused(&scope));
            let store_id = scope.placement().store_id().as_str();
            app.snapshots.take_loss(store_id);
            let outcomes = writes.await.unwrap();
            let retained = store
                .take_snapshots(scope.clone(), 456, ids.clone(), now + 1)
                .await
                .unwrap();
            assert!(
                retained.iter().all(Option::is_none),
                "capture persisted after cleanup paused: {outcomes:?}"
            );
            assert_eq!(outcomes, vec![None; ids.len()]);
            assert_eq!(app.snapshots.take_loss(store_id), ids.len() as u64);

            app.snapshots.maintain_at(&app, Some(now)).await;
            assert!(!app.snapshots.capture_paused(&scope));
            assert_eq!(
                observe_configured_creates(&app, &store, &scope, observations)
                    .await
                    .unwrap(),
                vec![Some(SnapshotOutcome::Stored); ids.len()]
            );
            let retained = store
                .take_snapshots(scope.clone(), 456, ids.clone(), now + 1)
                .await
                .unwrap();
            for (row, id) in retained.into_iter().zip(ids) {
                let row = row.unwrap();
                assert_eq!(row.message_id, id);
                assert_eq!(row.fields.content.as_deref(), Some("captured"));
            }
        }
        drop(store);
        drop(app);
        for suffix in [
            "",
            "-wal",
            "-shm",
            ".guild.db",
            ".guild.db-wal",
            ".guild.db-shm",
        ] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn capture_writes_recheck_configuration_for_batches_and_split_items() {
        let path = std::env::temp_dir().join(format!(
            "snapshot_capture_config_{:032x}.db",
            rand::random::<u128>()
        ));
        let mut cfg = crate::config::BotProcessConfig::default();
        cfg.db.path = path.clone();
        cfg.db.pool_size = 2;
        let db = crate::db::Db::open(&path, 2).unwrap();
        let app = BotState::new(cfg, db).unwrap();
        let enrollment = crate::db::enroll_guild(&app.db, 789, 100)
            .await
            .unwrap()
            .unwrap();
        let scope = app.tenant.activate(enrollment).await.unwrap().unwrap();
        let store = app.tenant.store(&scope).await.unwrap();
        app.snapshots.scopes.lock().insert(789, scope.clone());
        let now = chrono::Utc::now().timestamp_millis();
        let observation = |id| SnapshotObservation {
            channel_id: 456,
            message_id: id,
            created_at_ms: now,
            observed_at_ms: now,
            edit_at_ms: None,
            coverage_epoch: 1,
            fields: SnapshotFields {
                content: Some("captured".into()),
                author_id: Some(42),
                ..Default::default()
            },
        };
        crate::db::upsert_log_channel(&app.db, 789, "message_delete", 456)
            .await
            .unwrap();
        app.invalidate_configs();
        // Reproduce an eligible first work item followed by a configuration
        // change before its drain reaches the production write helper.
        let first_config = app.configs.get(&app.db, 789).await.unwrap();
        assert!(capture_enabled(&first_config));
        let initial =
            observe_configured_creates(&app, &store, &scope, vec![observation(1), observation(2)])
                .await
                .unwrap();
        assert_eq!(initial, vec![Some(SnapshotOutcome::Stored); 2]);
        crate::db::delete_log_channel(&app.db, 789, "message_delete")
            .await
            .unwrap();
        app.invalidate_configs();
        assert!(capture_enabled(&first_config), "the old admission is stale");
        for ids in [vec![3, 4], vec![5]] {
            let expected = vec![None; ids.len()];
            let outcomes = observe_configured_creates(
                &app,
                &store,
                &scope,
                ids.into_iter().map(observation).collect(),
            )
            .await
            .unwrap();
            assert_eq!(outcomes, expected);
        }
        crate::db::upsert_log_channel(&app.db, 789, "message_edit", 456)
            .await
            .unwrap();
        app.invalidate_configs();
        assert_eq!(
            observe_configured_creates(&app, &store, &scope, vec![observation(6)])
                .await
                .unwrap(),
            vec![Some(SnapshotOutcome::Stored)]
        );
        // Both the batch and one-at-a-time pressure path must leave no payload
        // behind when disabled, including the process memory copy. Re-enabling
        // still permits new observations.
        let mut cached: Vec<_> = app
            .snapshots
            .recent
            .lock()
            .entries
            .keys()
            .map(|(_, _, id)| *id)
            .collect();
        cached.sort_unstable();
        assert_eq!(cached, vec![1, 2, 6]);
        let retained = store
            .take_snapshots(scope, 456, vec![1, 2, 3, 4, 5, 6], now + 1)
            .await
            .unwrap();
        let mut ids: Vec<_> = retained
            .into_iter()
            .flatten()
            .map(|row| row.message_id)
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2, 6]);
        drop(store);
        drop(app);
        for suffix in [
            "",
            "-wal",
            "-shm",
            ".guild.db",
            ".guild.db-wal",
            ".guild.db-shm",
        ] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn bulk_delete_snapshot_storage_failure_peels_automod_marks() {
        let path = std::env::temp_dir().join(format!(
            "snapshot_capture_bulk_del_err_{:032x}.db",
            rand::random::<u128>()
        ));
        let mut cfg = crate::config::BotProcessConfig::default();
        cfg.db.path = path.clone();
        cfg.db.pool_size = 1;
        let db = crate::db::Db::open(&path, 1).unwrap();
        let app = Arc::new(BotState::new(cfg, db).unwrap());
        app.activation.mark_lock_held(true);
        let enrollment = crate::db::enroll_guild(&app.db, 789, 100)
            .await
            .unwrap()
            .unwrap();
        let scope = app.tenant.activate(enrollment).await.unwrap().unwrap();
        let store = app.tenant.store(&scope).await.unwrap();
        crate::db::upsert_log_channel(&app.db, 789, "automod_delete", 456)
            .await
            .unwrap();
        crate::db::upsert_log_channel(&app.db, 789, "message_bulk_delete", 456)
            .await
            .unwrap();
        app.invalidate_configs();
        app.snapshots.scopes.lock().insert(789, scope.clone());
        app.snapshots.content_access.lock().insert(789);
        let delete_attempt = app
            .automod_delete_marks
            .insert(789, 456, 10, "badword", "Bad words");
        app.automod_delete_marks
            .confirm(789, 456, 10, delete_attempt);
        store.close_membership(scope.clone()).await.unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        let epoch = app.snapshots.epoch.current();
        assert!(
            store
                .take_snapshots_at_epoch(scope.clone(), 456, vec![10, 11], now, epoch)
                .await
                .is_err(),
            "closed membership must fail snapshot reads for the delete worker Err path"
        );
        let mut transcript = logging::BulkTranscript::new(2);
        let peeled = peel_automod_bulk_chunk(
            &app,
            GuildId::new(789),
            ChannelId::new(456),
            &[MessageId::new(10), MessageId::new(11)],
            &mut transcript,
            true,
        );
        assert_eq!(peeled.len(), 1);
        assert_eq!(peeled[0].event, "automod_delete");
        assert_eq!(transcript.unknown, 1);
        assert!(
            !app.automod_delete_marks.inner.lock().contains(789, 456, 10),
            "automod mark must be consumed for the peeled id"
        );
        drop(store);
        drop(app);
        for suffix in [
            "",
            "-wal",
            "-shm",
            ".guild.db",
            ".guild.db-wal",
            ".guild.db-shm",
        ] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    async fn test_delete_context() -> Context {
        test_delete_context_with(Arc::new(serenity::http::Http::new("fixture"))).await
    }

    async fn test_delete_context_with(http: Arc<serenity::http::Http>) -> Context {
        use serenity::gateway::{
            Shard, ShardManager, ShardManagerOptions, ShardMessenger, ShardRunner,
            ShardRunnerOptions,
        };
        use serenity::model::gateway::{GatewayIntents, ShardInfo};
        use serenity::model::id::ShardId;
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        // Test-only loopback WebSocket URL for a local accept() fixture, not production traffic.
        let addr = listener.local_addr().unwrap();
        let url = Arc::new(tokio::sync::Mutex::new(format!(
            "ws://127.0.0.1:{}", // nosemgrep: javascript.lang.security.detect-insecure-websocket
            addr.port()
        )));
        let accept = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            tokio_tungstenite::accept_async(stream).await.unwrap()
        });
        let intents = GatewayIntents::empty();
        let shard = Shard::new(
            Arc::clone(&url),
            "fixture",
            ShardInfo {
                id: ShardId(0),
                total: 1,
            },
            intents,
            None,
        )
        .await
        .unwrap();
        let socket = accept.await.unwrap();
        let data = Arc::default();
        let cache = Arc::new(serenity::cache::Cache::new());
        let (manager, _) = ShardManager::new(ShardManagerOptions {
            data: Arc::clone(&data),
            event_handlers: vec![],
            raw_event_handlers: vec![],
            shard_index: 0,
            shard_init: 0,
            shard_total: 1,
            ws_url: url,
            cache: Arc::clone(&cache),
            http: Arc::clone(&http),
            intents,
            presence: None,
        });
        let runner = ShardRunner::new(ShardRunnerOptions {
            data: Arc::clone(&data),
            event_handlers: vec![],
            raw_event_handlers: vec![],
            manager: Arc::clone(&manager),
            shard,
            cache: Arc::clone(&cache),
            http: Arc::clone(&http),
        });
        let ctx = Context {
            data,
            shard: ShardMessenger::new(&runner),
            shard_id: ShardId(0),
            http,
            cache,
        };
        manager.shutdown_all().await;
        drop(runner);
        drop(socket);
        ctx
    }

    #[tokio::test]
    async fn batched_store_failure_is_one_gap_for_every_admitted_row() {
        let path = std::env::temp_dir().join(format!(
            "snapshot_capture_batch_err_{:032x}.db",
            rand::random::<u128>()
        ));
        let mut cfg = crate::config::BotProcessConfig::default();
        cfg.db.path = path.clone();
        cfg.db.pool_size = 1;
        let db = crate::db::Db::open(&path, 1).unwrap();
        let app = BotState::new(cfg, db).unwrap();
        let enrollment = crate::db::enroll_guild(&app.db, 789, 100)
            .await
            .unwrap()
            .unwrap();
        let scope = app.tenant.activate(enrollment).await.unwrap().unwrap();
        let store = app.tenant.store(&scope).await.unwrap();
        crate::db::upsert_log_channel(&app.db, 789, "message_delete", 456)
            .await
            .unwrap();
        app.invalidate_configs();
        let now = chrono::Utc::now().timestamp_millis();
        let observation = |id| SnapshotObservation {
            channel_id: 456,
            message_id: id,
            created_at_ms: now,
            observed_at_ms: now,
            edit_at_ms: None,
            coverage_epoch: 1,
            fields: SnapshotFields {
                content: Some("captured".into()),
                author_id: Some(42),
                ..Default::default()
            },
        };
        store.close_membership(scope.clone()).await.unwrap();
        let before = app.snapshots.epoch.current();
        let error =
            observe_configured_creates(&app, &store, &scope, vec![observation(1), observation(2)])
                .await
                .unwrap_err();
        assert!(
            !matches!(
                error.downcast_ref::<SnapshotError>(),
                Some(SnapshotError::BatchCapacity)
            ),
            "{error:#}"
        );
        assert_eq!(
            app.snapshots.epoch.current(),
            before + 1,
            "one failed batch write is one coverage gap"
        );
        let store_id = scope.placement().store_id().as_str();
        assert_eq!(app.snapshots.take_loss(store_id), 2);
        drop(store);
        drop(app);
        for suffix in [
            "",
            "-wal",
            "-shm",
            ".guild.db",
            ".guild.db-wal",
            ".guild.db-shm",
        ] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn batch_write_rechecks_deletion_after_admission() {
        use std::future::Future;

        let path = std::env::temp_dir().join(format!(
            "snapshot_capture_batch_tombstone_{:032x}.db",
            rand::random::<u128>()
        ));
        let mut cfg = crate::config::BotProcessConfig::default();
        cfg.db.path = path.clone();
        cfg.db.pool_size = 1;
        let db = crate::db::Db::open(&path, 1).unwrap();
        let app = BotState::new(cfg, db).unwrap();
        let enrollment = crate::db::enroll_guild(&app.db, 789, 100)
            .await
            .unwrap()
            .unwrap();
        let scope = app.tenant.activate(enrollment).await.unwrap().unwrap();
        let store = app.tenant.store(&scope).await.unwrap();
        crate::db::upsert_log_channel(&app.db, 789, "message_delete", 456)
            .await
            .unwrap();
        app.invalidate_configs();
        assert!(capture_enabled(
            &app.configs.get(&app.db, 789).await.unwrap()
        ));
        let now = chrono::Utc::now().timestamp_millis();
        let observation = |id| SnapshotObservation {
            channel_id: 456,
            message_id: id,
            created_at_ms: now,
            observed_at_ms: now,
            edit_at_ms: None,
            coverage_epoch: 1,
            fields: SnapshotFields {
                content: Some("captured".into()),
                author_id: Some(42),
                ..Default::default()
            },
        };
        let mut write = Box::pin(observe_configured_creates(
            &app,
            &store,
            &scope,
            vec![observation(1), observation(2)],
        ));
        std::future::poll_fn(|cx| match write.as_mut().poll(cx) {
            std::task::Poll::Pending => std::task::Poll::Ready(()),
            std::task::Poll::Ready(_) => panic!("batch admission did not yield before the write"),
        })
        .await;
        app.snapshots
            .unhooked_delete(GuildId::new(789), ChannelId::new(456), &[MessageId::new(1)]);
        let outcomes = write.await.unwrap();
        assert_eq!(outcomes, vec![None, Some(SnapshotOutcome::Stored)]);
        let retained = store
            .take_snapshots(scope, 456, vec![1, 2], now + 1)
            .await
            .unwrap();
        assert!(
            retained[0].is_none(),
            "deleted row was stored: {retained:?}"
        );
        assert_eq!(
            retained[1].as_ref().unwrap().fields.content.as_deref(),
            Some("captured")
        );
        drop(store);
        drop(app);
        for suffix in [
            "",
            "-wal",
            "-shm",
            ".guild.db",
            ".guild.db-wal",
            ".guild.db-shm",
        ] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[test]
    fn observed_empty_differs_from_missing_and_utf8_clip_is_safe() {
        let missing = fields(None, None, Some(9), None);
        let empty = fields(Some(""), Some(&[]), Some(9), None);
        assert_eq!(missing.content, None);
        assert_eq!(missing.attachments, None);
        assert_eq!(empty.content.as_deref(), Some(""));
        assert!(empty.attachments.as_ref().unwrap().is_empty());

        let long = "🦀".repeat(MAX_TEXT_BYTES / 4 + 1);
        let retained = fields(Some(&long), None, Some(9), None);
        assert!(retained.content_truncated);
        assert_eq!(retained.content_original_bytes, Some(long.len() as i64));
        assert_eq!(retained.content.as_deref().unwrap().len(), MAX_TEXT_BYTES);
    }

    #[test]
    fn saturated_report_slots_block_bulk_fallback_tasks() {
        let path = std::env::temp_dir().join(format!(
            "snapshot_bulk_fallback_{:032x}.db",
            rand::random::<u128>()
        ));
        let mut cfg = crate::config::BotProcessConfig::default();
        cfg.db.path = path.clone();
        cfg.db.pool_size = 1;
        let db = crate::db::Db::open(&path, 1).unwrap();
        let app = BotState::new(cfg, db).unwrap();
        let held: Vec<_> = (0..32)
            .map(|_| {
                app.snapshots
                    .report_slots
                    .clone()
                    .try_acquire_owned()
                    .unwrap()
            })
            .collect();
        let before = app
            .log_delivery_failures
            .load(std::sync::atomic::Ordering::Relaxed);
        assert!(acquire_report_slot(&app).is_none());
        let after = app
            .log_delivery_failures
            .load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(after, before + 1);
        drop(held);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn saturated_report_slots_increment_delivery_failures() {
        let path = std::env::temp_dir().join(format!(
            "snapshot_report_slots_{:032x}.db",
            rand::random::<u128>()
        ));
        let mut cfg = crate::config::BotProcessConfig::default();
        cfg.db.path = path.clone();
        cfg.db.pool_size = 1;
        let db = crate::db::Db::open(&path, 1).unwrap();
        let app = BotState::new(cfg, db).unwrap();
        let held: Vec<_> = (0..32)
            .map(|_| {
                app.snapshots
                    .report_slots
                    .clone()
                    .try_acquire_owned()
                    .unwrap()
            })
            .collect();
        assert!(acquire_report_slot(&app).is_none());
        assert_eq!(
            app.log_delivery_failures
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        drop(held);
        assert!(acquire_report_slot(&app).is_some());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn retained_unicode_attachment_metadata_clipping_is_explicit_without_omission() {
        let filename = "🦀".repeat(80);
        let url = format!("https://example.invalid/{}", "🌕".repeat(300));
        let content_type = format!("image/{}", "🦀".repeat(40));
        let attachment: serenity::model::channel::Attachment =
            serde_json::from_value(serde_json::json!({
                "id": "1", "filename": filename, "url": url, "proxy_url": url,
                "size": 100, "content_type": content_type
            }))
            .unwrap();
        let observed = fields(None, Some(&[attachment]), Some(9), None);
        assert!(observed.attachments_truncated);
        assert_eq!(observed.attachments_omitted, Some(0));
        let retained = observed.attachments.unwrap();
        assert_eq!(retained.len(), 1);
        assert!(retained[0].filename.len() <= 256);
        assert!(retained[0].url.len() <= 1024);
        assert!(retained[0].content_type.as_ref().unwrap().len() <= 128);
    }

    #[derive(Default)]
    struct RecordingSink {
        messages: std::sync::Mutex<Vec<String>>,
    }

    #[serenity::async_trait]
    impl logging::LogDelivery for RecordingSink {
        async fn send(
            &self,
            channel: ChannelId,
            message: serenity::builder::CreateMessage,
        ) -> anyhow::Result<()> {
            self.messages
                .lock()
                .unwrap()
                .push(format!("{channel} {}", serde_json::to_value(message)?));
            Ok(())
        }
    }

    fn temp_app(prefix: &str) -> (Arc<BotState>, std::path::PathBuf) {
        let path =
            std::env::temp_dir().join(format!("{prefix}_{:032x}.db", rand::random::<u128>()));
        let mut cfg = crate::config::BotProcessConfig::default();
        cfg.db.path = path.clone();
        cfg.db.pool_size = 1;
        cfg.log.discord_concurrency = 4;
        let db = crate::db::Db::open(&path, 1).unwrap();
        let app = BotState::new(cfg, db).unwrap();
        app.activation.mark_lock_held(true);
        (app, path)
    }

    fn remove_db(path: &std::path::Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    fn message_json(id: MessageId, content: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id.to_string(), "channel_id": "456", "guild_id": "789",
            "author": {"id": "42", "username": "advisor", "discriminator": "0"},
            "content": content, "timestamp": chrono::Utc::now().to_rfc3339(),
            "edited_timestamp": null, "tts": false, "mention_everyone": false,
            "mentions": [], "mention_roles": [], "attachments": [], "embeds": [],
            "pinned": false, "type": 0
        })
    }

    async fn wait_for_body(
        bodies: &std::sync::Mutex<Vec<String>>,
        what: &str,
        matches: impl Fn(&str) -> bool,
    ) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let sent = bodies.lock().unwrap().clone();
            if let Some(body) = sent.iter().find(|body| matches(body)) {
                return body.clone();
            }
            assert!(Instant::now() < deadline, "no {what} report: {sent:#?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// #633: the guild store cannot be opened, so nothing reaches the snapshot
    /// table. Reports still show the text this process observed.
    #[tokio::test]
    async fn unavailable_snapshot_store_logs_edit_and_deletes_from_memory_cache() {
        let (app, path) = temp_app("snapshot_memory_cache");
        // This process has no credentials for another installation's store, so
        // `tenant.store` fails the same way as an unreachable guild store.
        let (other, other_path) = temp_app("snapshot_memory_cache_other");
        let enrollment = crate::db::enroll_guild(&other.db, 789, 100)
            .await
            .unwrap()
            .unwrap();
        let scope = other.tenant.activate(enrollment).await.unwrap().unwrap();
        assert!(app.tenant.store(&scope).await.is_err());
        for event in ["message_edit", "message_delete", "message_bulk_delete"] {
            crate::db::upsert_log_channel(&app.db, 789, event, 901)
                .await
                .unwrap();
        }
        app.invalidate_configs();
        app.snapshots.scopes.lock().insert(789, scope);
        let (http, bodies, server) = recording_delete_proxy().await;
        let ctx = test_delete_context_with(http).await;
        let snowflake = |offset: u64| {
            let ms = chrono::Utc::now().timestamp_millis() as u64 - 1_420_070_400_000;
            MessageId::new((ms << 22) + offset)
        };
        let (edited, bulk_cached, bulk_unseen) = (snowflake(1), snowflake(2), snowflake(3));
        for (id, content) in [
            (edited, "first draft text"),
            (bulk_cached, "bulk member text"),
        ] {
            let create: serenity::model::event::MessageCreateEvent =
                serde_json::from_value(message_json(id, content)).unwrap();
            app.snapshots
                .on_gateway_event(&app, ctx.clone(), &Event::MessageCreate(create));
        }
        let update: MessageUpdateEvent = serde_json::from_value(serde_json::json!({
            "id": edited.to_string(), "channel_id": "456", "guild_id": "789",
            "author": {"id": "42", "username": "advisor", "discriminator": "0"},
            "content": "second draft text",
            "edited_timestamp": chrono::Utc::now().to_rfc3339()
        }))
        .unwrap();
        app.snapshots
            .on_gateway_event(&app, ctx.clone(), &Event::MessageUpdate(update));
        let delete: serenity::model::event::MessageDeleteEvent =
            serde_json::from_value(serde_json::json!({
                "id": edited.to_string(), "channel_id": "456", "guild_id": "789"
            }))
            .unwrap();
        app.snapshots
            .on_gateway_event(&app, ctx.clone(), &Event::MessageDelete(delete));
        let bulk: serenity::model::event::MessageDeleteBulkEvent =
            serde_json::from_value(serde_json::json!({
                "guild_id": "789", "channel_id": "456",
                "ids": [bulk_cached.to_string(), bulk_unseen.to_string()]
            }))
            .unwrap();
        app.snapshots
            .on_gateway_event(&app, ctx.clone(), &Event::MessageDeleteBulk(bulk));

        let edit = wait_for_body(&bodies, "edit", |body| body.contains("Message edited")).await;
        assert!(edit.contains("first draft text"), "{edit}");
        assert!(edit.contains("second draft text"), "{edit}");
        assert!(edit.contains("from memory cache"), "{edit}");
        let single = wait_for_body(&bodies, "delete", |body| {
            body.contains("Message deleted") && !body.contains("Bulk")
        })
        .await;
        assert!(single.contains("second draft text"), "{single}");
        assert!(!single.contains("not captured"), "{single}");
        assert!(single.contains("from memory cache"), "{single}");
        assert!(single.contains("<@42>"), "{single}");
        let transcript =
            wait_for_body(&bodies, "bulk", |body| body.contains("bulk member text")).await;
        assert!(
            transcript.contains(&format!(
                "Message {bulk_cached} | author 42 | from memory cache"
            )),
            "{transcript}"
        );
        assert!(
            transcript.contains(&format!(
                "Message {bulk_unseen} | snapshot storage unavailable"
            )),
            "{transcript}"
        );
        assert!(
            app.snapshots.recent.lock().entries.is_empty(),
            "deleted messages stayed in the memory cache"
        );
        server.abort();
        drop(ctx);
        drop(other);
        drop(app);
        remove_db(&path);
        remove_db(&other_path);
        for path in [&path, &other_path] {
            for suffix in [".guild.db", ".guild.db-wal", ".guild.db-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
            }
        }
    }

    /// The store opens, but its create write and the delete read both fail.
    #[tokio::test]
    async fn failed_snapshot_read_logs_delete_from_memory_cache() {
        let (app, path) = temp_app("snapshot_memory_cache_read");
        let enrollment = crate::db::enroll_guild(&app.db, 789, 100)
            .await
            .unwrap()
            .unwrap();
        let scope = app.tenant.activate(enrollment).await.unwrap().unwrap();
        let store = app.tenant.store(&scope).await.unwrap();
        crate::db::upsert_log_channel(&app.db, 789, "message_delete", 901)
            .await
            .unwrap();
        app.invalidate_configs();
        app.snapshots.scopes.lock().insert(789, scope.clone());
        store.close_membership(scope).await.unwrap();
        let (http, bodies, server) = recording_delete_proxy().await;
        let ctx = test_delete_context_with(http).await;
        let ms = chrono::Utc::now().timestamp_millis() as u64 - 1_420_070_400_000;
        let id = MessageId::new((ms << 22) + 1);
        let create: serenity::model::event::MessageCreateEvent =
            serde_json::from_value(message_json(id, "observed before the outage")).unwrap();
        app.snapshots
            .on_gateway_event(&app, ctx.clone(), &Event::MessageCreate(create));
        let delete: serenity::model::event::MessageDeleteEvent =
            serde_json::from_value(serde_json::json!({
                "id": id.to_string(), "channel_id": "456", "guild_id": "789"
            }))
            .unwrap();
        app.snapshots
            .on_gateway_event(&app, ctx.clone(), &Event::MessageDelete(delete));
        let report =
            wait_for_body(&bodies, "delete", |body| body.contains("Message deleted")).await;
        assert!(report.contains("observed before the outage"), "{report}");
        assert!(report.contains("from memory cache"), "{report}");
        assert!(!report.contains("snapshot storage unavailable"), "{report}");
        server.abort();
        drop(ctx);
        drop(store);
        drop(app);
        remove_db(&path);
        for suffix in [".guild.db", ".guild.db-wal", ".guild.db-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    /// Activates membership `joined_at_ns` for guild 789 and makes it the
    /// captured scope, as `refresh_scope` does.
    async fn cache_scope(app: &Arc<BotState>, joined_at_ns: i64) -> GuildDataScope {
        let enrollment = crate::db::enroll_guild(&app.db, 789, joined_at_ns)
            .await
            .unwrap()
            .unwrap();
        let scope = app.tenant.activate(enrollment).await.unwrap().unwrap();
        app.snapshots.scopes.lock().insert(789, scope.clone());
        scope
    }

    fn remove_db_and_store(path: &std::path::Path) {
        remove_db(path);
        for suffix in [".guild.db", ".guild.db-wal", ".guild.db-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    fn cache_observation(id: i64, created_at_ms: i64, text: &str) -> SnapshotObservation {
        SnapshotObservation {
            channel_id: 456,
            message_id: id,
            created_at_ms,
            observed_at_ms: created_at_ms,
            edit_at_ms: None,
            coverage_epoch: 0,
            fields: SnapshotFields {
                content: Some(text.into()),
                author_id: Some(42),
                ..Default::default()
            },
        }
    }

    #[tokio::test]
    async fn memory_cache_respects_count_byte_and_age_caps() {
        let (app, path) = temp_app("snapshot_memory_cache_caps");
        let scope = cache_scope(&app, 100).await;
        let now = chrono::Utc::now().timestamp_millis();
        let observation = cache_observation;
        let mut recent = RecentMessages::default();
        for id in 1..=(MAX_RECENT as i64 + 10) {
            recent.observe(&scope, &observation(id, now, "short"), true, now);
        }
        assert_eq!(recent.entries.len(), MAX_RECENT);
        assert!(
            recent.get((789, 456, 10), &scope, now).is_none(),
            "oldest kept"
        );
        assert!(
            recent
                .get((789, 456, MAX_RECENT as u64 + 10), &scope, now)
                .is_some()
        );

        let mut recent = RecentMessages::default();
        let large = "x".repeat(MAX_TEXT_BYTES);
        for id in 1..=1000 {
            recent.observe(&scope, &observation(id, now, &large), true, now);
            assert!(recent.bytes <= MAX_RECENT_BYTES);
        }
        assert!(recent.entries.len() < 1000);
        assert_eq!(
            recent.bytes,
            recent
                .entries
                .values()
                .map(|entry| entry.size)
                .sum::<usize>()
        );
        assert!(recent.get((789, 456, 1000), &scope, now).is_some());

        let mut recent = RecentMessages::default();
        let old = now - RECENT_LIFETIME_MS + 1000;
        recent.observe(&scope, &observation(1, old, "old"), true, now);
        assert!(recent.get((789, 456, 1), &scope, now).is_some());
        let later = now + 1000;
        recent.observe(&scope, &observation(2, later, "new"), true, later);
        assert!(
            recent.take((789, 456, 1), &scope, later).is_none(),
            "expired row"
        );
        assert!(!recent.entries.contains_key(&(789, 456, 1)));
        assert!(
            recent
                .observe(&scope, &observation(3, old - 2000, "late"), true, later)
                .is_none()
        );
        assert!(!recent.entries.contains_key(&(789, 456, 3)));
        drop(app);
        remove_db_and_store(&path);
    }

    /// Creation time, not insertion order, sets expiry: an edit can first
    /// cache an old message behind a newer one. Periodic maintenance releases
    /// expired entries even when no further observation arrives.
    #[tokio::test]
    async fn memory_cache_releases_expired_entries_out_of_order_and_when_idle() {
        let (app, path) = temp_app("snapshot_memory_cache_expiry");
        let scope = cache_scope(&app, 100).await;
        let now = chrono::Utc::now().timestamp_millis();
        let old = now - RECENT_LIFETIME_MS + 1000;
        let mut recent = RecentMessages::default();
        recent.observe(&scope, &cache_observation(1, now, "live head"), true, now);
        recent.observe(
            &scope,
            &cache_observation(2, old, "older message"),
            true,
            now,
        );
        let later = now + 2000;
        recent.observe(&scope, &cache_observation(3, later, "newest"), true, later);
        assert!(
            recent.entries.contains_key(&(789, 456, 2)),
            "front-only trim stops at the live head"
        );
        recent.expire(later);
        assert!(!recent.entries.contains_key(&(789, 456, 2)));
        assert_eq!(recent.entries.len(), 2);
        assert!(!recent.order.contains(&(789, 456, 2)));
        assert_eq!(
            recent.bytes,
            recent
                .entries
                .values()
                .map(|entry| entry.size)
                .sum::<usize>()
        );

        app.snapshots
            .remember_recent(&scope, &cache_observation(4, old, "idle message"), true);
        assert_eq!(app.snapshots.recent.lock().entries.len(), 1);
        app.snapshots.maintain_at(&app, Some(later)).await;
        let recent = app.snapshots.recent.lock();
        assert!(
            recent.entries.is_empty(),
            "idle cache kept an expired entry"
        );
        assert_eq!(recent.bytes, 0);
        drop(recent);
        drop(app);
        remove_db_and_store(&path);
    }

    /// Same first-supply rule as the guild-store edit merge.
    #[tokio::test]
    async fn memory_cache_keeps_first_supplied_fields_at_one_edit_timestamp() {
        let (app, path) = temp_app("snapshot_memory_cache_first_supply");
        let scope = cache_scope(&app, 100).await;
        let now = chrono::Utc::now().timestamp_millis();
        let attachment = |id: i64| SnapshotAttachment {
            id,
            filename: format!("file-{id}"),
            size: 1,
            content_type: None,
            url: String::new(),
        };
        let edit = |at: Option<i64>, content: Option<&str>, attachments: Option<(i64, i64)>| {
            SnapshotObservation {
                channel_id: 456,
                message_id: 1,
                created_at_ms: now,
                observed_at_ms: now,
                edit_at_ms: at,
                coverage_epoch: 0,
                fields: SnapshotFields {
                    content: content.map(Into::into),
                    content_original_bytes: content.map(|text| text.len() as i64),
                    attachments: attachments.map(|(id, _)| vec![attachment(id)]),
                    attachments_omitted: attachments.map(|(_, omitted)| omitted),
                    attachments_truncated: attachments.is_some_and(|(_, omitted)| omitted > 0),
                    author_id: Some(42),
                    ..Default::default()
                },
            }
        };
        let mut recent = RecentMessages::default();
        let at = now + 10;
        recent.observe(&scope, &edit(None, Some("A"), Some((1, 0))), true, now);
        recent.observe(&scope, &edit(Some(at), Some("B"), None), false, now);
        // A conflicting duplicate at the same timestamp keeps B.
        recent.observe(&scope, &edit(Some(at), Some("conflict"), None), false, now);
        // A complementary partial at that timestamp still fills attachments,
        // and the first attachment value at that timestamp then stays.
        recent.observe(&scope, &edit(Some(at), None, Some((2, 3))), false, now);
        recent.observe(&scope, &edit(Some(at), None, Some((9, 0))), false, now);
        // An older edit is ignored.
        recent.observe(&scope, &edit(Some(at - 1), Some("stale"), None), false, now);
        let row = recent.get((789, 456, 1), &scope, now).unwrap();
        assert_eq!(row.fields.content.as_deref(), Some("B"));
        assert_eq!(row.fields.content_original_bytes, Some(1));
        let ids: Vec<_> = row
            .fields
            .attachments
            .iter()
            .flatten()
            .map(|a| a.id)
            .collect();
        assert_eq!(ids, vec![2]);
        assert_eq!(row.fields.attachments_omitted, Some(3));
        assert!(row.fields.attachments_truncated);
        // A later edit supplies new values.
        recent.observe(&scope, &edit(Some(at + 1), Some("later"), None), false, now);
        let row = recent.take((789, 456, 1), &scope, now).unwrap();
        assert_eq!(row.fields.content.as_deref(), Some("later"));
        assert_eq!(row.fields.content_original_bytes, Some(5));
        let ids: Vec<_> = row
            .fields
            .attachments
            .iter()
            .flatten()
            .map(|a| a.id)
            .collect();
        assert_eq!(ids, vec![2]);
        drop(app);
        remove_db_and_store(&path);
    }

    #[tokio::test]
    async fn memory_cache_merges_edits_and_never_keeps_deleted_messages() {
        let (app, path) = temp_app("snapshot_memory_cache_edits");
        let scope = cache_scope(&app, 100).await;
        let now = chrono::Utc::now().timestamp_millis();
        let observation = |edit_at_ms: Option<i64>, content: Option<&str>| SnapshotObservation {
            channel_id: 456,
            message_id: 1,
            created_at_ms: now,
            observed_at_ms: now,
            edit_at_ms,
            coverage_epoch: 0,
            fields: SnapshotFields {
                content: content.map(Into::into),
                author_id: Some(42),
                ..Default::default()
            },
        };
        assert!(
            app.snapshots
                .remember_recent(&scope, &observation(Some(now), Some("edit")), false)
                .is_none()
        );
        assert!(app.snapshots.recent.lock().entries.is_empty());
        app.snapshots
            .remember_recent(&scope, &observation(None, Some("original")), true);
        let before = app
            .snapshots
            .remember_recent(&scope, &observation(Some(now + 2), Some("second")), false)
            .unwrap();
        assert_eq!(before.fields.content.as_deref(), Some("original"));
        assert_eq!(before.generation, logging::MEMORY_CACHE_GENERATION);
        // An older edit and an edit without text keep the latest text. Neither
        // the newer row nor this revision itself is a prior of the older edit
        // or of a replay at the same timestamp.
        assert!(
            app.snapshots
                .remember_recent(&scope, &observation(Some(now + 1), Some("first")), false)
                .is_none(),
            "a newer cached revision was returned as an older edit's prior"
        );
        assert!(
            app.snapshots
                .remember_recent(&scope, &observation(Some(now + 2), Some("second")), false)
                .is_none(),
            "an edit revision was returned as its own prior"
        );
        let before = app
            .snapshots
            .remember_recent(&scope, &observation(Some(now + 3), None), false)
            .unwrap();
        assert_eq!(before.fields.content.as_deref(), Some("second"));
        app.snapshots
            .unhooked_delete(GuildId::new(789), ChannelId::new(456), &[MessageId::new(1)]);
        assert!(app.snapshots.recent.lock().entries.is_empty());
        app.snapshots
            .remember_recent(&scope, &observation(None, Some("replayed")), true);
        assert!(
            app.snapshots.recent.lock().entries.is_empty(),
            "a deleted identity was cached again"
        );
        drop(app);
        remove_db_and_store(&path);
    }

    /// Work queued under one membership must not restore cleared rows, and a
    /// row captured under one membership is never used by another, including
    /// a scope replacement without a Guild Delete callback.
    #[tokio::test]
    async fn memory_cache_is_bound_to_the_captured_scope() {
        let (app, path) = temp_app("snapshot_memory_cache_scope");
        let first = cache_scope(&app, 100).await;
        let now = chrono::Utc::now().timestamp_millis();
        let create = |id: i64, text: &str| cache_observation(id, now, text);
        let edit = |id: i64| SnapshotObservation {
            edit_at_ms: Some(now + 1),
            ..cache_observation(id, now, "edited")
        };
        app.snapshots
            .remember_recent(&first, &create(1, "old membership text"), true);
        assert_eq!(app.snapshots.recent.lock().entries.len(), 1);
        app.snapshots.clear_guild(789);
        assert!(app.snapshots.recent.lock().entries.is_empty());
        // The worker reaches work that was queued before the guild was removed.
        app.snapshots
            .remember_recent(&first, &create(1, "old membership text"), true);
        assert!(
            app.snapshots.recent.lock().entries.is_empty(),
            "stale work restored old membership content"
        );

        app.snapshots.refresh_scope(&app, 789).await;
        assert!(
            app.snapshots
                .scope(789)
                .is_some_and(|scope| same_scope(&scope, &first))
        );
        app.snapshots
            .remember_recent(&first, &create(2, "first membership text"), true);
        app.snapshots
            .remember_recent(&first, &create(5, "first membership text"), true);
        crate::db::enroll_guild(&app.db, 789, 200)
            .await
            .unwrap()
            .unwrap();
        app.snapshots.refresh_scope(&app, 789).await;
        let second = app.snapshots.scope(789).unwrap();
        assert!(!same_scope(&first, &second), "membership was not replaced");
        // Old work cannot write or consume, and new work cannot read or
        // consume, a row from the earlier membership.
        assert!(
            app.snapshots
                .take_recent(&first, ChannelId::new(456), MessageId::new(5))
                .is_none(),
            "a delete queued under the replaced membership used its cached row"
        );
        assert!(
            app.snapshots
                .remember_recent(&first, &create(3, "late old work"), true)
                .is_none()
        );
        assert!(
            !app.snapshots
                .recent
                .lock()
                .entries
                .contains_key(&(789, 456, 3))
        );
        assert!(
            app.snapshots
                .remember_recent(&second, &edit(2), false)
                .is_none()
        );
        assert!(
            app.snapshots
                .take_recent(&second, ChannelId::new(456), MessageId::new(2))
                .is_none()
        );
        assert!(app.snapshots.recent.lock().entries.is_empty());
        // The current membership still has its outage fallback.
        app.snapshots
            .remember_recent(&second, &create(4, "current membership text"), true);
        let row = app
            .snapshots
            .take_recent(&second, ChannelId::new(456), MessageId::new(4))
            .unwrap();
        assert_eq!(
            row.fields.content.as_deref(),
            Some("current membership text")
        );
        drop(app);
        remove_db_and_store(&path);
    }

    /// `remember_recent` holds `recent` while taking `deleted`, and
    /// `mark_deleted` holds `deleted` while taking `known_humans`. Guild
    /// clearing must not hold `known_humans` while it waits for `recent`.
    #[test]
    fn clear_guild_does_not_hold_known_humans_while_waiting_for_recent() {
        let (app, path) = temp_app("snapshot_memory_cache_lock_order");
        app.snapshots.content_access.lock().insert(789);
        app.snapshots.remember_human((789, 456, 1));
        let recent = app.snapshots.recent.lock();
        let clearing = {
            let app = Arc::clone(&app);
            std::thread::spawn(move || app.snapshots.clear_guild(789))
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.snapshots.content_access.lock().contains(&789) {
            assert!(Instant::now() < deadline, "guild clear did not start");
            std::thread::sleep(Duration::from_millis(1));
        }
        let deleted = app.snapshots.deleted.lock();
        let known = app
            .snapshots
            .known_humans
            .try_lock_for(Duration::from_secs(2));
        let progressed = known.is_some();
        drop(known);
        drop(deleted);
        drop(recent);
        clearing.join().unwrap();
        assert!(
            progressed,
            "guild clear held known_humans while waiting for the recent cache"
        );
        assert!(!app.snapshots.known_human((789, 456, 1)));
        drop(app);
        remove_db(&path);
    }

    #[test]
    fn missing_or_refused_edit_prior_uses_the_memory_cache() {
        let row = |generation: i64, text: &str| MessageSnapshot {
            guild_id: 789,
            channel_id: 456,
            message_id: 1,
            fields: SnapshotFields {
                content: Some(text.into()),
                author_id: Some(42),
                ..Default::default()
            },
            created_at_ms: 1,
            first_observed_at_ms: 1,
            last_observed_at_ms: 1,
            edit_at_ms: None,
            expires_at_ms: i64::MAX,
            generation,
            coverage_epoch: 0,
            continuous: false,
            content_supplied: true,
            attachments_supplied: false,
        };
        let cached = row(logging::MEMORY_CACHE_GENERATION, "cached prior");
        let report_for = |outcome, before, after| {
            let mut report = MessageReport {
                retained_expiries: Default::default(),
                guild_id: GuildId::new(789),
                event: "message_edit",
                vars: std::collections::BTreeMap::new(),
            };
            report.vars.insert("after".into(), "event text".into());
            let transition = SnapshotTransition {
                outcome,
                before,
                after,
            };
            apply_transition_report(&mut report, &transition);
            apply_cached_edit_prior(&mut report, &transition, Some(&cached));
            report
        };
        for outcome in [SnapshotOutcome::Missing, SnapshotOutcome::Stored] {
            let report = report_for(outcome, None, None);
            assert!(
                report.vars["before"].contains("cached prior"),
                "{outcome:?}"
            );
            assert_eq!(report.vars["after"], "event text");
            assert_eq!(report.vars["coverage"], "from memory cache");
        }
        let refused = report_for(SnapshotOutcome::CapacityRefused, None, None);
        assert!(refused.vars["before"].contains("cached prior"));
        assert_eq!(refused.vars["after"], "event text");
        assert!(refused.vars["coverage"].starts_with("from memory cache"));
        assert!(refused.vars["coverage"].contains("not retained (capacity)"));
        // A durable prior stays authoritative.
        let stored = report_for(
            SnapshotOutcome::CapacityRefused,
            None,
            Some(row(3, "stored")),
        );
        assert!(stored.vars["before"].contains("stored"));
        assert!(stored.vars["coverage"].starts_with("last retained observation"));
        let stored = report_for(SnapshotOutcome::Stored, Some(row(3, "stored")), None);
        assert!(stored.vars["before"].contains("stored"));
    }

    /// The create write was lost; the store is healthy again and has no row.
    /// An author-omitted edit returns `Missing` and uses the cached prior.
    #[tokio::test]
    async fn missing_durable_row_uses_cached_before_for_author_omitted_edit() {
        let (app, path) = temp_app("snapshot_memory_cache_missing_row");
        let scope = cache_scope(&app, 100).await;
        crate::db::upsert_log_channel(&app.db, 789, "message_edit", 901)
            .await
            .unwrap();
        app.invalidate_configs();
        app.snapshots.content_access.lock().insert(789);
        let (http, bodies, server) = recording_delete_proxy().await;
        let ctx = test_delete_context_with(http).await;
        let now = chrono::Utc::now().timestamp_millis();
        let id = MessageId::new(((now as u64 - 1_420_070_400_000) << 22) + 1);
        app.snapshots.remember_recent(
            &scope,
            &cache_observation(id.get() as i64, now, "cached original"),
            true,
        );
        let update: MessageUpdateEvent = serde_json::from_value(serde_json::json!({
            "id": id.to_string(), "channel_id": "456", "guild_id": "789",
            "content": "current edit", "edited_timestamp": chrono::Utc::now().to_rfc3339()
        }))
        .unwrap();
        app.snapshots
            .on_gateway_event(&app, ctx.clone(), &Event::MessageUpdate(update));
        let body = wait_for_body(&bodies, "edit", |body| body.contains("Message edited")).await;
        server.abort();
        assert!(body.contains("cached original"), "{body}");
        assert!(body.contains("current edit"), "{body}");
        assert!(body.contains("from memory cache"), "{body}");
        assert!(!body.contains("not captured"), "{body}");
        drop(ctx);
        drop(app);
        remove_db_and_store(&path);
    }

    /// A cleanup failure pauses durable capture. The edit report still uses
    /// the cached prior and keeps the pause visible; nothing is stored.
    #[tokio::test]
    async fn cleanup_paused_edit_uses_cached_before() {
        let (app, path) = temp_app("snapshot_memory_cache_paused_edit");
        let scope = cache_scope(&app, 100).await;
        crate::db::upsert_log_channel(&app.db, 789, "message_edit", 901)
            .await
            .unwrap();
        app.invalidate_configs();
        app.snapshots.content_access.lock().insert(789);
        let (http, bodies, server) = recording_delete_proxy().await;
        let ctx = test_delete_context_with(http).await;
        let now = chrono::Utc::now().timestamp_millis();
        let id = MessageId::new(((now as u64 - 1_420_070_400_000) << 22) + 1);
        app.snapshots.remember_recent(
            &scope,
            &cache_observation(id.get() as i64, now, "cached original"),
            true,
        );
        // The real maintenance failure path: the store rejects this cleanup time.
        app.snapshots.maintain_at(&app, Some(-1)).await;
        assert!(app.snapshots.capture_paused(&scope));
        let update: MessageUpdateEvent = serde_json::from_value(serde_json::json!({
            "id": id.to_string(), "channel_id": "456", "guild_id": "789",
            "content": "current edit", "edited_timestamp": chrono::Utc::now().to_rfc3339()
        }))
        .unwrap();
        app.snapshots
            .on_gateway_event(&app, ctx.clone(), &Event::MessageUpdate(update));
        let body = wait_for_body(&bodies, "edit", |body| body.contains("Message edited")).await;
        server.abort();
        assert!(body.contains("cached original"), "{body}");
        assert!(body.contains("current edit"), "{body}");
        assert!(body.contains("from memory cache"), "{body}");
        assert!(
            body.contains("capture paused after cleanup failure"),
            "{body}"
        );
        assert!(!body.contains("not captured"), "{body}");
        let store = app.tenant.store(&scope).await.unwrap();
        let retained = store
            .take_snapshots(scope.clone(), 456, vec![id.get() as i64], now + 1)
            .await
            .unwrap();
        assert!(
            retained.iter().all(Option::is_none),
            "paused capture stored a row"
        );
        drop(store);
        drop(ctx);
        drop(app);
        remove_db_and_store(&path);
    }

    /// During a store outage, an edit that arrives after a newer edit of the
    /// same message is reported without claiming the newer text as its prior.
    #[tokio::test]
    async fn out_of_order_edit_does_not_report_newer_cached_revision_as_before() {
        let (app, path) = temp_app("snapshot_memory_cache_edit_order");
        let (other, other_path) = temp_app("snapshot_memory_cache_edit_order_other");
        let enrollment = crate::db::enroll_guild(&other.db, 789, 100)
            .await
            .unwrap()
            .unwrap();
        let scope = other.tenant.activate(enrollment).await.unwrap().unwrap();
        assert!(app.tenant.store(&scope).await.is_err());
        crate::db::upsert_log_channel(&app.db, 789, "message_edit", 901)
            .await
            .unwrap();
        app.invalidate_configs();
        app.snapshots.scopes.lock().insert(789, scope.clone());
        let (http, bodies, server) = recording_delete_proxy().await;
        let ctx = test_delete_context_with(http).await;
        let now = chrono::Utc::now();
        let id = MessageId::new(((now.timestamp_millis() as u64 - 1_420_070_400_000) << 22) + 1);
        let create = serde_json::from_value(message_json(id, "original create text")).unwrap();
        app.snapshots
            .on_gateway_event(&app, ctx.clone(), &Event::MessageCreate(create));
        let edit = |at: chrono::DateTime<chrono::Utc>, text: &str| {
            Event::MessageUpdate(
                serde_json::from_value(serde_json::json!({
                    "id": id.to_string(), "channel_id": "456", "guild_id": "789",
                    "author": {"id": "42", "username": "advisor", "discriminator": "0"},
                    "content": text, "edited_timestamp": at.to_rfc3339()
                }))
                .unwrap(),
            )
        };
        let newer = now + chrono::Duration::seconds(2);
        app.snapshots
            .on_gateway_event(&app, ctx.clone(), &edit(newer, "newer edit text"));
        let first = wait_for_body(&bodies, "newer edit", |body| {
            body.contains("newer edit text")
        })
        .await;
        assert!(first.contains("original create text"), "{first}");
        app.snapshots.on_gateway_event(
            &app,
            ctx.clone(),
            &edit(newer - chrono::Duration::seconds(1), "older edit text"),
        );
        let body = wait_for_body(&bodies, "older edit", |body| {
            body.contains("older edit text")
        })
        .await;
        server.abort();
        let json: serde_json::Value =
            serde_json::from_str(body.split("\r\n\r\n").last().unwrap()).unwrap();
        let before = json["embeds"][0]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|field| field["name"].as_str().unwrap().contains("Before"))
            .unwrap()["value"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(!before.contains("newer edit text"), "{body}");
        assert!(!body.contains("from memory cache"), "{body}");
        let cached = app
            .snapshots
            .take_recent(&scope, ChannelId::new(456), id)
            .unwrap();
        assert_eq!(cached.fields.content.as_deref(), Some("newer edit text"));
        drop(ctx);
        drop(other);
        drop(app);
        remove_db_and_store(&path);
        remove_db_and_store(&other_path);
    }

    #[tokio::test]
    async fn unqueued_bulk_without_a_report_slot_still_observes_pending_automod() {
        let (app, path) = temp_app("snapshot_unqueued_bulk");
        crate::db::upsert_log_channel(&app.db, 789, "message_bulk_delete", 903)
            .await
            .unwrap();
        app.invalidate_configs();
        let attempt = app
            .automod_delete_marks
            .insert(789, 456, 123, "badword", "Bad words");
        let held: Vec<_> = (0..32)
            .map(|_| acquire_report_slot(&app).unwrap())
            .collect();
        let ctx = test_delete_context().await;
        let bulk: serenity::model::event::MessageDeleteBulkEvent =
            serde_json::from_value(serde_json::json!({
                "guild_id": "789",
                "channel_id": "456",
                "ids": ["123"]
            }))
            .unwrap();
        app.snapshots
            .on_gateway_event(&app, ctx, &Event::MessageDeleteBulk(bulk));
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let observed = app
                .automod_delete_marks
                .gateway_observed_unsettled(789, 456, 123);
            let reserved = app.automod_delete_marks.bulk_log_contains(789, 456, 123);
            if observed && !reserved {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "unqueued bulk did not observe and settle the pending automod id"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let removal = app
            .automod_delete_marks
            .remove_unconfirmed(789, 456, 123, attempt);
        assert_eq!(
            removal.action,
            crate::automod_delete_marks::UnconfirmedMarkRemoval::ReplayPlainDelete
        );
        let sink = RecordingSink::default();
        logging::replay_unconfirmed_plain_delete(
            &app,
            &sink,
            GuildId::new(789),
            ChannelId::new(456),
            MessageId::new(123),
            &removal,
        )
        .await;
        let messages = sink.messages.lock().unwrap();
        assert_eq!(messages.len(), 1, "{messages:?}");
        drop(held);
        drop(app);
        remove_db(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn saturated_report_slots_do_not_spawn_unqueued_bulk_tasks() {
        let (app, path) = temp_app("snapshot_saturated_spawn");
        let attempt = app
            .automod_delete_marks
            .insert(789, 456, 1_000, "badword", "Bad words");
        let held: Vec<_> = (0..32)
            .map(|_| acquire_report_slot(&app).unwrap())
            .collect();
        let ctx = test_delete_context().await;
        let before = Arc::strong_count(&app);
        for offset in 0..128u64 {
            let id = 1_000 + offset;
            let bulk: serenity::model::event::MessageDeleteBulkEvent =
                serde_json::from_value(serde_json::json!({
                    "guild_id": "789",
                    "channel_id": "456",
                    "ids": [id.to_string()]
                }))
                .unwrap();
            app.snapshots
                .on_gateway_event(&app, ctx.clone(), &Event::MessageDeleteBulk(bulk));
        }
        assert_eq!(
            Arc::strong_count(&app),
            before,
            "report slots saturated, but unqueued bulk deletes still spawned tasks"
        );
        assert!(
            app.automod_delete_marks
                .gateway_observed_unsettled(789, 456, 1_000),
            "pending automod delete was not observed inline"
        );
        assert!(!app.automod_delete_marks.bulk_log_contains(789, 456, 1_000));
        let removal = app
            .automod_delete_marks
            .remove_unconfirmed(789, 456, 1_000, attempt);
        assert_eq!(
            removal.action,
            crate::automod_delete_marks::UnconfirmedMarkRemoval::ReplayPlainDelete
        );
        drop(held);
        drop(ctx);
        drop(app);
        remove_db(&path);
    }

    async fn recording_delete_proxy() -> (
        Arc<serenity::http::Http>,
        Arc<std::sync::Mutex<Vec<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&bodies);
        let server = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut request = Vec::new();
                let mut header_end = None;
                loop {
                    let mut buf = [0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                    if let Some(pos) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                        header_end = Some(pos + 4);
                        break;
                    }
                }
                let Some(header_end) = header_end else {
                    break;
                };
                let header = String::from_utf8_lossy(&request[..header_end]);
                let length = header
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        if !name.eq_ignore_ascii_case("content-length") {
                            return None;
                        }
                        value.trim().parse::<usize>().ok()
                    })
                    .unwrap_or(0);
                while request.len() < header_end + length {
                    let mut buf = [0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                }
                recorded
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&request).into_owned());
                let body = r#"{"id":"1","channel_id":"901","author":{"id":"1","username":"bot","discriminator":"0","avatar":null},"content":"ok","timestamp":"2020-01-01T00:00:00+00:00","edited_timestamp":null,"tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],"attachments":[],"embeds":[],"pinned":false,"type":0}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        let http = serenity::http::HttpBuilder::new("fixture")
            .proxy(format!("http://{address}"))
            .application_id(serenity::model::id::ApplicationId::new(1))
            .ratelimiter_disabled(true)
            .build();
        (Arc::new(http), bodies, server)
    }

    async fn peeled_automod_under_held_report_slots(held_slots: usize) {
        let (app, path) = temp_app("snapshot_peeled_saturated");
        crate::db::upsert_log_channel(&app.db, 789, "automod_delete", 901)
            .await
            .unwrap();
        app.invalidate_configs();
        let confirmed = app
            .automod_delete_marks
            .insert(789, 456, 123, "badword", "Bad words");
        assert!(
            app.automod_delete_marks
                .confirm(789, 456, 123, confirmed)
                .is_none()
        );
        let pending = app
            .automod_delete_marks
            .insert(789, 456, 124, "badword", "Bad words");
        let (http, bodies, server) = recording_delete_proxy().await;
        let ctx = test_delete_context_with(http).await;
        let held: Vec<_> = (0..held_slots)
            .map(|_| acquire_report_slot(&app).unwrap())
            .collect();
        let before = Arc::strong_count(&app);
        let bulk: serenity::model::event::MessageDeleteBulkEvent =
            serde_json::from_value(serde_json::json!({
                "guild_id": "789",
                "channel_id": "456",
                "ids": ["123", "124"]
            }))
            .unwrap();
        app.snapshots
            .on_gateway_event(&app, ctx.clone(), &Event::MessageDeleteBulk(bulk));
        assert_eq!(
            Arc::strong_count(&app),
            before + 1,
            "peeled automod delete took more than one admitted task"
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let sent = bodies.lock().unwrap().clone();
            if sent.iter().any(|body| {
                body.contains("Bad words") && body.contains("123") && body.contains("901")
            }) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "peeled automod report was dropped under {held_slots} held report slots: {sent:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            app.automod_delete_marks
                .gateway_observed_unsettled(789, 456, 124),
            "pending automod delete inside the bulk was not kept"
        );
        assert!(!app.automod_delete_marks.bulk_log_contains(789, 456, 124));
        let removal = app
            .automod_delete_marks
            .remove_unconfirmed(789, 456, 124, pending);
        assert_eq!(
            removal.action,
            crate::automod_delete_marks::UnconfirmedMarkRemoval::ReplayPlainDelete
        );
        drop(held);
        server.abort();
        drop(ctx);
        drop(app);
        remove_db(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn saturated_slots_deliver_peeled_automod_with_pending_member() {
        peeled_automod_under_held_report_slots(32).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn last_report_slot_hands_off_to_the_peeled_automod_report() {
        peeled_automod_under_held_report_slots(31).await;
    }

    #[tokio::test]
    async fn bulk_cap_omits_an_unreserved_id_so_rest_failure_logs_once() {
        use crate::automod_delete_marks::MAX_AUTOMOD_DELETE_MARKS;
        let (app, path) = temp_app("snapshot_bulk_cap");
        crate::db::upsert_log_channel(&app.db, 789, "message_bulk_delete", 903)
            .await
            .unwrap();
        app.invalidate_configs();
        let mut first_attempt = None;
        for message in 1..=MAX_AUTOMOD_DELETE_MARKS as u64 {
            let attempt =
                app.automod_delete_marks
                    .insert(789, 456, message, "badword", "Bad words");
            if message == 1 {
                first_attempt = Some(attempt);
            }
            let (_, reserved) = app
                .automod_delete_marks
                .observe_and_reserve_bulk_delete(789, 456, message, None, true);
            assert!(reserved, "setup reservation failed at {message}");
        }
        // Free one mark without freeing its bulk row, so the bulk cap stays full.
        app.automod_delete_marks.remove_unconfirmed(
            789,
            456,
            1,
            first_attempt.expect("first mark"),
        );
        let overflow = MAX_AUTOMOD_DELETE_MARKS as u64 + 1;
        let attempt = app
            .automod_delete_marks
            .insert(789, 456, overflow, "badword", "Bad words");
        let mut transcript = logging::BulkTranscript::new(1);
        let peeled = peel_automod_bulk_chunk(
            &app,
            GuildId::new(789),
            ChannelId::new(456),
            &[MessageId::new(overflow)],
            &mut transcript,
            false,
        );
        assert!(peeled.is_empty());
        assert!(
            transcript.pending_automod.is_empty(),
            "an id that missed the bulk cap stayed in the transcript"
        );
        assert_eq!(transcript.total, 0);
        assert!(!transcript.worth_sending());
        assert!(
            !app.automod_delete_marks
                .bulk_log_contains(789, 456, overflow)
        );
        let removal = app
            .automod_delete_marks
            .remove_unconfirmed(789, 456, overflow, attempt);
        assert_eq!(
            removal.action,
            crate::automod_delete_marks::UnconfirmedMarkRemoval::ReplayPlainDelete
        );
        let sink = RecordingSink::default();
        logging::replay_unconfirmed_plain_delete(
            &app,
            &sink,
            GuildId::new(789),
            ChannelId::new(456),
            MessageId::new(overflow),
            &removal,
        )
        .await;
        let messages = sink.messages.lock().unwrap();
        assert_eq!(
            messages.len(),
            1,
            "cap miss must still produce one fallback, not a transcript plus a replay: {messages:?}"
        );
        drop(app);
        remove_db(&path);
    }

    #[tokio::test]
    async fn settled_ambiguous_rows_release_capacity_for_the_next_bulk_delete() {
        use crate::automod_delete_marks::MAX_AUTOMOD_DELETE_MARKS;
        use std::time::{Duration, Instant};
        let (app, path) = temp_app("snapshot_ambiguous_cap");
        crate::db::upsert_log_channel(&app.db, 789, "message_bulk_delete", 903)
            .await
            .unwrap();
        app.invalidate_configs();
        for message in 1..=MAX_AUTOMOD_DELETE_MARKS as u64 {
            let attempt =
                app.automod_delete_marks
                    .insert(789, 456, message, "badword", "Bad words");
            let (_, reserved) = app
                .automod_delete_marks
                .observe_and_reserve_bulk_delete(789, 456, message, None, true);
            assert!(reserved, "setup reservation failed at {message}");
            assert!(
                app.automod_delete_marks
                    .finish_bulk_delivery(
                        789,
                        456,
                        &[message],
                        logging::BulkDeliveryOutcome::Ambiguous,
                    )
                    .is_empty()
            );
            assert!(
                app.automod_delete_marks
                    .confirm(789, 456, message, attempt)
                    .is_none(),
                "ambiguous bulk already owns the report for {message}"
            );
        }
        let blocked = MAX_AUTOMOD_DELETE_MARKS as u64 + 1;
        let blocked_attempt =
            app.automod_delete_marks
                .insert(789, 456, blocked, "badword", "Bad words");
        let mut transcript = logging::BulkTranscript::new(1);
        let peeled = peel_automod_bulk_chunk(
            &app,
            GuildId::new(789),
            ChannelId::new(456),
            &[MessageId::new(blocked)],
            &mut transcript,
            false,
        );
        assert!(peeled.is_empty());
        assert!(
            transcript.pending_automod.is_empty(),
            "a full ambiguous cache kept an id it could not reserve"
        );
        assert!(!transcript.worth_sending());
        let sink = RecordingSink::default();
        if let Some(settlement) =
            app.automod_delete_marks
                .confirm(789, 456, blocked, blocked_attempt)
        {
            logging::emit_reconciled_automod_delete(
                &app,
                &sink,
                789,
                456,
                blocked,
                &settlement.rule_id,
                &settlement.rule_name,
                &settlement.reason,
                settlement.retained.as_ref(),
                settlement.fallback_to_bulk,
            )
            .await;
        }
        assert!(
            sink.messages.lock().unwrap().is_empty(),
            "bulk-only success has no individual destination: {:?}",
            sink.messages.lock().unwrap()
        );
        let still = blocked + 1;
        app.automod_delete_marks
            .insert(789, 456, still, "badword", "Bad words");
        let (_, reserved) = app
            .automod_delete_marks
            .observe_and_reserve_bulk_delete(789, 456, still, None, true);
        assert!(
            !reserved,
            "the suppression window released capacity immediately"
        );
        app.automod_delete_marks
            .prune_at(Instant::now() + Duration::from_secs(10 * 60 + 5));
        let recovered = still + 1;
        app.automod_delete_marks
            .insert(789, 456, recovered, "badword", "Bad words");
        let mut transcript = logging::BulkTranscript::new(1);
        let peeled = peel_automod_bulk_chunk(
            &app,
            GuildId::new(789),
            ChannelId::new(456),
            &[MessageId::new(recovered)],
            &mut transcript,
            false,
        );
        assert!(peeled.is_empty());
        assert_eq!(transcript.pending_automod, vec![MessageId::new(recovered)]);
        assert!(transcript.worth_sending());
        assert!(
            app.automod_delete_marks
                .bulk_log_contains(789, 456, recovered)
        );
        drop(app);
        remove_db(&path);
    }

    #[test]
    fn author_provenance_is_bounded_and_eviction_marks_a_gap() {
        let ingress = SnapshotIngress::default();
        let start_epoch = ingress.epoch.current();
        for id in 1..=(MAX_KNOWN as u64 + 1) {
            ingress.remember_human((1, 2, id));
        }
        assert!(!ingress.known_human((1, 2, 1)));
        assert!(ingress.known_human((1, 2, MAX_KNOWN as u64 + 1)));
        assert_eq!(ingress.known_humans.lock().entries.len(), MAX_KNOWN);
        assert!(ingress.epoch.current() > start_epoch);
    }

    #[test]
    fn deleted_identity_remains_marked_after_exact_map_would_overflow() {
        let mut bloom = DeletionBloom::new();
        let first = (1, 2, 3);
        bloom.insert(first);
        for id in 4..=MAX_KNOWN as u64 + 4 {
            bloom.insert((1, 2, id));
        }
        assert!(bloom.contains(first));
        bloom.started = Instant::now() - Duration::from_secs(25 * 3600);
        assert!(!bloom.contains(first));
    }

    #[test]
    fn entire_bulk_event_is_deduplicated_before_hundred_id_storage_chunks() {
        let ids: Vec<_> = (1..=101)
            .map(MessageId::new)
            .chain([MessageId::new(1), MessageId::new(101)])
            .collect();
        let unique = unique_ids(&ids);
        assert_eq!(unique.len(), 101);
        assert_eq!(unique.chunks(100).count(), 2);
        assert_eq!(unique[0], MessageId::new(1));
        assert_eq!(unique[100], MessageId::new(101));
    }

    #[test]
    fn refused_deletion_identity_blocks_late_create_and_edit() {
        let ingress = SnapshotIngress::default();
        let guild = GuildId::new(789);
        let channel = ChannelId::new(456);
        let id = MessageId::new(123);
        assert!(!ingress.deleted((guild.get(), channel.get(), id.get())));
        ingress.unhooked_delete(guild, channel, &[id]);
        assert!(ingress.deleted((guild.get(), channel.get(), id.get())));
    }

    #[test]
    fn known_store_loss_never_attaches_to_an_unrelated_store() {
        let ingress = SnapshotIngress::default();
        ingress.gap_store("store-a");
        ingress.gap(); // Unattributed Gateway gap.
        assert_eq!(ingress.take_loss("store-b"), 0);
        assert_eq!(ingress.take_loss("store-a"), 1);
        assert_eq!(ingress.take_loss("store-a"), 0);
    }

    #[test]
    fn maintenance_drains_more_than_four_expiry_batches_without_waiting_for_another_tick() {
        let path =
            std::env::temp_dir().join(format!("snapshot_drain_{:032x}.db", rand::random::<u128>()));
        let mut cfg = crate::config::BotProcessConfig::default();
        cfg.db.path = path.clone();
        cfg.db.pool_size = 2;
        let db = crate::db::Db::open(&path, 2).unwrap();
        let app = BotState::new(cfg, db).unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let enrollment = crate::db::enroll_guild(&app.db, 789, 100)
                .await
                .unwrap()
                .unwrap();
            let scope = app.tenant.activate(enrollment).await.unwrap().unwrap();
            let store = app.tenant.store(&scope).await.unwrap();
            let now = chrono::Utc::now().timestamp_millis();
            for id in 1..=401 {
                let outcome = store
                    .observe_snapshot(
                        scope.clone(),
                        SnapshotObservation {
                            channel_id: 456,
                            message_id: id,
                            created_at_ms: now - 23 * 3_600_000,
                            observed_at_ms: now,
                            edit_at_ms: None,
                            coverage_epoch: 1,
                            fields: SnapshotFields {
                                content: Some("A".into()),
                                author_id: Some(42),
                                ..Default::default()
                            },
                        },
                    )
                    .await
                    .unwrap();
                assert_eq!(outcome, SnapshotOutcome::Stored);
            }
            app.snapshots.scopes.lock().insert(789, scope.clone());
            app.snapshots.content_access.lock().insert(789);
            assert_eq!(
                app.snapshots
                    .maintain_at(&app, Some(now + 2 * 3_600_000))
                    .await,
                std::time::Duration::from_secs(30)
            );
            assert_eq!(
                app.snapshots
                    .maintain_at(&app, Some(now + 2 * 3_600_000 + 1))
                    .await,
                std::time::Duration::from_secs(120)
            );
            let usage = store.measure_usage(false).await.unwrap();
            assert_eq!(usage.snapshot_count, Some(0));
            assert_eq!(usage.snapshot_health.unwrap().expired_count, 401);
            // A verified cleanup error pauses growing capture for this store;
            // a later successful check and measurement admit it again.
            app.snapshots.maintain_at(&app, Some(-1)).await;
            assert!(app.snapshots.capture_paused(&scope));
            app.snapshots
                .maintain_at(&app, Some(now + 2 * 3_600_000 + 1))
                .await;
            assert!(!app.snapshots.capture_paused(&scope));
        });
        drop(app);
        for suffix in [
            "",
            "-wal",
            "-shm",
            ".guild.db",
            ".guild.db-wal",
            ".guild.db-shm",
        ] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn refresh_scope_preserves_cached_scope_when_tenant_lookup_fails() {
        let path =
            std::env::temp_dir().join(format!("scope_refresh_{:032x}", rand::random::<u128>()));
        let mut cfg = crate::config::BotProcessConfig::default();
        cfg.db.path = path.clone();
        cfg.db.pool_size = 2;
        let db = crate::db::Db::open(&path, 2).unwrap();
        let app = BotState::new(cfg, db).unwrap();
        let enrollment = crate::db::enroll_guild(&app.db, 789, 100)
            .await
            .unwrap()
            .unwrap();
        let scope = app.tenant.activate(enrollment).await.unwrap().unwrap();
        app.snapshots.scopes.lock().insert(789, scope);
        app.snapshots.content_access.lock().insert(789);
        app.db
            .with_conn(|conn| {
                conn.execute_batch_bounded(
                    "DELETE FROM guild_store_placements WHERE guild_id=789",
                    64 * 1024 * 1024,
                )?;
                Ok(())
            })
            .await
            .unwrap();
        app.snapshots.refresh_scope(&app, 789).await;
        assert!(app.snapshots.scopes.lock().contains_key(&789));
        drop(app);
        for suffix in [
            "",
            "-wal",
            "-shm",
            ".guild.db",
            ".guild.db-wal",
            ".guild.db-shm",
        ] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn refresh_scope_clears_scope_when_enrollment_is_absent() {
        let path =
            std::env::temp_dir().join(format!("scope_clear_{:032x}", rand::random::<u128>()));
        let mut cfg = crate::config::BotProcessConfig::default();
        cfg.db.path = path.clone();
        cfg.db.pool_size = 2;
        let db = crate::db::Db::open(&path, 2).unwrap();
        let app = BotState::new(cfg, db).unwrap();
        let enrollment = crate::db::enroll_guild(&app.db, 789, 100)
            .await
            .unwrap()
            .unwrap();
        let scope = app.tenant.activate(enrollment).await.unwrap().unwrap();
        app.snapshots.scopes.lock().insert(789, scope);
        crate::db::retire_guild_enrollment(&app.db, 789, 100)
            .await
            .unwrap();
        app.snapshots.refresh_scope(&app, 789).await;
        assert!(!app.snapshots.scopes.lock().contains_key(&789));
        drop(app);
        for suffix in [
            "",
            "-wal",
            "-shm",
            ".guild.db",
            ".guild.db-wal",
            ".guild.db-shm",
        ] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[test]
    fn known_losses_are_counted_for_every_physical_store() {
        let ingress = SnapshotIngress::default();
        for index in 0..1025 {
            ingress.gap_store(&format!("store-{index}"));
        }
        assert_eq!(ingress.lost_pending.lock().len(), 1025);
        assert_eq!(ingress.take_loss("store-1024"), 1);
        for index in 0..1025 {
            ingress
                .health_initialized
                .lock()
                .insert(format!("store-{index}"));
        }
        assert!(ingress.health_initialized.lock().contains("store-1024"));
    }

    #[test]
    fn ingress_refuses_item_and_byte_overload_without_exceeding_bounds() {
        let ingress = SnapshotIngress::default();
        let permits: Vec<_> = (0..MAX_PENDING)
            .map(|_| ingress.sender.try_reserve().expect("item slot"))
            .collect();
        assert!(ingress.sender.try_reserve().is_err());
        drop(permits);
        assert_eq!(
            ingress.reserve_bytes(MAX_PENDING_BYTES),
            ByteAdmission::Reserved
        );
        assert_eq!(ingress.reserve_bytes(1), ByteAdmission::Refused);
        assert_eq!(ingress.bytes.load(Ordering::Acquire), MAX_PENDING_BYTES);
        ingress.bytes.fetch_sub(MAX_PENDING_BYTES, Ordering::AcqRel);
        assert_eq!(ingress.reserve_bytes(1), ByteAdmission::Reserved);
    }

    #[test]
    fn capacity_refusal_keeps_supplied_after_and_labels_old_row_as_prior_only() {
        use crate::db::guild_data::{MessageSnapshot, SnapshotFields};
        let prior = MessageSnapshot {
            guild_id: 1,
            channel_id: 2,
            message_id: 3,
            fields: SnapshotFields {
                content: Some("A".into()),
                author_id: Some(42),
                ..Default::default()
            },
            created_at_ms: 1,
            first_observed_at_ms: 2,
            last_observed_at_ms: 3,
            edit_at_ms: None,
            expires_at_ms: 86_400_001,
            generation: 1,
            coverage_epoch: 10,
            continuous: true,
            content_supplied: false,
            attachments_supplied: false,
        };
        let mut report = MessageReport {
            retained_expiries: Default::default(),
            guild_id: GuildId::new(1),
            event: "message_edit",
            vars: std::collections::BTreeMap::new(),
        };
        report.vars.insert("after".into(), "B".into());
        report.vars.insert("attachments".into(), "2".into());
        let refused = SnapshotTransition {
            outcome: SnapshotOutcome::CapacityRefused,
            before: None,
            after: Some(prior),
        };
        apply_transition_report(&mut report, &refused);
        assert_eq!(report.vars["before"], "A");
        assert_eq!(report.vars["after"], "B");
        assert_eq!(report.vars["attachments"], "2");
        assert!(report.vars["coverage"].contains("not retained"));
        assert!(report.vars["coverage"].contains("gap"));

        let missing = SnapshotTransition {
            after: None,
            ..refused
        };
        apply_transition_report(&mut report, &missing);
        assert_eq!(report.vars["before"], "unavailable (not captured)");
        assert_eq!(report.vars["after"], "B");
        assert!(report.vars["coverage"].contains("prior observation unavailable"));
    }

    #[test]
    fn overview_usage_without_snapshot_count_still_triggers_pressure_cleanup() {
        let pressure = jmodguard::capacity::PRESSURE_BYTES;
        assert!(pressure_cleanup_due(pressure, None));
        assert!(pressure_cleanup_due(pressure, Some(1)));
        assert!(!pressure_cleanup_due(pressure, Some(0)));
        assert!(!pressure_cleanup_due(pressure - 1, None));
    }
}
