//! Short-window cross-channel attachment burst.
//!
//! Exact-image spam still requires a finished hash set. This tracker fires on
//! its own settings: a content hash when one exists, otherwise size, filename,
//! and content type. Account age is not an input.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serenity::builder::{
    Builder, CreateActionRow, CreateButton, CreateInteractionResponse,
    CreateInteractionResponseMessage, EditInteractionResponse,
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
    ATTACHMENT_BURST_TIMEOUT_MISSED,
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
}

struct ChannelSight {
    message: u64,
    at: Instant,
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
}

struct MessageNote {
    author: u64,
    at: Instant,
    attachments: BTreeSet<u64>,
}

struct State {
    guilds: HashMap<u64, GuildState>,
    authors: usize,
    staged: HashMap<u64, Staged>,
    messages: HashMap<(u64, u64, u64), MessageNote>,
    reviews: HashMap<u64, Review>,
    next_review: u64,
    next_generation: u64,
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
}

pub struct Tracker {
    settings: Settings,
    process_nonce: u64,
    state: Mutex<State>,
    hits: AtomicU64,
    slots: Arc<tokio::sync::Semaphore>,
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
            }),
            hits: AtomicU64::new(0),
            slots: Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT)),
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
        state.drop_expired(Instant::now(), window);
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

    pub(crate) fn staged_generation(&self, attachment_id: u64) -> Option<u64> {
        self.state
            .lock()
            .staged
            .get(&attachment_id)
            .map(|staged| staged.generation)
    }

    /// Returns the generation stored with this file. A later restage replaces it.
    pub(crate) fn stage(&self, sighting: Sighting, file: FileMeta) -> u64 {
        if !self.usable() {
            self.state.lock().clear();
            return 0;
        }
        let mut state = self.state.lock();
        if state.staged.len() >= MAX_STAGED && !state.staged.contains_key(&file.attachment_id) {
            state.drop_oldest_staged();
        }
        let generation = state.next_generation;
        state.next_generation = state.next_generation.wrapping_add(1).max(1);
        state.staged.insert(
            file.attachment_id,
            Staged {
                generation,
                sighting,
                size: file.size,
                filename: bound_chars(&file.filename, NAME_CHARS),
                content_type: media_type(&file.content_type),
            },
        );
        generation
    }

    #[cfg(test)]
    fn resolve_hash(&self, attachment_id: u64, hash: [u8; 32]) -> Option<Hit> {
        let staged = self.state.lock().staged.remove(&attachment_id)?;
        self.record(staged.sighting, Key::Hash(hash))
    }

    pub(crate) fn resolve_hash_at(
        &self,
        attachment_id: u64,
        generation: u64,
        hash: [u8; 32],
    ) -> Option<Hit> {
        let staged = self.take_staged(attachment_id, generation)?;
        self.record(staged.sighting, Key::Hash(hash))
    }

    #[cfg(test)]
    fn resolve_incomplete(&self, attachment_id: u64) -> Option<Hit> {
        self.resolve_incomplete_at_any(attachment_id)
    }

    pub(crate) fn resolve_incomplete_at(&self, attachment_id: u64, generation: u64) -> Option<Hit> {
        let staged = self.take_staged(attachment_id, generation)?;
        let sighting = staged.sighting;
        self.record(sighting, fallback_key(staged))
    }

    #[cfg(test)]
    fn resolve_incomplete_at_any(&self, attachment_id: u64) -> Option<Hit> {
        let staged = self.state.lock().staged.remove(&attachment_id)?;
        let sighting = staged.sighting;
        self.record(sighting, fallback_key(staged))
    }

    fn take_staged(&self, attachment_id: u64, generation: u64) -> Option<Staged> {
        let mut state = self.state.lock();
        match state.staged.get(&attachment_id) {
            Some(staged) if staged.generation == generation => state.staged.remove(&attachment_id),
            _ => None,
        }
    }

    pub(crate) fn forget_message(&self, guild: u64, channel: u64, message: u64) {
        let min_channels = self.settings.min_channels;
        self.state
            .lock()
            .forget_message(guild, channel, message, min_channels);
    }

    /// False when this message already carries the same attachment ids.
    /// A replay keeps the original sighting time and does not clear a hit.
    pub(crate) fn prepare_sighting(&self, sighting: Sighting, attachment_ids: &[u64]) -> bool {
        if !self.usable() {
            self.state.lock().clear();
            return false;
        }
        let wanted = attachment_ids.iter().copied().collect::<BTreeSet<_>>();
        let mut state = self.state.lock();
        let key = (sighting.guild, sighting.channel, sighting.message);
        if state
            .messages
            .get(&key)
            .is_some_and(|note| note.author == sighting.author && note.attachments == wanted)
        {
            return false;
        }
        let min_channels = self.settings.min_channels;
        state.forget_message(
            sighting.guild,
            sighting.channel,
            sighting.message,
            min_channels,
        );
        state.remember_message(sighting, wanted);
        true
    }

    fn component_id(&self, action: &str, review_id: u64) -> String {
        format!("jmgburst:{:016x}:{action}:{review_id}", self.process_nonce)
    }

    pub(crate) fn set_review_body(&self, review_id: u64, body: String) {
        let mut state = self.state.lock();
        if let Some(review) = state.reviews.get_mut(&review_id) {
            review.body = body.chars().take(1800).collect();
        }
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

    fn record(&self, sighting: Sighting, key: Key) -> Option<Hit> {
        if !self.usable() {
            self.state.lock().clear();
            return None;
        }
        let window = Duration::from_secs(self.settings.window_secs);
        let min_channels = self.settings.min_channels;
        let mut state = self.state.lock();
        state.prepare(sighting.guild, sighting.at, window);
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
                state.evict_author(sighting.guild, sighting.author);
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
            if !author_state.keys.contains_key(&key)
                && author_state.keys.len() >= MAX_KEYS_PER_AUTHOR
            {
                author_state.drop_oldest_key_except(&key);
            }
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
            bucket.channels.insert(
                sighting.channel,
                ChannelSight {
                    message: sighting.message,
                    at: sighting.at,
                },
            );
            let newest = bucket
                .channels
                .values()
                .map(|sight| sight.at)
                .max()
                .unwrap_or(sighting.at);
            bucket
                .channels
                .retain(|_, sight| newest.saturating_duration_since(sight.at) <= window);
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
            if bucket.acted {
                return None;
            }
            bucket.acted = true;
            let channels: Vec<u64> = bucket.channels.keys().copied().collect();
            let messages = channels
                .iter()
                .map(|channel| (*channel, bucket.channels[channel].message))
                .collect();
            Some((channels, messages))
        }?;
        let (channels, messages) = noted;
        let review_id = state.next_review;
        state.next_review = state.next_review.wrapping_add(1).max(1);
        if state.reviews.len() >= MAX_REVIEWS {
            state.drop_oldest_review();
        }
        state.reviews.insert(
            review_id,
            Review {
                guild: sighting.guild,
                user: sighting.author,
                body: String::new(),
                at: sighting.at,
            },
        );
        drop(state);
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
        })
    }
}

impl State {
    fn clear(&mut self) {
        self.guilds.clear();
        self.authors = 0;
        self.staged.clear();
        self.messages.clear();
        self.reviews.clear();
    }

    fn prepare(&mut self, guild: u64, now: Instant, window: Duration) {
        let slack = window.saturating_add(Duration::from_secs(PRUNE_SLACK_SECS));
        self.drop_expired(now, slack);
        if !self.guilds.contains_key(&guild) && self.guilds.len() >= MAX_GUILDS {
            self.remove_oldest_guild_except(guild);
        }
    }

    fn drop_expired(&mut self, now: Instant, slack: Duration) {
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
        self.messages
            .retain(|_, note| now.saturating_duration_since(note.at) <= slack);
    }

    fn drop_old_reviews(&mut self, now: Instant, keep: Duration) {
        self.reviews
            .retain(|_, review| now.saturating_duration_since(review.at) <= keep);
    }

    fn evict_author(&mut self, guild: u64, keep: u64) {
        let Some(guild_state) = self.guilds.get_mut(&guild) else {
            return;
        };
        let oldest = guild_state
            .authors
            .iter()
            .filter(|(author, _)| **author != keep)
            .min_by_key(|(_, author)| author.newest)
            .map(|(author, _)| *author);
        if let Some(author) = oldest {
            guild_state.authors.remove(&author);
            self.authors = self.authors.saturating_sub(1);
        }
    }

    fn remove_oldest_guild_except(&mut self, keep: u64) {
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
            self.messages.retain(|(seen, _, _), _| *seen != guild);
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

    fn remember_message(&mut self, sighting: Sighting, attachments: BTreeSet<u64>) {
        let key = (sighting.guild, sighting.channel, sighting.message);
        if self.messages.len() >= MAX_MESSAGES && !self.messages.contains_key(&key) {
            let oldest = self
                .messages
                .iter()
                .min_by_key(|(_, note)| note.at)
                .map(|(key, _)| *key);
            if let Some(oldest) = oldest {
                self.messages.remove(&oldest);
            }
        }
        self.messages.insert(
            key,
            MessageNote {
                author: sighting.author,
                at: sighting.at,
                attachments,
            },
        );
    }

    fn forget_message(&mut self, guild: u64, channel: u64, message: u64, min_channels: usize) {
        self.staged.retain(|_, staged| {
            !(staged.sighting.guild == guild
                && staged.sighting.channel == channel
                && staged.sighting.message == message)
        });
        self.messages.remove(&(guild, channel, message));
        let Some(guild_state) = self.guilds.get_mut(&guild) else {
            return;
        };
        let authors: Vec<u64> = guild_state.authors.keys().copied().collect();
        for author in authors {
            let Some(author_state) = guild_state.authors.get_mut(&author) else {
                continue;
            };
            author_state.keys.retain(|_, bucket| {
                bucket.channels.retain(|seen_channel, sight| {
                    !(*seen_channel == channel && sight.message == message)
                });
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

impl AuthorState {
    fn drop_oldest_key_except(&mut self, keep: &Key) {
        let oldest = self
            .keys
            .iter()
            .filter(|(key, _)| *key != keep)
            .min_by_key(|(_, bucket)| bucket.newest)
            .map(|(key, _)| key.clone());
        if let Some(key) = oldest {
            self.keys.remove(&key);
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

fn fallback_key(staged: Staged) -> Key {
    Key::Fallback {
        size: staged.size,
        filename: staged.filename,
        content_type: staged.content_type,
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

    pub(crate) fn stage(
        app: &Arc<BotState>,
        ctx: &Context,
        sighting: Sighting,
        files: Vec<FileMeta>,
    ) -> Self {
        let attachment_ids = files
            .iter()
            .map(|file| file.attachment_id)
            .collect::<Vec<_>>();
        if !app
            .attachment_burst
            .prepare_sighting(sighting, &attachment_ids)
        {
            return Self::inert();
        }
        let mut ids = Vec::with_capacity(files.len());
        for file in files {
            let id = file.attachment_id;
            let generation = app.attachment_burst.stage(sighting, file);
            if generation != 0 {
                ids.push((id, generation));
            }
        }
        let armed = !ids.is_empty();
        Self {
            app: Some(Arc::clone(app)),
            ctx: Some(ctx.clone()),
            ids,
            armed,
        }
    }

    pub(crate) fn hand_to_job(&mut self) {
        self.armed = false;
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

pub(crate) fn resolve_known_hashes(
    app: &Arc<BotState>,
    ctx: &Context,
    files: &[jmodguard::image_spam::Attachment],
    hashes: &[jmodguard::image_spam::Hash],
) {
    let mut hits = Vec::new();
    let mut files = files.iter();
    for hash in hashes {
        let Some(file) = files.next() else {
            break;
        };
        let Some(generation) = app.attachment_burst.staged_generation(file.id) else {
            continue;
        };
        if let Some(hit) = app
            .attachment_burst
            .resolve_hash_at(file.id, generation, *hash)
        {
            hits.push(hit);
        }
    }
    for file in files {
        let Some(generation) = app.attachment_burst.staged_generation(file.id) else {
            continue;
        };
        if let Some(hit) = app
            .attachment_burst
            .resolve_incomplete_at(file.id, generation)
        {
            hits.push(hit);
        }
    }
    dispatch(app, ctx, hits);
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
        handle.spawn(async move {
            let _permit = permit;
            enforce(&app, &ctx, hit).await;
        });
    }
}

async fn enforce(app: &Arc<BotState>, ctx: &Context, hit: Hit) {
    if !app.activation.may_record() {
        return;
    }
    let enrollment = cases::capture(app, hit.guild).await;
    if !membership_still(app, hit.guild, &enrollment).await {
        return;
    }
    let timed_out = apply_timeout(app, ctx, &hit).await;
    let action = if timed_out {
        templates::render(
            ATTACHMENT_BURST_TIMEOUT_ACTION,
            &std::collections::BTreeMap::from([(
                "duration".to_string(),
                templates::format_duration(hit.timeout_secs),
            )]),
        )
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
        .set_review_body(hit.review_id, rendered.clone());
    let buttons = vec![CreateActionRow::Buttons(vec![
        CreateButton::new(app.attachment_burst.component_id("r", hit.review_id))
            .label("Release")
            .style(ButtonStyle::Success),
        CreateButton::new(app.attachment_burst.component_id("b", hit.review_id))
            .label("Ban")
            .style(ButtonStyle::Danger),
    ])];
    logging::emit_with_components(
        app,
        ctx.http.as_ref(),
        GuildId::new(hit.guild),
        "automod",
        vars,
        buttons,
    )
    .await;
    if timed_out {
        let (channel, message) = hit.messages.first().copied().unwrap_or((0, 0));
        cases::record(
            app,
            enrollment,
            crate::db::guild_data::CaseAction::Timeout,
            crate::db::guild_data::CaseOutcome::Succeeded,
            hit.author,
            None,
            Some(reason),
            cases::text_excerpt(channel, message, hit.author, &evidence(&hit)),
            cases::application_source(),
        )
        .await;
    }
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

async fn apply_timeout(app: &Arc<BotState>, ctx: &Context, hit: &Hit) -> bool {
    let Some(mut member) = member(ctx, GuildId::new(hit.guild), UserId::new(hit.author)).await
    else {
        tracing::warn!("attachment burst timeout skipped because the member was not available");
        return false;
    };
    let Ok(until) = Timestamp::from_unix_timestamp(
        chrono::Utc::now()
            .timestamp()
            .saturating_add(i64::try_from(hit.timeout_secs).unwrap_or(i64::MAX)),
    ) else {
        return false;
    };
    if !app.activation.may_record() {
        return false;
    }
    match raced(
        app,
        member.disable_communication_until_datetime(&ctx.http, until),
    )
    .await
    {
        Some(Ok(())) => true,
        _ => {
            tracing::warn!("attachment burst timeout was not applied");
            false
        }
    }
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
    let Some(admitted) = admitted else {
        return true;
    };
    cases::capture(app, guild)
        .await
        .is_some_and(|current| current.id == admitted.id)
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
    let enrollment = cases::capture(app, press.guild_id.get()).await;
    if !membership_still(app, press.guild_id.get(), &enrollment).await {
        app.attachment_burst.restore_review(press.review_id, review);
        let _ = ephemeral(ctx, &press, ATTACHMENT_BURST_FAILED).await;
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
    let decision = match press.action {
        ButtonAction::Release => release_member(app, ctx, &review, press.user_id, enrollment).await,
        ButtonAction::Ban => ban_member(app, ctx, &review, press.user_id, enrollment).await,
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
    let mut vars = std::collections::BTreeMap::from([
        ("body".to_string(), review.body),
        ("decision".to_string(), label.to_string()),
        ("actor".to_string(), format!("user {}", press.user_id)),
    ]);
    let content = templates::render(ATTACHMENT_BURST_DECISION, &vars);
    vars.clear();
    let _ = EditInteractionResponse::new()
        .content(content)
        .components(Vec::new())
        .execute(&ctx.http, press.token.as_str())
        .await;
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
    enrollment: Option<crate::db::GuildEnrollment>,
) -> ReviewDecision {
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
    let Some(result) = raced(app, member.enable_communication(&ctx.http)).await else {
        return ReviewDecision::Failed;
    };
    let error = result.err();
    let Some(outcome) = cases::discord_attempt_outcome(true, error.as_ref()) else {
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
    enrollment: Option<crate::db::GuildEnrollment>,
) -> ReviewDecision {
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
        ReviewDecision::Banned
    } else {
        ReviewDecision::Failed
    }
}

async fn ephemeral(ctx: &Context, press: &ButtonPress, text: &str) -> serenity::Result<()> {
    CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new()
            .content(text)
            .ephemeral(true),
    )
    .execute(&ctx.http, (press.id, press.token.as_str()))
    .await
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings::default()
    }

    fn tracker() -> Tracker {
        Tracker::new(settings())
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
        tracker
            .state
            .lock()
            .drop_expired(old + Duration::from_secs(10_000), Duration::from_secs(1));
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
            dispatch(
                &app,
                &ctx,
                vec![Hit {
                    guild: 10,
                    author,
                    review_id: author,
                    channels: vec![1, 2],
                    messages: vec![(1, 10)],
                    key: Key::Hash([1; 32]),
                    timeout_secs: 600,
                    window_secs: 10,
                }],
            );
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
            let _ = read_request(&mut stream).await;
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
            enforce(
                &app_task,
                &ctx,
                Hit {
                    guild: 10,
                    author: 99,
                    review_id: 1,
                    channels: vec![1, 2],
                    messages: vec![(1, 10)],
                    key: Key::Hash([1; 32]),
                    timeout_secs: 600,
                    window_secs: 10,
                },
            )
            .await;
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
    async fn review_ban_owns_one_gateway_echo() {
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
                    write_response(&mut stream, "204 No Content", "").await;
                } else {
                    write_response(&mut stream, "200 OK", "{}").await;
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
        let decision = ban_member(
            &app,
            &ctx,
            &Review {
                guild: 10,
                user: 99,
                body: String::new(),
                at: Instant::now(),
            },
            UserId::new(7),
            Some(enrollment.clone()),
        )
        .await;
        assert!(matches!(decision, ReviewDecision::Banned));
        let echo = app.consume_gateway_ban_echo(10, 99).resolve();
        assert!(echo.suppress_case);
        assert!(echo.owned_by_bot);
        assert!(echo.operation_id.is_some());
        assert!(!crate::handlers::record_unresolved_gateway_observation(
            echo.suppress_case
        ));
        let (store, scope) = app.tenant.live(enrollment).await.unwrap();
        assert_eq!(
            store
                .case_count(scope, crate::tenant::case_limits())
                .await
                .unwrap(),
            1
        );
        server.abort();
        close_app(&path);
    }
}
