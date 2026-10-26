//! Short-window cross-channel attachment burst.
//!
//! Exact-image spam still requires a finished hash set. This tracker fires on
//! its own settings: a content hash when one exists, otherwise size, filename,
//! and content type. Account age is not an input.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serenity::builder::{
    Builder, CreateActionRow, CreateButton, CreateInteractionResponse,
    CreateInteractionResponseFollowup, CreateInteractionResponseMessage, EditInteractionResponse,
};
use serenity::client::Context;
use serenity::model::application::{ButtonStyle, ComponentInteraction};
use serenity::model::id::{GuildId, InteractionId, UserId};
use serenity::model::timestamp::Timestamp;

use crate::commands::ModeratorStatus;
use crate::state::BotState;
use crate::{cases, logging, templates};
use jmodguard::attachment_burst::Settings;
use jmodguard::defaults::{
    ATTACHMENT_BURST_BANNED, ATTACHMENT_BURST_DECISION, ATTACHMENT_BURST_DENIED,
    ATTACHMENT_BURST_FAILED, ATTACHMENT_BURST_HANDLED, ATTACHMENT_BURST_REASON,
    ATTACHMENT_BURST_RELEASED, ATTACHMENT_BURST_REVIEW_REASON, ATTACHMENT_BURST_TIMEOUT_ACTION,
    ATTACHMENT_BURST_TIMEOUT_MISSED, ATTACHMENT_BURST_TIMEOUT_UNCERTAIN,
};

const MAX_GUILDS: usize = 256;
const MAX_AUTHORS: usize = 512;
const MAX_AUTHORS_PER_GUILD: usize = 64;
const MAX_KEYS_PER_AUTHOR: usize = 32;
const MAX_CHANNELS_PER_KEY: usize = 16;
const MAX_STAGED: usize = 1024;
const MAX_MESSAGES: usize = 1024;
const MAX_IN_FLIGHT: usize = 4;
const MAX_REVIEWS: usize = 256;
const PRUNE_SLACK_SECS: u64 = 180;
const NAME_CHARS: usize = 256;
const TYPE_CHARS: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
enum Key {
    Hash([u8; 32]),
    Fallback {
        size: u64,
        filename: String,
        content_type: String,
    },
}

#[derive(Clone, Copy)]
pub(crate) struct Sighting {
    pub guild: u64,
    pub author: u64,
    pub channel: u64,
    pub message: u64,
    pub at: Instant,
}

pub(crate) struct Admission {
    pub revision: u64,
    pub edited_ms: Option<i64>,
    pub received: Instant,
    pub eligible: bool,
}

pub(crate) struct FileMeta {
    pub attachment_id: u64,
    pub size: u64,
    pub filename: String,
    pub content_type: String,
}

struct Staged {
    generation: u64,
    sighting: Sighting,
    size: u64,
    filename: String,
    content_type: String,
    fallback: bool,
}

struct ChannelSight {
    message: u64,
    at: Instant,
    lifetime: Arc<()>,
    claimed_at: Option<Instant>,
}

struct KeyBucket {
    channels: BTreeMap<u64, ChannelSight>,
    acted: bool,
    newest: Instant,
}

struct AuthorState {
    keys: HashMap<Key, KeyBucket>,
    newest: Instant,
}

struct GuildState {
    authors: HashMap<u64, AuthorState>,
    newest: Instant,
}

struct Review {
    guild: u64,
    user: u64,
    body: String,
    at: Instant,
    enrollment: Option<crate::db::GuildEnrollment>,
}

struct MessageNote {
    author: u64,
    // Historical ownership survives channel replacement and hash promotion.
    // Production admits <=10 files, each contributing <=1 fallback +1 hash.
    keys: Vec<Key>,
    at: Instant,
    attachments: BTreeSet<u64>,
    edited_ms: i64,
    received: Instant,
    invalidated_revision: u64,
    deleted: bool,
    lifetime: Option<Arc<()>>,
}

struct State {
    guilds: HashMap<u64, GuildState>,
    authors: usize,
    staged: HashMap<u64, Staged>,
    messages: HashMap<(u64, u64, u64), MessageNote>,
    reviews: HashMap<u64, Review>,
    next_review: u64,
    next_generation: u64,
    revision: u64,
    scope_revision: u64,
    retired_through: u64,
}

pub(crate) struct Hit {
    guild: u64,
    author: u64,
    review_id: u64,
    channels: Vec<u64>,
    messages: Vec<(u64, u64)>,
    key: Key,
    timeout_secs: u64,
    window_secs: u64,
    proof: Vec<Weak<()>>,
}

impl Hit {
    fn still_valid(&self) -> bool {
        self.proof.iter().all(|sight| sight.strong_count() > 0)
    }
}

type TimeoutLocks = HashMap<(u64, u64), Weak<tokio::sync::Mutex<()>>>;

pub struct Tracker {
    settings: Settings,
    process_nonce: u64,
    state: Mutex<State>,
    hits: AtomicU64,
    slots: Arc<tokio::sync::Semaphore>,
    timeout_locks: Mutex<TimeoutLocks>,
}

impl Tracker {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            process_nonce: fresh_nonce(),
            state: Mutex::new(State {
                guilds: HashMap::new(),
                authors: 0,
                staged: HashMap::new(),
                messages: HashMap::new(),
                reviews: HashMap::new(),
                next_review: 1,
                next_generation: 1,
                revision: 0,
                scope_revision: 0,
                retired_through: 0,
            }),
            hits: AtomicU64::new(0),
            slots: Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT)),
            timeout_locks: Mutex::new(HashMap::new()),
        }
    }

    pub fn prune(&self) {
        let mut state = self.state.lock();
        if !self.settings.enabled {
            state.clear();
            return;
        }
        let window =
            Duration::from_secs(self.settings.window_secs.saturating_add(PRUNE_SLACK_SECS));
        state.drop_expired(Instant::now(), window, self.settings.min_channels);
        state.drop_old_reviews(Instant::now(), Duration::from_secs(86_400));
    }

    pub(crate) fn accepts(&self) -> bool {
        self.usable()
    }

    pub(crate) fn window_secs(&self) -> u64 {
        self.settings.window_secs
    }

    #[cfg(test)]
    pub(crate) fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn staged_generation(&self, attachment_id: u64) -> Option<u64> {
        self.state
            .lock()
            .staged
            .get(&attachment_id)
            .filter(|stage| !stage.fallback)
            .map(|staged| staged.generation)
    }

    /// Returns the generation stored with this file. A later restage replaces it.
    #[cfg(test)]
    pub(crate) fn stage(&self, sighting: Sighting, file: FileMeta) -> u64 {
        if !self.usable() {
            self.state.lock().clear();
            return 0;
        }
        let mut state = self.state.lock();
        Self::stage_locked(&mut state, sighting, file, self.settings.min_channels)
    }

    fn stage_locked(
        state: &mut State,
        sighting: Sighting,
        file: FileMeta,
        min_channels: usize,
    ) -> u64 {
        if state.staged.len() >= MAX_STAGED && !state.staged.contains_key(&file.attachment_id) {
            state.drop_oldest_staged();
        }
        let generation = state.next_generation;
        state.next_generation = state.next_generation.wrapping_add(1).max(1);
        let message = (sighting.guild, sighting.channel, sighting.message);
        if !state.messages.contains_key(&message) {
            state.remember_message(sighting, BTreeSet::new(), min_channels);
        }
        state
            .messages
            .get_mut(&message)
            .expect("message")
            .attachments
            .insert(file.attachment_id);
        state.staged.insert(
            file.attachment_id,
            Staged {
                generation,
                sighting,
                size: file.size,
                filename: bound_chars(&file.filename, NAME_CHARS),
                content_type: media_type(&file.content_type),
                fallback: false,
            },
        );
        generation
    }

    #[cfg(test)]
    fn resolve_hash(&self, attachment_id: u64, hash: [u8; 32]) -> Option<Hit> {
        self.resolve_hash_at(attachment_id, self.staged_generation(attachment_id)?, hash)
    }

    pub(crate) fn resolve_hash_at(
        &self,
        attachment_id: u64,
        generation: u64,
        hash: [u8; 32],
    ) -> Option<Hit> {
        let mut state = self.state.lock();
        let staged = state.take_staged(attachment_id, generation)?;
        let was_acted = state.retract_fallback(&staged);
        self.record_locked(&mut state, staged.sighting, Key::Hash(hash), was_acted)
    }

    #[cfg(test)]
    fn resolve_incomplete(&self, attachment_id: u64) -> Option<Hit> {
        self.resolve_incomplete_at_any(attachment_id)
    }

    pub(crate) fn resolve_incomplete_at(&self, attachment_id: u64, generation: u64) -> Option<Hit> {
        let mut state = self.state.lock();
        let staged = state.staged.get_mut(&attachment_id)?;
        if staged.generation != generation || staged.fallback {
            return None;
        }
        // Retain the exact observation for a later job that hashes this post.
        // The existing stage cap and message invalidation still own its lifetime.
        staged.fallback = true;
        let sighting = staged.sighting;
        let key = fallback_key(staged);
        self.record_locked(&mut state, sighting, key, false)
    }

    #[cfg(test)]
    fn resolve_incomplete_at_any(&self, attachment_id: u64) -> Option<Hit> {
        self.resolve_incomplete_at(attachment_id, self.staged_generation(attachment_id)?)
    }

    pub(crate) fn forget_message(&self, guild: u64, channel: u64, message: u64) {
        self.invalidate_message(guild, channel, message, true);
    }

    pub(crate) fn invalidate_message(&self, guild: u64, channel: u64, message: u64, deleted: bool) {
        self.invalidate_message_at(guild, channel, message, deleted, None);
    }

    pub(crate) fn invalidate_message_at(
        &self,
        guild: u64,
        channel: u64,
        message: u64,
        deleted: bool,
        edited_ms: Option<i64>,
    ) {
        let mut state = self.state.lock();
        if !deleted
            && edited_ms.is_some_and(|edited| {
                state
                    .messages
                    .get(&(guild, channel, message))
                    .is_some_and(|note| edited < note.edited_ms)
            })
        {
            return;
        }
        state.revision = state.revision.wrapping_add(1);
        let invalidated_revision = state.revision;
        state.forget_message(guild, channel, message, self.settings.min_channels);
        let key = (guild, channel, message);
        if !state.messages.contains_key(&key) {
            state.remember_message(
                Sighting {
                    guild,
                    author: 0,
                    channel,
                    message,
                    at: Instant::now(),
                },
                BTreeSet::new(),
                self.settings.min_channels,
            );
        }
        if let Some(note) = state.messages.get_mut(&key) {
            note.invalidated_revision = invalidated_revision;
            note.deleted |= deleted;
            note.lifetime = None;
            note.keys.clear();
            if let Some(edited_ms) = edited_ms {
                note.edited_ms = note.edited_ms.max(edited_ms);
            }
        }
    }

    pub(crate) fn revision(&self) -> u64 {
        self.state.lock().revision
    }

    pub(crate) fn author_of(&self, guild: u64, channel: u64, message: u64) -> Option<u64> {
        self.state
            .lock()
            .messages
            .get(&(guild, channel, message))
            .filter(|note| !note.deleted && note.author != 0)
            .map(|note| note.author)
    }

    pub(crate) fn invalidate_author(&self, guild: u64, author: u64) {
        self.invalidate_scope(guild, Some(author));
    }

    pub(crate) fn invalidate_guild(&self, guild: u64) {
        self.invalidate_scope(guild, None);
    }

    fn invalidate_scope(&self, guild: u64, author: Option<u64>) {
        let mut state = self.state.lock();
        state.revision = state.revision.wrapping_add(1);
        state.scope_revision = state.revision;
        for ((seen_guild, _, _), note) in &mut state.messages {
            if *seen_guild == guild && author.is_none_or(|a| a == note.author) {
                note.lifetime = None;
                note.keys.clear();
                note.deleted = true;
            }
        }
        state.staged.retain(|_, staged| {
            staged.sighting.guild != guild || author.is_some_and(|a| a != staged.sighting.author)
        });
        if let Some(author) = author {
            if let Some(guild) = state.guilds.get_mut(&guild)
                && guild.authors.remove(&author).is_some()
            {
                state.authors = state.authors.saturating_sub(1);
            }
        } else if let Some(guild) = state.guilds.remove(&guild) {
            state.authors = state.authors.saturating_sub(guild.authors.len());
        }
    }

    fn stage_sighting(
        &self,
        sighting: Sighting,
        files: Vec<FileMeta>,
        admission: Admission,
    ) -> Vec<(u64, u64)> {
        let mut state = self.state.lock();
        let note = state
            .messages
            .get(&(sighting.guild, sighting.channel, sighting.message));
        let stale = admission.revision < state.scope_revision
            || note.is_some_and(|note| admission.revision < note.invalidated_revision);
        // Rejected observations may revoke a known superseded snapshot, but
        // must not allocate new evidence or ordering notes for unknown messages.
        if (stale || !admission.eligible) && note.is_none() {
            return Vec::new();
        }
        if !self.usable()
            || !self.prepare_locked(
                &mut state,
                sighting,
                &files.iter().map(|f| f.attachment_id).collect::<Vec<_>>(),
                admission.edited_ms,
                admission.received,
            )
            || stale
            || !admission.eligible
        {
            return Vec::new();
        }
        files
            .into_iter()
            .map(|file| {
                let id = file.attachment_id;
                (
                    id,
                    Self::stage_locked(&mut state, sighting, file, self.settings.min_channels),
                )
            })
            .collect()
    }

    /// False when this message already carries the same attachment ids.
    /// A replay keeps the original sighting time and does not clear a hit.
    #[cfg(test)]
    pub(crate) fn prepare_sighting(&self, sighting: Sighting, attachment_ids: &[u64]) -> bool {
        if !self.usable() {
            self.state.lock().clear();
            return false;
        }
        let mut state = self.state.lock();
        self.prepare_locked(&mut state, sighting, attachment_ids, None, Instant::now())
    }

    fn prepare_locked(
        &self,
        state: &mut State,
        sighting: Sighting,
        attachment_ids: &[u64],
        edited_ms: Option<i64>,
        received: Instant,
    ) -> bool {
        let wanted = attachment_ids.iter().copied().collect::<BTreeSet<_>>();
        let key = (sighting.guild, sighting.channel, sighting.message);
        if !state.messages.contains_key(&key) && sighting.message <= state.retired_through {
            return false;
        }
        let mut watermark = edited_ms.unwrap_or(0);
        let mut invalidated_revision = 0;
        if let Some(note) = state.messages.get_mut(&key) {
            if note.deleted
                || edited_ms.is_some_and(|edited| edited < note.edited_ms)
                || (edited_ms.is_none() && received < note.received)
            {
                return false;
            }
            watermark = watermark.max(note.edited_ms);
            invalidated_revision = note.invalidated_revision;
            if note.author == sighting.author && note.attachments == wanted {
                note.edited_ms = watermark;
                note.received = note.received.max(received);
                return false;
            }
        }
        let min_channels = self.settings.min_channels;
        state.forget_message(
            sighting.guild,
            sighting.channel,
            sighting.message,
            min_channels,
        );
        state.remember_message(sighting, wanted, min_channels);
        let note = state.messages.get_mut(&key).expect("remembered message");
        note.edited_ms = watermark;
        note.received = received;
        note.invalidated_revision = invalidated_revision;
        true
    }

    fn component_id(&self, action: &str, review_id: u64) -> String {
        format!("jmgburst:{:016x}:{action}:{review_id}", self.process_nonce)
    }

    fn open_review(&self, hit: &Hit, enrollment: Option<crate::db::GuildEnrollment>, body: String) {
        let mut state = self.state.lock();
        if state.reviews.len() >= MAX_REVIEWS {
            state.drop_oldest_review();
        }
        state.reviews.insert(
            hit.review_id,
            Review {
                guild: hit.guild,
                user: hit.author,
                body: templates::render_bounded(
                    "{body}",
                    &BTreeMap::from([("body".to_string(), body)]),
                    1800,
                ),
                at: Instant::now(),
                enrollment,
            },
        );
    }

    fn claim_review(&self, review_id: u64, guild: u64, nonce: u64) -> Option<Review> {
        if nonce != self.process_nonce {
            return None;
        }
        let mut state = self.state.lock();
        let review = state.reviews.get(&review_id)?;
        if review.guild != guild {
            return None;
        }
        state.reviews.remove(&review_id)
    }

    fn restore_review(&self, review_id: u64, review: Review) {
        let mut state = self.state.lock();
        if state.reviews.len() >= MAX_REVIEWS {
            state.drop_oldest_review();
        }
        state.reviews.insert(review_id, review);
    }

    fn usable(&self) -> bool {
        self.settings.enabled
            && (2..=10).contains(&self.settings.min_channels)
            && self.settings.window_secs > 0
    }

    #[cfg(test)]
    fn record(&self, sighting: Sighting, key: Key) -> Option<Hit> {
        self.record_locked(&mut self.state.lock(), sighting, key, false)
    }

    // Settlement and invalidation share this lock, so a removed stage cannot
    // be reintroduced as a live sight after forget_message has completed.
    fn record_locked(
        &self,
        state: &mut State,
        sighting: Sighting,
        key: Key,
        was_acted: bool,
    ) -> Option<Hit> {
        if !self.usable() {
            state.clear();
            return None;
        }
        let window = Duration::from_secs(self.settings.window_secs);
        let min_channels = self.settings.min_channels;
        state.prepare(sighting.guild, min_channels);
        let message_key = (sighting.guild, sighting.channel, sighting.message);
        // The physical message owns pending evidence, even when its channel's
        // representative is replaced by a later post of the same attachment.
        if !state.messages.contains_key(&message_key) {
            if sighting.message <= state.retired_through {
                return None;
            }
            state.remember_message(sighting, BTreeSet::new(), min_channels);
        }
        state.messages.get(&message_key)?.lifetime.as_ref()?;
        state.evict_key(sighting, &key, min_channels);
        // Eviction can retire another attachment of this same physical message.
        // Acquire its proof only after retirement has finished.
        let note = state.messages.get_mut(&message_key)?;
        let lifetime = note.lifetime.clone()?;
        if !note.keys.contains(&key) {
            note.keys.push(key.clone());
        }
        let author_exists = state
            .guilds
            .get(&sighting.guild)
            .is_some_and(|guild| guild.authors.contains_key(&sighting.author));
        if !author_exists {
            let guild_full = state
                .guilds
                .get(&sighting.guild)
                .is_some_and(|guild| guild.authors.len() >= MAX_AUTHORS_PER_GUILD);
            if guild_full || state.authors >= MAX_AUTHORS {
                state.evict_author(guild_full.then_some(sighting.guild), min_channels);
            }
            if state.authors >= MAX_AUTHORS {
                return None;
            }
        }
        let inserted_author = {
            let guild_state = state
                .guilds
                .entry(sighting.guild)
                .or_insert_with(|| GuildState {
                    authors: HashMap::new(),
                    newest: sighting.at,
                });
            guild_state.newest = guild_state.newest.max(sighting.at);
            if let std::collections::hash_map::Entry::Vacant(entry) =
                guild_state.authors.entry(sighting.author)
            {
                entry.insert(AuthorState {
                    keys: HashMap::new(),
                    newest: sighting.at,
                });
                true
            } else {
                false
            }
        };
        if inserted_author {
            state.authors += 1;
        }
        let noted = {
            let guild_state = state.guilds.get_mut(&sighting.guild)?;
            let author_state = guild_state.authors.get_mut(&sighting.author)?;
            author_state.newest = author_state.newest.max(sighting.at);
            let bucket = author_state
                .keys
                .entry(key.clone())
                .or_insert_with(|| KeyBucket {
                    channels: BTreeMap::new(),
                    acted: false,
                    newest: sighting.at,
                });
            if !bucket.channels.contains_key(&sighting.channel)
                && bucket.channels.len() >= MAX_CHANNELS_PER_KEY
            {
                return None;
            }
            let inherited = bucket
                .channels
                .get(&sighting.channel)
                .and_then(|current| current.claimed_at);
            let claimed_at = if was_acted {
                Some(inherited.map_or(sighting.at, |at| at.max(sighting.at)))
            } else {
                inherited
            };
            if bucket
                .channels
                .get(&sighting.channel)
                .is_none_or(|current| current.at <= sighting.at)
            {
                bucket.channels.insert(
                    sighting.channel,
                    ChannelSight {
                        message: sighting.message,
                        at: sighting.at,
                        lifetime,
                        claimed_at,
                    },
                );
            } else if was_acted && let Some(current) = bucket.channels.get_mut(&sighting.channel) {
                current.claimed_at = claimed_at;
            }
            let newest = bucket
                .channels
                .values()
                .map(|sight| sight.at)
                .max()
                .unwrap_or(sighting.at);
            bucket
                .channels
                .retain(|_, sight| newest.saturating_duration_since(sight.at) <= window);
            for sight in bucket.channels.values_mut() {
                if sight
                    .claimed_at
                    .is_some_and(|at| newest.saturating_duration_since(at) > window)
                {
                    sight.claimed_at = None;
                }
            }
            bucket.newest = bucket
                .channels
                .values()
                .map(|sight| sight.at)
                .max()
                .unwrap_or(newest);
            if bucket.channels.len() < min_channels {
                bucket.acted = false;
                return None;
            }
            if bucket.acted
                || bucket
                    .channels
                    .values()
                    .any(|sight| sight.claimed_at.is_some())
            {
                bucket.acted = true;
                return None;
            }
            bucket.acted = true;
            let channels: Vec<u64> = bucket.channels.keys().copied().collect();
            let messages = channels
                .iter()
                .map(|channel| (*channel, bucket.channels[channel].message))
                .collect();
            let proof = bucket
                .channels
                .values()
                .map(|sight| Arc::downgrade(&sight.lifetime))
                .collect();
            Some((channels, messages, proof))
        }?;
        let (channels, messages, proof) = noted;
        let review_id = state.next_review;
        state.next_review = state.next_review.wrapping_add(1).max(1);
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(Hit {
            guild: sighting.guild,
            author: sighting.author,
            review_id,
            channels,
            messages,
            key,
            timeout_secs: self.settings.timeout_secs,
            window_secs: self.settings.window_secs,
            proof,
        })
    }
}

impl State {
    // Remove only this physical observation's obsolete metadata contribution.
    // Another file in the same message may still own the same fallback key.
    fn retract_fallback(&mut self, staged: &Staged) -> bool {
        if !staged.fallback {
            return false;
        }
        let sight = staged.sighting;
        let key = fallback_key(staged);
        let another = self
            .messages
            .get(&(sight.guild, sight.channel, sight.message))
            .is_some_and(|note| {
                note.attachments.iter().any(|id| {
                    self.staged
                        .get(id)
                        .is_some_and(|other| other.fallback && fallback_key(other) == key)
                })
            });
        let Some(author) = self
            .guilds
            .get_mut(&sight.guild)
            .and_then(|guild| guild.authors.get_mut(&sight.author))
        else {
            return false;
        };
        let Some(bucket) = author.keys.get_mut(&key) else {
            return false;
        };
        let acted = bucket.acted;
        if !another
            && bucket
                .channels
                .get(&sight.channel)
                .is_some_and(|current| current.message == sight.message)
        {
            bucket.channels.remove(&sight.channel);
        }
        // Preserve the old claim while its other observations are promoted.
        if bucket.channels.is_empty() {
            author.keys.remove(&key);
        }
        acted
    }

    fn take_staged(&mut self, attachment_id: u64, generation: u64) -> Option<Staged> {
        match self.staged.get(&attachment_id) {
            Some(staged) if staged.generation == generation => self.staged.remove(&attachment_id),
            _ => None,
        }
    }

    fn clear(&mut self) {
        self.guilds.clear();
        self.authors = 0;
        self.staged.clear();
        self.messages.clear();
        self.reviews.clear();
    }

    fn prepare(&mut self, guild: u64, min_channels: usize) {
        // Global expiry belongs to the existing periodic prune, not message work.
        if !self.guilds.contains_key(&guild) && self.guilds.len() >= MAX_GUILDS {
            self.remove_oldest_guild_except(guild, min_channels);
        }
    }

    fn drop_expired(&mut self, now: Instant, slack: Duration, min_channels: usize) {
        let guilds: Vec<u64> = self.guilds.keys().copied().collect();
        for guild in guilds {
            let Some(guild_state) = self.guilds.get_mut(&guild) else {
                continue;
            };
            let authors: Vec<u64> = guild_state.authors.keys().copied().collect();
            for author in authors {
                let Some(author_state) = guild_state.authors.get_mut(&author) else {
                    continue;
                };
                author_state
                    .keys
                    .retain(|_, bucket| now.saturating_duration_since(bucket.newest) <= slack);
                if author_state.keys.is_empty() {
                    guild_state.authors.remove(&author);
                    self.authors = self.authors.saturating_sub(1);
                }
            }
            if guild_state.authors.is_empty() {
                self.guilds.remove(&guild);
            }
        }
        let stale: Vec<u64> = self
            .staged
            .iter()
            .filter(|(_, staged)| now.saturating_duration_since(staged.sighting.at) > slack)
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            self.staged.remove(&id);
        }
        let stale: Vec<_> = self
            .messages
            .iter()
            .filter(|(_, note)| now.saturating_duration_since(note.at) > slack)
            .map(|(key, _)| *key)
            .collect();
        for (guild, channel, message) in stale {
            // A suspended callback can outlive this bounded ordering note.
            // Keep its invalidation floor without retaining another registry.
            if let Some(note) = self.messages.get(&(guild, channel, message)) {
                self.scope_revision = self.scope_revision.max(note.invalidated_revision);
            }
            self.forget_message(guild, channel, message, min_channels);
            self.messages.remove(&(guild, channel, message));
        }
    }

    fn drop_old_reviews(&mut self, now: Instant, keep: Duration) {
        self.reviews
            .retain(|_, review| now.saturating_duration_since(review.at) <= keep);
    }

    fn evict_key(&mut self, sighting: Sighting, incoming: &Key, min_channels: usize) {
        let Some(author) = self
            .guilds
            .get_mut(&sighting.guild)
            .and_then(|guild| guild.authors.get_mut(&sighting.author))
        else {
            return;
        };
        if author.keys.contains_key(incoming) || author.keys.len() < MAX_KEYS_PER_AUTHOR {
            return;
        }
        let Some(oldest) = author
            .keys
            .iter()
            .min_by_key(|(_, bucket)| bucket.newest)
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        author.keys.remove(&oldest);
        // The current channel representatives omit older physical owners that
        // may still have pending hashes or enforcement proof for this key.
        let messages: Vec<_> = self
            .messages
            .iter()
            .filter(|((guild, _, _), note)| {
                *guild == sighting.guild
                    && note.author == sighting.author
                    && note.keys.contains(&oldest)
            })
            .map(|((_, channel, message), _)| (*channel, *message))
            .collect();
        for (channel, message) in messages {
            self.forget_message(sighting.guild, channel, message, min_channels);
        }
    }

    fn evict_author(&mut self, within_guild: Option<u64>, min_channels: usize) {
        // A per-guild cap evicts locally; a process-wide cap needs a victim
        // across guilds, including when the incoming guild is not tracked yet.
        let oldest = self
            .guilds
            .iter()
            .filter(|(guild, _)| within_guild.is_none_or(|only| only == **guild))
            .flat_map(|(guild, state)| {
                state
                    .authors
                    .iter()
                    .map(move |(author, state)| ((*guild, *author), state.newest))
            })
            .min_by_key(|(_, newest)| *newest)
            .map(|(key, _)| key);
        if let Some((guild, author)) = oldest {
            // Retire physical owners before losing their one-shot claims.
            // Keep the ordering notes so unchanged snapshots remain replays.
            let messages: Vec<_> = self
                .messages
                .iter()
                .filter(|((seen, _, _), note)| *seen == guild && note.author == author)
                .map(|(key, _)| *key)
                .collect();
            for (guild, channel, message) in messages {
                self.forget_message(guild, channel, message, min_channels);
            }
            if let Some(guild_state) = self.guilds.get_mut(&guild)
                && guild_state.authors.remove(&author).is_some()
            {
                self.authors = self.authors.saturating_sub(1);
            }
        }
    }

    fn remove_oldest_guild_except(&mut self, keep: u64, min_channels: usize) {
        let oldest = self
            .guilds
            .iter()
            .filter(|(guild, _)| **guild != keep)
            .min_by_key(|(_, guild)| guild.newest)
            .map(|(guild, _)| *guild);
        if let Some(guild) = oldest
            && let Some(removed) = self.guilds.remove(&guild)
        {
            self.authors = self.authors.saturating_sub(removed.authors.len());
            let messages: Vec<_> = self
                .messages
                .keys()
                .filter(|(seen, _, _)| *seen == guild)
                .copied()
                .collect();
            for (guild, channel, message) in messages {
                self.retired_through = self.retired_through.max(message);
                self.forget_message(guild, channel, message, min_channels);
                self.messages.remove(&(guild, channel, message));
            }
        }
    }

    fn drop_oldest_staged(&mut self) {
        let oldest = self
            .staged
            .iter()
            .min_by_key(|(_, staged)| staged.sighting.at)
            .map(|(id, _)| *id);
        if let Some(id) = oldest {
            self.staged.remove(&id);
        }
    }

    fn drop_oldest_review(&mut self) {
        let oldest = self
            .reviews
            .iter()
            .min_by_key(|(_, review)| review.at)
            .map(|(id, _)| *id);
        if let Some(id) = oldest {
            self.reviews.remove(&id);
        }
    }

    fn remember_message(
        &mut self,
        sighting: Sighting,
        attachments: BTreeSet<u64>,
        min_channels: usize,
    ) {
        let key = (sighting.guild, sighting.channel, sighting.message);
        if self.messages.len() >= MAX_MESSAGES && !self.messages.contains_key(&key) {
            let oldest = self
                .messages
                .iter()
                .min_by_key(|(_, note)| note.at)
                .map(|(key, _)| *key);
            if let Some(oldest) = oldest {
                self.retired_through = self.retired_through.max(oldest.2);
                self.forget_message(oldest.0, oldest.1, oldest.2, min_channels);
                self.messages.remove(&oldest);
            }
        }
        self.messages.insert(
            key,
            MessageNote {
                author: sighting.author,
                keys: Vec::new(),
                at: sighting.at,
                attachments,
                edited_ms: 0,
                received: sighting.at,
                invalidated_revision: 0,
                deleted: false,
                lifetime: Some(Arc::new(())),
            },
        );
    }

    fn forget_message(&mut self, guild: u64, channel: u64, message: u64, min_channels: usize) {
        let Some(note) = self.messages.get_mut(&(guild, channel, message)) else {
            return;
        };
        note.lifetime = None;
        note.keys.clear();
        let author = note.author;
        for id in &note.attachments {
            if self.staged.get(id).is_some_and(|stage| {
                (
                    stage.sighting.guild,
                    stage.sighting.channel,
                    stage.sighting.message,
                ) == (guild, channel, message)
            }) {
                self.staged.remove(id);
            }
        }
        let Some(guild_state) = self.guilds.get_mut(&guild) else {
            return;
        };
        if let Some(author_state) = guild_state.authors.get_mut(&author) {
            author_state.keys.retain(|_, bucket| {
                if bucket
                    .channels
                    .get(&channel)
                    .is_some_and(|sight| sight.message == message)
                {
                    bucket.channels.remove(&channel);
                }
                if bucket.channels.len() < min_channels {
                    bucket.acted = false;
                }
                !bucket.channels.is_empty()
            });
            if author_state.keys.is_empty() {
                guild_state.authors.remove(&author);
                self.authors = self.authors.saturating_sub(1);
            }
        }
        if guild_state.authors.is_empty() {
            self.guilds.remove(&guild);
        }
    }
}

fn fresh_nonce() -> u64 {
    let mut bytes = [0u8; 8];
    let read = std::fs::File::open("/dev/urandom").and_then(|mut file| {
        use std::io::Read as _;
        file.read_exact(&mut bytes)
    });
    if read.is_ok() {
        let value = u64::from_ne_bytes(bytes);
        if value != 0 {
            return value;
        }
    }
    let tick = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(1);
    let mixed = tick ^ u64::from(std::process::id()).rotate_left(17) ^ 0xA5A5_A5A5_A5A5_A5A5;
    if mixed == 0 { 1 } else { mixed }
}

fn bound_chars(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

fn media_type(value: &str) -> String {
    let head = value.split(';').next().unwrap_or("").trim();
    bound_chars(head, TYPE_CHARS).to_ascii_lowercase()
}

fn fallback_key(staged: &Staged) -> Key {
    Key::Fallback {
        size: staged.size,
        filename: staged.filename.clone(),
        content_type: staged.content_type.clone(),
    }
}

/// Holds image identities until a hash job accepts them or this value is dropped.
pub(crate) struct Handoff {
    app: Option<Arc<BotState>>,
    ctx: Option<Context>,
    ids: Vec<(u64, u64)>,
    armed: bool,
}

impl Handoff {
    pub(crate) fn inert() -> Self {
        Self {
            app: None,
            ctx: None,
            ids: Vec::new(),
            armed: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn stage(
        app: &Arc<BotState>,
        ctx: &Context,
        sighting: Sighting,
        files: Vec<FileMeta>,
    ) -> Self {
        Self::stage_at(
            app,
            ctx,
            sighting,
            files,
            Admission {
                revision: app.attachment_burst.revision(),
                edited_ms: None,
                received: Instant::now(),
                eligible: true,
            },
        )
    }

    pub(crate) fn stage_at(
        app: &Arc<BotState>,
        ctx: &Context,
        sighting: Sighting,
        files: Vec<FileMeta>,
        admission: Admission,
    ) -> Self {
        let ids = app
            .attachment_burst
            .stage_sighting(sighting, files, admission);
        let armed = !ids.is_empty();
        Self {
            app: Some(Arc::clone(app)),
            ctx: Some(ctx.clone()),
            ids,
            armed,
        }
    }

    pub(crate) fn hand_to_job(&mut self) -> Vec<(u64, u64)> {
        self.armed = false;
        std::mem::take(&mut self.ids)
    }

    pub(crate) fn observations(&self) -> Vec<(u64, u64)> {
        self.ids.clone()
    }

    pub(crate) fn finish_incomplete(&mut self) {
        self.armed = false;
        let (Some(app), Some(ctx)) = (self.app.as_ref(), self.ctx.as_ref()) else {
            return;
        };
        let hits = self
            .ids
            .drain(..)
            .filter_map(|(id, generation)| {
                app.attachment_burst.resolve_incomplete_at(id, generation)
            })
            .collect();
        dispatch(app, ctx, hits);
    }
}

impl Drop for Handoff {
    fn drop(&mut self) {
        if self.armed {
            self.finish_incomplete();
        }
    }
}

/// Resolves any file the hash job did not turn into a hash.
pub(crate) struct PendingBurst {
    app: Arc<BotState>,
    ctx: Context,
    ids: Vec<(u64, u64)>,
}

impl PendingBurst {
    pub(crate) fn new(app: &Arc<BotState>, ctx: &Context, ids: Vec<(u64, u64)>) -> Self {
        Self {
            app: Arc::clone(app),
            ctx: ctx.clone(),
            ids,
        }
    }

    pub(crate) fn resolve_hash(&self, id: u64, hash: jmodguard::image_spam::Hash) {
        let Some((_, generation)) = self.ids.iter().find(|(attachment, _)| *attachment == id)
        else {
            return;
        };
        if let Some(hit) = self
            .app
            .attachment_burst
            .resolve_hash_at(id, *generation, hash)
        {
            dispatch_hit(&self.app, &self.ctx, hit);
        }
    }

    pub(crate) fn resolve_cached(
        &self,
        files: &[jmodguard::image_spam::Attachment],
        hashes: &[jmodguard::image_spam::Hash],
    ) {
        let tracker = &self.app.attachment_burst;
        let mut state = tracker.state.lock();
        let stages: Vec<_> = files
            .iter()
            .filter_map(|file| {
                let (_, generation) = self.ids.iter().find(|(id, _)| *id == file.id)?;
                state.take_staged(file.id, *generation)
            })
            .collect();
        // A cached set is distinct and sorted, not parallel to the file list.
        // Consume every owned completed stage. Only a complete physical-message
        // snapshot can contribute that set; partial ownership must not fallback.
        let Some(first) = stages.first() else { return };
        let sighting = first.sighting;
        let complete = stages.len() == files.len()
            && stages.iter().all(|stage| {
                (
                    stage.sighting.guild,
                    stage.sighting.author,
                    stage.sighting.channel,
                    stage.sighting.message,
                ) == (
                    sighting.guild,
                    sighting.author,
                    sighting.channel,
                    sighting.message,
                )
            });
        let mut was_acted = false;
        for stage in &stages {
            was_acted |= state.retract_fallback(stage);
        }
        let hits = if complete {
            hashes
                .iter()
                .filter_map(|hash| {
                    tracker.record_locked(&mut state, sighting, Key::Hash(*hash), was_acted)
                })
                .collect()
        } else {
            Vec::new()
        };
        drop(state);
        dispatch(&self.app, &self.ctx, hits);
    }
}

impl Drop for PendingBurst {
    fn drop(&mut self) {
        let hits = self
            .ids
            .drain(..)
            .filter_map(|(id, generation)| {
                self.app
                    .attachment_burst
                    .resolve_incomplete_at(id, generation)
            })
            .collect();
        dispatch(&self.app, &self.ctx, hits);
    }
}

pub(crate) fn dispatch_hit(app: &Arc<BotState>, ctx: &Context, hit: Hit) {
    dispatch(app, ctx, vec![hit]);
}

fn dispatch(app: &Arc<BotState>, ctx: &Context, hits: Vec<Hit>) {
    if hits.is_empty() || !app.activation.may_record() {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    for hit in hits {
        let Ok(permit) = Arc::clone(&app.attachment_burst.slots).try_acquire_owned() else {
            tracing::warn!("attachment burst enforcement dropped because too many are in flight");
            continue;
        };
        let app = Arc::clone(app);
        let ctx = ctx.clone();
        // Keep settings and shutdown behind admitted work, including before its first poll.
        let rule_hold = app.activation.hold_rule();
        handle.spawn(async move {
            let _permit = permit;
            let _rule_hold = rule_hold;
            enforce(&app, &ctx, hit).await;
        });
    }
}

async fn enforce(app: &Arc<BotState>, ctx: &Context, hit: Hit) {
    if !app.activation.may_record() || !hit.still_valid() {
        return;
    }
    let enrollment = match crate::db::guild_enrollment(&app.db, hit.guild as i64).await {
        Ok(enrollment) => enrollment,
        Err(error) => {
            tracing::warn!("attachment burst enrollment read failed: {error:#}");
            return;
        }
    };
    if !membership_still(app, hit.guild, &enrollment).await {
        return;
    }
    let outcome = apply_timeout(app, ctx, &hit, &enrollment).await;
    let timed_out = matches!(outcome, Some(crate::db::guild_data::CaseOutcome::Succeeded));
    if let Some(outcome) = outcome.as_ref() {
        let (channel, message) = hit.messages.first().copied().unwrap_or((0, 0));
        cases::record(
            app,
            enrollment.clone(),
            crate::db::guild_data::CaseAction::Timeout,
            outcome.clone(),
            hit.author,
            None,
            Some(templates::render(
                ATTACHMENT_BURST_REASON,
                &BTreeMap::from([
                    ("channels".to_string(), hit.channels.len().to_string()),
                    (
                        "window".to_string(),
                        templates::format_duration(hit.window_secs),
                    ),
                ]),
            )),
            cases::text_excerpt(channel, message, hit.author, &evidence(&hit)),
            cases::application_source(),
        )
        .await;
    }
    if !timed_out && !hit.still_valid() {
        return;
    }
    let action = if timed_out {
        templates::render(
            ATTACHMENT_BURST_TIMEOUT_ACTION,
            &std::collections::BTreeMap::from([(
                "duration".to_string(),
                templates::format_duration(hit.timeout_secs),
            )]),
        )
    } else if matches!(outcome, Some(crate::db::guild_data::CaseOutcome::Uncertain)) {
        ATTACHMENT_BURST_TIMEOUT_UNCERTAIN.to_string()
    } else {
        ATTACHMENT_BURST_TIMEOUT_MISSED.to_string()
    };
    let reason = templates::render(
        ATTACHMENT_BURST_REASON,
        &std::collections::BTreeMap::from([
            ("channels".to_string(), hit.channels.len().to_string()),
            (
                "window".to_string(),
                templates::format_duration(hit.window_secs),
            ),
        ]),
    );
    let mut vars = std::collections::BTreeMap::from([
        ("rule".to_string(), "Attachment burst".to_string()),
        ("mention".to_string(), format!("<@{}>", hit.author)),
        ("user_id".to_string(), hit.author.to_string()),
        (
            "channel".to_string(),
            hit.channels
                .first()
                .map(|channel| format!("<#{channel}>"))
                .unwrap_or_else(|| "unknown".to_string()),
        ),
        ("action".to_string(), action),
        ("reason".to_string(), reason.clone()),
        ("content".to_string(), evidence(&hit)),
    ]);
    let rendered = logging::preview_automod(app, GuildId::new(hit.guild), &mut vars).await;
    app.attachment_burst
        .open_review(&hit, enrollment.clone(), rendered.clone());
    let buttons = vec![CreateActionRow::Buttons(vec![
        CreateButton::new(app.attachment_burst.component_id("r", hit.review_id))
            .label(templates::render("Release", &BTreeMap::new()))
            .style(ButtonStyle::Success),
        CreateButton::new(app.attachment_burst.component_id("b", hit.review_id))
            .label(templates::render("Ban", &BTreeMap::new()))
            .style(ButtonStyle::Danger),
    ])];
    logging::emit_with_components(
        app,
        ctx.http.as_ref(),
        GuildId::new(hit.guild),
        "automod",
        vars,
        buttons,
        || timed_out || hit.still_valid(),
    )
    .await;
}

fn evidence(hit: &Hit) -> String {
    let places = hit
        .messages
        .iter()
        .map(|(channel, message)| format!("channel {channel}, message {message}"))
        .collect::<Vec<_>>()
        .join("; ");
    let identity = match &hit.key {
        Key::Hash(hash) => format!("sha256 {}", hex(hash)),
        Key::Fallback {
            size,
            filename,
            content_type,
        } => format!("size {size}; name {filename}; type {content_type}"),
    };
    if places.is_empty() {
        identity
    } else {
        format!("{identity}; {places}")
    }
}

fn hex(hash: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in hash {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

async fn apply_timeout(
    app: &Arc<BotState>,
    ctx: &Context,
    hit: &Hit,
    enrollment: &Option<crate::db::GuildEnrollment>,
) -> Option<crate::db::guild_data::CaseOutcome> {
    // Only the four admitted actions retain these locks. Reclaim dead entries
    // before adding one, and keep each member's read and write in the same turn.
    let lock = {
        let mut locks = app.attachment_burst.timeout_locks.lock();
        locks.retain(|_, lock| lock.strong_count() > 0);
        let entry = locks.entry((hit.guild, hit.author)).or_default();
        if let Some(lock) = entry.upgrade() {
            lock
        } else {
            let lock = Arc::new(tokio::sync::Mutex::new(()));
            *entry = Arc::downgrade(&lock);
            lock
        }
    };
    let _guard = raced(app, async { Ok(lock.lock().await) }).await?.ok()?;
    let Some(mut member) =
        fresh_member(ctx, GuildId::new(hit.guild), UserId::new(hit.author)).await
    else {
        tracing::warn!("attachment burst timeout skipped because the member was not available");
        return None;
    };
    let Ok(until) = Timestamp::from_unix_timestamp(
        chrono::Utc::now()
            .timestamp()
            .saturating_add(i64::try_from(hit.timeout_secs).unwrap_or(i64::MAX)),
    ) else {
        return None;
    };
    if member
        .communication_disabled_until
        .is_some_and(|current| current >= until)
    {
        return None;
    }
    if !app.activation.may_record()
        || !membership_still(app, hit.guild, enrollment).await
        || !hit.still_valid()
    {
        return None;
    }
    attempted_outcome(
        app,
        member.disable_communication_until_datetime(&ctx.http, until),
    )
    .await
}

async fn member(
    ctx: &Context,
    guild: GuildId,
    user: UserId,
) -> Option<serenity::model::guild::Member> {
    if let Some(found) = ctx
        .cache
        .guild(guild)
        .and_then(|guild| guild.members.get(&user).cloned())
    {
        return Some(found);
    }
    fresh_member(ctx, guild, user).await
}

async fn fresh_member(
    ctx: &Context,
    guild: GuildId,
    user: UserId,
) -> Option<serenity::model::guild::Member> {
    match tokio::time::timeout(
        crate::discord::DISCORD_REST_TIMEOUT,
        guild.member(&ctx.http, user),
    )
    .await
    {
        Ok(Ok(found)) => Some(found),
        _ => None,
    }
}

enum ButtonAction {
    Release,
    Ban,
}

fn parse_button(custom_id: &str) -> Option<(ButtonAction, u64, u64)> {
    let mut parts = custom_id.split(':');
    if parts.next() != Some("jmgburst") {
        return None;
    }
    let nonce = u64::from_str_radix(parts.next()?, 16).ok()?;
    let action = match parts.next() {
        Some("r") => ButtonAction::Release,
        Some("b") => ButtonAction::Ban,
        _ => return None,
    };
    let id = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((action, nonce, id))
}

async fn membership_still(
    app: &BotState,
    guild: u64,
    admitted: &Option<crate::db::GuildEnrollment>,
) -> bool {
    crate::db::guild_enrollment(&app.db, guild as i64)
        .await
        .is_ok_and(|current| current == *admitted)
        && !crate::db::guild_stores::guild_is_write_fenced(&app.db, guild as i64)
            .await
            .unwrap_or(true)
}

async fn attempted_outcome<T>(
    app: &BotState,
    call: impl std::future::Future<Output = serenity::Result<T>>,
) -> Option<crate::db::guild_data::CaseOutcome> {
    let started = std::sync::atomic::AtomicBool::new(false);
    let result = raced(app, async {
        started.store(true, Ordering::Relaxed);
        call.await
    })
    .await;
    if !started.load(Ordering::Relaxed) || !app.activation.may_record() {
        return None;
    }
    Some(match result {
        Some(result) => cases::classify_discord(result.as_ref().err()),
        None => crate::db::guild_data::CaseOutcome::Uncertain,
    })
}

async fn raced<T>(
    app: &BotState,
    call: impl std::future::Future<Output = serenity::Result<T>>,
) -> Option<serenity::Result<T>> {
    match tokio::time::timeout(
        crate::discord::DISCORD_REST_TIMEOUT,
        crate::discord::race_discord_call(&app.activation, call),
    )
    .await
    {
        Ok(crate::discord::DiscordCall::Finished(result)) => Some(result),
        _ => None,
    }
}

struct ButtonPress {
    id: InteractionId,
    token: String,
    guild_id: GuildId,
    user_id: UserId,
    roles: Vec<u64>,
    action: ButtonAction,
    nonce: u64,
    review_id: u64,
}

pub(crate) async fn handle_component(
    app: &Arc<BotState>,
    ctx: &Context,
    interaction: ComponentInteraction,
) {
    let Some((action, nonce, review_id)) = parse_button(&interaction.data.custom_id) else {
        return;
    };
    let Some(guild_id) = interaction.guild_id else {
        return;
    };
    let press = ButtonPress {
        id: interaction.id,
        token: interaction.token.clone(),
        guild_id,
        user_id: interaction.user.id,
        roles: interaction
            .member
            .as_ref()
            .map(|member| member.roles.iter().map(|role| role.get()).collect())
            .unwrap_or_default(),
        action,
        nonce,
        review_id,
    };
    drop(interaction);
    if crate::commands::cached_moderator_status(
        app,
        &ctx.cache,
        press.guild_id,
        press.user_id.get(),
        &press.roles,
    ) != ModeratorStatus::Moderator
    {
        let _ = ephemeral(ctx, &press, ATTACHMENT_BURST_DENIED).await;
        return;
    }
    let Some(review) =
        app.attachment_burst
            .claim_review(press.review_id, press.guild_id.get(), press.nonce)
    else {
        let _ = ephemeral(ctx, &press, ATTACHMENT_BURST_HANDLED).await;
        return;
    };
    if press.user_id.get() == review.user {
        app.attachment_burst.restore_review(press.review_id, review);
        let _ = ephemeral(ctx, &press, ATTACHMENT_BURST_DENIED).await;
        return;
    }
    let acked = CreateInteractionResponse::Acknowledge
        .execute(&ctx.http, (press.id, press.token.as_str()))
        .await
        .is_ok();
    if !acked {
        app.attachment_burst.restore_review(press.review_id, review);
        return;
    }
    // A read failure says nothing about the original enrollment. Keep the
    // claim retryable, but never rebind it to a replacement membership.
    match crate::db::guild_enrollment(&app.db, review.guild as i64).await {
        Ok(current) if current == review.enrollment => {}
        Ok(_) => {
            let _ = review_read_failed(ctx, &press).await;
            return;
        }
        Err(error) => {
            tracing::warn!("attachment burst review enrollment read failed: {error:#}");
            app.attachment_burst.restore_review(press.review_id, review);
            let _ = review_read_failed(ctx, &press).await;
            return;
        }
    }
    let decision = match press.action {
        ButtonAction::Release => release_member(app, ctx, &review, press.user_id).await,
        ButtonAction::Ban => ban_member(app, ctx, &review, press.user_id).await,
    };
    let (label, ok) = match decision {
        ReviewDecision::Released => (ATTACHMENT_BURST_RELEASED, true),
        ReviewDecision::Banned => (ATTACHMENT_BURST_BANNED, true),
        ReviewDecision::Failed => (ATTACHMENT_BURST_FAILED, false),
    };
    if !ok {
        app.attachment_burst.restore_review(press.review_id, review);
        return;
    }
    let _ = decision_edit(review.body, label, press.user_id)
        .execute(&ctx.http, press.token.as_str())
        .await;
}

fn decision_edit(body: String, label: &str, actor: UserId) -> EditInteractionResponse {
    let vars = std::collections::BTreeMap::from([
        ("body".to_string(), body),
        ("decision".to_string(), label.to_string()),
        ("actor".to_string(), format!("user {actor}")),
    ]);
    let content = templates::render_bounded(ATTACHMENT_BURST_DECISION, &vars, 2000);
    EditInteractionResponse::new()
        .content(content)
        .allowed_mentions(
            serenity::builder::CreateAllowedMentions::new()
                .all_users(false)
                .all_roles(false)
                .everyone(false)
                .replied_user(false),
        )
        .components(Vec::new())
}

enum ReviewDecision {
    Released,
    Banned,
    Failed,
}

async fn release_member(
    app: &Arc<BotState>,
    ctx: &Context,
    review: &Review,
    actor: UserId,
) -> ReviewDecision {
    let enrollment = review.enrollment.clone();
    if !membership_still(app, review.guild, &enrollment).await {
        return ReviewDecision::Failed;
    }
    let Some(mut member) = member(ctx, GuildId::new(review.guild), UserId::new(review.user)).await
    else {
        return ReviewDecision::Failed;
    };
    if !app.activation.may_record() || !membership_still(app, review.guild, &enrollment).await {
        return ReviewDecision::Failed;
    }
    let Some(outcome) = attempted_outcome(app, member.enable_communication(&ctx.http)).await else {
        return ReviewDecision::Failed;
    };
    cases::record(
        app,
        enrollment,
        crate::db::guild_data::CaseAction::TimeoutRemoval,
        outcome.clone(),
        review.user,
        Some(actor.get()),
        Some(ATTACHMENT_BURST_REVIEW_REASON.to_string()),
        crate::db::guild_data::CaseEvidence::Unavailable,
        cases::application_source(),
    )
    .await;
    if matches!(outcome, crate::db::guild_data::CaseOutcome::Succeeded) {
        ReviewDecision::Released
    } else {
        ReviewDecision::Failed
    }
}

async fn ban_member(
    app: &Arc<BotState>,
    ctx: &Context,
    review: &Review,
    actor: UserId,
) -> ReviewDecision {
    let enrollment = review.enrollment.clone();
    if !app.activation.may_record() || !membership_still(app, review.guild, &enrollment).await {
        return ReviewDecision::Failed;
    }
    let decision = crate::member_directory::observe_ban_handoff(
        app,
        ctx.http.as_ref(),
        review.guild,
        review.user,
        crate::member_directory::MarkerSave::Missing,
    )
    .await;
    match decision {
        crate::member_directory::BanContinuation::Stop => ReviewDecision::Failed,
        crate::member_directory::BanContinuation::AlreadyFinished { reason } => {
            cases::settle_present_application_ban(app, enrollment, review.user, reason.as_deref())
                .await;
            ReviewDecision::Banned
        }
        crate::member_directory::BanContinuation::RecordOnly { reason } => {
            cases::settle_present_application_ban(
                app,
                enrollment.clone(),
                review.user,
                reason.as_deref(),
            )
            .await;
            if app.activation.may_record() {
                let _ = crate::db::record_enrolled_ban_if_absent(
                    &app.db,
                    enrollment,
                    review.user as i64,
                    None,
                    None,
                    Some(actor.get() as i64),
                    None,
                    ATTACHMENT_BURST_REVIEW_REASON,
                    None,
                    crate::db::BanRecord::Retry,
                    None,
                )
                .await;
            }
            ReviewDecision::Banned
        }
        crate::member_directory::BanContinuation::BanThenRecord => {
            ban_then_record(app, ctx, review, actor, enrollment).await
        }
    }
}

async fn ban_then_record(
    app: &Arc<BotState>,
    ctx: &Context,
    review: &Review,
    actor: UserId,
    enrollment: Option<crate::db::GuildEnrollment>,
) -> ReviewDecision {
    if !app.activation.may_record() || !membership_still(app, review.guild, &enrollment).await {
        return ReviewDecision::Failed;
    }
    let pending = crate::member_directory::begin_ban_attempt(app, review.guild, review.user).await;
    if !app.activation.may_record() {
        return ReviewDecision::Failed;
    }
    let Ok(operation_id) =
        crate::db::guild_data::CaseId::try_from(pending.operation_id().to_owned())
    else {
        pending
            .finish(app, crate::db::guild_data::CaseOutcome::Failed)
            .await;
        return ReviewDecision::Failed;
    };
    let attempted = cases::begin_application_attempt(
        app,
        enrollment.clone(),
        crate::db::guild_data::CaseAction::Ban,
        review.user,
        Some(actor.get()),
        Some(ATTACHMENT_BURST_REVIEW_REASON.to_string()),
        crate::db::guild_data::CaseEvidence::Unavailable,
        operation_id,
    )
    .await;
    let discord_reason =
        cases::ban_dispatch_reason(ATTACHMENT_BURST_REVIEW_REASON, attempted.as_ref());
    let operation_id = pending.operation_id().to_owned();
    let suppression = app.suppress_next_gateway_case(
        crate::state::GatewayCaseAction::Ban,
        review.guild,
        review.user,
        Some(&operation_id),
    );
    if !membership_still(app, review.guild, &enrollment).await {
        app.release_gateway_case_suppression(suppression);
        if let Some(attempt) = attempted {
            attempt
                .finish(app, crate::db::guild_data::CaseOutcome::Failed)
                .await;
        }
        pending
            .finish(app, crate::db::guild_data::CaseOutcome::Failed)
            .await;
        return ReviewDecision::Failed;
    }
    let Some(result) = raced(
        app,
        GuildId::new(review.guild).ban_with_reason(
            &ctx.http,
            UserId::new(review.user),
            0,
            &discord_reason,
        ),
    )
    .await
    else {
        if let Some(attempt) = attempted {
            attempt
                .finish(app, crate::db::guild_data::CaseOutcome::Uncertain)
                .await;
        }
        pending
            .finish(app, crate::db::guild_data::CaseOutcome::Uncertain)
            .await;
        return ReviewDecision::Failed;
    };
    let outcome = cases::classify_discord(result.as_ref().err());
    if matches!(outcome, crate::db::guild_data::CaseOutcome::Failed) {
        app.release_gateway_case_suppression(suppression);
    }
    if let Some(attempt) = attempted {
        attempt.finish(app, outcome.clone()).await;
    }
    pending.finish(app, outcome.clone()).await;
    if matches!(outcome, crate::db::guild_data::CaseOutcome::Succeeded) {
        if app.activation.may_record()
            && let Err(error) = crate::db::record_enrolled_ban_if_absent(
                &app.db,
                enrollment,
                review.user as i64,
                None,
                None,
                Some(actor.get() as i64),
                None,
                ATTACHMENT_BURST_REVIEW_REASON,
                None,
                crate::db::BanRecord::Fresh,
                Some(&operation_id),
            )
            .await
        {
            tracing::warn!("attachment burst ban mirror recording failed: {error:#}");
        }
        ReviewDecision::Banned
    } else {
        ReviewDecision::Failed
    }
}

async fn ephemeral(ctx: &Context, press: &ButtonPress, text: &str) -> serenity::Result<()> {
    CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new()
            .content(templates::render(text, &BTreeMap::new()))
            .ephemeral(true),
    )
    .execute(&ctx.http, (press.id, press.token.as_str()))
    .await
    .map(|_| ())
}

async fn review_read_failed(ctx: &Context, press: &ButtonPress) -> serenity::Result<()> {
    CreateInteractionResponseFollowup::new()
        .content(templates::render(ATTACHMENT_BURST_FAILED, &BTreeMap::new()))
        .ephemeral(true)
        .execute(&ctx.http, (None, press.token.as_str()))
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn automatic_timeout_preserves_a_longer_current_sanction() {
        let (app, path) = open_app();
        let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let mutations = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&mutations);
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let Some((method, route)) = read_request(&mut stream).await else {
                    continue;
                };
                let user = route.rsplit('/').next().unwrap();
                if method == "PATCH" {
                    seen.lock().push(user.to_string());
                }
                let until = match user {
                    "99" => Some((chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339()),
                    "100" => Some((chrono::Utc::now() + chrono::Duration::seconds(1)).to_rfc3339()),
                    _ => None,
                };
                let member = serde_json::json!({"user":{"id":user,"username":"member","discriminator":"0","avatar":null},"roles":[],"joined_at":"2020-01-01T00:00:00+00:00","deaf":false,"mute":false,"flags":0,"communication_disabled_until":until});
                write_response(&mut stream, "200 OK", &member.to_string()).await;
            }
        });
        let ctx = fixture_context(http_at(address)).await;
        // A stale Gateway cache must not conceal the longer current REST timeout.
        let mut guild = serenity::model::guild::Guild::default();
        guild.id = GuildId::new(10);
        let mut cached = serenity::model::guild::Member::default();
        cached.guild_id = guild.id;
        cached.user.id = UserId::new(99);
        guild.members.insert(cached.user.id, cached);
        let mut event: serenity::model::event::GuildCreateEvent =
            serde_json::from_value(serde_json::to_value(&guild).unwrap()).unwrap();
        ctx.cache.update(&mut event);
        for author in [99, 100, 101] {
            enforce(&app, &ctx, test_hit(&app, author)).await;
        }
        let review_body = app
            .attachment_burst
            .state
            .lock()
            .reviews
            .values()
            .find(|review| review.user == 99)
            .unwrap()
            .body
            .clone();
        let (store, scope) = app.tenant.live(enrollment).await.unwrap();
        let cases = store
            .case_count(scope, crate::tenant::case_limits())
            .await
            .unwrap();
        let dispatched = mutations.lock().clone();
        server.abort();
        let _ = server.await;
        drop(app);
        close_app(&path);
        assert_eq!(
            dispatched,
            vec!["100", "101"],
            "retain a longer timeout; still apply to a shorter or absent timeout"
        );
        assert_eq!(
            cases, 2,
            "a skipped automatic timeout must not create an attempted case"
        );
        assert!(
            review_body.contains(ATTACHMENT_BURST_TIMEOUT_MISSED),
            "review must not claim a new timeout was applied"
        );
    }

    #[test]
    fn unrelated_message_invalidation_preserves_pending_admission() {
        for known in [false, true] {
            let tracker = tracker();
            let now = Instant::now();
            let original = sight(4, 10, 100, now);
            if known {
                tracker.stage_sighting(
                    original,
                    vec![burst_file(1)],
                    admission(tracker.revision(), 10),
                );
            }
            let receipt = tracker.revision();
            tracker.forget_message(2, 90, 999);
            let ids = tracker.stage_sighting(original, vec![burst_file(2)], admission(receipt, 20));
            assert_eq!(
                ids.len(),
                1,
                "unrelated deletion lost known={known} observation"
            );
            assert!(tracker.resolve_hash_at(2, ids[0].1, [1; 32]).is_none());
            let next = tracker.stage_sighting(
                sight(4, 11, 101, now),
                vec![burst_file(3)],
                admission(tracker.revision(), 20),
            );
            assert!(tracker.resolve_hash_at(3, next[0].1, [1; 32]).is_some());
        }
        let tracker = tracker();
        let now = Instant::now();
        let first = sight(4, 10, 100, now);
        tracker.stage_sighting(
            first,
            vec![burst_file(1)],
            admission(tracker.revision(), 10),
        );
        let old = tracker.revision();
        tracker.invalidate_message_at(1, 10, 100, false, Some(20));
        assert!(
            tracker
                .stage_sighting(first, vec![burst_file(2)], admission(old, 30))
                .is_empty(),
            "the affected message still rejects its old receipt"
        );
        // Its invalidation watermark must survive replacement of the ordering note.
        assert!(
            tracker
                .stage_sighting(first, vec![burst_file(3)], admission(old, 40))
                .is_empty()
        );
        let old = tracker.revision();
        tracker.invalidate_guild(1);
        assert!(
            tracker
                .stage_sighting(
                    sight(4, 20, 200, now),
                    vec![burst_file(4)],
                    admission(old, 50)
                )
                .is_empty(),
            "scope retirement still rejects unknown in-flight observations"
        );
    }

    #[tokio::test]
    async fn enrollment_read_error_cannot_authorize_a_timeout() {
        use jmodguard::db::SqlExecutor as _;
        let (app, path) = open_app();
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let mutations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = Arc::clone(&mutations);
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let Some((method, route)) = read_request(&mut stream).await else {
                    continue;
                };
                if method == "PATCH" {
                    seen.fetch_add(1, Ordering::SeqCst);
                }
                let user = route.rsplit('/').next().unwrap_or("99");
                let member = serde_json::json!({"user":{"id":user,"username":"member","discriminator":"0","avatar":null},"roles":[],"joined_at":"2020-01-01T00:00:00+00:00","deaf":false,"mute":false,"flags":0});
                write_response(&mut stream, "200 OK", &member.to_string()).await;
            }
        });
        let ctx = fixture_context(http_at(address)).await;
        assert!(
            membership_still(&app, 10, &None).await,
            "verified absence remains supported"
        );
        enforce(&app, &ctx, test_hit(&app, 99)).await;
        assert_eq!(
            mutations.load(Ordering::SeqCst),
            1,
            "positive control must reach the real timeout route"
        );
        crate::db::enroll_guild(&app.db, 10, 100).await.unwrap();
        app.db.with_conn(|conn| {
            conn.execute_bounded("UPDATE guild_enrollments SET joined_at_ns='unreadable fixture value' WHERE guild_id=10", [] as [libsql::Value;0], 65536)?;
            Ok(())
        }).await.unwrap();
        assert!(crate::db::guild_enrollment(&app.db, 10).await.is_err());
        assert!(
            !crate::db::guild_stores::guild_is_write_fenced(&app.db, 10)
                .await
                .unwrap()
        );
        let accepted = membership_still(&app, 10, &None).await;
        enforce(&app, &ctx, test_hit(&app, 100)).await;
        let after_error = mutations.load(Ordering::SeqCst);
        server.abort();
        let _ = server.await;
        drop(app);
        close_app(&path);
        assert_eq!(
            after_error, 1,
            "failed enrollment read must not authorize another timeout"
        );
        assert!(
            !accepted,
            "mutation rechecks must distinguish lookup failure from verified absence"
        );
    }

    fn burst_file(id: u64) -> FileMeta {
        FileMeta {
            attachment_id: id,
            size: 42,
            filename: "spam.png".into(),
            content_type: "image/png".into(),
        }
    }

    #[test]
    fn rejected_updates_revoke_only_superseded_proof() {
        for rejection in ["revision", "age", "refresh"] {
            for replacement in [false, true] {
                let tracker = tracker();
                let now = Instant::now();
                let first = sight(4, 10, 100, now);
                let a = tracker.stage_sighting(
                    first,
                    vec![burst_file(1)],
                    admission(tracker.revision(), 10),
                );
                let b = tracker.stage_sighting(
                    sight(4, 11, 101, now),
                    vec![burst_file(2)],
                    admission(tracker.revision(), 10),
                );
                assert!(tracker.resolve_hash_at(a[0].0, a[0].1, [1; 32]).is_none());
                let hit = tracker.resolve_hash_at(b[0].0, b[0].1, [1; 32]).unwrap();
                let mut edit = admission(tracker.revision(), 20);
                match rejection {
                    "revision" => tracker.forget_message(2, 90, 999),
                    "age" => edit.eligible = false,
                    _ => {
                        assert!(
                            tracker
                                .stage_sighting(
                                    first,
                                    vec![burst_file(1)],
                                    Admission {
                                        revision: tracker.revision(),
                                        edited_ms: None,
                                        received: now + Duration::from_secs(2),
                                        eligible: true,
                                    }
                                )
                                .is_empty()
                        );
                    }
                }
                let files = if replacement {
                    vec![burst_file(3)]
                } else {
                    vec![]
                };
                tracker.stage_sighting(first, files, edit);
                assert!(!hit.still_valid(), "{rejection}, replacement={replacement}");
                // An older real edit cannot overwrite the replacement or its watermark.
                assert!(
                    tracker
                        .stage_sighting(
                            first,
                            vec![burst_file(4)],
                            admission(tracker.revision(), 15)
                        )
                        .is_empty()
                );
                assert_eq!(tracker.state.lock().messages[&(1, 10, 100)].edited_ms, 20);
            }
        }
    }

    #[test]
    fn late_timestamp_free_refresh_cannot_erase_a_newer_snapshot() {
        let tracker = tracker();
        let now = Instant::now();
        let sighting = sight(4, 10, 100, now);
        let mut newer = admission(tracker.revision(), 20);
        newer.received = now + Duration::from_secs(1);
        let ids = tracker.stage_sighting(sighting, vec![burst_file(2)], newer);
        assert_eq!(ids.len(), 1);
        assert!(
            tracker
                .stage_sighting(
                    sighting,
                    vec![burst_file(1)],
                    Admission {
                        revision: tracker.revision(),
                        edited_ms: None,
                        received: now,
                        eligible: true,
                    }
                )
                .is_empty()
        );
        assert_eq!(tracker.staged_generation(2), Some(ids[0].1));
    }

    #[test]
    fn capacity_eviction_keeps_deleted_snapshots_rejected() {
        for guild_cap in [false, true] {
            let tracker = tracker();
            let now = Instant::now();
            let first = sight(4, 10, 100, now);
            tracker.prepare_sighting(first, &[1]);
            tracker.record(first, hash_key(1));
            tracker.record(sight(4, 11, 101, now), hash_key(2));
            tracker.forget_message(1, 10, 100);
            if guild_cap {
                for n in 0..MAX_GUILDS {
                    let mut other = sight(4, 20, 1000 + n as u64, now + Duration::from_secs(1));
                    other.guild = 2 + n as u64;
                    tracker.record(other, hash_key(3));
                }
            } else {
                for n in 0..MAX_MESSAGES {
                    tracker.prepare_sighting(
                        sight(4, 20, 1000 + n as u64, now + Duration::from_secs(1)),
                        &[1000 + n as u64],
                    );
                }
            }
            assert!(!tracker.state.lock().messages.contains_key(&(1, 10, 100)));
            assert!(
                tracker
                    .stage_sighting(
                        first,
                        vec![burst_file(1)],
                        admission(tracker.revision(), 10)
                    )
                    .is_empty()
            );
            assert_eq!(
                tracker
                    .stage_sighting(
                        sight(4, 20, 10000, now + Duration::from_secs(2)),
                        vec![burst_file(10000)],
                        admission(tracker.revision(), 30)
                    )
                    .len(),
                1
            );
        }
    }

    #[test]
    fn losing_an_ordering_note_retires_only_its_owned_evidence() {
        for removal in ["message-cap", "guild-cap", "expiry"] {
            let tracker = tracker();
            let now = Instant::now();
            let first = sight(4, 10, 100, now);
            let generation = tracker.stage(first, burst_file(1));
            tracker.resolve_incomplete_at(1, generation);
            let second = tracker.stage(sight(4, 11, 101, now), burst_file(2));
            let hit = tracker.resolve_incomplete_at(2, second).unwrap();
            tracker.stage(first, burst_file(3));
            let mut other = sight(8, 30, 500, now + Duration::from_secs(2));
            other.guild = 2;
            // Reused attachment handles must be checked against their physical owner.
            let other_generation = tracker.stage(other, burst_file(3));
            tracker.resolve_incomplete_at(3, other_generation);
            match removal {
                "message-cap" => {
                    for n in 0..MAX_MESSAGES {
                        tracker.prepare_sighting(
                            sight(9, 20, 1000 + n as u64, now + Duration::from_secs(1)),
                            &[1000 + n as u64],
                        );
                    }
                }
                "guild-cap" => {
                    for n in 0..MAX_GUILDS {
                        let mut added = sight(9, 20, 1000 + n as u64, now + Duration::from_secs(1));
                        added.guild = 3 + n as u64;
                        tracker.record(added, hash_key(7));
                    }
                }
                _ => tracker.state.lock().drop_expired(
                    now + Duration::from_secs(5),
                    Duration::from_secs(3),
                    settings().min_channels,
                ),
            }
            assert!(
                !tracker.state.lock().messages.contains_key(&(1, 10, 100)),
                "{removal}"
            );
            tracker.forget_message(1, 10, 100);
            assert!(!hit.still_valid(), "{removal} must leave no unowned proof");
            assert!(tracker.resolve_hash_at(1, generation, [1; 32]).is_none());
            assert!(
                tracker
                    .resolve_hash_at(3, other_generation, [9; 32])
                    .is_none()
            );
            let mut next = other;
            next.channel = 31;
            next.message = 20_000;
            next.at = now + Duration::from_secs(3);
            let next_generation = tracker.stage(next, burst_file(20_000));
            assert!(
                tracker
                    .resolve_hash_at(20_000, next_generation, [9; 32])
                    .is_some(),
                "{removal} must preserve unrelated staged ownership"
            );
        }
    }

    #[test]
    fn expired_ordering_notes_keep_old_admissions_fenced() {
        let tracker = tracker();
        let now = Instant::now();
        let original = sight(4, 10, 100, now);
        let old_receipt = tracker.revision();
        tracker.invalidate_message_at(1, 10, 100, false, Some(20));
        let fresh_receipt = tracker.revision();
        tracker.state.lock().drop_expired(
            now + Duration::from_secs(1000),
            Duration::from_secs(1),
            settings().min_channels,
        );
        assert!(!tracker.state.lock().messages.contains_key(&(1, 10, 100)));
        assert!(
            tracker
                .stage_sighting(original, vec![burst_file(1)], admission(old_receipt, 30))
                .is_empty()
        );
        assert_eq!(
            tracker
                .stage_sighting(original, vec![burst_file(2)], admission(fresh_receipt, 40))
                .len(),
            1,
            "a fresh edit of the old message ID remains admissible"
        );
        assert_eq!(
            tracker
                .stage_sighting(
                    sight(8, 20, 50, now),
                    vec![burst_file(3)],
                    admission(fresh_receipt, 40)
                )
                .len(),
            1,
            "expiry must not impose a global message-ID floor"
        );
    }

    #[test]
    fn delayed_completion_preserves_newest_channel_evidence() {
        for key in [
            hash_key(1),
            Key::Fallback {
                size: 42,
                filename: "spam.png".into(),
                content_type: "image/png".into(),
            },
        ] {
            let tracker = tracker();
            let now = Instant::now();
            assert!(
                tracker
                    .record(sight(4, 10, 101, now + Duration::from_secs(3)), key.clone())
                    .is_none()
            );
            assert!(
                tracker
                    .record(sight(4, 10, 100, now), key.clone())
                    .is_none()
            );
            assert!(
                tracker
                    .record(sight(4, 11, 102, now + Duration::from_secs(12)), key)
                    .is_some()
            );
        }
    }

    #[test]
    fn author_eviction_cannot_repeat_a_physical_burst() {
        let tracker = tracker();
        let now = Instant::now();
        let mut stages = Vec::new();
        for (channel, message, attachment) in [(10, 100, 1), (11, 101, 2)] {
            let ids = tracker.stage_sighting(
                sight(4, channel, message, now),
                vec![burst_file(attachment)],
                admission(tracker.revision(), 0),
            );
            stages.push(ids[0]);
        }
        assert!(
            tracker
                .resolve_incomplete_at(stages[0].0, stages[0].1)
                .is_none()
        );
        let hit = tracker
            .resolve_incomplete_at(stages[1].0, stages[1].1)
            .unwrap();
        let later = now + Duration::from_secs(1);
        let unrelated = tracker.stage(sight(8, 20, 500, later), burst_file(500));
        assert!(tracker.resolve_incomplete_at(500, unrelated).is_none());
        for author in 10..10 + MAX_AUTHORS_PER_GUILD as u64 - 1 {
            tracker.record(sight(author, 30, 1000 + author, later), hash_key(7));
        }
        assert!(!tracker.state.lock().guilds[&1].authors.contains_key(&4));
        for (attachment, generation) in stages {
            assert!(
                tracker
                    .resolve_hash_at(attachment, generation, [1; 32])
                    .is_none(),
                "delayed hashes must not repeat an evicted physical burst"
            );
        }
        assert_eq!(tracker.hits(), 1);
        assert!(!hit.still_valid(), "eviction must retire its owned proof");
        assert!(tracker.resolve_hash_at(500, unrelated, [9; 32]).is_none());
        let next = tracker.stage(sight(8, 21, 501, later), burst_file(501));
        assert!(
            tracker.resolve_hash_at(501, next, [9; 32]).is_some(),
            "eviction must preserve another author's pending observations"
        );
    }

    #[test]
    fn key_eviction_cannot_repeat_a_physical_burst() {
        let tracker = tracker();
        let now = Instant::now();
        let mut stages = Vec::new();
        let mut hit = None;
        // Include a superseded same-channel observation, not just the two
        // representatives left in the evicted bucket.
        for (channel, message, attachment) in [(10, 100, 1), (11, 101, 2), (10, 102, 3)] {
            let ids = tracker.stage_sighting(
                sight(4, channel, message, now),
                vec![burst_file(attachment)],
                admission(tracker.revision(), 0),
            );
            stages.push(ids[0]);
            if let Some(found) = tracker.resolve_incomplete_at(ids[0].0, ids[0].1) {
                assert!(hit.replace(found).is_none());
            }
        }
        let hit = hit.unwrap();
        let later = now + Duration::from_secs(1);
        let unrelated = tracker.stage(
            sight(4, 20, 500, later + Duration::from_secs(1)),
            FileMeta {
                filename: "unrelated.png".into(),
                ..burst_file(500)
            },
        );
        assert!(tracker.resolve_incomplete_at(500, unrelated).is_none());
        for key in 0..MAX_KEYS_PER_AUTHOR - 1 {
            tracker.record(sight(4, 30, 1000 + key as u64, later), hash_key(key as u8));
        }
        assert_eq!(
            tracker.state.lock().guilds[&1].authors[&4].keys.len(),
            MAX_KEYS_PER_AUTHOR
        );
        for (attachment, generation) in stages {
            assert!(
                tracker
                    .resolve_hash_at(attachment, generation, [99; 32])
                    .is_none(),
                "key eviction must not produce a second hit from the same physical messages"
            );
        }
        assert_eq!(tracker.hits(), 1);
        assert!(
            !hit.still_valid(),
            "eviction must retire the old owned proof"
        );
        assert!(tracker.resolve_hash_at(500, unrelated, [100; 32]).is_none());
        let next = tracker.stage(
            sight(4, 21, 501, later + Duration::from_secs(2)),
            burst_file(501),
        );
        assert!(
            tracker.resolve_hash_at(501, next, [100; 32]).is_some(),
            "key eviction must preserve unrelated observations of the same author"
        );
        assert!(tracker.state.lock().guilds[&1].authors[&4].keys.len() <= MAX_KEYS_PER_AUTHOR);
    }

    #[test]
    fn key_eviction_retires_superseded_hash_owners() {
        let tracker = tracker();
        let now = Instant::now();
        let mut delayed = Vec::new();
        for (channel, message, id) in [(10, 100, 1), (11, 101, 3)] {
            let stages = tracker.stage_sighting(
                sight(4, channel, message, now),
                vec![burst_file(id), burst_file(id + 1)],
                admission(tracker.revision(), 0),
            );
            tracker.resolve_hash_at(stages[0].0, stages[0].1, [99; 32]);
            delayed.push(stages[1]);
        }
        assert_eq!(tracker.hits(), 1);
        let later = now + Duration::from_secs(1);
        for (channel, message, id) in [(10, 102, 5), (11, 103, 6)] {
            let generation = tracker.stage(sight(4, channel, message, later), burst_file(id));
            assert!(tracker.resolve_hash_at(id, generation, [99; 32]).is_none());
        }
        for key in 0..MAX_KEYS_PER_AUTHOR {
            tracker.record(
                sight(4, 30, 1000 + key as u64, later + Duration::from_secs(1)),
                hash_key(key as u8),
            );
        }
        for (id, generation) in delayed {
            assert!(
                tracker.resolve_hash_at(id, generation, [99; 32]).is_none(),
                "superseded hash owners must not replay through another attachment"
            );
        }
        assert_eq!(tracker.hits(), 1);
    }

    #[test]
    fn key_eviction_cannot_resurrect_its_incoming_physical_owner() {
        let tracker = tracker();
        let now = Instant::now();
        // Distinct keys from one physical message all share its lifetime.
        for key in 0..MAX_KEYS_PER_AUTHOR {
            tracker.record(sight(4, 10, 100, now), hash_key(key as u8));
        }
        tracker.record(sight(4, 10, 100, now), hash_key(99));
        assert!(
            tracker
                .record(sight(4, 11, 101, now), hash_key(99))
                .is_none(),
            "retiring the incoming message must not resurrect its proof under a new key"
        );
        let state = tracker.state.lock();
        assert_eq!(state.authors, 1);
        assert_eq!(state.guilds[&1].authors[&4].keys.len(), 1);
        assert!(state.messages[&(1, 10, 100)].lifetime.is_none());
        drop(state);
        for key in 0..MAX_KEYS_PER_AUTHOR - 1 {
            tracker.record(sight(4, 30, 1000 + key as u64, now), hash_key(key as u8));
        }
        tracker.record(sight(4, 10, 100, now), hash_key(200));
        assert_eq!(
            tracker.state.lock().guilds[&1].authors[&4].keys.len(),
            MAX_KEYS_PER_AUTHOR,
            "a retired incoming message must not evict live observations"
        );
    }

    #[test]
    fn completed_observations_leave_metadata_and_keep_one_shot_claims() {
        for matched_fallback in [false, true] {
            let tracker = tracker();
            let now = Instant::now();
            let first = tracker.stage(sight(4, 10, 100, now), burst_file(1));
            assert!(tracker.resolve_incomplete_at(1, first).is_none());
            let second = tracker.stage(sight(4, 11, 101, now), burst_file(2));
            if matched_fallback {
                assert!(tracker.resolve_incomplete_at(2, second).is_some());
            }
            assert!(tracker.resolve_hash_at(1, first, [1; 32]).is_none());
            assert_eq!(
                tracker.resolve_hash_at(2, second, [1; 32]).is_some(),
                !matched_fallback
            );
            assert_eq!(tracker.hits(), 1);
            let third = tracker.stage(sight(4, 12, 102, now), burst_file(3));
            assert!(tracker.resolve_hash_at(3, third, [1; 32]).is_none());
            assert_eq!(tracker.hits(), 1);
        }
        let tracker = tracker();
        let now = Instant::now();
        let a = tracker.stage(sight(4, 10, 100, now), burst_file(1));
        tracker.resolve_incomplete_at(1, a);
        tracker.resolve_hash_at(1, a, [1; 32]);
        let b = tracker.stage(sight(4, 11, 101, now), burst_file(2));
        assert!(
            tracker.resolve_incomplete_at(2, b).is_none(),
            "completed A must no longer match unfinished metadata"
        );
        assert!(
            tracker.resolve_hash_at(2, b, [2; 32]).is_none(),
            "completed unequal bytes must not match"
        );
        assert_eq!(tracker.hits(), 0);
    }

    #[test]
    fn promoting_one_file_preserves_other_unfinished_file_in_same_message() {
        let tracker = tracker();
        let now = Instant::now();
        let a = tracker.stage(sight(4, 10, 100, now), burst_file(1));
        let b = tracker.stage(sight(4, 10, 100, now), burst_file(2));
        tracker.resolve_incomplete_at(1, a);
        tracker.resolve_incomplete_at(2, b);
        tracker.resolve_hash_at(1, a, [1; 32]);
        let c = tracker.stage(sight(4, 11, 101, now), burst_file(3));
        assert!(
            tracker.resolve_incomplete_at(3, c).is_some(),
            "file 2 still owns the metadata evidence"
        );
    }

    #[test]
    fn promoted_claim_survives_channel_refresh_only_inside_its_window() {
        for later in [4, 12] {
            let tracker = tracker();
            let now = Instant::now();
            let a = tracker.stage(sight(4, 10, 100, now), burst_file(1));
            let b = tracker.stage(sight(4, 11, 101, now), burst_file(2));
            tracker.resolve_incomplete_at(1, a);
            assert!(tracker.resolve_incomplete_at(2, b).is_some());
            tracker.resolve_hash_at(1, a, [1; 32]);
            // B remains unfinished; a newer post replaces A's hash representative.
            let a_new = tracker.stage(
                sight(4, 10, 102, now + Duration::from_secs(3)),
                burst_file(3),
            );
            assert!(tracker.resolve_hash_at(3, a_new, [1; 32]).is_none());
            let c = tracker.stage(
                sight(4, 12, 103, now + Duration::from_secs(later)),
                burst_file(4),
            );
            assert_eq!(
                tracker.resolve_hash_at(4, c, [1; 32]).is_some(),
                later == 12
            );
        }
    }

    fn settings() -> Settings {
        Settings::default()
    }

    fn tracker() -> Tracker {
        Tracker::new(settings())
    }

    fn admission(revision: u64, edited_ms: i64) -> Admission {
        Admission {
            revision,
            edited_ms: Some(edited_ms),
            received: Instant::now(),
            eligible: true,
        }
    }

    fn sight(author: u64, channel: u64, message: u64, at: Instant) -> Sighting {
        Sighting {
            guild: 1,
            author,
            channel,
            message,
            at,
        }
    }

    fn hash_key(byte: u8) -> Key {
        Key::Hash([byte; 32])
    }

    #[test]
    fn same_hash_across_two_channels_triggers() {
        // No account-age argument exists on this path.
        let tracker = tracker();
        let now = Instant::now();
        assert!(
            tracker
                .record(sight(4, 10, 100, now), hash_key(1))
                .is_none()
        );
        let hit = tracker
            .record(sight(4, 11, 101, now), hash_key(1))
            .expect("second channel");
        assert_eq!(hit.channels, vec![10, 11]);
        assert_eq!(hit.author, 4);
        assert!(
            tracker
                .record(sight(4, 12, 102, now), hash_key(1))
                .is_none(),
            "a later channel in the same window must not time out again"
        );
    }

    #[test]
    fn fallback_size_name_and_type_triggers() {
        let tracker = tracker();
        let now = Instant::now();
        tracker.stage(
            sight(4, 10, 100, now),
            FileMeta {
                attachment_id: 1,
                size: 42,
                filename: "spam.png".into(),
                content_type: "Image/PNG; charset=binary".into(),
            },
        );
        tracker.stage(
            sight(4, 11, 101, now),
            FileMeta {
                attachment_id: 2,
                size: 42,
                filename: "spam.png".into(),
                content_type: "image/png".into(),
            },
        );
        assert!(tracker.resolve_incomplete(1).is_none());
        assert!(tracker.resolve_incomplete(2).is_some());
    }

    #[test]
    fn different_hashes_do_not_use_the_fallback() {
        let tracker = tracker();
        let now = Instant::now();
        for (id, channel, hash) in [(1, 10, 1), (2, 11, 2)] {
            tracker.stage(
                sight(4, channel, id, now),
                FileMeta {
                    attachment_id: id,
                    size: 42,
                    filename: "spam.png".into(),
                    content_type: "image/png".into(),
                },
            );
            assert!(tracker.resolve_hash(id, [hash; 32]).is_none());
        }
        assert_eq!(tracker.hits.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn same_channel_repeats_do_not_trigger() {
        let tracker = tracker();
        let now = Instant::now();
        assert!(
            tracker
                .record(sight(4, 10, 100, now), hash_key(1))
                .is_none()
        );
        assert!(
            tracker
                .record(sight(4, 10, 101, now), hash_key(1))
                .is_none()
        );
    }

    #[test]
    fn outside_window_does_not_trigger() {
        let tracker = tracker();
        let now = Instant::now();
        let later = now + Duration::from_secs(settings().window_secs + 1);
        assert!(
            tracker
                .record(sight(4, 10, 100, now), hash_key(1))
                .is_none()
        );
        assert!(
            tracker
                .record(sight(4, 11, 101, later), hash_key(1))
                .is_none()
        );
    }

    #[test]
    fn different_members_do_not_cross_trigger() {
        let tracker = tracker();
        let now = Instant::now();
        assert!(
            tracker
                .record(sight(4, 10, 100, now), hash_key(1))
                .is_none()
        );
        assert!(
            tracker
                .record(sight(5, 11, 101, now), hash_key(1))
                .is_none()
        );
    }

    #[test]
    fn disabled_config_stores_nothing() {
        let mut settings = settings();
        settings.enabled = false;
        let tracker = Tracker::new(settings);
        let now = Instant::now();
        tracker.stage(
            sight(4, 10, 100, now),
            FileMeta {
                attachment_id: 1,
                size: 9,
                filename: "a.png".into(),
                content_type: "image/png".into(),
            },
        );
        assert!(tracker.resolve_incomplete(1).is_none());
        assert!(
            tracker
                .record(sight(4, 11, 101, now), hash_key(1))
                .is_none()
        );
        assert_eq!(tracker.state.lock().authors, 0);
        assert!(tracker.state.lock().staged.is_empty());
    }

    #[test]
    fn author_limit_applies_across_guilds() {
        let tracker = tracker();
        let now = Instant::now();
        // Stay below every per-guild cap while crossing the process-wide cap.
        let per_guild = MAX_AUTHORS_PER_GUILD / 2;
        for author in 0..MAX_AUTHORS * 2 {
            tracker.record(
                Sighting {
                    guild: (author / per_guild + 1) as u64,
                    ..sight(author as u64, 10, author as u64 + 1, now)
                },
                hash_key(1),
            );
            let state = tracker.state.lock();
            let actual: usize = state.guilds.values().map(|guild| guild.authors.len()).sum();
            assert_eq!(
                state.authors, actual,
                "author accounting must match retained state"
            );
            assert!(
                actual <= MAX_AUTHORS,
                "global author cap exceeded across guilds"
            );
            if author + 1 == MAX_AUTHORS {
                assert_eq!(actual, MAX_AUTHORS, "fixture must reach the global cap");
            }
            assert!(
                state
                    .guilds
                    .values()
                    .all(|guild| guild.authors.len() < MAX_AUTHORS_PER_GUILD)
            );
        }
    }

    #[test]
    fn global_author_eviction_admits_new_guild_and_retires_old_proof() {
        let tracker = tracker();
        let now = Instant::now();
        tracker.record(sight(0, 10, 1, now), hash_key(1));
        let old_hit = tracker.record(sight(0, 11, 2, now), hash_key(1)).unwrap();
        let later = now + Duration::from_secs(1);
        for author in 1..MAX_AUTHORS {
            tracker.record(
                Sighting {
                    guild: (author / (MAX_AUTHORS_PER_GUILD / 2) + 1) as u64,
                    ..sight(author as u64, 10, author as u64 + 2, later)
                },
                hash_key(1),
            );
        }
        assert_eq!(tracker.state.lock().authors, MAX_AUTHORS);
        for (channel, message) in [(10, 600), (11, 601)] {
            tracker.record(
                Sighting {
                    guild: 999,
                    ..sight(999, channel, message, later)
                },
                hash_key(1),
            );
        }
        assert_eq!(
            tracker.hits(),
            2,
            "a new guild must still detect bursts at capacity"
        );
        assert_eq!(tracker.state.lock().authors, MAX_AUTHORS);
        assert!(
            !old_hit.still_valid(),
            "global eviction must retire owned proof"
        );
        for (channel, message) in [(10, 1), (11, 2)] {
            assert!(
                tracker
                    .record(sight(0, channel, message, now), hash_key(1))
                    .is_none()
            );
        }
        assert_eq!(
            tracker.hits(),
            2,
            "evicted physical messages must remain replays"
        );
    }

    #[test]
    fn state_prunes_and_stays_bounded() {
        let tracker = tracker();
        let now = Instant::now();
        for author in 0..1_000 {
            let _ = tracker.record(sight(author, 10, author, now), hash_key(1));
        }
        assert!(tracker.state.lock().authors <= MAX_AUTHORS_PER_GUILD);
        assert!(tracker.state.lock().authors <= MAX_AUTHORS);
        for key in 0..100 {
            let _ = tracker.record(sight(4, 10, key, now), hash_key(key as u8));
        }
        let keys = tracker
            .state
            .lock()
            .guilds
            .get(&1)
            .and_then(|guild| guild.authors.get(&4))
            .map(|author| author.keys.len())
            .unwrap_or(0);
        assert!(keys <= MAX_KEYS_PER_AUTHOR);
        let old = now;
        let _ = tracker.record(sight(8, 10, 1, old), hash_key(3));
        tracker.state.lock().drop_expired(
            old + Duration::from_secs(10_000),
            Duration::from_secs(1),
            settings().min_channels,
        );
        assert_eq!(tracker.state.lock().authors, 0);

        for id in 0..2_000 {
            tracker.stage(
                sight(9, 10, id, now),
                FileMeta {
                    attachment_id: id,
                    size: 1,
                    filename: "a.png".into(),
                    content_type: "image/png".into(),
                },
            );
        }
        assert!(tracker.state.lock().staged.len() <= MAX_STAGED);
        for message in 0..2_000u64 {
            let _ = tracker.prepare_sighting(sight(9, 20, message, now), &[message]);
        }
        assert!(tracker.state.lock().messages.len() <= MAX_MESSAGES);
    }

    #[test]
    fn concurrent_channels_trigger_once() {
        let tracker = Arc::new(tracker());
        let now = Instant::now();
        let mut threads = Vec::new();
        for channel in 1..=8 {
            let tracker = Arc::clone(&tracker);
            threads.push(std::thread::spawn(move || {
                tracker.record(sight(4, channel, channel, now), hash_key(7))
            }));
        }
        let hits = threads
            .into_iter()
            .filter_map(|thread| thread.join().expect("thread"))
            .count();
        assert_eq!(hits, 1);
        assert_eq!(tracker.hits.load(Ordering::Relaxed), 1);
        let channels = tracker
            .state
            .lock()
            .guilds
            .get(&1)
            .and_then(|guild| guild.authors.get(&4))
            .and_then(|author| author.keys.get(&hash_key(7)))
            .map(|bucket| bucket.channels.len())
            .unwrap_or(0);
        assert_eq!(channels, 8);
    }

    #[test]
    fn resolving_and_forgetting_cannot_resurrect_a_sighting() {
        for hashed in [true, false] {
            for _ in 0..250 {
                let tracker = tracker();
                let now = Instant::now();
                let meta = |id| FileMeta {
                    attachment_id: id,
                    size: 42,
                    filename: "spam.png".into(),
                    content_type: "image/png".into(),
                };
                let first = tracker.stage(sight(4, 10, 100, now), meta(1));
                let resolve = |id, generation| {
                    if hashed {
                        tracker.resolve_hash_at(id, generation, [1; 32])
                    } else {
                        tracker.resolve_incomplete_at(id, generation)
                    }
                };
                assert!(resolve(1, first).is_none());
                let second = tracker.stage(sight(4, 11, 101, now), meta(2));
                let start = std::sync::Barrier::new(2);
                let hit = std::thread::scope(|scope| {
                    let settling = scope.spawn(|| {
                        start.wait();
                        resolve(2, second)
                    });
                    start.wait();
                    tracker.forget_message(1, 11, 101);
                    settling.join().unwrap()
                });
                assert!(
                    hit.is_none_or(|hit| !hit.still_valid()),
                    "a completed invalidation revokes any concurrent hit"
                );
                assert!(
                    tracker.state.lock().guilds[&1].authors[&4]
                        .keys
                        .values()
                        .all(|bucket| !bucket.channels.contains_key(&11)),
                    "settlement cannot restore a forgotten channel"
                );
            }
        }
    }

    #[test]
    fn forgetting_a_message_drops_its_channel() {
        let tracker = tracker();
        let now = Instant::now();
        assert!(
            tracker
                .record(sight(4, 10, 100, now), hash_key(1))
                .is_none()
        );
        tracker.forget_message(1, 10, 100);
        assert!(
            tracker
                .record(sight(4, 11, 101, now), hash_key(1))
                .is_none()
        );
    }

    #[test]
    fn button_ids_round_trip() {
        let tracker = tracker();
        let release = tracker.component_id("r", 15);
        let ban = tracker.component_id("b", 15);
        assert!(matches!(
            parse_button(&release),
            Some((ButtonAction::Release, nonce, 15)) if nonce == tracker.process_nonce
        ));
        assert!(matches!(
            parse_button(&ban),
            Some((ButtonAction::Ban, nonce, 15)) if nonce == tracker.process_nonce
        ));
        assert!(parse_button("jmgburst:r:15").is_none());
        assert!(parse_button("jmgburst:r:15:extra").is_none());
        assert!(parse_button("other:r:15").is_none());
    }

    #[test]
    fn restarted_tracker_cannot_claim_another_members_review() {
        let now = Instant::now();
        let first = tracker();
        assert!(first.record(sight(4, 10, 100, now), hash_key(1)).is_none());
        let older = first
            .record(sight(4, 11, 101, now), hash_key(1))
            .expect("first review");
        first.open_review(&older, None, String::new());
        let button = first.component_id("b", older.review_id);
        let (action, nonce, review_id) = parse_button(&button).expect("button");
        assert!(matches!(action, ButtonAction::Ban));
        let restarted = tracker();
        assert!(
            restarted
                .record(sight(8, 10, 200, now), hash_key(2))
                .is_none()
        );
        let newer = restarted
            .record(sight(8, 11, 201, now), hash_key(2))
            .expect("restarted review");
        restarted.open_review(&newer, None, String::new());
        assert_eq!(older.review_id, newer.review_id);
        assert!(restarted.claim_review(review_id, 1, nonce).is_none());
        let review = restarted
            .claim_review(newer.review_id, 1, restarted.process_nonce)
            .expect("current button");
        assert_eq!(review.user, 8);
    }

    #[test]
    fn replaying_the_same_attachments_does_not_rearm() {
        let tracker = tracker();
        let now = Instant::now();
        assert!(tracker.prepare_sighting(sight(4, 10, 100, now), &[1]));
        assert!(tracker.prepare_sighting(sight(4, 11, 101, now), &[2]));
        assert!(
            tracker
                .record(sight(4, 10, 100, now), hash_key(1))
                .is_none()
        );
        assert!(
            tracker
                .record(sight(4, 11, 101, now), hash_key(1))
                .is_some()
        );
        assert!(!tracker.prepare_sighting(sight(4, 10, 100, now + Duration::from_secs(3)), &[1]));
        assert!(
            tracker
                .record(sight(4, 11, 101, now), hash_key(1))
                .is_none(),
            "a replay must leave the existing hit latched"
        );
        assert!(tracker.prepare_sighting(sight(4, 10, 100, now), &[9]));
        assert!(
            tracker
                .record(sight(4, 12, 102, now), hash_key(1))
                .is_some(),
            "a replaced attachment drops that channel and can hit again"
        );
    }

    #[test]
    fn claim_review_is_one_shot_and_guild_scoped() {
        let tracker = tracker();
        let now = Instant::now();
        assert!(
            tracker
                .record(sight(4, 10, 100, now), hash_key(1))
                .is_none()
        );
        let hit = tracker
            .record(sight(4, 11, 101, now), hash_key(1))
            .expect("hit");
        tracker.open_review(&hit, None, String::new());
        assert!(
            tracker
                .claim_review(hit.review_id, 99, tracker.process_nonce)
                .is_none()
        );
        let review = tracker
            .claim_review(hit.review_id, 1, tracker.process_nonce)
            .expect("claim");
        assert_eq!(review.user, 4);
        assert!(
            tracker
                .claim_review(hit.review_id, 1, tracker.process_nonce)
                .is_none()
        );
        tracker.restore_review(hit.review_id, review);
        assert!(
            tracker
                .claim_review(hit.review_id, 1, tracker.process_nonce)
                .is_some()
        );
    }

    fn open_app() -> (Arc<BotState>, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "attachment_burst_{:032x}.db",
            rand::random::<u128>()
        ));
        let app = crate::state::BotState::new(
            jmodguard::config::BotProcessConfig::default(),
            crate::db::Db::open(&path, 2).unwrap(),
        )
        .unwrap();
        app.activation.mark_lock_held(true);
        (app, path)
    }

    fn close_app(path: &std::path::Path) {
        jmodguard::tenant::remove_sidecars(path);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    async fn fixture_context(http: Arc<serenity::http::Http>) -> Context {
        use serenity::gateway::{
            Shard, ShardManager, ShardManagerOptions, ShardMessenger, ShardRunner,
            ShardRunnerOptions,
        };
        use serenity::model::gateway::{GatewayIntents, ShardInfo};
        use serenity::model::id::ShardId;
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let url = Arc::new(tokio::sync::Mutex::new(format!(
            "ws://{}",
            listener.local_addr().unwrap()
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

    fn http_at(address: std::net::SocketAddr) -> Arc<serenity::http::Http> {
        Arc::new(
            serenity::http::HttpBuilder::new("token")
                .proxy(format!("http://{address}"))
                .ratelimiter_disabled(true)
                .build(),
        )
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<(String, String)> {
        use tokio::io::AsyncReadExt as _;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            let n = stream.read(&mut tmp).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&tmp[..n]);
            let Some(end) = buf.windows(4).position(|window| window == b"\r\n\r\n") else {
                if buf.len() > 65_536 {
                    return None;
                }
                continue;
            };
            let header = String::from_utf8_lossy(&buf[..end]).to_string();
            let length = header
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let mut have = buf.len() - (end + 4);
            while have < length {
                let n = stream.read(&mut tmp).await.ok()?;
                if n == 0 {
                    return None;
                }
                have += n;
            }
            let mut parts = header.lines().next()?.split_whitespace();
            let method = parts.next()?.to_string();
            let path = parts.next()?.to_string();
            return Some((method, path));
        }
    }

    async fn write_response(stream: &mut tokio::net::TcpStream, status: &str, body: &str) {
        use tokio::io::AsyncWriteExt as _;
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
    }

    fn test_hit(app: &BotState, author: u64) -> Hit {
        let now = Instant::now();
        let sight = |channel| Sighting {
            guild: 10,
            author,
            channel,
            message: channel * 100 + author,
            at: now,
        };
        assert!(app.attachment_burst.record(sight(1), hash_key(1)).is_none());
        app.attachment_burst
            .record(sight(2), hash_key(1))
            .expect("two channels")
    }

    #[tokio::test]
    async fn enforcement_stays_bounded_and_drops_the_rest() {
        use std::sync::atomic::AtomicUsize;
        use tokio::io::AsyncReadExt as _;
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let open = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let accepted_task = Arc::clone(&accepted);
        let open_task = Arc::clone(&open);
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                accepted_task.fetch_add(1, Ordering::SeqCst);
                let open_task = Arc::clone(&open_task);
                tokio::spawn(async move {
                    while open_task.load(Ordering::SeqCst) {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    let _ = stream.read(&mut [0u8; 1024]).await;
                    write_response(
                        &mut stream,
                        "404 Not Found",
                        r#"{"code":10007,"message":"Unknown Member"}"#,
                    )
                    .await;
                });
            }
        });
        let (app, path) = open_app();
        let ctx = fixture_context(http_at(address)).await;
        for author in 1..=8 {
            dispatch(&app, &ctx, vec![test_hit(&app, author)]);
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while accepted.load(Ordering::SeqCst) < MAX_IN_FLIGHT && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(accepted.load(Ordering::SeqCst), MAX_IN_FLIGHT);
        open.store(false, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(200)).await;
        server.abort();
        close_app(&path);
    }

    #[tokio::test]
    async fn automatic_timeouts_serialize_each_member_before_reading() {
        use hyper::{
            Body, Response, Server,
            service::{make_service_fn, service_fn},
        };
        use std::convert::Infallible;

        let (app, path) = open_app();
        let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment).await.unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let deadlines = Arc::new(Mutex::new(HashMap::<u64, serde_json::Value>::new()));
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let other_done = Arc::new(tokio::sync::Notify::new());
        let service = make_service_fn({
            let requests = Arc::clone(&requests);
            let deadlines = Arc::clone(&deadlines);
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            let other_done = Arc::clone(&other_done);
            move |_| {
                let requests = Arc::clone(&requests);
                let deadlines = Arc::clone(&deadlines);
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                let other_done = Arc::clone(&other_done);
                async move {
                    Ok::<_, Infallible>(service_fn(move |request: hyper::Request<Body>| {
                        let requests = Arc::clone(&requests);
                        let deadlines = Arc::clone(&deadlines);
                        let started = Arc::clone(&started);
                        let release = Arc::clone(&release);
                        let other_done = Arc::clone(&other_done);
                        async move {
                            let user: u64 = request
                                .uri()
                                .path()
                                .rsplit('/')
                                .next()
                                .unwrap()
                                .parse()
                                .unwrap();
                            let patch = request.method() == hyper::Method::PATCH;
                            let first_patch = {
                                let mut requests = requests.lock();
                                let first =
                                    patch && user == 99 && !requests.contains(&(true, user));
                                requests.push((patch, user));
                                first
                            };
                            if patch {
                                let body =
                                    hyper::body::to_bytes(request.into_body()).await.unwrap();
                                let body: serde_json::Value =
                                    serde_json::from_slice(&body).unwrap();
                                if first_patch {
                                    started.notify_one();
                                    release.notified().await;
                                }
                                deadlines
                                    .lock()
                                    .insert(user, body["communication_disabled_until"].clone());
                                if user == 100 {
                                    other_done.notify_one();
                                }
                            }
                            let until = deadlines.lock().get(&user).cloned();
                            let body = serde_json::json!({"user":{"id":user.to_string(),"username":"member","discriminator":"0","avatar":null},"roles":[],"joined_at":"2020-01-01T00:00:00+00:00","deaf":false,"mute":false,"flags":0,"communication_disabled_until":until});
                            Ok::<_, Infallible>(Response::new(Body::from(body.to_string())))
                        }
                    }))
                }
            }
        });
        let server = Server::bind(&"127.0.0.1:0".parse().unwrap()).serve(service);
        let ctx = fixture_context(http_at(server.local_addr())).await;
        let server = tokio::spawn(server);
        dispatch(&app, &ctx, vec![test_hit(&app, 99)]);
        tokio::time::timeout(Duration::from_secs(5), started.notified())
            .await
            .unwrap();
        let now = Instant::now();
        let sight = |channel| Sighting {
            guild: 10,
            author: 99,
            channel,
            message: channel * 100 + 99,
            at: now,
        };
        assert!(app.attachment_burst.record(sight(3), hash_key(2)).is_none());
        let second = app.attachment_burst.record(sight(4), hash_key(2)).unwrap();
        dispatch(&app, &ctx, vec![second, test_hit(&app, 100)]);
        tokio::time::timeout(Duration::from_secs(5), other_done.notified())
            .await
            .unwrap();
        // The other member reaches Discord while the first PATCH is still blocked.
        // Allow the same-member task to reach its read if it is not serialized.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let before_release = requests.lock().clone();
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(5), async {
            while app.attachment_burst.slots.available_permits() != MAX_IN_FLIGHT {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let after = requests.lock().clone();
        let reviews = app.attachment_burst.state.lock().reviews.len();
        server.abort();
        let _ = server.await;
        drop(app);
        close_app(&path);
        assert_eq!(
            before_release
                .iter()
                .filter(|request| **request == (false, 99))
                .count(),
            1,
            "second burst must wait for the first timeout before reading this member"
        );
        assert!(
            before_release.contains(&(true, 100)),
            "another member must remain independent"
        );
        assert_eq!(
            after
                .iter()
                .filter(|request| **request == (false, 99))
                .count(),
            2,
            "the waiting burst must re-read the current timeout"
        );
        assert_eq!(reviews, 3, "serialization must retain each admitted review");
    }

    #[tokio::test]
    async fn timeout_case_keeps_the_admitted_enrollment() {
        use std::sync::atomic::AtomicBool;
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(AtomicBool::new(false));
        let started_task = Arc::clone(&started);
        let release_task = Arc::clone(&release);
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (method, _) = read_request(&mut stream).await.unwrap();
            assert_eq!(method, "GET");
            write_response(
                &mut stream,
                "200 OK",
                r#"{"user":{"id":"99","username":"member","discriminator":"0","avatar":null},"roles":[],"joined_at":"2020-01-01T00:00:00+00:00","deaf":false,"mute":false,"flags":0}"#,
            ).await;
            let (mut stream, _) = listener.accept().await.unwrap();
            let (method, _) = read_request(&mut stream).await.unwrap();
            assert_eq!(
                method, "PATCH",
                "pause after the real mutation was dispatched"
            );
            started_task.notify_one();
            while !release_task.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            write_response(
                &mut stream,
                "200 OK",
                r#"{"user":{"id":"99","username":"member","discriminator":"0","avatar":null},"roles":[],"joined_at":"2020-01-01T00:00:00+00:00","deaf":false,"mute":false,"flags":0}"#,
            )
            .await;
            loop {
                let Ok((mut extra, _)) = listener.accept().await else {
                    break;
                };
                if read_request(&mut extra).await.is_some() {
                    write_response(&mut extra, "200 OK", "{}").await;
                }
            }
        });
        let (app, path) = open_app();
        let old = crate::db::enroll_guild(&app.db, 10, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(old.clone()).await.unwrap();
        let ctx = fixture_context(http_at(address)).await;
        let mut user = serenity::model::user::User::default();
        user.id = UserId::new(99);
        user.name = "member".into();
        let mut guild = serenity::model::guild::Guild::default();
        guild.id = GuildId::new(10);
        guild.owner_id = UserId::new(1);
        let mut member = serenity::model::guild::Member::default();
        member.guild_id = guild.id;
        member.user = user.clone();
        guild.members.insert(user.id, member);
        let mut event: serenity::model::event::GuildCreateEvent =
            serde_json::from_value(serde_json::to_value(&guild).unwrap()).unwrap();
        ctx.cache.update(&mut event);
        let app_task = Arc::clone(&app);
        let task = tokio::spawn(async move {
            enforce(&app_task, &ctx, test_hit(&app_task, 99)).await;
        });
        tokio::time::timeout(Duration::from_secs(5), started.notified())
            .await
            .expect("timeout request");
        crate::db::retire_guild_enrollment(&app.db, 10, 100)
            .await
            .unwrap();
        app.tenant.schedule_erasure(old).await.unwrap();
        app.tenant.drive_erasure().await.unwrap();
        let new = crate::db::enroll_guild(&app.db, 10, 101)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(new.clone()).await.unwrap();
        release.store(true, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("enforce")
            .unwrap();
        let (store, scope) = app.tenant.live(new).await.unwrap();
        assert_eq!(
            store
                .case_count(scope, crate::tenant::case_limits())
                .await
                .unwrap(),
            0
        );
        server.abort();
        close_app(&path);
    }

    #[tokio::test]
    async fn retained_review_cannot_act_after_rejoin() {
        use std::sync::atomic::AtomicUsize;
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let mutations = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&mutations);
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let Some((method, path)) = read_request(&mut stream).await else {
                    continue;
                };
                if method == "GET" && path.contains("/bans/") {
                    write_response(
                        &mut stream,
                        "404 Not Found",
                        r#"{"code":10026,"message":"Unknown Ban"}"#,
                    )
                    .await;
                } else {
                    if method == "PUT" || method == "PATCH" {
                        observed.fetch_add(1, Ordering::SeqCst);
                    }
                    write_response(&mut stream, "200 OK", r#"{"user":{"id":"99","username":"member","discriminator":"0","avatar":null},"roles":[],"joined_at":"2020-01-01T00:00:00+00:00","deaf":false,"mute":false,"flags":0}"#).await;
                }
            }
        });
        let (app, path) = open_app();
        let old = crate::db::enroll_guild(&app.db, 10, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(old.clone()).await.unwrap();
        let ctx = fixture_context(http_at(address)).await;
        let now = Instant::now();
        let sighting = |channel, message| Sighting {
            guild: 10,
            author: 99,
            channel,
            message,
            at: now,
        };
        assert!(
            app.attachment_burst
                .record(sighting(1, 100), hash_key(1))
                .is_none()
        );
        let hit = app
            .attachment_burst
            .record(sighting(2, 101), hash_key(1))
            .unwrap();
        let review_id = hit.review_id;
        enforce(&app, &ctx, hit).await;
        mutations.store(0, Ordering::SeqCst);
        crate::db::retire_guild_enrollment(&app.db, 10, 100)
            .await
            .unwrap();
        app.tenant.schedule_erasure(old).await.unwrap();
        app.tenant.drive_erasure().await.unwrap();
        let new = crate::db::enroll_guild(&app.db, 10, 101)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(new.clone()).await.unwrap();
        let review = app
            .attachment_burst
            .claim_review(review_id, 10, app.attachment_burst.process_nonce)
            .unwrap();
        let ban = ban_member(&app, &ctx, &review, UserId::new(7)).await;
        let release = release_member(&app, &ctx, &review, UserId::new(7)).await;
        server.abort();
        let _ = server.await;
        assert!(
            matches!(ban, ReviewDecision::Failed),
            "old review must not ban in replacement enrollment"
        );
        assert!(
            matches!(release, ReviewDecision::Failed),
            "old review must not release in replacement enrollment"
        );
        assert_eq!(mutations.load(Ordering::SeqCst), 0);
        let (store, scope) = app.tenant.live(new).await.unwrap();
        assert_eq!(
            store
                .case_count(scope, crate::tenant::case_limits())
                .await
                .unwrap(),
            0
        );
        close_app(&path);
    }

    #[tokio::test]
    async fn invalidated_hits_do_not_dispatch_timeouts_or_reviews() {
        use std::sync::atomic::AtomicUsize;
        for (delay_lookup, replace) in [(false, false), (true, false), (true, true)] {
            let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .await
                .unwrap();
            let address = listener.local_addr().unwrap();
            let mutations = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&mutations);
            let started = Arc::new(tokio::sync::Notify::new());
            let resume = Arc::new(tokio::sync::Notify::new());
            let started_task = Arc::clone(&started);
            let resume_task = Arc::clone(&resume);
            let server = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let Some((method, _)) = read_request(&mut stream).await else {
                        continue;
                    };
                    if method == "GET" && delay_lookup {
                        started_task.notify_one();
                        resume_task.notified().await;
                    } else if method == "PATCH" || method == "POST" {
                        observed.fetch_add(1, Ordering::SeqCst);
                    }
                    write_response(&mut stream, "200 OK", r#"{"user":{"id":"99","username":"member","discriminator":"0","avatar":null},"roles":[],"joined_at":"2020-01-01T00:00:00+00:00","deaf":false,"mute":false,"flags":0}"#).await;
                }
            });
            let (app, path) = open_app();
            let ctx = fixture_context(http_at(address)).await;
            let now = Instant::now();
            let sighting = |channel, message| Sighting {
                guild: 10,
                author: 99,
                channel,
                message,
                at: now,
            };
            assert!(
                app.attachment_burst
                    .record(sighting(1, 100), hash_key(1))
                    .is_none()
            );
            let hit = app
                .attachment_burst
                .record(sighting(2, 101), hash_key(1))
                .unwrap();
            let review_id = hit.review_id;
            if !delay_lookup {
                app.attachment_burst.forget_message(10, 1, 100);
            }
            let task_app = Arc::clone(&app);
            let task = tokio::spawn(async move { enforce(&task_app, &ctx, hit).await });
            if delay_lookup {
                tokio::time::timeout(Duration::from_secs(5), started.notified())
                    .await
                    .unwrap();
                if replace {
                    app.attachment_burst
                        .prepare_sighting(sighting(1, 100), &[999]);
                } else {
                    app.attachment_burst.forget_message(10, 1, 100);
                }
                resume.notify_one();
            }
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap();
            server.abort();
            let _ = server.await;
            assert_eq!(
                mutations.load(Ordering::SeqCst),
                0,
                "invalidated evidence must not punish (delayed={delay_lookup}, replaced={replace})"
            );
            assert!(
                app.attachment_burst
                    .claim_review(review_id, 10, app.attachment_burst.process_nonce)
                    .is_none(),
                "invalidated evidence must not open a review"
            );
            close_app(&path);
        }
    }

    #[tokio::test]
    async fn review_ban_owns_one_gateway_echo() {
        for early_echo in [false, true] {
            let (app, path) = open_app();
            let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
                .await
                .unwrap()
                .unwrap();
            app.tenant.activate(enrollment.clone()).await.unwrap();
            let gateway_app = Arc::clone(&app);
            let gateway_enrollment = enrollment.clone();
            let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .await
                .unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        break;
                    };
                    let Some((method, path)) = read_request(&mut stream).await else {
                        continue;
                    };
                    if method == "GET" && path.contains("/bans/") {
                        write_response(
                            &mut stream,
                            "404 Not Found",
                            r#"{"code":10026,"message":"Unknown Ban"}"#,
                        )
                        .await;
                    } else if method == "PUT" && path.contains("/bans/") {
                        if early_echo {
                            let echo = gateway_app.consume_gateway_ban_echo(10, 99).resolve();
                            assert!(echo.owned_by_bot && echo.suppress_case);
                            assert!(echo.operation_id.is_some());
                            crate::db::record_gateway_ban(
                                &gateway_app.db,
                                Some(gateway_enrollment.clone()),
                                99,
                                None,
                                None,
                                None,
                                true,
                                echo.operation_id.as_deref(),
                                false,
                            )
                            .await
                            .unwrap();
                        }
                        write_response(&mut stream, "204 No Content", "").await;
                    } else {
                        write_response(&mut stream, "200 OK", "{}").await;
                    }
                }
            });

            let ctx = fixture_context(http_at(address)).await;
            let decision = ban_member(
                &app,
                &ctx,
                &Review {
                    guild: 10,
                    user: 99,
                    body: String::new(),
                    at: Instant::now(),
                    enrollment: Some(enrollment.clone()),
                },
                UserId::new(7),
            )
            .await;
            assert!(matches!(decision, ReviewDecision::Banned));
            if !early_echo {
                let echo = app.consume_gateway_ban_echo(10, 99).resolve();
                assert!(echo.suppress_case);
                assert!(echo.owned_by_bot);
                assert!(echo.operation_id.is_some());
                assert!(!crate::handlers::record_unresolved_gateway_observation(
                    echo.suppress_case
                ));
            }
            let rows = crate::db::list_enrolled_bans(&app.reads, enrollment.clone(), 10)
                .await
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].moderator_id, Some(7));
            assert_eq!(
                rows[0].reason.as_deref(),
                Some(ATTACHMENT_BURST_REVIEW_REASON)
            );
            let (store, scope) = app.tenant.live(enrollment).await.unwrap();
            assert_eq!(
                store
                    .case_count(scope, crate::tenant::case_limits())
                    .await
                    .unwrap(),
                1
            );
            server.abort();
            let _ = server.await;
            close_app(&path);
        }
    }
    #[tokio::test]
    async fn fenced_burst_mutations() {
        use jmodguard::db::SqlExecutor as _;
        use std::sync::atomic::AtomicUsize;
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let changes = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&changes);
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let Some((method, path)) = read_request(&mut stream).await else {
                    continue;
                };
                if method == "GET" && path.contains("/bans/") {
                    write_response(
                        &mut stream,
                        "404 Not Found",
                        r#"{"code":10026,"message":"Unknown Ban"}"#,
                    )
                    .await;
                } else {
                    if method == "PUT" || method == "PATCH" {
                        observed.fetch_add(1, Ordering::SeqCst);
                    }
                    if method == "PUT" {
                        write_response(&mut stream, "204 No Content", "").await;
                    } else {
                        write_response(&mut stream,"200 OK",r#"{"user":{"id":"99","username":"member","discriminator":"0","avatar":null},"roles":[],"joined_at":"2020-01-01T00:00:00+00:00","deaf":false,"mute":false,"flags":0}"#).await;
                    }
                }
            }
        });
        let (app, path) = open_app();
        let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();
        let destination = crate::db::guild_stores::StoreId::new();
        crate::db::guild_stores::register_store(
            &app.db,
            destination.clone(),
            crate::db::guild_stores::StorePolicy::Dedicated { guild_id: 10 },
        )
        .await
        .unwrap();
        app.db.with_conn(move |conn| { conn.execute_bounded("INSERT INTO guild_write_fences(guild_id,generation,phase,destination_store_id,destination_revision,created_at) VALUES(10,?1,'freeze',?2,?1,unixepoch())",libsql::params![format!("{:032x}",9u128),destination.as_str()],65536)?; Ok(()) }).await.unwrap();
        assert!(
            crate::db::guild_stores::guild_is_write_fenced(&app.db, 10)
                .await
                .unwrap()
        );
        let ctx = fixture_context(http_at(address)).await;
        enforce(&app, &ctx, test_hit(&app, 99)).await;
        let after_timeout = changes.load(Ordering::SeqCst);
        let review = Review {
            guild: 10,
            user: 99,
            body: String::new(),
            at: Instant::now(),
            enrollment: Some(enrollment),
        };
        let _ = release_member(&app, &ctx, &review, UserId::new(7)).await;
        let after_release = changes.load(Ordering::SeqCst);
        let _ = ban_member(&app, &ctx, &review, UserId::new(7)).await;
        let after_ban = changes.load(Ordering::SeqCst);
        server.abort();
        close_app(&path);
        assert_eq!(
            (after_timeout, after_release, after_ban),
            (0, 0, 0),
            "fenced timeout, release, ban wrote Discord"
        );
    }

    #[test]
    fn repeated_channel_keeps_pending_hit() {
        let tracker = tracker();
        let now = Instant::now();
        assert!(
            tracker
                .record(sight(4, 10, 100, now), hash_key(1))
                .is_none()
        );
        let hit = tracker.record(sight(4, 11, 101, now), hash_key(1)).unwrap();
        assert!(hit.still_valid());
        assert!(
            tracker
                .record(sight(4, 10, 102, now), hash_key(1))
                .is_none()
        );
        assert!(
            hit.still_valid(),
            "same-channel repeat destroyed pending proof"
        );
        tracker.forget_message(1, 10, 100);
        assert!(
            !hit.still_valid(),
            "deleting the original still revokes its proof"
        );
    }

    #[test]
    fn admission_respects_deletes_edits_and_membership_invalidation() {
        let tracker = tracker();
        let now = Instant::now();
        let file = |id| FileMeta {
            attachment_id: id,
            size: 42,
            filename: "a.png".into(),
            content_type: "image/png".into(),
        };
        let first = sight(4, 10, 100, now);
        let captured = tracker.revision();
        tracker.forget_message(1, 10, 100);
        assert!(
            tracker
                .stage_sighting(first, vec![file(1)], admission(captured, 10))
                .is_empty()
        );
        assert!(
            tracker
                .stage_sighting(first, vec![file(1)], admission(tracker.revision(), 10))
                .is_empty(),
            "delete tombstone rejects fresh replay"
        );
        let current = sight(4, 11, 101, now);
        let ids = tracker.stage_sighting(current, vec![file(2)], admission(tracker.revision(), 20));
        assert_eq!(ids.len(), 1);
        assert_eq!(tracker.author_of(1, 11, 101), Some(4));
        // An unchanged edit advances ordering without refreshing evidence time.
        assert!(
            tracker
                .stage_sighting(current, vec![file(2)], admission(tracker.revision(), 30))
                .is_empty()
        );
        assert!(
            tracker
                .stage_sighting(current, vec![file(3)], admission(tracker.revision(), 25))
                .is_empty()
        );
        assert_eq!(tracker.staged_generation(2), Some(ids[0].1));
        tracker.invalidate_author(1, 4);
        assert!(tracker.staged_generation(2).is_none());
        assert!(
            tracker
                .stage_sighting(current, vec![file(2)], admission(tracker.revision(), 30))
                .is_empty(),
            "membership invalidation cannot be undone by replay"
        );
        assert!(
            tracker
                .stage_sighting(current, vec![file(3)], admission(tracker.revision(), 40))
                .is_empty(),
            "another pre-departure snapshot cannot revive the physical message"
        );
        let next = sight(4, 12, 102, now);
        let revision = tracker.revision();
        assert_eq!(
            tracker
                .stage_sighting(next, vec![file(4)], admission(revision, 40))
                .len(),
            1
        );
        tracker.invalidate_guild(1);
        assert!(tracker.staged_generation(4).is_none());
        assert!(
            tracker
                .stage_sighting(next, vec![file(5)], admission(revision, 50))
                .is_empty()
        );
        let edited = sight(4, 13, 103, now);
        assert_eq!(
            tracker
                .stage_sighting(edited, vec![file(6)], admission(tracker.revision(), 10))
                .len(),
            1
        );
        tracker.invalidate_message_at(1, 13, 103, false, Some(30));
        assert!(
            tracker
                .stage_sighting(edited, vec![file(7)], admission(tracker.revision(), 20))
                .is_empty(),
            "failed config read still advances edit ordering"
        );
        assert_eq!(
            tracker
                .stage_sighting(edited, vec![file(8)], admission(tracker.revision(), 40))
                .len(),
            1,
            "a genuine later replacement remains admissible"
        );
    }

    #[test]
    fn decision_edits_suppress_mentions_and_bound_utf16() {
        let tracker = tracker();
        let now = Instant::now();
        tracker.record(sight(4, 10, 100, now), hash_key(1));
        let hit = tracker.record(sight(4, 11, 101, now), hash_key(1)).unwrap();
        tracker.open_review(&hit, None, format!("<@4> @everyone {}", "😀".repeat(1000)));
        let review = tracker
            .claim_review(hit.review_id, 1, tracker.process_nonce)
            .unwrap();
        assert!(review.body.encode_utf16().count() <= 1800);
        for label in [ATTACHMENT_BURST_RELEASED, ATTACHMENT_BURST_BANNED] {
            let value =
                serde_json::to_value(decision_edit(review.body.clone(), label, UserId::new(7)))
                    .unwrap();
            assert!(value["content"].as_str().unwrap().encode_utf16().count() <= 2000);
            assert!(value["content"].as_str().unwrap().contains(label));
            assert_eq!(value["allowed_mentions"]["parse"], serde_json::json!([]));
            assert_eq!(value["allowed_mentions"]["replied_user"], false);
            assert_eq!(value["components"], serde_json::json!([]));
        }
    }
    #[tokio::test]
    async fn timeout_paths_do_not_record_after_writer_cutoff() {
        let mut counts = Vec::new();
        for release in [false, true] {
            let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .await
                .unwrap();
            let address = listener.local_addr().unwrap();
            let patch_started = Arc::new(tokio::sync::Notify::new());
            let observed = Arc::clone(&patch_started);
            let server = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let Some((method, _)) = read_request(&mut stream).await else {
                        continue;
                    };
                    if method == "PATCH" {
                        observed.notify_one();
                        std::future::pending::<()>().await;
                    } else {
                        write_response(&mut stream, "200 OK", r#"{"user":{"id":"99","username":"member","discriminator":"0","avatar":null},"roles":[],"joined_at":"2020-01-01T00:00:00+00:00","deaf":false,"mute":false,"flags":0}"#).await;
                    }
                }
            });
            let (app, path) = open_app();
            // A real lease keeps selected-case storage writable after activation
            // cutoff, just as during shutdown before the durable lease is released.
            assert_eq!(
                crate::activation::poll_writer_lock(&app.db, &app.activation)
                    .await
                    .unwrap(),
                crate::activation::WriterLockPoll::Ready
            );
            let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
                .await
                .unwrap()
                .unwrap();
            app.tenant.activate(enrollment.clone()).await.unwrap();
            let ctx = fixture_context(http_at(address)).await;
            let operation = async {
                if release {
                    let decision = release_member(
                        &app,
                        &ctx,
                        &Review {
                            guild: 10,
                            user: 99,
                            body: String::new(),
                            at: Instant::now(),
                            enrollment: Some(enrollment.clone()),
                        },
                        UserId::new(7),
                    )
                    .await;
                    assert!(matches!(decision, ReviewDecision::Failed));
                } else {
                    enforce(&app, &ctx, test_hit(&app, 99)).await;
                }
            };
            let cutoff = async {
                tokio::time::timeout(Duration::from_secs(5), patch_started.notified())
                    .await
                    .unwrap();
                assert!(app.activation.may_record());
                app.activation.begin_shutdown();
                app.activation.cut_off();
                assert!(!app.activation.may_record());
            };
            tokio::join!(operation, cutoff);
            let (store, scope) = app.tenant.live(enrollment).await.unwrap();
            counts.push(
                store
                    .case_ids(scope, None, 10, crate::tenant::case_limits())
                    .await
                    .unwrap()
                    .len(),
            );
            server.abort();
            let _ = server.await;
            close_app(&path);
        }
        assert_eq!(
            counts,
            vec![0, 0],
            "automatic timeout and Release must stop recording at cutoff"
        );
    }

    #[tokio::test]
    async fn uncertain_timeout_records_case() {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let stall = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stalled = Arc::clone(&stall);
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let Some((method, _)) = read_request(&mut stream).await else {
                    continue;
                };
                if method == "PATCH" {
                    observed.fetch_add(1, Ordering::SeqCst);
                    if stalled.load(Ordering::SeqCst) {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                    }
                    write_response(
                        &mut stream,
                        "500 Internal Server Error",
                        r#"{"code":0,"message":"unknown result"}"#,
                    )
                    .await;
                } else {
                    write_response(&mut stream,"200 OK",r#"{"user":{"id":"99","username":"member","discriminator":"0","avatar":null},"roles":[],"joined_at":"2020-01-01T00:00:00+00:00","deaf":false,"mute":false,"flags":0}"#).await;
                }
            }
        });
        let (app, path) = open_app();
        let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();
        let ctx = fixture_context(http_at(address)).await;
        enforce(&app, &ctx, test_hit(&app, 99)).await;
        assert!(
            calls.load(Ordering::SeqCst) > 0,
            "positive control: mutation was attempted"
        );
        // A Release request that reached Discord but exceeded our outer deadline
        // has an unknown outcome and still needs its selected case.
        stall.store(true, Ordering::SeqCst);
        let decision = release_member(
            &app,
            &ctx,
            &Review {
                guild: 10,
                user: 99,
                body: String::new(),
                at: Instant::now(),
                enrollment: Some(enrollment.clone()),
            },
            UserId::new(7),
        )
        .await;
        assert!(matches!(decision, ReviewDecision::Failed));
        assert!(calls.load(Ordering::SeqCst) >= 2);
        let (store, scope) = app.tenant.live(enrollment).await.unwrap();
        let ids = store
            .case_ids(scope.clone(), None, 10, crate::tenant::case_limits())
            .await
            .unwrap();
        let cases = store
            .case_summaries(scope, ids, crate::tenant::case_limits())
            .await
            .unwrap();
        assert_eq!(cases.len(), 2);
        assert!(
            cases
                .values()
                .all(|case| matches!(case.outcome, crate::db::guild_data::CaseOutcome::Uncertain))
        );
        assert!(cases.values().any(|case| matches!(
            case.action,
            crate::db::guild_data::CaseAction::TimeoutRemoval
        )));
        server.abort();
        close_app(&path);
    }
    #[tokio::test]
    async fn review_ban_deadline_settles_case_and_audit_identity() {
        use std::sync::atomic::AtomicUsize;
        let (app, path) = open_app();
        let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let puts = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&puts);
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let Some((method, path)) = read_request(&mut stream).await else {
                    continue;
                };
                if method == "GET" && path.contains("/bans/") {
                    write_response(
                        &mut stream,
                        "404 Not Found",
                        r#"{"code":10026,"message":"Unknown Ban"}"#,
                    )
                    .await;
                } else if method == "PUT" {
                    seen.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(90)).await;
                } else {
                    write_response(&mut stream, "200 OK", "{}").await;
                }
            }
        });
        let ctx = fixture_context(http_at(address)).await;
        let review = Review {
            guild: 10,
            user: 99,
            body: String::new(),
            at: Instant::now(),
            enrollment: Some(enrollment.clone()),
        };
        assert!(matches!(
            ban_member(&app, &ctx, &review, UserId::new(7)).await,
            ReviewDecision::Failed
        ));
        server.abort();
        let _ = server.await;
        assert_eq!(
            puts.load(Ordering::SeqCst),
            1,
            "actual ban PUT must reach fixture"
        );
        let (store, scope) = app.tenant.live(enrollment).await.unwrap();
        let ids = store
            .case_ids(scope.clone(), None, 10, crate::tenant::case_limits())
            .await
            .unwrap();
        let summaries = store
            .case_summaries(scope, ids, crate::tenant::case_limits())
            .await
            .unwrap();
        assert_eq!(summaries.len(), 1);
        let outcome = summaries.values().next().unwrap().outcome.clone();
        let audit = app.remember_discord_audit(crate::db::guild_data::CaseAction::Ban, 10, 99, 777);
        assert!(
            audit.is_some(),
            "uncertain dispatched ban must retain its application identity"
        );
        drop(app);
        close_app(&path);
        assert!(
            matches!(outcome, crate::db::guild_data::CaseOutcome::Uncertain),
            "ban reached Discord but deadline left {outcome:?}"
        );
    }

    fn review_interaction(
        app: &BotState,
        ctx: &Context,
        enrollment: crate::db::GuildEnrollment,
    ) -> ComponentInteraction {
        use serenity::model::id::{ChannelId, RoleId};
        let mut role = serenity::model::guild::Role::default();
        role.id = RoleId::new(20);
        role.permissions = serenity::model::permissions::Permissions::BAN_MEMBERS;
        let mut guild = serenity::model::guild::Guild::default();
        guild.id = GuildId::new(10);
        guild.roles.insert(role.id, role);
        let mut event: serenity::model::event::GuildCreateEvent =
            serde_json::from_value(serde_json::to_value(&guild).unwrap()).unwrap();
        ctx.cache.update(&mut event);
        assert_eq!(
            crate::commands::cached_moderator_status(app, &ctx.cache, GuildId::new(10), 7, &[20]),
            ModeratorStatus::Moderator
        );
        let hit = test_hit(app, 99);
        let id = app.attachment_burst.component_id("r", hit.review_id);
        app.attachment_burst
            .open_review(&hit, Some(enrollment), String::new());
        let mut user = serenity::model::user::User::default();
        user.id = UserId::new(7);
        user.name = "moderator".into();
        let mut member = serenity::model::guild::Member::default();
        member.guild_id = GuildId::new(10);
        member.user = user.clone();
        member.roles = vec![RoleId::new(20)];
        let mut message = serenity::model::channel::Message::default();
        message.id = serenity::model::id::MessageId::new(500);
        message.channel_id = ChannelId::new(10);
        serde_json::from_value(serde_json::json!({"id":"1","application_id":"1","type":3,"data":{"custom_id":id,"component_type":2},"guild_id":"10","channel_id":"10","member":member,"user":user,"token":"fixture","version":1,"message":message,"locale":"en-US","app_permissions":"8","entitlements":[],"authorizing_integration_owners":{},"context":0,"attachment_size_limit":10000000})).unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn review_ack_precedes_database_wait() {
        let (app, path) = open_app();
        let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let started = Arc::new(std::sync::Mutex::new(Instant::now()));
        let clock = Arc::clone(&started);
        let latency = Arc::new(AtomicU64::new(u64::MAX));
        let observed = Arc::clone(&latency);
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let Some((_, path)) = read_request(&mut stream).await else {
                    continue;
                };
                if path.contains("/interactions/") {
                    let ms = clock.lock().unwrap().elapsed().as_millis() as u64;
                    observed.store(ms, Ordering::SeqCst);
                    if ms > 3000 {
                        write_response(
                            &mut stream,
                            "404 Not Found",
                            r#"{"code":10062,"message":"Unknown interaction"}"#,
                        )
                        .await;
                    } else {
                        write_response(&mut stream, "204 No Content", "").await;
                    }
                } else {
                    write_response(&mut stream,"200 OK",r#"{"user":{"id":"99","username":"member","discriminator":"0","avatar":null},"roles":[],"joined_at":"2020-01-01T00:00:00+00:00","deaf":false,"mute":false,"flags":0}"#).await;
                }
            }
        });
        let ctx = fixture_context(http_at(address)).await;
        let interaction = review_interaction(&app, &ctx, enrollment);
        let ready = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut blockers = Vec::new();
        for _ in 0..2 {
            let app = Arc::clone(&app);
            let ready = Arc::clone(&ready);
            blockers.push(tokio::spawn(async move {
                app.db
                    .with_conn(move |_| {
                        ready.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_secs(4));
                        Ok(())
                    })
                    .await
                    .unwrap()
            }));
        }
        while ready.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        *started.lock().unwrap() = Instant::now();
        handle_component(&app, &ctx, interaction).await;
        for blocker in blockers {
            blocker.await.unwrap();
        }
        let actual = latency.load(Ordering::SeqCst);
        server.abort();
        let _ = server.await;
        drop(app);
        close_app(&path);
        assert!(
            actual < 3000,
            "initial component acknowledgement took {actual} ms, exceeding Discord deadline"
        );
    }

    #[tokio::test]
    async fn review_read_error_preserves_original_claim() {
        use jmodguard::db::SqlExecutor as _;
        let (app, path) = open_app();
        let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let mutations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = Arc::clone(&mutations);
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let Some((method, route)) = read_request(&mut stream).await else {
                    continue;
                };
                if method == "PATCH" && route.contains("/members/") {
                    seen.fetch_add(1, Ordering::SeqCst);
                }
                if route.contains("/members/") {
                    write_response(&mut stream, "200 OK", r#"{"user":{"id":"99","username":"member","discriminator":"0","avatar":null},"roles":[],"joined_at":"2020-01-01T00:00:00+00:00","deaf":false,"mute":false,"flags":0}"#).await;
                } else {
                    write_response(&mut stream, "204 No Content", "").await;
                }
            }
        });
        let ctx = fixture_context(http_at(address)).await;
        let interaction = review_interaction(&app, &ctx, enrollment.clone());
        app.db
            .with_conn(|conn| {
                conn.execute_bounded(
                    "UPDATE guild_enrollments SET joined_at_ns='unreadable fixture value' WHERE guild_id=10",
                    [] as [libsql::Value; 0],
                    65536,
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(crate::db::guild_enrollment(&app.db, 10).await.is_err());
        let mut retry = interaction.clone();
        retry.id = InteractionId::new(2);
        handle_component(&app, &ctx, interaction).await;
        assert_eq!(mutations.load(Ordering::SeqCst), 0);
        app.db
            .with_conn(|conn| {
                conn.execute_bounded(
                    "UPDATE guild_enrollments SET joined_at_ns=100 WHERE guild_id=10",
                    [] as [libsql::Value; 0],
                    65536,
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(cases::capture(&app, 10).await, Some(enrollment));
        let retained = app.attachment_burst.state.lock().reviews.len();
        assert_eq!(
            retained, 1,
            "the failed read must restore the original claim"
        );
        handle_component(&app, &ctx, retry).await;
        assert_eq!(
            mutations.load(Ordering::SeqCst),
            1,
            "the healthy retry must actually release the member"
        );
        assert!(app.attachment_burst.state.lock().reviews.is_empty());
        server.abort();
        let _ = server.await;
        drop(app);
        close_app(&path);
        assert_eq!(
            retained, 1,
            "a read error permanently consumed the visible review without an action"
        );
    }
    #[tokio::test]
    async fn decision_edits_preserve_log_date_formatting() {
        let (app, path) = open_app();
        let raw = "2026-10-26T02:03:04.123456+00:00";
        let expected = "Oct 26, 2026 • 02:03:04 UTC";
        let mut vars = BTreeMap::from([
            ("time".to_string(), raw.to_string()),
            ("rule".to_string(), "Attachment burst".to_string()),
            ("mention".to_string(), "user 99".to_string()),
            ("user_id".to_string(), "99".to_string()),
            ("channel".to_string(), "channel 1".to_string()),
            ("action".to_string(), "timeout 10m".to_string()),
            ("reason".to_string(), "fixture".to_string()),
            ("content".to_string(), "fixture".to_string()),
        ]);
        let preview = logging::preview_automod(&app, GuildId::new(10), &mut vars).await;
        let bodies: Vec<_> = [ATTACHMENT_BURST_RELEASED, ATTACHMENT_BURST_BANNED]
            .into_iter()
            .map(|label| {
                let rendered =
                    serde_json::to_value(decision_edit(preview.clone(), label, UserId::new(7)))
                        .unwrap();
                rendered["content"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(
            vars["time"], raw,
            "display formatting must preserve raw inputs"
        );
        drop(app);
        close_app(&path);
        for body in bodies {
            assert!(
                body.contains(expected),
                "decision must keep documented whole-second UTC display; got {body}"
            );
            assert!(!body.contains(raw));
        }
    }

    #[tokio::test]
    async fn automatic_timeout_participates_in_shutdown_drain() {
        let (app, path) = open_app();
        let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let seen = Arc::clone(&started);
        let resume = Arc::clone(&release);
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let Some((method, _)) = read_request(&mut stream).await else {
                    continue;
                };
                if method == "PATCH" {
                    seen.notify_one();
                    resume.notified().await;
                }
                write_response(&mut stream, "200 OK", r#"{"user":{"id":"99","username":"member","discriminator":"0","avatar":null},"roles":[],"joined_at":"2020-01-01T00:00:00+00:00","deaf":false,"mute":false,"flags":0}"#).await;
            }
        });
        let ctx = fixture_context(http_at(address)).await;
        let ingress_hold = app.activation.hold_rule();
        dispatch(&app, &ctx, vec![test_hit(&app, 99)]);
        let before_poll = (
            app.activation.in_flight(),
            app.activation.moderation_depth(),
        );
        tokio::time::timeout(Duration::from_secs(5), started.notified())
            .await
            .expect("real timeout PATCH reached fixture");
        drop(ingress_hold);
        let counters = (
            app.activation.in_flight(),
            app.activation.moderation_depth(),
        );
        let activation = Arc::clone(&app.activation);
        activation.begin_shutdown();
        let drain = tokio::spawn(async move {
            crate::activation::drain_for_shutdown_notices(&activation, async {}).await;
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let waited_for_timeout = !drain.is_finished();
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(5), async {
            while app.attachment_burst.slots.available_permits() != MAX_IN_FLIGHT {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("enforcement settled");
        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .unwrap()
            .unwrap();
        let (store, scope) = app.tenant.live(enrollment).await.unwrap();
        let ids = store
            .case_ids(scope.clone(), None, 10, crate::tenant::case_limits())
            .await
            .unwrap();
        let summaries = store
            .case_summaries(scope, ids, crate::tenant::case_limits())
            .await
            .unwrap();
        server.abort();
        let _ = server.await;
        let after_settlement = (
            app.activation.in_flight(),
            app.activation.moderation_depth(),
        );
        drop(app);
        close_app(&path);
        assert_eq!(before_poll, (2, 2), "work must be held before spawning");
        assert_eq!(
            after_settlement,
            (0, 0),
            "settled work must release its hold"
        );
        assert_eq!(
            counters,
            (1, 1),
            "detached work must retain shutdown and settings ordering"
        );
        assert_eq!(summaries.len(), 1);
        assert!(
            summaries
                .values()
                .all(|case| case.outcome == crate::db::guild_data::CaseOutcome::Succeeded)
        );
        assert!(
            waited_for_timeout,
            "shutdown skipped drain while an admitted automatic timeout PATCH was outstanding"
        );
    }
}
