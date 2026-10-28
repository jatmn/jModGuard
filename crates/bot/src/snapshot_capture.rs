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
    GuildDataScope, GuildDataStore, SNAPSHOT_CAPTURE_BATCH, SnapshotAttachment, SnapshotError,
    SnapshotFields, SnapshotHealthState, SnapshotHealthUpdate, SnapshotObservation,
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
    receiver: Mutex<Option<mpsc::Receiver<Work>>>,
    bytes: AtomicUsize,
    epoch: AtomicU64,
    started_at_ms: i64,
    /// Loss is charged only to the physical store whose scoped work failed.
    /// Unknown gateway/catalog gaps still advance the observation epoch.
    lost_pending: Mutex<HashMap<String, u64>>,
    scopes: Mutex<HashMap<u64, GuildDataScope>>,
    known_humans: Mutex<KnownHumans>,
    deleted: Mutex<DeletedIdentities>,
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
            receiver: Mutex::new(Some(receiver)),
            bytes: AtomicUsize::new(0),
            epoch: AtomicU64::new(chrono::Utc::now().timestamp_micros() as u64),
            started_at_ms: chrono::Utc::now().timestamp_millis(),
            lost_pending: Mutex::new(HashMap::new()),
            scopes: Mutex::new(HashMap::new()),
            known_humans: Mutex::new(KnownHumans::default()),
            deleted: Mutex::new(DeletedIdentities::default()),
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
                        // No collector queue. Admit a report when a slot is free.
                        // When it is not, still observe and settle so a pending
                        // automod id is not lost before REST finishes.
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
                            epoch: self.epoch.load(Ordering::Acquire),
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
                        epoch: self.epoch.load(Ordering::Acquire),
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
                epoch: self.epoch.load(Ordering::Acquire),
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
        let mut known = self.known_humans.lock();
        known.entries.retain(|(g, _, _)| *g != guild);
        known.order.retain(|((g, _, _), _)| *g != guild);
        self.content_access.lock().remove(&guild);
        self.gap();
    }

    pub fn gap(&self) {
        self.epoch.fetch_add(1, Ordering::Relaxed);
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
        deleted.exact.contains_key(&key) || deleted.bloom.contains(key)
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
        if !self.reserve_bytes(bytes) {
            self.gap_scope(&scope);
            return false;
        }
        self.start_worker(app);
        let item = Work {
            scope: scope.clone(),
            context,
            body,
            bytes,
            epoch: self.epoch.load(Ordering::Acquire),
        };
        if self.sender.try_send(item).is_err() {
            self.bytes.fetch_sub(bytes, Ordering::AcqRel);
            self.gap_scope(&scope);
            return false;
        }
        true
    }

    fn reserve_bytes(&self, bytes: usize) -> bool {
        self.bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                old.checked_add(bytes)
                    .filter(|next| *next <= MAX_PENDING_BYTES)
            })
            .is_ok()
    }

    fn start_worker(&self, app: &Arc<BotState>) {
        let receiver = self.receiver.lock().take();
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
    256usize
        .saturating_add(observation.fields.content.as_ref().map_or(0, String::len))
        .saturating_add(
            observation
                .fields
                .author_name
                .as_ref()
                .map_or(0, String::len),
        )
        .saturating_add(observation.fields.attachments.as_ref().map_or(0, |items| {
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
        match app.configs.get(&app.db, scope.guild_id() as u64).await {
            Ok(config) if capture_enabled(&config) => {
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
        let store = match app.tenant.store(&work.scope).await {
            Ok(store) => store,
            Err(error) => {
                tracing::warn!("message snapshot store unavailable: {error:#}");
                app.snapshots.gap_scope(&work.scope);
                if let Body::Delete {
                    guild,
                    channel,
                    ref ids,
                    ..
                } = work.body
                {
                    app.snapshots.mark_deleted(guild, channel, ids);
                }
                match work.body {
                    Body::Edit(_, Some(mut report), _) => {
                        report
                            .vars
                            .insert("coverage".into(), "snapshot storage unavailable".into());
                        submit_report(&app, &work.context, report);
                    }
                    Body::Delete {
                        guild,
                        channel,
                        ids,
                        bulk: false,
                    } => {
                        if let Some(id) = ids.first()
                            && let Some(mut report) =
                                logging::observed_delete_report(&app, guild, channel, *id, None)
                        {
                            report
                                .vars
                                .insert("coverage".into(), "snapshot storage unavailable".into());
                            submit_report(&app, &work.context, report);
                        }
                    }
                    Body::Delete {
                        guild,
                        channel,
                        ids,
                        bulk: true,
                    } => {
                        submit_peeled_bulk_deletes(&app, &work.context, guild, channel, ids).await;
                    }
                    _ => {}
                }
                continue;
            }
        };
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
                        report.vars.insert(
                            "coverage".into(),
                            "capture paused after cleanup failure".into(),
                        );
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
                        report
                            .vars
                            .insert("coverage".into(), "configuration unavailable".into());
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
                            report
                                .vars
                                .insert("coverage".into(), "snapshot storage unavailable".into());
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
                let ignored_authors: HashSet<i64> = config
                    .as_ref()
                    .and_then(|cfg| cfg.log_filters.get("message_bulk_delete"))
                    .map(|filter| {
                        filter
                            .users
                            .iter()
                            .filter_map(|id| id.parse::<i64>().ok())
                            .collect()
                    })
                    .unwrap_or_default();
                let automod_individual = config
                    .as_ref()
                    .is_some_and(|cfg| logging::channel_for(cfg, "automod_delete").is_some());
                let mut single = None;
                let mut single_unknown = false;
                for chunk in ids.chunks(100) {
                    let values = chunk.iter().map(|id| id.get() as i64).collect();
                    match store
                        .take_snapshots_at_epoch(
                            work.scope.clone(),
                            channel.get() as i64,
                            values,
                            now,
                            app.snapshots.epoch.load(Ordering::Acquire),
                        )
                        .await
                    {
                        Ok(rows) => {
                            for (id, row) in chunk.iter().zip(rows) {
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
                                    record_observed_bulk_member(
                                        &app,
                                        &work.context,
                                        &mut transcript,
                                        *id,
                                        row.as_ref(),
                                        placement,
                                        automod_individual,
                                        author_filtered,
                                        false,
                                    );
                                } else {
                                    single = row;
                                }
                            }
                        }
                        Err(error) => {
                            tracing::warn!("message snapshot delete unavailable: {error:#}");
                            app.snapshots.gap_scope(&work.scope);
                            if *bulk {
                                peel_automod_bulk_chunk(
                                    &app,
                                    &work.context,
                                    *guild,
                                    *channel,
                                    chunk,
                                    &mut transcript,
                                    automod_individual,
                                );
                            } else {
                                single_unknown = true;
                            }
                        }
                    }
                }
                app.snapshots.mark_deleted(*guild, *channel, ids);
                if *bulk {
                    let pending = transcript.pending_automod.clone();
                    if transcript.worth_sending() {
                        submit_bulk_report(&app, &work.context, *guild, *channel, transcript);
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
        epoch: app.snapshots.epoch.load(Ordering::Acquire),
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
            submit_peeled_bulk_deletes_admitted(&app, &context, guild, channel, ids).await;
        });
    } else {
        let app = Arc::clone(app);
        tokio::spawn(async move {
            abandon_unsent_bulk_deletes(&app, &context, guild, channel, &ids).await;
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn record_observed_bulk_member(
    app: &Arc<BotState>,
    context: &Context,
    transcript: &mut logging::BulkTranscript,
    id: MessageId,
    row: Option<&crate::db::guild_data::MessageSnapshot>,
    placement: logging::BulkDeletePlacement,
    automod_individual: bool,
    author_filtered: bool,
    storage_unknown: bool,
) {
    let report = placement.report;
    if logging::peel_automod_from_bulk(&report, automod_individual) {
        transcript.peel_automod_delete();
        if let Some(report) = report {
            submit_report(app, context, report);
        }
        return;
    }
    // Cap miss or a concurrent REST replay: the id has no bulk row, so it must
    // not stay in a transcript that can still be sent.
    if report.is_none() && !placement.reserved_pending {
        transcript.drop_unreserved();
        return;
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
}

pub(crate) fn peel_automod_bulk_chunk(
    app: &Arc<BotState>,
    context: &Context,
    guild: GuildId,
    channel: ChannelId,
    ids: &[MessageId],
    transcript: &mut logging::BulkTranscript,
    automod_individual: bool,
) {
    for id in ids {
        let placement =
            logging::place_bulk_delete(app, guild, channel, *id, None, !automod_individual);
        record_observed_bulk_member(
            app,
            context,
            transcript,
            *id,
            None,
            placement,
            automod_individual,
            false,
            true,
        );
    }
}

async fn submit_peeled_bulk_deletes(
    app: &Arc<BotState>,
    context: &Context,
    guild: GuildId,
    channel: ChannelId,
    ids: Vec<MessageId>,
) {
    let Some(permit) = acquire_report_slot(app) else {
        // The transcript cannot be sent. Still observe pending ids so a later
        // REST failure can fall back instead of finding no bulk delivery.
        abandon_unsent_bulk_deletes(app, context, guild, channel, &ids).await;
        return;
    };
    let app = Arc::clone(app);
    let context = context.clone();
    tokio::spawn(async move {
        let _permit = permit;
        submit_peeled_bulk_deletes_admitted(&app, &context, guild, channel, ids).await;
    });
}

async fn abandon_unsent_bulk_deletes(
    app: &Arc<BotState>,
    context: &Context,
    guild: GuildId,
    channel: ChannelId,
    ids: &[MessageId],
) {
    let cfg = app.configs.get(&app.db, guild.get()).await.ok();
    let automod_individual = cfg
        .as_ref()
        .is_some_and(|cfg| logging::channel_for(cfg, "automod_delete").is_some());
    let mut transcript = logging::BulkTranscript::new(ids.len());
    peel_automod_bulk_chunk(
        app,
        context,
        guild,
        channel,
        ids,
        &mut transcript,
        automod_individual,
    );
    let pending = transcript.pending_automod;
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
}

async fn submit_peeled_bulk_deletes_admitted(
    app: &Arc<BotState>,
    context: &Context,
    guild: GuildId,
    channel: ChannelId,
    ids: Vec<MessageId>,
) {
    let cfg = app.configs.get(&app.db, guild.get()).await.ok();
    let automod_individual = cfg
        .as_ref()
        .is_some_and(|cfg| logging::channel_for(cfg, "automod_delete").is_some());
    let mut transcript = logging::BulkTranscript::new(ids.len());
    peel_automod_bulk_chunk(
        app,
        context,
        guild,
        channel,
        &ids,
        &mut transcript,
        automod_individual,
    );
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

fn submit_bulk_report(
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
            let app = Arc::clone(app);
            let context = context.clone();
            tokio::spawn(async move {
                logging::settle_bulk_delivery(
                    &app,
                    context.http.as_ref(),
                    guild,
                    channel,
                    &pending,
                    logging::BulkDeliveryOutcome::Failed,
                )
                .await;
            });
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
    let Some(permit) = acquire_report_slot(app) else {
        return;
    };
    let app = Arc::clone(app);
    let context = context.clone();
    tokio::spawn(async move {
        let _permit = permit;
        logging::emit(&app, &context, report.guild_id, report.event, report.vars).await;
    });
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
        // behind when disabled. Re-enabling still permits new observations.
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
        let epoch = app.snapshots.epoch.load(Ordering::Acquire);
        assert!(
            store
                .take_snapshots_at_epoch(scope.clone(), 456, vec![10, 11], now, epoch)
                .await
                .is_err(),
            "closed membership must fail snapshot reads for the delete worker Err path"
        );
        let ctx = test_delete_context().await;
        let mut transcript = logging::BulkTranscript::new(2);
        peel_automod_bulk_chunk(
            &app,
            &ctx,
            GuildId::new(789),
            ChannelId::new(456),
            &[MessageId::new(10), MessageId::new(11)],
            &mut transcript,
            true,
        );
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
        use serenity::gateway::{
            Shard, ShardManager, ShardManagerOptions, ShardMessenger, ShardRunner,
            ShardRunnerOptions,
        };
        use serenity::model::gateway::{GatewayIntents, ShardInfo};
        use serenity::model::id::ShardId;
        let http = Arc::new(serenity::http::Http::new("fixture"));
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        // Test-only loopback WebSocket URL for a local accept() fixture, not production traffic.
        let addr = listener.local_addr().unwrap();
        let url = Arc::new(tokio::sync::Mutex::new(format!(
            "ws://127.0.0.1:{}",
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
        let before = app.snapshots.epoch.load(Ordering::Relaxed);
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
            app.snapshots.epoch.load(Ordering::Relaxed),
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
        let ctx = test_delete_context().await;
        let mut transcript = logging::BulkTranscript::new(1);
        peel_automod_bulk_chunk(
            &app,
            &ctx,
            GuildId::new(789),
            ChannelId::new(456),
            &[MessageId::new(overflow)],
            &mut transcript,
            false,
        );
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

    #[test]
    fn author_provenance_is_bounded_and_eviction_marks_a_gap() {
        let ingress = SnapshotIngress::default();
        let start_epoch = ingress.epoch.load(Ordering::Relaxed);
        for id in 1..=(MAX_KNOWN as u64 + 1) {
            ingress.remember_human((1, 2, id));
        }
        assert!(!ingress.known_human((1, 2, 1)));
        assert!(ingress.known_human((1, 2, MAX_KNOWN as u64 + 1)));
        assert_eq!(ingress.known_humans.lock().entries.len(), MAX_KNOWN);
        assert!(ingress.epoch.load(Ordering::Relaxed) > start_epoch);
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
        assert!(ingress.reserve_bytes(MAX_PENDING_BYTES));
        assert!(!ingress.reserve_bytes(1));
        assert_eq!(ingress.bytes.load(Ordering::Acquire), MAX_PENDING_BYTES);
        ingress.bytes.fetch_sub(MAX_PENDING_BYTES, Ordering::AcqRel);
        assert!(ingress.reserve_bytes(1));
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
