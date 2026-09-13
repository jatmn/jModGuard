//! Bounded exact-text observations. Punishment uses `spam_actions`, not automod.
use crate::spam_actions::{self, Kind};
use crate::state::{BotState, CachedGuildConfig};
use jmodguard::db::guild_data::TextEvidence;
use jmodguard::text_spam::{self, Policy};
use parking_lot::Mutex;
use serenity::{
    client::Context,
    model::{channel::Message, event::MessageUpdateEvent},
};
use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
    time::{Duration, Instant},
};

const KEEP: Duration = Duration::from_secs(660);
const MAX_AUTHORS: usize = 512;
const MAX_AUTHORS_PER_GUILD: usize = 64;
const MAX_POSTS: usize = 16;
const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_BYTES_GUILD: usize = 1024 * 1024;
const MAX_TEXT: usize = 4000;
type Key = (u64, u64, u64);

/// Admission result for a single observe/consider call. Wrappers map this to
/// invalidate / report / no-op; do not overload `Option` for multiple meanings.
#[derive(Clone, Debug, Eq, PartialEq)]
enum AdmitOutcome {
    /// Replace path saw the same trimmed text; keep the indexed observation.
    Unchanged,
    /// Observation stored or updated; no cross-channel match yet.
    Admitted,
    /// Cross-channel threshold met.
    Matched(Vec<(u64, u64)>),
    /// Update could not be stored (capacity, stale revision, etc.).
    Rejected,
}

#[derive(Clone)]
struct Post {
    id: u64,
    channel: u64,
    at: Instant,
    edited_ms: i64,
    text: String,
    cost: usize,
    revision: u64,
}
struct Bucket {
    retired_before: u64,
    window: Duration,
    last_seen: Instant,
    token: (u64, u64),
    posts: Vec<Post>,
    reported_until: Instant,
}
struct Guild {
    bytes: usize,
    authors: HashMap<u64, Bucket>,
    watermarks: HashMap<u64, Watermark>,
}
struct Watermark {
    retired_before: u64,
    last_seen: Instant,
}
struct State {
    revision: u64,
    guilds: HashMap<u64, Guild>,
    index: HashMap<Key, u64>,
    authors: usize,
    bytes: usize,
}
pub struct Detector {
    state: Mutex<State>,
}
impl Default for Detector {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                revision: 0,
                guilds: HashMap::new(),
                index: HashMap::new(),
                authors: 0,
                bytes: 0,
            }),
        }
    }
}
impl Detector {
    pub fn revision(&self) -> u64 {
        self.state.lock().revision
    }
    pub fn prune(&self) {
        let now = Instant::now();
        let mut state = self.state.lock();
        let State {
            guilds,
            index,
            authors,
            bytes,
            ..
        } = &mut *state;
        guilds.retain(|guild, g| {
            let ids: Vec<u64> = g.authors.keys().copied().collect();
            for author in ids {
                if let Some(window) = g.authors.get(&author).map(|b| b.window) {
                    retire(
                        g.authors.get_mut(&author).unwrap(),
                        *guild,
                        &mut g.bytes,
                        index,
                        bytes,
                        |p| now.duration_since(p.at) >= window,
                    );
                }
                if g.authors.get(&author).is_some_and(|b| b.posts.is_empty()) {
                    park_author(g, authors, author, now);
                }
            }
            g.watermarks
                .retain(|_, w| now.duration_since(w.last_seen) < KEEP);
            !g.authors.is_empty() || !g.watermarks.is_empty()
        });
    }
    pub fn invalidate(&self, guild: u64, channel: u64, message: u64) {
        let mut state = self.state.lock();
        state.revision = state.revision.wrapping_add(1);
        let Some(author) = state.index.get(&(guild, channel, message)).copied() else {
            return;
        };
        drop_author_posts(&mut state, guild, author, |p| {
            p.id == message && p.channel == channel
        });
    }
    pub fn invalidate_author(&self, guild: u64, author: u64) {
        let mut state = self.state.lock();
        state.revision = state.revision.wrapping_add(1);
        drop_author_posts(&mut state, guild, author, |_| true);
    }
    pub fn invalidate_guild(&self, guild: u64) {
        let mut state = self.state.lock();
        state.revision = state.revision.wrapping_add(1);
        let authors: Vec<u64> = state
            .guilds
            .get(&guild)
            .map(|g| g.authors.keys().copied().collect())
            .unwrap_or_default();
        for author in authors {
            drop_author_posts(&mut state, guild, author, |_| true);
        }
    }
    pub fn author_of(&self, guild: u64, channel: u64, message: u64) -> Option<u64> {
        self.state
            .lock()
            .index
            .get(&(guild, channel, message))
            .copied()
    }

    pub fn excerpts(
        &self,
        guild: u64,
        author: u64,
        messages: &[(u64, u64)],
    ) -> Vec<TextEvidence> {
        let state = self.state.lock();
        let Some(bucket) = state
            .guilds
            .get(&guild)
            .and_then(|g| g.authors.get(&author))
        else {
            return Vec::new();
        };
        messages
            .iter()
            .filter_map(|(channel, id)| {
                bucket
                    .posts
                    .iter()
                    .find(|p| p.channel == *channel && p.id == *id)
                    .map(|p| TextEvidence {
                        channel_id: *channel as i64,
                        message_id: *id as i64,
                        author_id: author as i64,
                        occurred_at_ms: p.edited_ms.max(1),
                        text: p.text.clone(),
                    })
            })
            .collect()
    }
    pub(crate) fn begin_report_window(
        &self,
        guild: u64,
        author: u64,
        window_secs: u64,
        now: Instant,
    ) -> bool {
        let mut state = self.state.lock();
        let Some(b) = state
            .guilds
            .get_mut(&guild)
            .and_then(|g| g.authors.get_mut(&author))
        else {
            return false;
        };
        if now < b.reported_until {
            return false;
        }
        b.reported_until = now + Duration::from_secs(window_secs);
        true
    }

    pub(crate) fn abort_report_window(&self, guild: u64, author: u64, now: Instant) {
        let mut state = self.state.lock();
        if let Some(b) = state
            .guilds
            .get_mut(&guild)
            .and_then(|g| g.authors.get_mut(&author))
        {
            b.reported_until = now;
        }
    }

    /// True when every matched message is still indexed under this author with the
    /// same config token and still forms a live cross-channel exact-text match.
    pub(crate) fn match_still_proving(
        &self,
        guild: u64,
        author: u64,
        token: (u64, u64),
        messages: &[(u64, u64)],
        policy: &Policy,
        now: Instant,
    ) -> bool {
        let state = self.state.lock();
        let Some(b) = state
            .guilds
            .get(&guild)
            .and_then(|g| g.authors.get(&author))
        else {
            return false;
        };
        if b.token != token {
            return false;
        }
        let window = Duration::from_secs(policy.window_secs);
        let posts: Vec<_> = messages
            .iter()
            .filter_map(|(channel, message)| {
                if state.index.get(&(guild, *channel, *message)) != Some(&author) {
                    return None;
                }
                b.posts
                    .iter()
                    .find(|p| p.channel == *channel && p.id == *message)
            })
            .collect();
        if posts.len() != messages.len() {
            return false;
        }
        if posts
            .iter()
            .any(|p| now.duration_since(p.at) >= window)
        {
            return false;
        }
        let text = &posts[0].text;
        if !posts.iter().all(|p| p.text == *text) {
            return false;
        }
        posts
            .iter()
            .map(|p| p.channel)
            .collect::<BTreeSet<_>>()
            .len() >= policy.min_channels
    }

    #[allow(clippy::too_many_arguments)] // Private bounded admission tuple, no external API.
    fn admit(
        &self,
        guild: u64,
        author: u64,
        post: Post,
        token: (u64, u64),
        policy: &Policy,
        now: Instant,
        replace: bool,
    ) -> AdmitOutcome {
        let mut state = self.state.lock();
        if post.revision != state.revision {
            return AdmitOutcome::Rejected;
        }
        let key = (guild, post.channel, post.id);
        let window = state
            .guilds
            .get(&guild)
            .and_then(|g| g.authors.get(&author).map(|b| b.window));
        if let Some(window) = window {
            let State {
                guilds,
                index,
                bytes,
                ..
            } = &mut *state;
            if let Some(g) = guilds.get_mut(&guild)
                && let Some(b) = g.authors.get_mut(&author)
            {
                retire(b, guild, &mut g.bytes, index, bytes, |p| {
                    now.duration_since(p.at) >= window
                });
            }
        }
        if state
            .guilds
            .get(&guild)
            .and_then(|g| g.authors.get(&author))
            .is_some_and(|b| b.token != token)
        {
            drop_author_posts(&mut state, guild, author, |_| true);
        }
        if replace && state.index.get(&key).copied() == Some(author) {
            let State { guilds, bytes, .. } = &mut *state;
            if let Some(g) = guilds.get_mut(&guild)
                && let Some(b) = g.authors.get_mut(&author)
                && let Some(idx) = b
                    .posts
                    .iter()
                    .position(|p| p.id == post.id && p.channel == post.channel)
            {
                if b.posts[idx].text == post.text || post.edited_ms < b.posts[idx].edited_ms {
                    return AdmitOutcome::Unchanged;
                }
                g.bytes = g.bytes.saturating_sub(b.posts[idx].cost);
                *bytes = bytes.saturating_sub(b.posts[idx].cost);
                b.posts[idx].at = post.at;
                b.posts[idx].edited_ms = post.edited_ms;
                b.posts[idx].text = post.text.clone();
                b.posts[idx].cost = post.cost;
                b.posts[idx].revision = post.revision;
                g.bytes += b.posts[idx].cost;
                *bytes += b.posts[idx].cost;
                b.last_seen = now;
                return match detect_match(b, policy, now) {
                    Some(matched) => AdmitOutcome::Matched(matched),
                    None => AdmitOutcome::Admitted,
                };
            }
        }
        let retired_before = state.guilds.get(&guild).and_then(|g| {
            g.authors
                .get(&author)
                .map(|b| b.retired_before)
                .or_else(|| g.watermarks.get(&author).map(|w| w.retired_before))
        });
        if state.index.contains_key(&key) || retired_before.is_some_and(|id| post.id <= id) {
            return AdmitOutcome::Rejected;
        }
        let exists = state
            .guilds
            .get(&guild)
            .is_some_and(|g| g.authors.contains_key(&author));
        if !exists
            && (state.authors >= MAX_AUTHORS
                || state
                    .guilds
                    .get(&guild)
                    .is_some_and(|g| g.authors.len() >= MAX_AUTHORS_PER_GUILD))
        {
            return AdmitOutcome::Rejected;
        }
        if state.bytes + post.cost > MAX_BYTES
            || state
                .guilds
                .get(&guild)
                .is_some_and(|g| g.bytes + post.cost > MAX_BYTES_GUILD)
        {
            return AdmitOutcome::Rejected;
        }
        let State {
            guilds,
            index,
            authors,
            bytes,
            ..
        } = &mut *state;
        let g = guilds.entry(guild).or_insert_with(|| Guild {
            bytes: 0,
            authors: HashMap::new(),
            watermarks: HashMap::new(),
        });
        let watermark = g.watermarks.remove(&author);
        let b = g.authors.entry(author).or_insert_with(|| Bucket {
            retired_before: watermark.map(|w| w.retired_before).unwrap_or(0),
            window: Duration::from_secs(policy.window_secs),
            last_seen: now,
            token,
            posts: vec![],
            reported_until: now,
        });
        if b.posts.len() >= MAX_POSTS {
            return AdmitOutcome::Rejected;
        }
        b.token = token;
        b.window = Duration::from_secs(policy.window_secs);
        b.last_seen = now;
        let cost = post.cost;
        b.posts.push(post);
        g.bytes += cost;
        *bytes += cost;
        if !exists {
            *authors += 1;
        }
        index.insert(key, author);
        match detect_match(b, policy, now) {
            Some(matched) => AdmitOutcome::Matched(matched),
            None => AdmitOutcome::Admitted,
        }
    }
}

fn detect_match(b: &Bucket, policy: &Policy, now: Instant) -> Option<Vec<(u64, u64)>> {
    if now < b.reported_until {
        return None;
    }
    match_posts(b, policy, now)
}

fn match_posts(b: &Bucket, policy: &Policy, now: Instant) -> Option<Vec<(u64, u64)>> {
    let live: Vec<_> = b
        .posts
        .iter()
        .filter(|p| now.duration_since(p.at) < Duration::from_secs(policy.window_secs))
        .collect();
    for anchor in &live {
        let matched: Vec<_> = live
            .iter()
            .filter(|p| p.text == anchor.text)
            .map(|p| (p.channel, p.id))
            .collect();
        if matched
            .iter()
            .map(|(c, _)| *c)
            .collect::<BTreeSet<_>>()
            .len()
            >= policy.min_channels
        {
            return Some(matched);
        }
    }
    None
}

fn retire(
    bucket: &mut Bucket,
    guild: u64,
    guild_bytes: &mut usize,
    index: &mut HashMap<Key, u64>,
    bytes: &mut usize,
    mut drop_post: impl FnMut(&Post) -> bool,
) {
    bucket.posts.retain(|p| {
        if !drop_post(p) {
            return true;
        }
        bucket.retired_before = bucket.retired_before.max(p.id);
        index.remove(&(guild, p.channel, p.id));
        *bytes -= p.cost;
        *guild_bytes -= p.cost;
        false
    });
}

fn park_author(g: &mut Guild, authors: &mut usize, author: u64, now: Instant) {
    let Some(b) = g.authors.remove(&author) else {
        return;
    };
    *authors = authors.saturating_sub(1);
    g.watermarks.insert(
        author,
        Watermark {
            retired_before: b.retired_before,
            last_seen: now,
        },
    );
    while g.watermarks.len() > MAX_AUTHORS_PER_GUILD {
        let oldest = g
            .watermarks
            .iter()
            .min_by_key(|(_, w)| w.last_seen)
            .map(|(id, _)| *id);
        if let Some(id) = oldest {
            g.watermarks.remove(&id);
        } else {
            break;
        }
    }
}

fn drop_author_posts(
    state: &mut State,
    guild: u64,
    author: u64,
    drop_post: impl FnMut(&Post) -> bool,
) {
    let Some(g) = state.guilds.get_mut(&guild) else {
        return;
    };
    if let Some(b) = g.authors.get_mut(&author) {
        retire(
            b,
            guild,
            &mut g.bytes,
            &mut state.index,
            &mut state.bytes,
            drop_post,
        );
    }
    if state
        .guilds
        .get(&guild)
        .and_then(|g| g.authors.get(&author))
        .is_some_and(|b| b.posts.is_empty())
    {
        let g = state.guilds.get_mut(&guild).unwrap();
        park_author(g, &mut state.authors, author, Instant::now());
    }
}

fn drop_stale_update(
    detector: &Detector,
    guild: u64,
    channel: u64,
    message: u64,
    replace: bool,
) {
    if replace {
        detector.invalidate(guild, channel, message);
    }
}

fn apply_admit_outcome(
    detector: &Detector,
    outcome: AdmitOutcome,
    guild: u64,
    channel: u64,
    message: u64,
    replace: bool,
) -> Option<Vec<(u64, u64)>> {
    match outcome {
        AdmitOutcome::Unchanged | AdmitOutcome::Admitted => None,
        AdmitOutcome::Rejected => {
            if replace {
                detector.invalidate(guild, channel, message);
            }
            None
        }
        AdmitOutcome::Matched(messages) => Some(messages),
    }
}

fn may_admit(
    rule: &crate::types::RuleDef,
    author: u64,
    channel: u64,
    roles: Option<&[u64]>,
) -> bool {
    rule.enabled
        && !rule.whitelist_users.contains(&author)
        && !rule.whitelist_channels.contains(&channel)
        && match roles {
            Some(roles) => !rule.whitelist_roles.iter().any(|r| roles.contains(r)),
            None => rule.whitelist_roles.is_empty(),
        }
}

pub fn observe(
    app: &Arc<BotState>,
    ctx: &Context,
    cfg: &CachedGuildConfig,
    msg: &Message,
    received: Instant,
    received_at: chrono::DateTime<chrono::Utc>,
    revision: u64,
) {
    let Some(guild) = msg.guild_id else { return };
    if msg.author.id == ctx.cache.current_user().id || msg.webhook_id.is_some() {
        return;
    }
    if msg.edited_timestamp.is_some() {
        return;
    }
    let Some(member) = &msg.member else { return };
    let roles: Vec<_> = member.roles.iter().map(|r| r.get()).collect();
    consider(
        app,
        ctx,
        cfg,
        guild.get(),
        msg.author.id.get(),
        msg.channel_id.get(),
        msg.id.get(),
        Some(&roles),
        Some(msg.content.as_str()),
        msg.timestamp.timestamp_millis(),
        received,
        received_at,
        revision,
        false,
    );
}

pub fn observe_update(
    app: &Arc<BotState>,
    ctx: &Context,
    cfg: &CachedGuildConfig,
    event: &MessageUpdateEvent,
    received: Instant,
    received_at: chrono::DateTime<chrono::Utc>,
    revision: u64,
) {
    let Some(guild) = event.guild_id else { return };
    if matches!(event.webhook_id, Some(Some(_))) {
        return;
    }
    if event.content.is_none() {
        return;
    }
    let roles = event
        .member
        .as_ref()
        .and_then(|m| m.as_ref())
        .map(|m| m.roles.iter().map(|r| r.get()).collect::<Vec<_>>());
    let author = event.author.as_ref().map(|u| u.id.get()).or_else(|| {
        app.text_spam
            .author_of(guild.get(), event.channel_id.get(), event.id.get())
    });
    let Some(author) = author else { return };
    if author == ctx.cache.current_user().id.get() {
        return;
    }
    let edited_ms = event
        .edited_timestamp
        .map(|t| t.timestamp_millis())
        .unwrap_or_else(|| received_at.timestamp_millis());
    consider(
        app,
        ctx,
        cfg,
        guild.get(),
        author,
        event.channel_id.get(),
        event.id.get(),
        roles.as_deref(),
        event.content.as_deref(),
        edited_ms,
        received,
        received_at,
        revision,
        true,
    );
}

#[allow(clippy::too_many_arguments)]
fn consider(
    app: &Arc<BotState>,
    ctx: &Context,
    cfg: &CachedGuildConfig,
    guild: u64,
    author: u64,
    channel: u64,
    message: u64,
    roles: Option<&[u64]>,
    content: Option<&str>,
    edited_ms: i64,
    received: Instant,
    received_at: chrono::DateTime<chrono::Utc>,
    revision: u64,
    replace: bool,
) {
    let Some(rule) = cfg.rules.get("repeat_text") else {
        drop_stale_update(&app.text_spam, guild, channel, message, replace);
        return;
    };
    let Ok(policy) = Policy::parse(rule) else {
        drop_stale_update(&app.text_spam, guild, channel, message, replace);
        return;
    };
    if !may_admit(rule, author, channel, roles) {
        drop_stale_update(&app.text_spam, guild, channel, message, replace);
        return;
    }
    let Some(raw) = content else {
        drop_stale_update(&app.text_spam, guild, channel, message, replace);
        return;
    };
    if raw.len() > MAX_TEXT {
        drop_stale_update(&app.text_spam, guild, channel, message, replace);
        return;
    }
    let text = text_spam::normalize(raw);
    if !text_spam::long_enough(&text, policy.min_length) {
        drop_stale_update(&app.text_spam, guild, channel, message, replace);
        return;
    }
    let age_ms = received_at.timestamp_millis().saturating_sub(edited_ms);
    if age_ms < 0 || age_ms as u64 >= policy.window_secs * 1000 {
        drop_stale_update(&app.text_spam, guild, channel, message, replace);
        return;
    }
    let Some(at) = received.checked_sub(Duration::from_millis(age_ms as u64)) else {
        drop_stale_update(&app.text_spam, guild, channel, message, replace);
        return;
    };
    let token = (cfg.version, cfg.durable_version);
    let outcome = app.text_spam.admit(
        guild,
        author,
        Post {
            id: message,
            channel,
            at,
            edited_ms,
            cost: 64 + text.len(),
            text: text.clone(),
            revision,
        },
        token,
        &policy,
        received,
        replace,
    );
    let messages = match apply_admit_outcome(&app.text_spam, outcome, guild, channel, message, replace) {
        Some(messages) => messages,
        None => return,
    };
    if !app
        .text_spam
        .begin_report_window(guild, author, policy.window_secs, received)
    {
        return;
    }
    let app = Arc::clone(app);
    let ctx = ctx.clone();
    let excerpts = {
        let mut excerpts = app.text_spam.excerpts(guild, author, &messages);
        if excerpts.is_empty() {
            excerpts.push(TextEvidence {
                channel_id: channel as i64,
                message_id: message as i64,
                author_id: author as i64,
                occurred_at_ms: edited_ms.max(1),
                text,
            });
        }
        excerpts
    };
    tokio::spawn(async move {
        let delivered = async {
            if app.poll_config_version().await.is_err() {
                return false;
            }
            let Ok(current) = app.configs.get(&app.db, guild).await else {
                return false;
            };
            if (current.version, current.durable_version) != token {
                return false;
            }
            let Some(rule) = current.rules.get("repeat_text") else {
                return false;
            };
            let Ok(policy) = Policy::parse(rule) else {
                return false;
            };
            spam_actions::report(
                &app,
                &ctx,
                spam_actions::Match {
                    kind: Kind::Text,
                    guild,
                    author,
                    channel,
                    enforce: policy.enforce,
                    token,
                    messages,
                    excerpts,
                    reason: format!(
                        "Exact trimmed text across {} channels in {} seconds (minimum {} scalars)",
                        policy.min_channels, policy.window_secs, policy.min_length
                    ),
                },
            )
            .await;
            true
        }
        .await;
        if !delivered {
            app.text_spam
                .abort_report_window(guild, author, Instant::now());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy() -> Policy {
        Policy {
            mode: "exact_text".into(),
            window_secs: 60,
            min_channels: 3,
            min_length: 4,
            enforce: false,
        }
    }
    fn post(id: u64, channel: u64, at: Instant, text: &str) -> Post {
        Post {
            id,
            channel,
            at,
            edited_ms: 0,
            text: text.into(),
            cost: 8,
            revision: 0,
        }
    }
    fn matched(outcome: AdmitOutcome) -> Option<Vec<(u64, u64)>> {
        match outcome {
            AdmitOutcome::Matched(messages) => Some(messages),
            _ => None,
        }
    }

    #[test]
    fn three_channels_match_and_same_channel_does_not() {
        let d = Detector::default();
        let now = Instant::now();
        let p = policy();
        assert_eq!(
            d.admit(1, 2, post(1, 10, now, "spam"), (0, 0), &p, now, false),
            AdmitOutcome::Admitted
        );
        assert_eq!(
            d.admit(1, 2, post(2, 11, now, "spam"), (0, 0), &p, now, false),
            AdmitOutcome::Admitted
        );
        let hit = matched(
            d.admit(1, 2, post(3, 12, now, "spam"), (0, 0), &p, now, false),
        );
        assert_eq!(hit.unwrap().len(), 3);
        let d = Detector::default();
        assert_eq!(
            d.admit(1, 2, post(1, 10, now, "spam"), (0, 0), &p, now, false),
            AdmitOutcome::Admitted
        );
        assert_eq!(
            d.admit(1, 2, post(2, 10, now, "spam"), (0, 0), &p, now, false),
            AdmitOutcome::Admitted
        );
        assert_eq!(
            d.admit(1, 2, post(3, 10, now, "spam"), (0, 0), &p, now, false),
            AdmitOutcome::Admitted
        );
    }
    #[test]
    fn unchanged_edit_does_not_refresh_and_changed_text_replaces() {
        let d = Detector::default();
        let now = Instant::now();
        let p = policy();
        let mut first = post(1, 10, now, "spam");
        first.revision = d.revision();
        assert_eq!(
            d.admit(1, 2, first.clone(), (0, 0), &p, now, false),
            AdmitOutcome::Admitted
        );
        let mut same = first.clone();
        same.edited_ms = 5;
        same.at = now + Duration::from_secs(1);
        assert_eq!(
            d.admit(1, 2, same, (0, 0), &p, now, true),
            AdmitOutcome::Unchanged
        );
        assert_eq!(d.state.lock().guilds[&1].authors[&2].posts[0].at, now);
        assert!(d.state.lock().index.contains_key(&(1, 10, 1)));
        let mut changed = post(1, 10, now + Duration::from_secs(2), "else");
        changed.edited_ms = 9;
        changed.revision = d.revision();
        assert_eq!(
            d.admit(1, 2, changed, (0, 0), &p, now, true),
            AdmitOutcome::Admitted
        );
        assert_eq!(d.state.lock().guilds[&1].authors[&2].posts[0].text, "else");
        assert!(d.state.lock().index.contains_key(&(1, 10, 1)));
    }

    #[test]
    fn apply_admit_outcome_preserves_unchanged_and_rejects_stale() {
        let d = Detector::default();
        let now = Instant::now();
        let p = policy();
        let mut first = post(1, 10, now, "spam");
        first.revision = d.revision();
        assert_eq!(
            d.admit(1, 2, first, (0, 0), &p, now, false),
            AdmitOutcome::Admitted
        );
        assert!(apply_admit_outcome(
            &d,
            AdmitOutcome::Unchanged,
            1,
            10,
            1,
            true
        )
        .is_none());
        assert!(d.state.lock().index.contains_key(&(1, 10, 1)));
        assert!(apply_admit_outcome(&d, AdmitOutcome::Rejected, 1, 10, 1, true).is_none());
        assert!(!d.state.lock().index.contains_key(&(1, 10, 1)));
    }

    #[test]
    fn match_still_proving_rejects_invalidated_or_stale_messages() {
        let d = Detector::default();
        let now = Instant::now();
        let p = policy();
        let token = (1, 2);
        for (id, channel) in [(10, 1), (11, 2)] {
            let mut item = post(id, channel, now, "spam");
            item.revision = d.revision();
            assert_eq!(
                d.admit(1, 2, item, token, &p, now, false),
                AdmitOutcome::Admitted
            );
        }
        let mut third = post(12, 3, now, "spam");
        third.revision = d.revision();
        let matched_messages = matched(d.admit(1, 2, third, token, &p, now, false)).unwrap();
        assert!(d.match_still_proving(1, 2, token, &matched_messages, &p, now));
        d.invalidate(1, 2, 11);
        assert!(!d.match_still_proving(1, 2, token, &matched_messages, &p, now));

        let fresh = Detector::default();
        for (id, channel) in [(10, 1), (11, 2)] {
            let mut item = post(id, channel, now, "spam");
            item.revision = fresh.revision();
            assert_eq!(
                fresh.admit(1, 2, item, token, &p, now, false),
                AdmitOutcome::Admitted
            );
        }
        let mut third = post(12, 3, now, "spam");
        third.revision = fresh.revision();
        let matched_messages = matched(fresh.admit(1, 2, third, token, &p, now, false)).unwrap();
        let mut changed = post(11, 2, now + Duration::from_secs(1), "other");
        changed.edited_ms = 9;
        changed.revision = fresh.revision();
        assert_eq!(
            fresh.admit(1, 2, changed, token, &p, now, true),
            AdmitOutcome::Admitted
        );
        assert!(!fresh.match_still_proving(1, 2, token, &matched_messages, &p, now));
    }

    #[test]
    fn extra_unrelated_text_does_not_block_an_exact_match() {
        let d = Detector::default();
        let now = Instant::now();
        let p = policy();
        assert_eq!(
            d.admit(1, 2, post(1, 10, now, "spam"), (0, 0), &p, now, false),
            AdmitOutcome::Admitted
        );
        assert_eq!(
            d.admit(1, 2, post(2, 11, now, "spam"), (0, 0), &p, now, false),
            AdmitOutcome::Admitted
        );
        assert_eq!(
            d.admit(1, 2, post(3, 12, now, "hello"), (0, 0), &p, now, false),
            AdmitOutcome::Admitted
        );
        let hit = matched(
            d.admit(1, 2, post(4, 13, now, "spam"), (0, 0), &p, now, false),
        );
        assert_eq!(hit.unwrap().len(), 3);
    }

    #[test]
    fn expired_ids_cannot_reenter_after_retirement() {
        let d = Detector::default();
        let now = Instant::now();
        let mut p = policy();
        p.window_secs = 1;
        let early = now - Duration::from_secs(2);
        assert_eq!(
            d.admit(1, 2, post(1, 10, early, "spam"), (0, 0), &p, now, false),
            AdmitOutcome::Admitted
        );
        d.prune();
        assert_eq!(
            d.admit(1, 2, post(1, 10, now, "spam"), (0, 0), &p, now, false),
            AdmitOutcome::Rejected
        );
        assert!(d.state.lock().index.is_empty());
    }

    #[test]
    fn omitted_roles_still_apply_user_and_channel_exemptions() {
        let mut rule = crate::defaults::default_rules().remove(2);
        assert!(may_admit(&rule, 1, 2, None));
        rule.whitelist_users.push(1);
        assert!(!may_admit(&rule, 1, 2, None));
        rule.whitelist_users.clear();
        rule.whitelist_channels.push(2);
        assert!(!may_admit(&rule, 1, 2, None));
        rule.whitelist_channels.clear();
        rule.whitelist_roles.push(3);
        assert!(!may_admit(&rule, 1, 2, None));
        assert!(!may_admit(&rule, 1, 2, Some(&[3])));
        assert!(may_admit(&rule, 1, 2, Some(&[4])));
    }

    #[test]
    fn rejected_update_drops_indexed_post() {
        let d = Detector::default();
        let now = Instant::now();
        let p = policy();
        let mut first = post(1, 10, now, "spam");
        first.revision = d.revision();
        assert_eq!(
            d.admit(1, 2, first, (0, 0), &p, now, false),
            AdmitOutcome::Admitted
        );
        assert!(d.state.lock().index.contains_key(&(1, 10, 1)));
        super::drop_stale_update(&d, 1, 10, 1, true);
        assert!(!d.state.lock().index.contains_key(&(1, 10, 1)));
        super::drop_stale_update(&d, 1, 10, 1, false);
        assert!(!d.state.lock().index.contains_key(&(1, 10, 1)));
    }

    #[test]
    fn match_excerpts_cover_every_matched_post() {
        let d = Detector::default();
        let now = Instant::now();
        let p = policy();
        assert_eq!(
            d.admit(1, 2, post(1, 10, now, "spam"), (0, 0), &p, now, false),
            AdmitOutcome::Admitted
        );
        assert_eq!(
            d.admit(1, 2, post(2, 11, now, "spam"), (0, 0), &p, now, false),
            AdmitOutcome::Admitted
        );
        let hit = matched(
            d.admit(1, 2, post(3, 12, now, "spam"), (0, 0), &p, now, false),
        )
        .unwrap();
        let excerpts = d.excerpts(1, 2, &hit);
        assert_eq!(excerpts.len(), 3);
        assert!(excerpts.iter().all(|e| e.text == "spam"));
        assert_eq!(
            excerpts
                .iter()
                .map(|e| (e.channel_id, e.message_id))
                .collect::<Vec<_>>(),
            vec![(10, 1), (11, 2), (12, 3)]
        );
    }
}
