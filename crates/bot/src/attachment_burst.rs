//! Short-window cross-channel attachment burst.
//!
//! Exact-image spam still requires a finished hash set. This tracker fires on
//! its own settings: a content hash when one exists, otherwise size, filename,
//! and content type. Account age is not an input.

use std::collections::{BTreeMap, HashMap};
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

struct State {
    guilds: HashMap<u64, GuildState>,
    authors: usize,
    staged: HashMap<u64, Staged>,
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
    state: Mutex<State>,
    hits: AtomicU64,
}

impl Tracker {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            state: Mutex::new(State {
                guilds: HashMap::new(),
                authors: 0,
                staged: HashMap::new(),
                reviews: HashMap::new(),
                next_review: 1,
                next_generation: 1,
            }),
            hits: AtomicU64::new(0),
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

    pub(crate) fn set_review_body(&self, review_id: u64, body: String) {
        let mut state = self.state.lock();
        if let Some(review) = state.reviews.get_mut(&review_id) {
            review.body = body.chars().take(1800).collect();
        }
    }

    fn claim_review(&self, review_id: u64, guild: u64) -> Option<Review> {
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

    fn forget_message(&mut self, guild: u64, channel: u64, message: u64, min_channels: usize) {
        self.staged.retain(|_, staged| {
            !(staged.sighting.guild == guild
                && staged.sighting.channel == channel
                && staged.sighting.message == message)
        });
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
        app.attachment_burst
            .forget_message(sighting.guild, sighting.channel, sighting.message);
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
    if hits.is_empty() {
        return;
    }
    let app = Arc::clone(app);
    let ctx = ctx.clone();
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    handle.spawn(async move {
        for hit in hits {
            enforce(&app, &ctx, hit).await;
        }
    });
}

async fn enforce(app: &Arc<BotState>, ctx: &Context, hit: Hit) {
    if !app.activation.may_record() {
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
        CreateButton::new(format!("jmgburst:r:{}", hit.review_id))
            .label("Release")
            .style(ButtonStyle::Success),
        CreateButton::new(format!("jmgburst:b:{}", hit.review_id))
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
        let enrollment = cases::capture(app, hit.guild).await;
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
    match crate::discord::race_discord_call(
        &app.activation,
        member.disable_communication_until_datetime(&ctx.http, until),
    )
    .await
    {
        crate::discord::DiscordCall::Finished(Ok(())) => true,
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
    guild.member(&ctx.http, user).await.ok()
}

enum ButtonAction {
    Release,
    Ban,
}

fn parse_button(custom_id: &str) -> Option<(ButtonAction, u64)> {
    let mut parts = custom_id.split(':');
    if parts.next() != Some("jmgburst") {
        return None;
    }
    let action = match parts.next() {
        Some("r") => ButtonAction::Release,
        Some("b") => ButtonAction::Ban,
        _ => return None,
    };
    let id = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((action, id))
}

struct ButtonPress {
    id: InteractionId,
    token: String,
    guild_id: GuildId,
    user_id: UserId,
    roles: Vec<u64>,
    action: ButtonAction,
    review_id: u64,
}

pub(crate) async fn handle_component(
    app: &Arc<BotState>,
    ctx: &Context,
    interaction: ComponentInteraction,
) {
    let Some((action, review_id)) = parse_button(&interaction.data.custom_id) else {
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
    let Some(review) = app
        .attachment_burst
        .claim_review(press.review_id, press.guild_id.get())
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
) -> ReviewDecision {
    let Some(mut member) = member(ctx, GuildId::new(review.guild), UserId::new(review.user)).await
    else {
        return ReviewDecision::Failed;
    };
    if !app.activation.may_record() {
        return ReviewDecision::Failed;
    }
    let raced =
        crate::discord::race_discord_call(&app.activation, member.enable_communication(&ctx.http))
            .await;
    let error = match raced {
        crate::discord::DiscordCall::Finished(result) => result.err(),
        crate::discord::DiscordCall::Cancelled => return ReviewDecision::Failed,
    };
    let Some(outcome) = cases::discord_attempt_outcome(true, error.as_ref()) else {
        return ReviewDecision::Failed;
    };
    let enrollment = cases::capture(app, review.guild).await;
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
    if !app.activation.may_record() {
        return ReviewDecision::Failed;
    }
    let guild = GuildId::new(review.guild);
    let user = UserId::new(review.user);
    let reason = ATTACHMENT_BURST_REVIEW_REASON;
    let raced = crate::discord::race_discord_call(
        &app.activation,
        guild.ban_with_reason(&ctx.http, user, 0, reason),
    )
    .await;
    let error = match raced {
        crate::discord::DiscordCall::Finished(result) => result.err(),
        crate::discord::DiscordCall::Cancelled => return ReviewDecision::Failed,
    };
    let Some(outcome) = cases::discord_attempt_outcome(true, error.as_ref()) else {
        return ReviewDecision::Failed;
    };
    let enrollment = cases::capture(app, review.guild).await;
    cases::record_application(
        app,
        enrollment,
        crate::db::guild_data::CaseAction::Ban,
        outcome.clone(),
        review.user,
        Some(actor.get()),
        Some(reason.to_string()),
        crate::db::guild_data::CaseEvidence::Unavailable,
    )
    .await;
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
        assert!(matches!(
            parse_button("jmgburst:r:15"),
            Some((ButtonAction::Release, 15))
        ));
        assert!(matches!(
            parse_button("jmgburst:b:15"),
            Some((ButtonAction::Ban, 15))
        ));
        assert!(parse_button("jmgburst:r:15:extra").is_none());
        assert!(parse_button("other:r:15").is_none());
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
        assert!(tracker.claim_review(hit.review_id, 99).is_none());
        let review = tracker.claim_review(hit.review_id, 1).expect("claim");
        assert_eq!(review.user, 4);
        assert!(tracker.claim_review(hit.review_id, 1).is_none());
        tracker.restore_review(hit.review_id, review);
        assert!(tracker.claim_review(hit.review_id, 1).is_some());
    }
}
