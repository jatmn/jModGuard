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
    GuildDataScope, GuildDataStore, SnapshotAttachment, SnapshotFields, SnapshotHealthState,
    SnapshotHealthUpdate, SnapshotObservation, SnapshotOutcome, SnapshotTransition,
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
            let mut collectors = self.collectors.lock();
            let generation = collectors.get(&shard).copied().unwrap_or(0).wrapping_add(1);
            collectors.insert(shard, generation);
            generation
        };
        self.contexts
            .lock()
            .insert(shard, (generation, context.clone()));
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
    }

    pub fn collector_active(&self, context: &Context) -> bool {
        self.collectors
            .lock()
            .contains_key(&(context.shard_id.get() as u64))
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
                    if !queued {
                        let report = logging::message_delete_report(
                            app,
                            guild,
                            event.channel_id,
                            event.message_id,
                        );
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
                        let mut transcript = logging::BulkTranscript::new(ids.len());
                        for id in ids {
                            transcript.push_unknown(id);
                        }
                        submit_bulk_report(app, &context, guild, event.channel_id, transcript);
                    }
                }
            }
            _ => {}
        }
    }

    pub async fn maintain(&self, app: &BotState) {
        self.maintain_at(app, None).await;
    }

    async fn maintain_at(&self, app: &BotState, now_override: Option<i64>) {
        let stores = match app.tenant.usage_stores().await {
            Ok(stores) => stores,
            Err(error) => {
                tracing::warn!("snapshot maintenance catalog unavailable: {error:#}");
                self.gap();
                return;
            }
        };
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
                        let mut seen = self.health_initialized.lock();
                        if seen.len() < 1024 {
                            seen.insert(store_id.to_owned());
                        }
                    } else {
                        self.restore_loss(store_id, lost);
                        self.cleanup_paused.lock().insert(store_id.to_owned());
                    }
                }
                let expired = match store.expire_snapshots(100, now).await {
                    Ok(count) => count,
                    Err(error) => {
                        tracing::warn!(store_id, "snapshot cleanup unavailable: {error:#}");
                        self.cleanup_paused.lock().insert(store_id.to_owned());
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
                    self.restore_loss(store_id, lost);
                }
            }
            tokio::task::yield_now().await;
        }
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
        let scope = match crate::db::guild_enrollment(&app.db, guild_id).await {
            Ok(Some(enrollment)) => app.tenant.scope(enrollment).await.ok(),
            _ => None,
        };
        let mut scopes = self.scopes.lock();
        if let Some(scope) = scope {
            scopes.insert(guild, scope);
        } else {
            scopes.remove(&guild);
            self.gap();
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
        self.gap_store(scope.placement().store_id().as_str());
    }

    fn gap_store(&self, store_id: &str) {
        self.gap();
        let key = store_id.to_owned();
        let mut pending = self.lost_pending.lock();
        if pending.contains_key(&key) || pending.len() < 1024 {
            let count = pending.entry(key).or_default();
            *count = count.saturating_add(1);
        }
    }

    fn take_loss(&self, store_id: &str) -> u64 {
        self.lost_pending.lock().remove(store_id).unwrap_or(0)
    }

    fn restore_loss(&self, store_id: &str, lost: u64) {
        if lost == 0 {
            return;
        }
        let mut pending = self.lost_pending.lock();
        if pending.contains_key(store_id) || pending.len() < 1024 {
            let count = pending.entry(store_id.to_owned()).or_default();
            *count = count.saturating_add(lost);
        }
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

    fn mark_deleted(&self, guild: GuildId, channel: ChannelId, ids: &[MessageId]) {
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

async fn run(app: Arc<BotState>, mut receiver: mpsc::Receiver<Work>) {
    let mut health: HashMap<String, (Instant, SnapshotHealthState, Option<&'static str>)> =
        HashMap::new();
    while let Some(mut work) = receiver.recv().await {
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
                        if let Some(id) = ids.first() {
                            let mut report =
                                logging::message_delete_report(&app, guild, channel, *id);
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
                        let mut transcript = logging::BulkTranscript::new(ids.len());
                        for id in ids {
                            transcript.push_unknown(id);
                        }
                        submit_bulk_report(&app, &work.context, guild, channel, transcript);
                    }
                    _ => {}
                }
                continue;
            }
        };
        let guild = work.scope.guild_id() as u64;
        let config = match app.configs.get(&app.db, guild).await {
            Ok(cfg) => Some(cfg),
            Err(error) => {
                tracing::warn!("message snapshot config unavailable: {error:#}");
                app.snapshots.gap_scope(&work.scope);
                None
            }
        };
        let configured = config.as_ref().is_some_and(|cfg| {
            ["message_edit", "message_delete", "message_bulk_delete"]
                .iter()
                .any(|event| cfg.log_channels.contains_key(*event))
        });
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
                match store
                    .observe_snapshot(work.scope.clone(), observation.clone())
                    .await
                {
                    Ok(SnapshotOutcome::Stored) => {
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
                    Ok(SnapshotOutcome::Duplicate | SnapshotOutcome::Stale) => {}
                    Ok(SnapshotOutcome::CapacityRefused) => {
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
                            tracing::warn!("snapshot pressure eviction unavailable: {error:#}");
                            app.snapshots
                                .cleanup_paused
                                .lock()
                                .insert(work.scope.placement().store_id().as_str().to_owned());
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
                    Ok(_) => app.snapshots.gap(),
                    Err(error) => {
                        tracing::warn!("message snapshot create unavailable: {error:#}");
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
                    ids.retain(|id| {
                        !app.snapshots
                            .deleted((guild.get(), channel.get(), id.get()))
                    });
                    if ids.is_empty() {
                        continue;
                    }
                }
                if !*bulk
                    && ids.first().is_some_and(|id| {
                        app.snapshots
                            .deleted((guild.get(), channel.get(), id.get()))
                    })
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
                                    if row
                                        .as_ref()
                                        .and_then(|row| row.fields.author_id)
                                        .is_some_and(|author| ignored_authors.contains(&author))
                                    {
                                        transcript.push_filtered();
                                    } else {
                                        transcript.push(*id, row.as_ref());
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
                                for id in chunk {
                                    transcript.push_unknown(*id);
                                }
                            } else {
                                single_unknown = true;
                            }
                        }
                    }
                }
                app.snapshots.mark_deleted(*guild, *channel, ids);
                if *bulk {
                    submit_bulk_report(&app, &work.context, *guild, *channel, transcript);
                } else if let Some(id) = ids.first() {
                    let mut report = logging::message_delete_report(&app, *guild, *channel, *id);
                    logging::apply_retained_delete(&mut report, single.as_ref());
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

fn submit_bulk_report(
    app: &Arc<BotState>,
    context: &Context,
    guild: GuildId,
    channel: ChannelId,
    transcript: logging::BulkTranscript,
) {
    let Ok(permit) = Arc::clone(&app.snapshots.report_slots).try_acquire_owned() else {
        return;
    };
    let app = Arc::clone(app);
    let context = context.clone();
    tokio::spawn(async move {
        let _permit = permit;
        logging::emit_retained_bulk(&app, &context, guild, channel, transcript).await;
    });
}

fn submit_report(app: &Arc<BotState>, context: &Context, report: MessageReport) {
    let Ok(permit) = Arc::clone(&app.snapshots.report_slots).try_acquire_owned() else {
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
            app.snapshots
                .maintain_at(&app, Some(now + 2 * 3_600_000))
                .await;
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
