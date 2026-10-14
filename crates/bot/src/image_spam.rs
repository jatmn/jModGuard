//! Bounded exact-image observations. Punishment uses `spam_actions`, not automod.
use crate::state::{BotState, CachedGuildConfig};
use jmodguard::db::guild_data::TextEvidence;
use jmodguard::image_spam::{Attachment, Hash, Hasher, Limits, Policy};
use parking_lot::Mutex;
use serenity::{
    client::Context,
    model::{channel::Message, event::MessageUpdateEvent, id::GuildId},
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const KEEP: Duration = Duration::from_secs(660);
type Key = (u64, u64, u64); // guild/channel/message

#[derive(Clone)]
enum AdmitOutcome {
    Unchanged,
    Admitted,
    Matched(Job),
    Rejected,
}

#[derive(Clone)]
enum ReplaceOutcome {
    Unchanged,
    Updated(Option<Job>),
}

#[derive(Clone)]
struct Post {
    id: u64,
    channel: u64,
    serial: u64,
    at: Instant,
    files: Vec<Attachment>,
    /// Distinct computed hashes; no image bytes are retained.
    hashes: Option<Vec<Hash>>,
    valid: bool,
    cost: usize,
    revision: u64,
    edited_ms: i64,
    /// Weak, per-dispatch receipt; never keeps enforcement alive in the detector.
    own_delete: Weak<AtomicBool>,
}

/// Bounded proof owned by one enforcement, usable only after a successful delete
/// and its Gateway echo. The live observation is still retired normally.
pub(crate) struct OwnDelete {
    post: Post,
    echoed: Arc<AtomicBool>,
    succeeded: bool,
}

impl OwnDelete {
    pub(crate) fn finish(&mut self, succeeded: bool) {
        self.succeeded = succeeded;
    }
}
#[derive(Debug, PartialEq)]
struct ImageMatch {
    messages: Vec<(u64, u64)>,
    proving: Vec<Hash>,
    known_bad: bool,
}
struct Bucket {
    retired_before: u64,
    window: Duration,
    last_seen: Instant,
    token: (u64, u64),
    posts: Vec<Post>,
    budget: Budget,
    reported_until: Instant,
    /// `reported_until` was reserved because analysis was blocked, not because a hit was logged.
    blocked_notice: bool,
}
/// Replay/rate tombstone after posts are gone; does not occupy live author slots.
struct Watermark {
    retired_before: u64,
    last_seen: Instant,
    token: (u64, u64),
    budget: Budget,
    reported_until: Instant,
}
struct Guild {
    bytes: usize,
    authors: HashMap<u64, Bucket>,
    watermarks: HashMap<u64, Watermark>,
    budget: Budget,
}
struct State {
    revision: u64,
    guilds: HashMap<u64, Guild>,
    index: HashMap<Key, u64>,
    authors: usize,
    serial: u64,
    bytes: usize,
    budget: Budget,
}
#[derive(Clone)]
struct Budget {
    start: Instant,
    requests: u64,
    bytes: u64,
}
impl Budget {
    fn new(now: Instant) -> Self {
        Self {
            start: now,
            requests: 0,
            bytes: 0,
        }
    }
    fn reset(&mut self, now: Instant) {
        if now.duration_since(self.start) >= Duration::from_secs(60) {
            *self = Self::new(now);
        }
    }
    fn fits(
        &mut self,
        now: Instant,
        requests: u64,
        bytes: u64,
        request_limit: u64,
        byte_limit: u64,
    ) -> bool {
        self.reset(now);
        self.requests
            .checked_add(requests)
            .is_some_and(|n| n <= request_limit)
            && self
                .bytes
                .checked_add(bytes)
                .is_some_and(|n| n <= byte_limit)
    }
    fn charge(&mut self, requests: u64, bytes: u64) {
        self.requests += requests;
        self.bytes += bytes;
    }
}
struct Occupancy<'a> {
    guilds: &'a mut HashMap<u64, Guild>,
    index: &'a mut HashMap<Key, u64>,
    bytes: &'a mut usize,
    authors: &'a mut usize,
}
impl State {
    fn occupancy(&mut self) -> Occupancy<'_> {
        let Self {
            guilds,
            index,
            bytes,
            authors,
            ..
        } = self;
        Occupancy {
            guilds,
            index,
            bytes,
            authors,
        }
    }
}
/// Replay tombstone is `retired_before` plus index removal — not a full-cost `Post`.
fn retire_posts(
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
fn retire_author(
    occupancy: &mut Occupancy<'_>,
    guild: u64,
    author: u64,
    max_watermarks: usize,
    drop_post: impl FnMut(&Post) -> bool,
) {
    let Some(g) = occupancy.guilds.get_mut(&guild) else {
        return;
    };
    {
        let Some(b) = g.authors.get_mut(&author) else {
            return;
        };
        retire_posts(
            b,
            guild,
            &mut g.bytes,
            occupancy.index,
            occupancy.bytes,
            drop_post,
        );
    }
    if g.authors
        .get(&author)
        .is_some_and(|bucket| bucket.posts.is_empty())
    {
        park_author(g, occupancy.authors, author, max_watermarks);
    }
}
fn park_author(g: &mut Guild, authors: &mut usize, author: u64, max_watermarks: usize) {
    let Some(b) = g.authors.remove(&author) else {
        return;
    };
    *authors -= 1;
    g.watermarks.insert(
        author,
        Watermark {
            retired_before: b.retired_before,
            last_seen: b.last_seen,
            token: b.token,
            budget: b.budget,
            reported_until: b.reported_until,
        },
    );
    while g.watermarks.len() > max_watermarks {
        let Some(oldest) = g
            .watermarks
            .iter()
            .filter(|(id, _)| **id != author)
            .min_by_key(|(_, w)| w.last_seen)
            .map(|(id, _)| *id)
            .or_else(|| {
                g.watermarks
                    .iter()
                    .min_by_key(|(_, w)| w.last_seen)
                    .map(|(id, _)| *id)
            })
        else {
            break;
        };
        g.watermarks.remove(&oldest);
    }
}
pub struct Detector {
    limits: Limits,
    state: Mutex<State>,
    hasher: Hasher,
    slots: Arc<tokio::sync::Semaphore>,
    dropped: std::sync::atomic::AtomicU64,
}
#[derive(Clone)]
struct Job {
    guild: u64,
    author: u64,
    token: (u64, u64),
    posts: Vec<Post>,
}
#[derive(Debug, PartialEq)]
enum Report {
    Verified(ImageMatch),
    Unverified,
}
#[derive(Clone, Copy, Debug)]
struct ReportWindow {
    token: (u64, u64),
    until: Instant,
}

pub(crate) struct MatchProof<'a> {
    pub token: (u64, u64),
    pub messages: &'a [(u64, u64)],
    pub evidence: &'a [TextEvidence],
    pub policy: &'a Policy,
    pub now: Instant,
}

impl Detector {
    pub fn new(limits: Limits) -> anyhow::Result<Self> {
        limits.validate()?;
        Ok(Self {
            slots: Arc::new(tokio::sync::Semaphore::new(limits.concurrency)),
            limits,
            state: Mutex::new(State {
                revision: 0,
                guilds: HashMap::new(),
                index: HashMap::new(),
                authors: 0,
                serial: 0,
                bytes: 0,
                budget: Budget::new(Instant::now()),
            }),
            hasher: Hasher::default(),
            dropped: std::sync::atomic::AtomicU64::new(0),
        })
    }
    pub(crate) fn hasher(&self) -> &Hasher {
        &self.hasher
    }
    pub(crate) fn download_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.limits.timeout_secs)
    }
    fn capacity_drop(&self) {
        use std::sync::atomic::Ordering;
        let count = self.dropped.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        if count.is_power_of_two() {
            tracing::warn!(
                count,
                "image test coverage limited by admission capacity or unsupported attachments"
            );
        }
    }
    /// Timer-only global scan; event work touches just one bounded author bucket.
    pub fn prune(&self) {
        self.prune_at(Instant::now());
    }
    fn prune_at(&self, now: Instant) {
        let mut state = self.state.lock();
        let max_watermarks = self.limits.max_authors_per_guild;
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
                if let Some(window) = g.authors.get(&author).map(|b| b.window)
                    && let Some(b) = g.authors.get_mut(&author)
                {
                    retire_posts(b, *guild, &mut g.bytes, index, bytes, |p| {
                        now.duration_since(p.at) >= window
                    });
                }
                if g.authors.get(&author).is_some_and(|b| b.posts.is_empty()) {
                    park_author(g, authors, author, max_watermarks);
                } else if g
                    .authors
                    .get(&author)
                    .is_some_and(|b| now.duration_since(b.last_seen) >= KEEP)
                    && let Some(b) = g.authors.remove(&author)
                {
                    *authors -= 1;
                    for p in &b.posts {
                        index.remove(&(*guild, p.channel, p.id));
                        *bytes -= p.cost;
                        g.bytes -= p.cost;
                    }
                }
            }
            g.watermarks
                .retain(|_, w| now.duration_since(w.last_seen) < KEEP);
            !g.authors.is_empty()
                || !g.watermarks.is_empty()
                || now.duration_since(g.budget.start) < Duration::from_secs(60)
        });
    }
    pub fn author_of(&self, guild: u64, channel: u64, message: u64) -> Option<u64> {
        self.state
            .lock()
            .index
            .get(&(guild, channel, message))
            .copied()
    }
    /// Retain only a short tombstone: a duplicate create must not resurrect an edit/delete.
    pub fn revision(&self) -> u64 {
        self.state.lock().revision
    }

    /// True when every matched message is still indexed under this author with the
    /// same config token and the current stored hashes still prove the exact-image match.
    pub(crate) fn match_still_proving(
        &self,
        guild: u64,
        author: u64,
        proof: MatchProof<'_>,
    ) -> bool {
        self.enforcement_still_proving(guild, author, proof, &[])
    }

    pub(crate) fn enforcement_still_proving(
        &self,
        guild: u64,
        author: u64,
        proof: MatchProof<'_>,
        own_deletes: &[OwnDelete],
    ) -> bool {
        let state = self.state.lock();
        let bucket = state
            .guilds
            .get(&guild)
            .and_then(|g| g.authors.get(&author));
        if bucket.is_some_and(|b| b.token != proof.token) {
            return false;
        }
        let posts: Vec<_> = proof
            .messages
            .iter()
            .filter_map(|(channel, message)| {
                if state.index.get(&(guild, *channel, *message)) == Some(&author) {
                    return bucket?
                        .posts
                        .iter()
                        .find(|p| p.channel == *channel && p.id == *message);
                }
                own_deletes
                    .iter()
                    .find(|deleted| {
                        deleted.post.channel == *channel
                            && deleted.post.id == *message
                            && deleted.succeeded
                            && deleted.echoed.load(Ordering::Acquire)
                    })
                    .map(|deleted| &deleted.post)
            })
            .collect();
        if posts.len() != proof.messages.len()
            || posts.iter().any(|p| !live(p, proof.policy, proof.now))
        {
            return false;
        }
        if posts.iter().any(|post| {
            let expected = proof.evidence.iter().find(|item| {
                item.channel_id == post.channel as i64
                    && item.message_id == post.id as i64
                    && item.author_id == author as i64
            });
            !matches!(
                (post.hashes.as_deref(), expected),
                (Some(hashes), Some(item)) if format_hashes(hashes) == item.text
            )
        }) {
            return false;
        }
        let Ok(Some(image_match)) =
            common_images(&posts, proof.policy, self.limits.comparison_steps)
        else {
            return false;
        };
        image_match.messages.len() == proof.messages.len()
            && proof
                .messages
                .iter()
                .all(|message| image_match.messages.contains(message))
    }

    pub(crate) fn begin_delete(
        &self,
        guild: u64,
        author: u64,
        token: (u64, u64),
        channel: u64,
        message: u64,
    ) -> Option<OwnDelete> {
        let mut state = self.state.lock();
        if state.index.get(&(guild, channel, message)) != Some(&author) {
            return None;
        }
        let bucket = state.guilds.get_mut(&guild)?.authors.get_mut(&author)?;
        if bucket.token != token {
            return None;
        }
        let post = bucket
            .posts
            .iter_mut()
            .find(|p| p.channel == channel && p.id == message)?;
        let deleted = OwnDelete {
            post: post.clone(),
            echoed: Arc::new(AtomicBool::new(false)),
            succeeded: false,
        };
        post.own_delete = Arc::downgrade(&deleted.echoed);
        Some(deleted)
    }

    pub fn invalidate(&self, guild: u64, channel: u64, message: u64) {
        self.invalidate_message(guild, channel, message, false);
    }

    pub(crate) fn deleted(&self, guild: u64, channel: u64, message: u64) {
        self.invalidate_message(guild, channel, message, true);
    }

    fn invalidate_message(&self, guild: u64, channel: u64, message: u64, delete_echo: bool) {
        let mut state = self.state.lock();
        state.revision = state.revision.wrapping_add(1);
        let Some(author) = state.index.get(&(guild, channel, message)).copied() else {
            return;
        };
        let mut occupancy = state.occupancy();
        retire_author(
            &mut occupancy,
            guild,
            author,
            self.limits.max_authors_per_guild,
            |p| {
                if p.id != message || p.channel != channel {
                    return false;
                }
                if delete_echo && let Some(echoed) = p.own_delete.upgrade() {
                    echoed.store(true, Ordering::Release);
                }
                true
            },
        );
    }
    pub fn invalidate_author(&self, guild: u64, author: u64) {
        let mut state = self.state.lock();
        state.revision = state.revision.wrapping_add(1);
        let mut occupancy = state.occupancy();
        retire_author(
            &mut occupancy,
            guild,
            author,
            self.limits.max_authors_per_guild,
            |_| true,
        );
    }
    pub fn invalidate_guild(&self, guild: u64) {
        let mut state = self.state.lock();
        state.revision = state.revision.wrapping_add(1);
        let author_ids: Vec<u64> = state
            .guilds
            .get(&guild)
            .map(|g| g.authors.keys().copied().collect())
            .unwrap_or_default();
        let mut occupancy = state.occupancy();
        for author in author_ids {
            retire_author(
                &mut occupancy,
                guild,
                author,
                self.limits.max_authors_per_guild,
                |_| true,
            );
        }
    }
    #[allow(clippy::too_many_arguments)] // Private bounded admission tuple, no external API.
    fn admit(
        &self,
        guild: u64,
        author: u64,
        mut post: Post,
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
        // Expire this author in O(posts), keeping a bounded ID watermark
        // so expired/edited creates cannot return after a settings change.
        {
            let mut occupancy = state.occupancy();
            if let Some(window) = occupancy
                .guilds
                .get(&guild)
                .and_then(|g| g.authors.get(&author))
                .map(|b| b.window)
            {
                retire_author(
                    &mut occupancy,
                    guild,
                    author,
                    self.limits.max_authors_per_guild,
                    |p| now.duration_since(p.at) >= window,
                );
            }
            if occupancy
                .guilds
                .get(&guild)
                .and_then(|g| g.authors.get(&author))
                .is_some_and(|b| b.token != token)
            {
                retire_author(
                    &mut occupancy,
                    guild,
                    author,
                    self.limits.max_authors_per_guild,
                    |_| true,
                );
            }
        }
        let retired_before = state.guilds.get(&guild).and_then(|g| {
            g.authors
                .get(&author)
                .map(|b| b.retired_before)
                .or_else(|| g.watermarks.get(&author).map(|w| w.retired_before))
        });
        if replace
            && let Some(outcome) =
                replace_existing(&mut state, guild, author, &post, token, policy, now)
        {
            return match outcome {
                ReplaceOutcome::Unchanged => AdmitOutcome::Unchanged,
                ReplaceOutcome::Updated(None) => AdmitOutcome::Admitted,
                ReplaceOutcome::Updated(Some(job)) => AdmitOutcome::Matched(job),
            };
        }
        if state.index.contains_key(&key) || retired_before.is_some_and(|id| post.id <= id) {
            return AdmitOutcome::Rejected;
        }
        let exists = state
            .guilds
            .get(&guild)
            .is_some_and(|g| g.authors.contains_key(&author));
        if !exists
            && (state.authors >= self.limits.max_authors
                || state
                    .guilds
                    .get(&guild)
                    .is_some_and(|g| g.authors.len() >= self.limits.max_authors_per_guild))
        {
            self.capacity_drop();
            return AdmitOutcome::Rejected;
        }
        post.cost = 256
            + post
                .files
                .iter()
                .map(Attachment::retained_bytes)
                .sum::<usize>();
        if state.bytes + post.cost > self.limits.metadata_bytes
            || state
                .guilds
                .get(&guild)
                .is_some_and(|g| g.bytes + post.cost > self.limits.metadata_bytes_per_guild)
            || post.cost > self.limits.metadata_bytes_per_guild
        {
            self.capacity_drop();
            return AdmitOutcome::Rejected;
        }
        let cost = post.cost;
        let serial = match state.serial.checked_add(1) {
            Some(serial) => serial,
            None => return AdmitOutcome::Rejected,
        };
        state.serial = serial;
        post.serial = serial;
        let g = state.guilds.entry(guild).or_insert_with(|| Guild {
            bytes: 0,
            authors: HashMap::new(),
            watermarks: HashMap::new(),
            budget: Budget::new(now),
        });
        let watermark = g.watermarks.remove(&author);
        let b = g.authors.entry(author).or_insert_with(|| Bucket {
            retired_before: watermark.as_ref().map(|w| w.retired_before).unwrap_or(0),
            window: Duration::from_secs(policy.window_secs),
            last_seen: now,
            token: watermark.as_ref().map(|w| w.token).unwrap_or(token),
            posts: vec![],
            budget: watermark
                .as_ref()
                .map(|w| w.budget.clone())
                .unwrap_or_else(|| Budget::new(now)),
            reported_until: watermark.as_ref().map(|w| w.reported_until).unwrap_or(now),
            blocked_notice: false,
        });
        b.token = token;
        if b.posts.len() >= self.limits.max_posts_per_author {
            self.capacity_drop();
            return AdmitOutcome::Rejected;
        }
        b.window = Duration::from_secs(policy.window_secs);
        b.last_seen = now;
        b.posts.push(post);
        g.bytes += cost;
        state.bytes += cost;
        if !exists {
            state.authors += 1;
        }
        state.index.insert(key, author);
        let b = &state.guilds[&guild].authors[&author];
        match candidate_job(guild, author, token, b, policy, now) {
            Some(job) => AdmitOutcome::Matched(job),
            None => AdmitOutcome::Admitted,
        }
    }
    fn reserve(&self, job: &Job, policy: &Policy, now: Instant) -> bool {
        let files = job
            .posts
            .iter()
            .filter(|p| p.hashes.is_none())
            .flat_map(|p| &p.files);
        let (requests, bytes) = files.fold((0, 0), |(n, b), f| (n + 1, b + f.size));
        let mut s = self.state.lock();
        if !s.budget.fits(
            now,
            requests,
            bytes,
            self.limits.requests_per_minute,
            self.limits.bytes_per_minute,
        ) {
            return false;
        }
        let Some(g) = s.guilds.get_mut(&job.guild) else {
            return false;
        };
        if !g.budget.fits(
            now,
            requests,
            bytes,
            self.limits.requests_per_guild_minute,
            u64::MAX,
        ) {
            return false;
        }
        let Some(b) = g.authors.get_mut(&job.author) else {
            return false;
        };
        if b.token != job.token
            || job.posts.iter().any(|old| {
                !b.posts
                    .iter()
                    .any(|p| p.serial == old.serial && live(p, policy, now))
            })
            || !b.budget.fits(
                now,
                requests,
                bytes,
                self.limits.requests_per_author_minute,
                u64::MAX,
            )
        {
            return false;
        }
        b.budget.charge(requests, bytes);
        g.budget.charge(requests, bytes);
        s.budget.charge(requests, bytes);
        true
    }
    fn complete(&self, job: Job, policy: &Policy, failed: bool, now: Instant) -> Option<Report> {
        let mut s = self.state.lock();
        let b = s.guilds.get_mut(&job.guild)?.authors.get_mut(&job.author)?;
        if b.token != job.token {
            return None;
        }
        // A verified hit owns the window. A blocked-analysis notice does not, so an
        // in-flight job that later proves the same posts can still report the match.
        if now < b.reported_until && !b.blocked_notice {
            return None;
        }
        // Every participating observation must still be the same live post.
        if job.posts.iter().any(|old| {
            !b.posts
                .iter()
                .any(|p| p.serial == old.serial && live(p, policy, now))
        }) {
            return None;
        }
        for old in job.posts {
            if let Some(p) = b.posts.iter_mut().find(|p| p.serial == old.serial) {
                p.hashes = old.hashes;
            }
        }
        let posts: Vec<_> = b.posts.iter().filter(|p| live(p, policy, now)).collect();
        let compared = common_images(&posts, policy, self.limits.comparison_steps);
        // Hashes that already prove the common set win over a download or budget failure
        // on some other file. The failure notice is used only when no match was proven.
        let report = match compared {
            Ok(Some(matched)) => Report::Verified(matched),
            Ok(None) if !failed => return None,
            Ok(None) | Err(()) => Report::Unverified,
        };
        match &report {
            Report::Verified(_) => {
                b.blocked_notice = false;
                b.reported_until = now + Duration::from_secs(policy.window_secs);
            }
            Report::Unverified => {
                if b.blocked_notice && now < b.reported_until {
                    return None;
                }
                b.blocked_notice = true;
                b.reported_until = now + Duration::from_secs(policy.window_secs);
            }
        }
        Some(report)
    }
    fn abort_report_window(
        &self,
        guild: u64,
        author: u64,
        reservation: ReportWindow,
        now: Instant,
    ) {
        let mut state = self.state.lock();
        if let Some(bucket) = state
            .guilds
            .get_mut(&guild)
            .and_then(|guild| guild.authors.get_mut(&author))
            && bucket.token == reservation.token
            && bucket.reported_until == reservation.until
        {
            bucket.reported_until = now;
        }
    }
}

fn finish_verified_report_window(
    detector: &Detector,
    guild: u64,
    author: u64,
    reservation: ReportWindow,
    disposition: crate::spam_actions::ReportDisposition,
    now: Instant,
) {
    if disposition == crate::spam_actions::ReportDisposition::Retryable {
        detector.abort_report_window(guild, author, reservation, now);
    }
}

fn file_key(post: &Post) -> Vec<(u64, u64)> {
    let mut keys: Vec<_> = post.files.iter().map(|f| (f.id, f.size)).collect();
    keys.sort_unstable();
    keys
}

fn candidate_job(
    guild: u64,
    author: u64,
    token: (u64, u64),
    bucket: &Bucket,
    policy: &Policy,
    now: Instant,
) -> Option<Job> {
    if now < bucket.reported_until {
        return None;
    }
    let posts: Vec<_> = bucket
        .posts
        .iter()
        .filter(|p| live(p, policy, now))
        .cloned()
        .collect();
    if policy.known_bad_hashes.is_empty()
        && (posts
            .iter()
            .map(|p| p.channel)
            .collect::<BTreeSet<_>>()
            .len()
            < policy.min_channels
            || !size_candidate(&posts, policy.min_attachments))
    {
        return None;
    }
    Some(Job {
        guild,
        author,
        token,
        posts,
    })
}

fn replace_existing(
    state: &mut State,
    guild: u64,
    author: u64,
    post: &Post,
    token: (u64, u64),
    policy: &Policy,
    now: Instant,
) -> Option<ReplaceOutcome> {
    let key = (guild, post.channel, post.id);
    if state.index.get(&key).copied() != Some(author) {
        return None;
    }
    let old = state
        .guilds
        .get(&guild)?
        .authors
        .get(&author)?
        .posts
        .iter()
        .find(|p| p.id == post.id && p.channel == post.channel)?;
    if file_key(old) == file_key(post) || post.edited_ms < old.edited_ms {
        return Some(ReplaceOutcome::Unchanged);
    }
    let serial = state.serial.checked_add(1)?;
    state.serial = serial;
    let g = state.guilds.get_mut(&guild)?;
    let b = g.authors.get_mut(&author)?;
    let old = b
        .posts
        .iter_mut()
        .find(|p| p.id == post.id && p.channel == post.channel)?;
    g.bytes = g.bytes.saturating_sub(old.cost);
    state.bytes = state.bytes.saturating_sub(old.cost);
    old.serial = serial;
    old.files = post.files.clone();
    old.hashes = None;
    old.at = post.at;
    old.edited_ms = post.edited_ms;
    old.revision = post.revision;
    old.valid = true;
    old.own_delete = Weak::new();
    old.cost = 256
        + post
            .files
            .iter()
            .map(Attachment::retained_bytes)
            .sum::<usize>();
    g.bytes += old.cost;
    state.bytes += old.cost;
    b.token = token;
    b.last_seen = now;
    Some(ReplaceOutcome::Updated(candidate_job(
        guild, author, token, b, policy, now,
    )))
}

fn apply_admit_outcome(
    detector: &Detector,
    outcome: AdmitOutcome,
    guild: u64,
    channel: u64,
    message: u64,
    replace: bool,
) -> Option<Job> {
    match outcome {
        AdmitOutcome::Unchanged | AdmitOutcome::Admitted => None,
        AdmitOutcome::Rejected => {
            if replace {
                detector.invalidate(guild, channel, message);
            }
            None
        }
        AdmitOutcome::Matched(job) => Some(job),
    }
}

fn spawn_image_job(
    app: &Arc<BotState>,
    ctx: &Context,
    detector: &Detector,
    job: Job,
    policy: Policy,
    channel: u64,
) {
    let Ok(permit) = Arc::clone(&detector.slots).try_acquire_owned() else {
        detector.capacity_drop();
        return;
    };
    let app = Arc::clone(app);
    let ctx = ctx.clone();
    tokio::spawn(async move {
        run_job(&app, &ctx, job, policy, channel, permit).await;
    });
}

fn live(p: &Post, policy: &Policy, now: Instant) -> bool {
    p.valid && now.duration_since(p.at) < Duration::from_secs(policy.window_secs)
}
fn size_candidate(posts: &[Post], minimum: usize) -> bool {
    posts.iter().enumerate().any(|(i, a)| {
        posts[i + 1..].iter().any(|b| {
            if a.channel == b.channel {
                return false;
            }
            let mut sizes: Vec<_> = b.files.iter().map(|f| f.size).collect();
            a.files
                .iter()
                .filter(|f| {
                    if let Some(i) = sizes.iter().position(|s| *s == f.size) {
                        sizes.swap_remove(i);
                        true
                    } else {
                        false
                    }
                })
                .count()
                >= minimum
        })
    })
}
/// Enumerate at most 1023 subsets per post (10 files). One common subset must
/// occur in the required number of DISTINCT channels, not a pairwise overlap chain.
fn common_images(
    posts: &[&Post],
    policy: &Policy,
    mut work: usize,
) -> Result<Option<ImageMatch>, ()> {
    // A managed digest recognizes a previously confirmed image on its first post.
    // Missing hashes are never proof. Membership/exemptions/replay guards remain shared.
    if !policy.known_bad_hashes.is_empty() {
        for anchor in posts {
            let Some(hashes) = &anchor.hashes else {
                continue;
            };
            for hash in hashes {
                work = work.checked_sub(1).ok_or(())?;
                if policy.known_bad_hashes.contains(hash) {
                    let messages = posts
                        .iter()
                        .filter(|p| p.hashes.as_ref().is_some_and(|hs| hs.contains(hash)))
                        .map(|p| (p.channel, p.id))
                        .collect();
                    return Ok(Some(ImageMatch {
                        messages,
                        proving: vec![*hash],
                        known_bad: true,
                    }));
                }
            }
        }
    }
    for anchor in posts {
        let Some(hashes) = &anchor.hashes else {
            continue;
        };
        for mask in 1usize..(1 << hashes.len()) {
            if mask.count_ones() as usize != policy.min_attachments {
                continue;
            }
            let subset: Vec<_> = hashes
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, h)| *h)
                .collect();
            let mut matched = Vec::new();
            for p in posts {
                let Some(hs) = &p.hashes else { continue };
                let cost = subset.len() * hs.len() + 1;
                work = work.checked_sub(cost).ok_or(())?;
                if subset.iter().all(|h| hs.contains(h)) {
                    matched.push((p.channel, p.id));
                }
            }
            if matched
                .iter()
                .map(|(c, _)| c)
                .collect::<BTreeSet<_>>()
                .len()
                >= policy.min_channels
            {
                return Ok(Some(ImageMatch {
                    messages: matched,
                    proving: subset,
                    known_bad: false,
                }));
            }
        }
    }
    Ok(None)
}

fn eligible(
    rule: &crate::types::RuleDef,
    author: u64,
    channel: u64,
    roles: &[u64],
    is_moderator: bool,
) -> bool {
    may_admit(rule, author, channel, Some(roles), is_moderator)
}

fn may_admit(
    rule: &crate::types::RuleDef,
    author: u64,
    channel: u64,
    roles: Option<&[u64]>,
    is_moderator: bool,
) -> bool {
    jmodguard::custom_rules::spam_scope_admits(rule, author, channel, roles, is_moderator)
}

/// Discord may omit image dimensions or the media type. An explicit image media
/// type is sufficient; without one, require dimensions and an image filename.
fn is_image_attachment(
    has_width: bool,
    has_height: bool,
    content_type: Option<&str>,
    filename: &str,
) -> bool {
    match content_type {
        Some(media_type) => media_type.starts_with("image/"),
        None => {
            has_width
                && has_height
                && filename.rsplit_once('.').is_some_and(|(_, extension)| {
                    ["jpg", "jpeg", "png", "gif", "webp", "avif"]
                        .iter()
                        .any(|image| extension.eq_ignore_ascii_case(image))
                })
        }
    }
}

/// Called before the generic automod bot-author exemption. Unknown membership and
/// webhooks are skipped; no privileged role/bot exemption is inferred.
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
    let Some(rule) = cfg.rules.get("attachment_flood") else {
        return;
    };
    let Some(policy) = policy_for_config(cfg, rule) else {
        return;
    };
    let Some(member) = &msg.member else { return };
    let roles: Vec<_> = member.roles.iter().map(|r| r.get()).collect();
    let is_moderator =
        crate::commands::cached_is_moderator(app, ctx, guild, msg.author.id.get(), &roles);
    if !eligible(
        rule,
        msg.author.id.get(),
        msg.channel_id.get(),
        &roles,
        is_moderator,
    ) {
        return;
    }
    // A replay cannot acquire a new window. Use the Discord create time, clamped
    // to this callback's monotonic reception time; old/future timestamps are skipped.
    // Age is measured at handler entry so async config load cannot shrink coverage.
    let age_ms = received_at.timestamp_millis() - msg.timestamp.timestamp_millis();
    if age_ms < 0 || age_ms as u64 >= policy.window_secs * 1000 || msg.edited_timestamp.is_some() {
        return;
    }
    let Some(at) = received.checked_sub(Duration::from_millis(age_ms as u64)) else {
        return;
    };
    let images: Vec<_> = msg
        .attachments
        .iter()
        .filter(|a| {
            is_image_attachment(
                a.width.is_some(),
                a.height.is_some(),
                a.content_type.as_deref(),
                &a.filename,
            )
        })
        .collect();
    if images.len() < minimum_files(&policy) {
        return;
    }
    let detector = &app.image_spam;
    if images.len() > detector.limits.max_files_per_post {
        detector.capacity_drop();
        return;
    }
    let mut files = vec![];
    for a in images {
        let Ok(file) = Attachment::new(
            msg.channel_id.get(),
            a.id.get(),
            a.size as u64,
            &a.url,
            detector.limits.max_file_bytes,
        ) else {
            detector.capacity_drop();
            return;
        };
        if !files.iter().any(|f: &Attachment| f.id == file.id) {
            files.push(file);
        }
    }
    let token = (cfg.version, cfg.durable_version);
    let outcome = detector.admit(
        guild.get(),
        msg.author.id.get(),
        Post {
            id: msg.id.get(),
            channel: msg.channel_id.get(),
            serial: 0,
            at,
            files,
            hashes: None,
            valid: true,
            own_delete: Weak::new(),
            cost: 0,
            revision,
            edited_ms: msg.timestamp.timestamp_millis(),
        },
        token,
        &policy,
        received,
        false,
    );
    if let Some(job) = apply_admit_outcome(
        detector,
        outcome,
        guild.get(),
        msg.channel_id.get(),
        msg.id.get(),
        false,
    ) {
        spawn_image_job(app, ctx, detector, job, policy, msg.channel_id.get());
    }
}

pub(crate) fn observe_update(
    app: &Arc<BotState>,
    ctx: &Context,
    cfg: &CachedGuildConfig,
    event: &MessageUpdateEvent,
    update: &crate::handlers::UpdateObservation,
    revision: u64,
) {
    let Some(guild) = event.guild_id else { return };
    if matches!(event.webhook_id, Some(Some(_))) {
        return;
    }
    let Some(attachments) = event.attachments.as_ref() else {
        return;
    };
    let channel = event.channel_id.get();
    let message = event.id.get();
    let guild_id = guild.get();
    let Some(rule) = cfg.rules.get("attachment_flood") else {
        app.image_spam.invalidate(guild_id, channel, message);
        return;
    };
    let Some(policy) = policy_for_config(cfg, rule) else {
        app.image_spam.invalidate(guild_id, channel, message);
        return;
    };
    let Some(author) = update.author else { return };
    if author == ctx.cache.current_user().id.get() {
        return;
    }
    let roles = event
        .member
        .as_ref()
        .and_then(|m| m.as_ref())
        .map(|m| m.roles.iter().map(|r| r.get()).collect::<Vec<_>>());
    let is_moderator = roles
        .as_deref()
        .is_some_and(|roles| crate::commands::cached_is_moderator(app, ctx, guild, author, roles));
    if !may_admit(rule, author, channel, roles.as_deref(), is_moderator) {
        app.image_spam.invalidate(guild_id, channel, message);
        return;
    }
    let images: Vec<_> = attachments
        .iter()
        .filter(|a| {
            is_image_attachment(
                a.width.is_some(),
                a.height.is_some(),
                a.content_type.as_deref(),
                &a.filename,
            )
        })
        .collect();
    if images.len() < minimum_files(&policy) {
        app.image_spam.invalidate(guild_id, channel, message);
        return;
    }
    let detector = &app.image_spam;
    if images.len() > detector.limits.max_files_per_post {
        detector.invalidate(guild_id, channel, message);
        detector.capacity_drop();
        return;
    }
    let mut files = vec![];
    for a in images {
        let Ok(file) = Attachment::new(
            event.channel_id.get(),
            a.id.get(),
            a.size as u64,
            &a.url,
            detector.limits.max_file_bytes,
        ) else {
            detector.invalidate(guild_id, channel, message);
            detector.capacity_drop();
            return;
        };
        if !files.iter().any(|f: &Attachment| f.id == file.id) {
            files.push(file);
        }
    }
    let edited_ms = event
        .edited_timestamp
        .map(|t| t.timestamp_millis())
        .unwrap_or_else(|| update.received_at.timestamp_millis());
    let age_ms = update
        .received_at
        .timestamp_millis()
        .saturating_sub(edited_ms);
    if age_ms < 0 || age_ms as u64 >= policy.window_secs * 1000 {
        detector.invalidate(guild_id, channel, message);
        return;
    }
    let Some(at) = update
        .received
        .checked_sub(Duration::from_millis(age_ms as u64))
    else {
        detector.invalidate(guild_id, channel, message);
        return;
    };
    let token = (cfg.version, cfg.durable_version);
    let outcome = detector.admit(
        guild.get(),
        author,
        Post {
            id: event.id.get(),
            channel: event.channel_id.get(),
            serial: 0,
            at,
            files,
            hashes: None,
            valid: true,
            own_delete: Weak::new(),
            cost: 0,
            revision,
            edited_ms,
        },
        token,
        &policy,
        update.received,
        true,
    );
    if let Some(job) = apply_admit_outcome(detector, outcome, guild_id, channel, message, true) {
        spawn_image_job(app, ctx, detector, job, policy, event.channel_id.get());
    }
}

async fn run_job(
    app: &Arc<BotState>,
    ctx: &Context,
    mut job: Job,
    policy: Policy,
    channel: u64,
    permit: tokio::sync::OwnedSemaphorePermit,
) {
    let detector = &app.image_spam;
    let (guild, author, token) = (job.guild, job.author, job.token);
    let mut failed = !detector.reserve(&job, &policy, Instant::now());
    if !failed {
        // One whole job deadline, not a fresh unbounded deadline per attachment.
        let outcome =
            tokio::time::timeout(Duration::from_secs(detector.limits.timeout_secs), async {
                for post in &mut job.posts {
                    if post.hashes.is_some() {
                        continue;
                    }
                    let mut unique = BTreeSet::new();
                    for file in &post.files {
                        let hash = detector
                            .hasher
                            .hash(file, Duration::from_secs(detector.limits.timeout_secs))
                            .await?;
                        unique.insert(hash);
                    }
                    post.hashes = Some(unique.into_iter().collect());
                }
                Ok::<_, anyhow::Error>(())
            })
            .await;
        failed = !matches!(outcome, Ok(Ok(())));
    }
    // Reconcile configuration before report/enforce. Hashing above is already bounded;
    // delete/ban below rely on per-operation Discord timeouts in spam_actions.
    if app.poll_config_version().await.is_err() {
        return;
    }
    let Ok(current) = app.configs.get(&app.db, guild).await else {
        return;
    };
    if (current.version, current.durable_version) != token {
        return;
    }
    let Some(rule) = current.rules.get("attachment_flood") else {
        return;
    };
    let Some(current_policy) = policy_for_config(&current, rule) else {
        return;
    };
    let excerpts: Vec<_> = job
        .posts
        .iter()
        .map(|p| {
            (
                p.channel,
                p.id,
                p.edited_ms,
                p.hashes.as_deref().map(format_hashes).unwrap_or_default(),
            )
        })
        .collect();
    let completed_at = Instant::now();
    let Some(report) = detector.complete(job, &policy, failed, completed_at) else {
        return;
    };
    let report_window = ReportWindow {
        token,
        until: completed_at + Duration::from_secs(policy.window_secs),
    };
    // The detector slot bounds hash/download and match verification only.
    // Discord logging and enforcement have their own concurrency/deadline controls.
    drop(permit);
    match report {
        Report::Unverified => {
            let vars = report_vars(Report::Unverified, author, channel, &policy);
            crate::logging::emit(app, ctx, GuildId::new(guild), "automod", vars).await;
        }
        Report::Verified(image_match) => {
            let messages = &image_match.messages;
            let excerpts = excerpts
                .into_iter()
                .filter(|(c, m, _, _)| messages.iter().any(|found| found == &(*c, *m)))
                .map(|(channel_id, message_id, occurred_at_ms, text)| {
                    jmodguard::db::guild_data::TextEvidence {
                        channel_id: channel_id as i64,
                        message_id: message_id as i64,
                        author_id: author as i64,
                        occurred_at_ms: occurred_at_ms.max(1),
                        text,
                    }
                })
                .collect();
            let disposition = crate::spam_actions::report(
                app,
                ctx,
                crate::spam_actions::Match {
                    kind: crate::spam_actions::Kind::Image,
                    guild,
                    author,
                    channel,
                    policy: crate::spam_actions::MatchPolicy::Image(current_policy),
                    token,
                    messages: messages.clone(),
                    excerpts,
                    proving_hashes: image_match.proving.clone(),
                    attachments: Vec::new(), // Exact-image evidence is hash-only; never capture bytes.
                    reason: match_reason(&image_match, &policy),
                },
            )
            .await;
            finish_verified_report_window(
                detector,
                guild,
                author,
                report_window,
                disposition,
                Instant::now(),
            );
        }
    }
}
fn format_hashes(hashes: &[Hash]) -> String {
    hashes
        .iter()
        .map(|h| h.iter().map(|b| format!("{b:02x}")).collect::<String>())
        .collect::<Vec<_>>()
        .join(",")
}
fn match_reason(found: &ImageMatch, policy: &Policy) -> String {
    if found.known_bad {
        "Verified image bytes match an operator-managed known-bad SHA-256 digest".into()
    } else {
        verified_reason(policy)
    }
}

pub(crate) fn policy_for_config(
    cfg: &CachedGuildConfig,
    rule: &crate::types::RuleDef,
) -> Option<Policy> {
    let mut policy = Policy::parse(rule).ok()?;
    policy.known_bad_hashes = cfg.known_bad_hashes.clone();
    Some(policy)
}

fn minimum_files(policy: &Policy) -> usize {
    if policy.known_bad_hashes.is_empty() {
        policy.min_attachments
    } else {
        1
    }
}

fn verified_reason(policy: &Policy) -> String {
    format!(
        "Verified identical image bytes: at least {} common distinct images across {} channels in {}",
        policy.min_attachments,
        policy.min_channels,
        crate::templates::format_duration(policy.window_secs)
    )
}
/// Safe automod evidence: message locations and proving hashes. Never image bytes or URLs.
pub(crate) fn image_notice_evidence(messages: &[(u64, u64)], proving: &[Hash]) -> String {
    let locations = messages
        .iter()
        .map(|(channel, message)| format!("channel {channel}, message {message}"))
        .collect::<Vec<_>>()
        .join("; ");
    let mut hashes = proving.to_vec();
    hashes.sort_unstable();
    hashes.dedup();
    let hashes = format_hashes(&hashes);
    if hashes.is_empty() {
        locations
    } else if locations.is_empty() {
        format!("sha256 {hashes}")
    } else {
        format!("sha256 {hashes}; {locations}")
    }
}
fn report_vars(
    report: Report,
    author: u64,
    channel: u64,
    policy: &Policy,
) -> BTreeMap<String, String> {
    let (rule, action, reason, content) = match report {
        Report::Verified(image_match) => (
            "Exact image spam test".to_string(),
            "log only; no punishment".to_string(),
            match_reason(&image_match, policy),
            image_notice_evidence(&image_match.messages, &image_match.proving),
        ),
        Report::Unverified => (
            "Exact image spam".to_string(),
            "not a spam hit".to_string(),
            "Download or work budget unavailable; image analysis did not finish. No spam match was established and no spam action was taken.".to_string(),
            String::new(),
        ),
    };
    BTreeMap::from([
        ("rule".into(), rule),
        ("mention".into(), format!("<@{author}>")),
        ("user_id".into(), author.to_string()),
        ("source_channel_id".into(), channel.to_string()),
        ("channel".into(), format!("<#{}>", channel)),
        ("action".into(), action),
        ("reason".into(), reason),
        ("content".into(), content),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn images_without_media_type_are_admitted_by_dimensions_and_filename() {
        assert!(is_image_attachment(true, true, None, "image.jpg"));
        assert!(is_image_attachment(true, true, None, "IMAGE.PNG"));
        assert!(is_image_attachment(
            true,
            true,
            Some("image/jpeg"),
            "upload.bin"
        ));
        assert!(is_image_attachment(
            false,
            false,
            Some("image/jpeg"),
            "upload.bin"
        ));
        assert!(!is_image_attachment(
            true,
            true,
            Some("video/mp4"),
            "image.jpg"
        ));
        assert!(!is_image_attachment(true, true, None, "movie.mp4"));
        assert!(!is_image_attachment(false, true, None, "image.jpg"));
    }

    fn policy() -> Policy {
        Policy {
            mode: "exact_image_test".into(),
            window_secs: 60,
            min_channels: 3,
            min_attachments: 2,
            enforce: false,
            known_bad_hashes: Default::default(),
        }
    }
    fn post(id: u64, channel: u64, at: Instant, values: &[u8]) -> Post {
        let hashes: BTreeSet<Hash> = values.iter().map(|v| [*v; 32]).collect();
        Post {
            id,
            channel,
            serial: 0,
            at,
            valid: true,
            own_delete: Weak::new(),
            cost: 0,
            revision: 0,
            files: values
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    Attachment::new(
                        channel,
                        i as u64 + 1,
                        3,
                        &format!(
                            "https://cdn.discordapp.com/attachments/{channel}/{}/a.png",
                            i + 1
                        ),
                        10,
                    )
                    .unwrap()
                })
                .collect(),
            hashes: Some(hashes.into_iter().collect()),
            edited_ms: 0,
        }
    }
    fn admit(d: &Detector, id: u64, channel: u64, at: Instant) -> Option<Job> {
        let mut p = post(id, channel, at, &[1, 2]);
        p.revision = d.revision();
        match d.admit(1, 2, p, (0, 0), &policy(), at, false) {
            AdmitOutcome::Matched(job) => Some(job),
            _ => None,
        }
    }
    fn job(d: &Detector, at: Instant) -> Job {
        assert!(admit(d, 10, 100, at).is_none());
        assert!(admit(d, 11, 101, at).is_none());
        admit(d, 12, 102, at).unwrap()
    }
    fn evidence(messages: &[(u64, u64)], author: u64, values: &[u8]) -> Vec<TextEvidence> {
        let hashes: BTreeSet<Hash> = values.iter().map(|value| [*value; 32]).collect();
        let text = format_hashes(&hashes.into_iter().collect::<Vec<_>>());
        messages
            .iter()
            .map(|(channel, message)| TextEvidence {
                channel_id: *channel as i64,
                message_id: *message as i64,
                author_id: author as i64,
                occurred_at_ms: 1,
                text: text.clone(),
            })
            .collect()
    }
    #[test]
    fn invalidation_during_config_load_prevents_late_admission() {
        let now = Instant::now();
        let d = Detector::new(Limits::default()).unwrap();
        let mut p = post(1, 1, now, &[1, 2]);
        p.revision = d.revision();
        d.invalidate(1, 1, 1); // The create callback has not reached admission yet.
        assert!(matches!(
            d.admit(1, 2, p, (0, 0), &policy(), now, false),
            AdmitOutcome::Rejected
        ));
        assert!(d.state.lock().index.is_empty());
    }

    #[test]
    fn unchanged_files_do_not_refresh_and_changed_files_replace() {
        let now = Instant::now();
        let d = Detector::new(Limits::default()).unwrap();
        let mut first = post(1, 10, now, &[1, 2]);
        first.revision = d.revision();
        first.edited_ms = 1;
        assert!(matches!(
            d.admit(1, 2, first.clone(), (0, 0), &policy(), now, false),
            AdmitOutcome::Admitted
        ));
        let mut same = first.clone();
        same.edited_ms = 5;
        same.at = now + Duration::from_secs(1);
        same.revision = d.revision();
        assert!(matches!(
            d.admit(1, 2, same, (0, 0), &policy(), now, true),
            AdmitOutcome::Unchanged
        ));
        assert_eq!(d.state.lock().guilds[&1].authors[&2].posts[0].at, now);
        assert!(d.state.lock().index.contains_key(&(1, 10, 1)));
        let mut changed = first;
        changed.at = now + Duration::from_secs(2);
        changed.edited_ms = 9;
        changed.revision = d.revision();
        changed.hashes = None;
        changed.files = vec![
            Attachment::new(
                10,
                50,
                3,
                "https://cdn.discordapp.com/attachments/10/50/a.png",
                10,
            )
            .unwrap(),
            Attachment::new(
                10,
                51,
                3,
                "https://cdn.discordapp.com/attachments/10/51/a.png",
                10,
            )
            .unwrap(),
        ];
        assert!(matches!(
            d.admit(1, 2, changed, (0, 0), &policy(), now, true),
            AdmitOutcome::Admitted
        ));
        let stored = &d.state.lock().guilds[&1].authors[&2].posts[0];
        assert_eq!(file_key(stored), vec![(50, 3), (51, 3)]);
        assert!(stored.hashes.is_none());
        assert_eq!(stored.at, now + Duration::from_secs(2));
    }

    #[test]
    fn apply_admit_outcome_preserves_unchanged_and_rejects_stale() {
        let d = Detector::new(Limits::default()).unwrap();
        let now = Instant::now();
        let mut first = post(1, 10, now, &[1, 2]);
        first.revision = d.revision();
        assert!(matches!(
            d.admit(1, 2, first, (0, 0), &policy(), now, false),
            AdmitOutcome::Admitted
        ));
        assert!(apply_admit_outcome(&d, AdmitOutcome::Unchanged, 1, 10, 1, true).is_none());
        assert!(d.state.lock().index.contains_key(&(1, 10, 1)));
        assert!(apply_admit_outcome(&d, AdmitOutcome::Rejected, 1, 10, 1, true).is_none());
        assert!(!d.state.lock().index.contains_key(&(1, 10, 1)));
    }

    fn complete_verified_window(d: &Detector, now: Instant) -> ReportWindow {
        let policy = policy();
        assert!(matches!(
            d.complete(job(d, now), &policy, false, now),
            Some(Report::Verified(_))
        ));
        ReportWindow {
            token: (0, 0),
            until: now + Duration::from_secs(policy.window_secs),
        }
    }

    #[test]
    fn only_retryable_verified_reports_release_the_exact_image_window() {
        let now = Instant::now();

        let consumed = Detector::new(Limits::default()).unwrap();
        let consumed_window = complete_verified_window(&consumed, now);
        finish_verified_report_window(
            &consumed,
            1,
            2,
            consumed_window,
            crate::spam_actions::ReportDisposition::Consumed,
            now + Duration::from_secs(1),
        );
        assert_eq!(
            consumed.state.lock().guilds[&1].authors[&2].reported_until,
            consumed_window.until
        );

        let retryable = Detector::new(Limits::default()).unwrap();
        let retryable_window = complete_verified_window(&retryable, now);
        finish_verified_report_window(
            &retryable,
            1,
            2,
            retryable_window,
            crate::spam_actions::ReportDisposition::Retryable,
            now + Duration::from_secs(1),
        );
        assert_eq!(
            retryable.state.lock().guilds[&1].authors[&2].reported_until,
            now + Duration::from_secs(1)
        );

        let stale = Detector::new(Limits::default()).unwrap();
        let stale_window = complete_verified_window(&stale, now);
        let newer_until = now + Duration::from_secs(120);
        stale
            .state
            .lock()
            .guilds
            .get_mut(&1)
            .unwrap()
            .authors
            .get_mut(&2)
            .unwrap()
            .reported_until = newer_until;
        finish_verified_report_window(
            &stale,
            1,
            2,
            stale_window,
            crate::spam_actions::ReportDisposition::Retryable,
            now + Duration::from_secs(1),
        );
        assert_eq!(
            stale.state.lock().guilds[&1].authors[&2].reported_until,
            newer_until
        );
    }

    #[test]
    fn match_still_proving_rejects_invalidated_messages() {
        let d = Detector::new(Limits::default()).unwrap();
        let now = Instant::now();
        let p = policy();
        let token = (0, 0);
        let job = job(&d, now);
        let matched = d
            .complete(job, &p, false, now)
            .and_then(|report| match report {
                Report::Verified(image_match) => Some(image_match.messages),
                _ => None,
            })
            .unwrap();
        let evidence = evidence(&matched, 2, &[1, 2]);
        assert!(d.match_still_proving(
            1,
            2,
            MatchProof {
                token,
                messages: &matched,
                evidence: &evidence,
                policy: &p,
                now,
            }
        ));
        d.invalidate(1, 101, 11);
        assert!(!d.match_still_proving(
            1,
            2,
            MatchProof {
                token,
                messages: &matched,
                evidence: &evidence,
                policy: &p,
                now,
            }
        ));
    }

    #[test]
    fn match_still_proving_rejects_replaced_attachments() {
        let d = Detector::new(Limits::default()).unwrap();
        let now = Instant::now();
        let p = policy();
        let token = (0, 0);
        let job = job(&d, now);
        let matched = d
            .complete(job, &p, false, now)
            .and_then(|report| match report {
                Report::Verified(image_match) => Some(image_match.messages),
                _ => None,
            })
            .unwrap();
        let evidence = evidence(&matched, 2, &[1, 2]);
        assert!(d.match_still_proving(
            1,
            2,
            MatchProof {
                token,
                messages: &matched,
                evidence: &evidence,
                policy: &p,
                now,
            }
        ));

        let mut replacement = post(11, 101, now + Duration::from_secs(1), &[3, 4]);
        replacement.revision = d.revision();
        replacement.edited_ms = 1;
        replacement.files = vec![
            Attachment::new(
                101,
                50,
                3,
                "https://cdn.discordapp.com/attachments/101/50/a.png",
                10,
            )
            .unwrap(),
            Attachment::new(
                101,
                51,
                3,
                "https://cdn.discordapp.com/attachments/101/51/b.png",
                10,
            )
            .unwrap(),
        ];
        assert!(matches!(
            d.admit(
                1,
                2,
                replacement,
                token,
                &p,
                now + Duration::from_secs(1),
                true
            ),
            AdmitOutcome::Admitted
        ));
        assert!(!d.match_still_proving(
            1,
            2,
            MatchProof {
                token,
                messages: &matched,
                evidence: &evidence,
                policy: &p,
                now: now + Duration::from_secs(1),
            }
        ));
    }

    #[test]
    fn attachment_replacement_cancels_an_inflight_hash_job() {
        let d = Detector::new(Limits::default()).unwrap();
        let now = Instant::now();
        let p = policy();
        let token = (0, 0);
        let old_job = job(&d, now);

        let mut replacement = post(11, 101, now + Duration::from_secs(1), &[3, 4]);
        replacement.revision = d.revision();
        replacement.edited_ms = 1;
        replacement.files = vec![
            Attachment::new(
                101,
                50,
                3,
                "https://cdn.discordapp.com/attachments/101/50/a.png",
                10,
            )
            .unwrap(),
            Attachment::new(
                101,
                51,
                3,
                "https://cdn.discordapp.com/attachments/101/51/b.png",
                10,
            )
            .unwrap(),
        ];
        assert!(matches!(
            d.admit(
                1,
                2,
                replacement,
                token,
                &p,
                now + Duration::from_secs(1),
                true
            ),
            AdmitOutcome::Admitted | AdmitOutcome::Matched(_)
        ));
        assert_eq!(
            d.complete(old_job, &p, false, now + Duration::from_secs(1)),
            None
        );
    }

    #[test]
    fn exact_common_set_allows_extras_order_and_rejects_overlap_chain() {
        let now = Instant::now();
        let a = post(1, 1, now, &[1, 2]);
        let b = post(2, 2, now, &[2, 1, 3]);
        let c = post(3, 3, now, &[1, 2]);
        assert_eq!(
            common_images(&[&a, &b, &c], &policy(), 100_000)
                .unwrap()
                .unwrap()
                .messages,
            vec![(1, 1), (2, 2), (3, 3)]
        );
        let c = post(3, 3, now, &[2, 3]);
        assert_eq!(common_images(&[&a, &b, &c], &policy(), 100_000), Ok(None));
        let c = post(3, 3, now, &[1, 1]);
        assert_eq!(common_images(&[&a, &b, &c], &policy(), 100_000), Ok(None));
        let c = post(3, 2, now, &[1, 2]);
        assert_eq!(common_images(&[&a, &b, &c], &policy(), 100_000), Ok(None));
        assert_eq!(common_images(&[&a, &b, &c], &policy(), 1), Err(()));
    }
    #[test]
    fn matcher_agrees_with_small_exhaustive_common_intersection_oracle() {
        let now = Instant::now();
        let values = |mask: u8| (0..4).filter(|i| mask & (1 << i) != 0).collect::<Vec<u8>>();
        for a in 0u8..16 {
            for b in 0u8..16 {
                for c in 0u8..16 {
                    let posts = [
                        post(1, 1, now, &values(a)),
                        post(2, 2, now, &values(b)),
                        post(3, 3, now, &values(c)),
                    ];
                    let refs: Vec<_> = posts.iter().collect();
                    let expected = (a & b & c).count_ones() >= 2;
                    assert_eq!(
                        common_images(&refs, &policy(), 100_000).unwrap().is_some(),
                        expected,
                        "{a} {b} {c}"
                    );
                }
            }
        }
    }
    #[test]
    fn candidates_are_author_guild_and_channel_scoped_and_metadata_is_not_proof() {
        let now = Instant::now();
        let d = Detector::new(Limits::default()).unwrap();
        assert!(admit(&d, 1, 10, now).is_none());
        assert!(admit(&d, 2, 10, now).is_none());
        assert!(matches!(
            d.admit(
                2,
                2,
                post(3, 11, now, &[1, 2]),
                (0, 0),
                &policy(),
                now,
                false
            ),
            AdmitOutcome::Admitted
        ));
        assert!(matches!(
            d.admit(
                1,
                3,
                post(4, 12, now, &[1, 2]),
                (0, 0),
                &policy(),
                now,
                false
            ),
            AdmitOutcome::Admitted
        ));
        assert!(admit(&d, 5, 11, now).is_none());
        let mut j = admit(&d, 6, 12, now).unwrap();
        for (i, p) in j.posts.iter_mut().enumerate() {
            p.hashes = Some(vec![[i as u8; 32], [i as u8 + 10; 32]]);
        }
        assert_eq!(d.complete(j, &policy(), false, now), None);
    }
    #[test]
    fn edit_delete_and_guild_loss_cancel_inflight_results_and_duplicate_creates() {
        for kind in 0..3 {
            let now = Instant::now();
            let d = Detector::new(Limits::default()).unwrap();
            let j = job(&d, now);
            match kind {
                0 => d.invalidate(1, 100, 10),
                1 => d.invalidate_guild(1),
                _ => d.invalidate_author(1, 2),
            }
            assert_eq!(d.complete(j, &policy(), false, now), None);
            assert!(admit(&d, 10, 100, now + Duration::from_secs(1)).is_none());
        }
    }
    #[test]
    fn invalidation_reclaims_slots_and_bytes_without_resurrecting_ids() {
        let now = Instant::now();
        let d = Detector::new(Limits {
            max_posts_per_author: 2,
            metadata_bytes: 4096,
            metadata_bytes_per_guild: 4096,
            ..Limits::default()
        })
        .unwrap();
        assert!(admit(&d, 1, 10, now).is_none());
        assert!(admit(&d, 2, 11, now).is_none());
        assert!(admit(&d, 3, 12, now).is_none());
        let before = d.state.lock();
        assert_eq!(before.guilds[&1].authors[&2].posts.len(), 2);
        let occupied = before.bytes;
        drop(before);
        d.invalidate(1, 10, 1);
        {
            let s = d.state.lock();
            let b = &s.guilds[&1].authors[&2];
            assert_eq!(b.posts.len(), 1);
            assert_eq!(b.posts[0].id, 2);
            assert!(b.retired_before >= 1);
            assert!(!s.index.contains_key(&(1, 10, 1)));
            assert!(s.bytes < occupied);
        }
        assert!(admit(&d, 1, 10, now).is_none());
        assert!(admit(&d, 3, 12, now).is_none());
        {
            let s = d.state.lock();
            let ids: Vec<_> = s.guilds[&1].authors[&2]
                .posts
                .iter()
                .map(|p| p.id)
                .collect();
            assert_eq!(ids, vec![2, 3]);
        }
        d.invalidate_author(1, 2);
        {
            let s = d.state.lock();
            assert!(!s.guilds[&1].authors.contains_key(&2));
            assert_eq!(s.bytes, 0);
            assert!(s.index.is_empty());
            assert_eq!(s.authors, 0);
            assert!(s.guilds[&1].watermarks[&2].retired_before >= 3);
        }
        assert!(admit(&d, 3, 12, now).is_none());
        assert!(admit(&d, 4, 13, now).is_none());
        {
            let s = d.state.lock();
            let ids: Vec<_> = s.guilds[&1].authors[&2]
                .posts
                .iter()
                .map(|p| p.id)
                .collect();
            assert_eq!(ids, vec![4]);
        }
    }
    #[test]
    fn original_window_expires_even_when_hashes_arrive_late() {
        let now = Instant::now();
        let d = Detector::new(Limits::default()).unwrap();
        let j = job(&d, now);
        assert_eq!(
            d.complete(j, &policy(), false, now + Duration::from_secs(60)),
            None
        );
        assert!(admit(&d, 10, 100, now + Duration::from_secs(61)).is_none());
        let s = d.state.lock();
        assert!(s.index.is_empty());
        assert_eq!(s.bytes, 0);
        assert_eq!(s.guilds[&1].watermarks[&2].retired_before, 12);
    }
    #[test]
    fn settings_change_drops_old_observations_and_old_job() {
        let now = Instant::now();
        let d = Detector::new(Limits::default()).unwrap();
        let j = job(&d, now);
        assert!(matches!(
            d.admit(
                1,
                2,
                post(13, 103, now, &[1, 2]),
                (1, 1),
                &policy(),
                now,
                false
            ),
            AdmitOutcome::Admitted
        ));
        assert_eq!(d.complete(j, &policy(), false, now), None);
        let s = d.state.lock();
        let b = &s.guilds[&1].authors[&2];
        assert_eq!(b.posts.len(), 1);
        assert_eq!(b.posts[0].id, 13);
        assert!(b.posts[0].valid);
        assert!(b.retired_before >= 12);
    }
    #[test]
    fn reports_are_throttled_and_failures_never_claim_a_match() {
        let now = Instant::now();
        let d = Detector::new(Limits::default()).unwrap();
        let mut pending = job(&d, now);
        for post in &mut pending.posts {
            post.hashes = None;
        }
        assert_eq!(
            d.complete(pending, &policy(), true, now),
            Some(Report::Unverified)
        );
        assert!(admit(&d, 13, 103, now).is_none());
        let vars = report_vars(Report::Unverified, 2, 100, &policy());
        assert!(vars["action"].contains("not a spam hit"));
        assert!(!vars["action"].contains("log only"));
        assert!(vars["content"].is_empty());
        assert!(vars["reason"].contains("No spam match was established"));
    }
    #[test]
    fn rate_and_memory_caps_fail_closed_without_reset_on_settings_change() {
        let now = Instant::now();
        let d = Detector::new(Limits {
            max_authors: 1,
            max_authors_per_guild: 1,
            requests_per_author_minute: 5,
            ..Limits::default()
        })
        .unwrap();
        let mut j = job(&d, now);
        for p in &mut j.posts {
            p.hashes = None;
        }
        assert!(!d.reserve(&j, &policy(), now)); // six files, five request budget
        assert!(matches!(
            d.admit(
                2,
                2,
                post(13, 103, now, &[1, 2]),
                (0, 0),
                &policy(),
                now,
                false
            ),
            AdmitOutcome::Rejected
        ));
        assert_eq!(d.state.lock().authors, 1);
        let mut b = Budget::new(now);
        assert!(b.fits(now, 3, 10, 3, 10));
        b.charge(3, 10);
        assert!(!b.fits(now, 1, 0, 3, 10));
        assert!(b.fits(now + Duration::from_secs(60), 3, 10, 3, 10));
        d.prune_at(now + KEEP);
        let s = d.state.lock();
        assert!(s.guilds.is_empty());
        assert!(s.index.is_empty());
        assert_eq!(s.bytes, 0);
        assert_eq!(s.authors, 0);
    }
    #[test]
    fn invalidation_parks_replay_watermark_without_holding_author_slots() {
        let now = Instant::now();
        let d = Detector::new(Limits {
            max_authors: 1,
            max_authors_per_guild: 1,
            ..Limits::default()
        })
        .unwrap();
        assert!(admit(&d, 1, 10, now).is_none());
        assert!(matches!(
            d.admit(
                1,
                3,
                post(2, 11, now, &[1, 2]),
                (0, 0),
                &policy(),
                now,
                false
            ),
            AdmitOutcome::Rejected
        ));
        d.invalidate_author(1, 2);
        assert_eq!(d.state.lock().authors, 0);
        let mut p = post(3, 12, now, &[1, 2]);
        p.revision = d.revision();
        assert!(matches!(
            d.admit(1, 3, p, (0, 0), &policy(), now, false),
            AdmitOutcome::Admitted
        ));
        {
            let s = d.state.lock();
            assert!(s.guilds[&1].authors.contains_key(&3));
            assert!(!s.guilds[&1].authors.contains_key(&2));
            assert!(s.guilds[&1].watermarks[&2].retired_before >= 1);
            assert_eq!(s.authors, 1);
        }
        assert!(admit(&d, 1, 10, now).is_none());
    }
    #[test]
    fn metadata_budget_and_author_post_limit_do_not_grow() {
        let now = Instant::now();
        let d = Detector::new(Limits {
            metadata_bytes: 4096,
            metadata_bytes_per_guild: 4096,
            ..Limits::default()
        })
        .unwrap();
        for id in 1..100 {
            admit(&d, id, id, now);
        }
        let s = d.state.lock();
        assert!(s.bytes <= 4096);
        assert!(s.index.len() < 10);
    }
    #[test]
    fn explicit_exemptions_apply_without_implicit_bot_or_moderator_bypass() {
        let mut rule = crate::defaults::default_rules().remove(1);
        assert!(eligible(&rule, 1, 2, &[3], false));
        rule.whitelist_roles.push(3);
        assert!(!eligible(&rule, 1, 2, &[3], false));
        assert!(eligible(&rule, 1, 2, &[], false));
        rule.whitelist_channels.push(2);
        assert!(!eligible(&rule, 1, 2, &[], false));
        rule.whitelist_channels.clear();
        rule.whitelist_users.push(1);
        assert!(!eligible(&rule, 1, 2, &[], false));
        assert!(!may_admit(&rule, 1, 2, None, false));
        rule.whitelist_users.clear();
        rule.whitelist_roles.push(9);
        assert!(!may_admit(&rule, 1, 2, None, false));
        assert!(may_admit(&rule, 1, 2, Some(&[]), false));
    }

    #[test]
    fn skip_moderators_and_include_channels_gate_image_admission() {
        let mut rule = crate::defaults::default_rules().remove(1);
        rule.skip_moderators = true;
        rule.include_channels = vec![10, 11];
        rule.whitelist_channels = vec![10];
        assert!(!may_admit(&rule, 1, 11, Some(&[]), true));
        assert!(may_admit(&rule, 1, 11, Some(&[]), false));
        assert!(!may_admit(&rule, 1, 10, Some(&[]), false));
        assert!(!may_admit(&rule, 1, 12, Some(&[]), false));
    }

    fn automod_notice(vars: &BTreeMap<String, String>) -> String {
        let template = crate::defaults::default_templates()
            .into_iter()
            .find(|(event, _)| *event == "automod")
            .map(|(_, template)| template)
            .expect("automod template");
        crate::templates::render(template, vars)
    }

    fn automod_notice_bounded(vars: &BTreeMap<String, String>) -> String {
        let template = crate::defaults::default_templates()
            .into_iter()
            .find(|(event, _)| *event == "automod")
            .map(|(_, template)| template)
            .expect("automod template");
        let mut vars = vars.clone();
        crate::logging::render_log_template("automod", template, &mut vars)
    }

    #[test]
    fn proven_hashes_keep_a_match_reason_and_evidence_when_download_is_marked_failed() {
        let now = Instant::now();
        let d = Detector::new(Limits::default()).unwrap();
        let Report::Verified(image_match) = d
            .complete(job(&d, now), &policy(), true, now)
            .expect("a hash-proven match is a hit")
        else {
            panic!("download failure must not replace a proven match with an unverified notice");
        };
        let vars = report_vars(Report::Verified(image_match), 2, 100, &policy());
        let notice = automod_notice(&vars);
        assert!(
            notice.contains("Verified identical image bytes"),
            "{notice}"
        );
        assert!(!notice.contains("no match established"), "{notice}");
        assert!(!notice.contains("budget"), "{notice}");
        assert!(
            notice.contains("**Evidence:**") && notice.contains("sha256"),
            "{notice}"
        );
        assert!(!notice.contains("http"), "{notice}");
        assert!(vars["action"].contains("log only"));
    }

    #[test]
    fn blocked_download_is_not_a_successful_spam_hit() {
        let now = Instant::now();
        let d = Detector::new(Limits::default()).unwrap();
        let mut pending = job(&d, now);
        for post in &mut pending.posts {
            post.hashes = None;
        }
        let report = d
            .complete(pending, &policy(), true, now)
            .expect("a blocked analysis still explains itself");
        let vars = report_vars(report, 2, 100, &policy());
        let notice = automod_notice(&vars);
        assert!(vars["action"].contains("not a spam hit"), "{notice}");
        assert!(!vars["action"].contains("log only"), "{notice}");
        assert!(notice.contains("did not finish"), "{notice}");
        assert!(notice.contains("budget"), "{notice}");
        assert!(
            !notice.contains("Verified identical image bytes"),
            "{notice}"
        );
        assert!(vars["content"].is_empty(), "{notice}");
        assert!(admit(&d, 13, 103, now).is_none());
    }

    #[test]
    fn comparison_budget_block_is_not_a_spam_hit() {
        let now = Instant::now();
        let detector = Detector::new(Limits {
            comparison_steps: 1,
            ..Limits::default()
        })
        .unwrap();
        assert_eq!(
            detector.complete(job(&detector, now), &policy(), false, now),
            Some(Report::Unverified)
        );
        let vars = report_vars(Report::Unverified, 2, 100, &policy());
        let notice = automod_notice(&vars);
        assert!(vars["action"].contains("not a spam hit"), "{notice}");
        assert!(vars["content"].is_empty(), "{notice}");
        assert!(notice.contains("work budget"), "{notice}");
        assert!(
            !notice.contains("Verified identical image bytes"),
            "{notice}"
        );
    }

    #[test]
    fn large_enforcement_notice_keeps_full_proving_hashes_within_budget() {
        let snowflake = |n: u64| 1_000_000_000_000_000_000u64 + n;
        let messages: Vec<_> = (0..16)
            .map(|i| (snowflake(i * 2), snowflake(i * 2 + 1)))
            .collect();
        let proving = vec![[0xab_u8; 32]];
        let hash_hex = proving[0]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let evidence = super::image_notice_evidence(&messages, &proving);
        let delete_notes = messages
            .iter()
            .map(|(channel, message)| format!("{channel}/{message} Succeeded"))
            .collect::<Vec<_>>()
            .join("; ");
        let content = format!("{evidence}; {delete_notes}");
        let policy = policy();
        let vars = BTreeMap::from([
            ("rule".into(), "Exact image spam".into()),
            ("mention".into(), format!("<@{}>", snowflake(99))),
            ("user_id".into(), snowflake(99).to_string()),
            ("source_channel_id".into(), snowflake(0).to_string()),
            ("channel".into(), format!("<#{}>", snowflake(0))),
            (
                "action".into(),
                "delete matched messages; ban Succeeded".into(),
            ),
            ("reason".into(), verified_reason(&policy)),
            ("content".into(), content),
            ("time".into(), "2026-10-09T00:00:00+00:00".into()),
        ]);
        let notice = automod_notice_bounded(&vars);
        assert!(
            notice.contains(&hash_hex),
            "expected full sha256 digest in notice: {notice}"
        );
        assert!(
            notice.contains("sha256"),
            "expected hash marker in notice: {notice}"
        );
        assert!(
            notice.encode_utf16().count() <= 2000,
            "notice exceeded Discord budget: {}",
            notice.encode_utf16().count()
        );
        assert!(
            !notice.ends_with('…') || notice.contains(&hash_hex),
            "truncation must not drop proving hashes: {notice}"
        );
    }

    #[test]
    fn in_flight_proven_match_replaces_a_blocked_notice_window() {
        let now = Instant::now();
        let detector = Detector::new(Limits::default()).unwrap();
        let mut pending = job(&detector, now);
        for post in &mut pending.posts {
            post.hashes = None;
        }
        assert_eq!(
            detector.complete(pending, &policy(), true, now),
            Some(Report::Unverified)
        );
        let mut posts = detector.state.lock().guilds[&1].authors[&2].posts.clone();
        for post in &mut posts {
            post.hashes = Some(vec![[1; 32], [2; 32]]);
        }
        let Report::Verified(image_match) = detector
            .complete(
                Job {
                    guild: 1,
                    author: 2,
                    token: (0, 0),
                    posts,
                },
                &policy(),
                false,
                now,
            )
            .expect("in-flight hashes prove the match")
        else {
            panic!("a blocked window must not hide a later proven match");
        };
        let vars = report_vars(Report::Verified(image_match), 2, 100, &policy());
        assert!(vars["reason"].contains("Verified identical image bytes"));
        assert!(vars["content"].contains("sha256"));
        assert!(!vars["reason"].contains("budget"));
        let posts = detector.state.lock().guilds[&1].authors[&2].posts.clone();
        assert_eq!(
            detector.complete(
                Job {
                    guild: 1,
                    author: 2,
                    token: (0, 0),
                    posts,
                },
                &policy(),
                true,
                now,
            ),
            None
        );
    }
    #[tokio::test]
    async fn managed_hash_survives_restart_and_proves_first_post_through_config_and_detector() {
        let path = std::env::temp_dir().join(format!(
            "known_bad_detector_{}.db",
            jmodguard::control::random_request_id().unwrap()
        ));
        let db = crate::db::Db::open(&path, 2).unwrap();
        let enrollment = crate::db::enroll_guild(&db, 1, 1).await.unwrap().unwrap();
        let digest = "01".repeat(32);
        let reply = crate::db::apply_control(
            &db,
            None,
            crate::db::ControlOp::AddKnownBadImage {
                scope: crate::db::ScopeWire {
                    global: false,
                    guild_id: enrollment.guild_id,
                    enrollment_id: enrollment.id,
                    joined_at_ns: enrollment.joined_at_ns,
                },
                hash: digest.clone(),
                note: None,
            },
            None,
        )
        .await
        .unwrap();
        assert!(matches!(reply, crate::db::ControlReply::Saved { .. }));
        drop(db);
        let db = crate::db::Db::open(&path, 2).unwrap();
        let app = BotState::new(jmodguard::config::BotProcessConfig::default(), db).unwrap();
        let cfg = app.configs.get(&app.db, 1).await.unwrap();
        let rule = cfg.rules.get("attachment_flood").unwrap();
        let policy = policy_for_config(&cfg, rule).unwrap();
        assert_eq!(minimum_files(&policy), 1);
        let now = Instant::now();
        let detector = Detector::new(Limits::default()).unwrap();
        let token = (cfg.version, cfg.durable_version);
        let AdmitOutcome::Matched(job) =
            detector.admit(1, 2, post(1, 10, now, &[1]), token, &policy, now, false)
        else {
            panic!("managed list admits the first post")
        };
        let Report::Verified(found) = detector.complete(job, &policy, false, now).unwrap() else {
            panic!("known digest is proof")
        };
        assert!(found.known_bad);
        assert_eq!(found.proving, vec![[1; 32]]);
        let evidence = vec![TextEvidence {
            channel_id: 10,
            message_id: 1,
            author_id: 2,
            occurred_at_ms: 1,
            text: digest,
        }];
        assert!(detector.match_still_proving(
            1,
            2,
            MatchProof {
                token,
                messages: &found.messages,
                evidence: &evidence,
                policy: &policy,
                now
            }
        ));
        let cleared = super::tests::policy();
        assert!(!detector.match_still_proving(
            1,
            2,
            MatchProof {
                token,
                messages: &found.messages,
                evidence: &evidence,
                policy: &cleared,
                now
            }
        ));
        detector.invalidate(1, 10, 1);
        assert!(!detector.match_still_proving(
            1,
            2,
            MatchProof {
                token,
                messages: &found.messages,
                evidence: &evidence,
                policy: &policy,
                now
            }
        ));
        drop(app);
        jmodguard::tenant::remove_sidecars(&path);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[derive(Clone, Copy)]
    enum DispatchChange {
        None,
        RemoveHash,
        Invalidate,
        Expire,
        OwnDeleteEcho,
        OwnEchoAndExternalDelete,
        OwnEchoAndExternalBulkDelete,
        OwnEchoAndRemoveHash,
        LateOwnDeleteEcho,
    }

    // Serenity exposes Context construction through a shard runner. Connect only
    // a local handshake fixture; no Gateway runner or Discord connection is started.
    async fn delete_context(http: Arc<serenity::http::Http>) -> Context {
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

    async fn image_dispatch_race(
        known_bad: bool,
        change: DispatchChange,
        trigger_method: &str,
        trigger_path: &str,
        expected_deletes: usize,
        expected_bans: usize,
    ) {
        use crate::spam_actions::{Kind, Match, MatchPolicy, ReportDisposition};
        let _serial = crate::spam_actions::tests::IMAGE_ENFORCEMENT_TEST_LOCK
            .lock()
            .await;
        use std::sync::atomic::{AtomicBool, Ordering};

        let path =
            std::env::temp_dir().join(format!("image_dispatch_{:032x}.db", rand::random::<u128>()));
        let app =
            BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap();
        assert!(
            crate::db::claim_active_bot(
                &app.db,
                app.activation.instance(),
                jmodguard::control::now_ms(),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap()
        );
        app.activation.mark_lock_held(true);
        app.activation.note_discord_connected();
        assert!(app.activation.allows_rule_trigger());
        let enrollment = crate::db::enroll_guild(&app.db, 1, 1)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();
        let scope = crate::db::ScopeWire {
            global: false,
            guild_id: 1,
            enrollment_id: enrollment.id.clone(),
            joined_at_ns: enrollment.joined_at_ns,
        };
        let mut rule = crate::defaults::default_rules().remove(1);
        rule.enabled = true;
        rule.skip_moderators = false;
        rule.dry_run = false;
        rule.params["mode"] = "exact_image".into();
        rule.actions = serde_json::from_value(serde_json::json!([
            {"action": "delete"}, {"action": "ban"}
        ]))
        .unwrap();
        crate::db::upsert_rule(&app.db, 1, "attachment_flood", &rule)
            .await
            .unwrap();
        if known_bad {
            crate::db::apply_control(
                &app.db,
                None,
                crate::db::ControlOp::AddKnownBadImage {
                    scope: scope.clone(),
                    hash: "01".repeat(32),
                    note: None,
                },
                None,
            )
            .await
            .unwrap();
        }
        app.poll_config_version().await.unwrap();
        let cfg = app.configs.get(&app.db, 1).await.unwrap();
        let policy = policy_for_config(&cfg, cfg.rules.get("attachment_flood").unwrap()).unwrap();
        assert!(policy.enforce);
        let token = (cfg.version, cfg.durable_version);
        let now = Instant::now();
        let mut matched = None;
        for i in 0..if known_bad { 1 } else { 3 } {
            if let AdmitOutcome::Matched(job) = app.image_spam.admit(
                1,
                99,
                post(10 + i, 100 + i, now, &[1, 2]),
                token,
                &policy,
                now,
                false,
            ) {
                matched = app.image_spam.complete(job, &policy, false, now);
            }
        }
        let Some(Report::Verified(matched)) = matched else {
            panic!("the real detector must prove the admitted image match");
        };
        assert_eq!(matched.known_bad, known_bad);
        let found = Match {
            kind: Kind::Image,
            guild: 1,
            author: 99,
            channel: 100,
            token,
            excerpts: evidence(&matched.messages, 99, &[1, 2]),
            reason: match_reason(&matched, &policy),
            policy: MatchPolicy::Image(policy.clone()),
            messages: matched.messages,
            proving_hashes: matched.proving,
            attachments: Vec::new(),
        };
        let lines = Arc::new(Mutex::new(Vec::new()));
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let http = Arc::new(
            serenity::http::HttpBuilder::new("token")
                .proxy(format!("http://{addr}"))
                .ratelimiter_disabled(true)
                .build(),
        );
        let ctx = delete_context(Arc::clone(&http)).await;
        let stop = Arc::new(AtomicBool::new(false));
        struct Server(Arc<AtomicBool>, Option<std::thread::JoinHandle<()>>);
        impl Drop for Server {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
                self.1.take().unwrap().join().unwrap();
            }
        }
        let changed = Arc::new(AtomicBool::new(false));
        let server_app = Arc::clone(&app);
        let server_changed = Arc::clone(&changed);
        let server_lines = Arc::clone(&lines);
        let server_stop = Arc::clone(&stop);
        let trigger = (trigger_method.to_owned(), trigger_path.to_owned());
        let runtime = tokio::runtime::Handle::current();
        let server = Server(
            stop,
            Some(std::thread::spawn(move || {
                crate::spam_actions::tests::serve_discord_stub(
                    listener,
                    server_lines,
                    server_stop,
                    |method, path| {
                        use serenity::client::EventHandler;
                        use serenity::model::id::{ChannelId, MessageId};
                        let handler = crate::handlers::Handler {
                            app: Arc::clone(&server_app),
                        };
                        if method == "DELETE"
                            && matches!(
                                change,
                                DispatchChange::OwnDeleteEcho
                                    | DispatchChange::OwnEchoAndExternalDelete
                                    | DispatchChange::OwnEchoAndExternalBulkDelete
                                    | DispatchChange::OwnEchoAndRemoveHash
                            )
                        {
                            let parts: Vec<_> = path.split('/').collect();
                            let message = parts.last().unwrap().parse::<u64>().unwrap();
                            let channel = parts[parts.len() - 3].parse::<u64>().unwrap();
                            runtime.block_on(handler.message_delete(
                                ctx.clone(),
                                ChannelId::new(channel),
                                MessageId::new(message),
                                Some(GuildId::new(1)),
                            ));
                            assert_eq!(
                                server_app.image_spam.author_of(1, channel, message),
                                None,
                                "own echoes must remove the live observation"
                            );
                        }
                        if method != trigger.0
                            || !path.ends_with(&trigger.1)
                            || server_changed.swap(true, Ordering::SeqCst)
                        {
                            return;
                        }
                        match change {
                            DispatchChange::None | DispatchChange::OwnDeleteEcho => {}
                            DispatchChange::OwnEchoAndExternalDelete => {
                                runtime.block_on(handler.message_delete(
                                    ctx.clone(),
                                    ChannelId::new(102),
                                    MessageId::new(12),
                                    Some(GuildId::new(1)),
                                ));
                            }
                            DispatchChange::OwnEchoAndExternalBulkDelete => {
                                runtime.block_on(handler.message_delete_bulk(
                                    ctx.clone(),
                                    ChannelId::new(102),
                                    vec![MessageId::new(12)],
                                    Some(GuildId::new(1)),
                                ));
                            }
                            DispatchChange::LateOwnDeleteEcho => {
                                for i in 0..if known_bad { 1 } else { 3 } {
                                    runtime.block_on(handler.message_delete(
                                        ctx.clone(),
                                        ChannelId::new(100 + i),
                                        MessageId::new(10 + i),
                                        Some(GuildId::new(1)),
                                    ));
                                    assert_eq!(
                                        server_app.image_spam.author_of(1, 100 + i, 10 + i),
                                        None
                                    );
                                }
                            }
                            DispatchChange::RemoveHash | DispatchChange::OwnEchoAndRemoveHash => {
                                runtime.block_on(async {
                                    let reply = crate::db::apply_control(
                                        &server_app.db,
                                        None,
                                        crate::db::ControlOp::RemoveKnownBadImage {
                                            scope: scope.clone(),
                                            hash: "01".repeat(32),
                                        },
                                        None,
                                    )
                                    .await
                                    .unwrap();
                                    assert!(matches!(reply, crate::db::ControlReply::Saved { .. }));
                                    // Do not refresh the cache: dispatch must observe the durable write.
                                });
                            }
                            DispatchChange::Invalidate => {
                                server_app.image_spam.invalidate(1, 102, 12)
                            }
                            DispatchChange::Expire => {
                                server_app
                                    .image_spam
                                    .state
                                    .lock()
                                    .guilds
                                    .get_mut(&1)
                                    .unwrap()
                                    .authors
                                    .get_mut(&99)
                                    .unwrap()
                                    .posts[0]
                                    .at = Instant::now() - Duration::from_secs(policy.window_secs);
                            }
                        }
                    },
                );
            })),
        );
        let disposition = crate::spam_actions::enforce(
            &app,
            &http,
            &serenity::cache::Cache::new(),
            found,
            "attachment_flood",
            "Exact image spam",
        )
        .await;
        drop(server);
        assert_eq!(disposition, ReportDisposition::Consumed);
        assert!(
            changed.load(Ordering::SeqCst),
            "race trigger must be reached"
        );
        let seen = lines.lock().clone();
        assert_eq!(
            seen.iter()
                .filter(|line| line.starts_with("DELETE "))
                .count(),
            expected_deletes,
            "unexpected message deletes: {seen:?}"
        );
        assert_eq!(
            seen.iter()
                .filter(|line| line.starts_with("PUT ") && line.contains("/bans/"))
                .count(),
            expected_bans,
            "unexpected bans: {seen:?}"
        );
        for i in 0..expected_deletes as u64 {
            assert!(
                seen.iter().any(|line| line
                    == &format!("DELETE /api/v10/channels/{}/messages/{}", 100 + i, 10 + i)),
                "every expected matched message must be deleted: {seen:?}"
            );
            if matches!(
                change,
                DispatchChange::OwnDeleteEcho
                    | DispatchChange::OwnEchoAndExternalDelete
                    | DispatchChange::OwnEchoAndExternalBulkDelete
                    | DispatchChange::OwnEchoAndRemoveHash
                    | DispatchChange::LateOwnDeleteEcho
            ) {
                assert_eq!(
                    app.image_spam.author_of(1, 100 + i, 10 + i),
                    None,
                    "enforcement must not resurrect deleted observations"
                );
            }
        }
        let (store, scope) = app.tenant.live(enrollment).await.unwrap();
        for id in store
            .case_ids(scope.clone(), None, 20, crate::tenant::case_limits())
            .await
            .unwrap()
        {
            let case = store
                .case(scope.clone(), id, crate::tenant::case_limits())
                .await
                .unwrap()
                .unwrap();
            assert_ne!(
                case.outcome,
                crate::db::guild_data::CaseOutcome::Attempted,
                "a skipped ban must settle its prepared case"
            );
        }
        drop(app);
        let _ = std::fs::remove_file(&path);
        crate::tenant::remove_sidecars(&path);
    }

    #[tokio::test]
    async fn known_bad_removal_during_member_lookup_skips_delete_and_ban() {
        image_dispatch_race(true, DispatchChange::RemoveHash, "GET", "/members/99", 0, 0).await;
    }

    #[tokio::test]
    async fn known_bad_removal_after_delete_skips_ban() {
        image_dispatch_race(
            true,
            DispatchChange::RemoveHash,
            "DELETE",
            "/messages/10",
            1,
            0,
        )
        .await;
    }

    #[tokio::test]
    async fn known_bad_removal_during_ban_lookup_skips_ban() {
        image_dispatch_race(true, DispatchChange::RemoveHash, "GET", "/bans/99", 1, 0).await;
    }

    #[tokio::test]
    async fn known_bad_still_listed_dispatches_delete_and_ban() {
        image_dispatch_race(true, DispatchChange::None, "GET", "/members/99", 1, 1).await;
    }

    #[tokio::test]
    async fn repeated_image_invalidation_after_delete_skips_remaining_actions() {
        image_dispatch_race(
            false,
            DispatchChange::Invalidate,
            "DELETE",
            "/messages/10",
            1,
            0,
        )
        .await;
    }

    #[tokio::test]
    async fn repeated_image_expiry_after_delete_skips_remaining_actions() {
        image_dispatch_race(
            false,
            DispatchChange::Expire,
            "DELETE",
            "/messages/10",
            1,
            0,
        )
        .await;
    }

    #[tokio::test]
    async fn repeated_image_invalidation_during_ban_lookup_skips_ban() {
        image_dispatch_race(false, DispatchChange::Invalidate, "GET", "/bans/99", 3, 0).await;
    }

    #[tokio::test]
    async fn repeated_image_expiry_during_ban_lookup_skips_ban() {
        image_dispatch_race(false, DispatchChange::Expire, "GET", "/bans/99", 3, 0).await;
    }

    #[tokio::test]
    async fn repeated_image_still_present_dispatches_deletes_and_ban() {
        image_dispatch_race(false, DispatchChange::None, "GET", "/members/99", 3, 1).await;
    }

    #[tokio::test]
    async fn repeated_image_own_delete_echoes_preserve_remaining_deletes_and_ban() {
        image_dispatch_race(
            false,
            DispatchChange::OwnDeleteEcho,
            "DELETE",
            "/messages/10",
            3,
            1,
        )
        .await;
    }

    #[tokio::test]
    async fn known_bad_own_delete_echo_preserves_ban() {
        image_dispatch_race(
            true,
            DispatchChange::OwnDeleteEcho,
            "DELETE",
            "/messages/10",
            1,
            1,
        )
        .await;
    }

    #[tokio::test]
    async fn repeated_image_own_echo_then_external_delete_cancels_remaining_actions() {
        image_dispatch_race(
            false,
            DispatchChange::OwnEchoAndExternalDelete,
            "DELETE",
            "/messages/10",
            1,
            0,
        )
        .await;
    }

    #[tokio::test]
    async fn repeated_image_external_delete_during_later_dispatch_cancels_ban() {
        image_dispatch_race(
            false,
            DispatchChange::OwnEchoAndExternalDelete,
            "DELETE",
            "/messages/11",
            2,
            0,
        )
        .await;
    }

    #[tokio::test]
    async fn known_bad_removal_after_own_echo_still_skips_ban() {
        image_dispatch_race(
            true,
            DispatchChange::OwnEchoAndRemoveHash,
            "GET",
            "/bans/99",
            1,
            0,
        )
        .await;
    }

    #[tokio::test]
    async fn known_bad_own_echo_after_rest_success_preserves_ban() {
        image_dispatch_race(
            true,
            DispatchChange::LateOwnDeleteEcho,
            "GET",
            "/bans/99",
            1,
            1,
        )
        .await;
    }

    #[tokio::test]
    async fn repeated_image_own_echoes_after_rest_success_preserve_ban() {
        image_dispatch_race(
            false,
            DispatchChange::LateOwnDeleteEcho,
            "GET",
            "/bans/99",
            3,
            1,
        )
        .await;
    }

    #[tokio::test]
    async fn repeated_image_own_echo_then_external_bulk_delete_cancels_remaining_actions() {
        image_dispatch_race(
            false,
            DispatchChange::OwnEchoAndExternalBulkDelete,
            "DELETE",
            "/messages/10",
            1,
            0,
        )
        .await;
    }

    #[test]
    fn own_delete_proof_requires_success_and_expires_at_original_time() {
        let detector = Detector::new(Limits::default()).unwrap();
        let now = Instant::now();
        let mut policy = policy();
        policy.known_bad_hashes.insert([1; 32]);
        detector.admit(1, 2, post(10, 100, now, &[1]), (0, 0), &policy, now, false);
        let mut deleted = detector.begin_delete(1, 2, (0, 0), 100, 10).unwrap();
        detector.deleted(1, 100, 10);
        let messages = [(100, 10)];
        let evidence = evidence(&messages, 2, &[1]);
        let proof = |at| MatchProof {
            token: (0, 0),
            messages: &messages,
            evidence: &evidence,
            policy: &policy,
            now: at,
        };
        assert!(
            !detector.enforcement_still_proving(1, 2, proof(now), std::slice::from_ref(&deleted)),
            "a Gateway echo alone cannot establish our delete succeeded"
        );
        deleted.finish(false);
        assert!(!detector.enforcement_still_proving(
            1,
            2,
            proof(now),
            std::slice::from_ref(&deleted)
        ));
        deleted.finish(true);
        assert!(detector.enforcement_still_proving(
            1,
            2,
            proof(now),
            std::slice::from_ref(&deleted)
        ));
        assert!(
            !detector.enforcement_still_proving(
                1,
                2,
                proof(now + Duration::from_secs(policy.window_secs)),
                std::slice::from_ref(&deleted)
            ),
            "own deletion must not extend the original observation window"
        );
        assert_eq!(detector.author_of(1, 100, 10), None);
        assert!(
            !detector.match_still_proving(1, 2, proof(now)),
            "the live detector must stay empty"
        );
    }

    #[test]
    fn own_delete_echo_cannot_revive_an_edited_observation() {
        let detector = Detector::new(Limits::default()).unwrap();
        let now = Instant::now();
        let mut policy = policy();
        policy.known_bad_hashes.insert([1; 32]);
        detector.admit(1, 2, post(10, 100, now, &[1]), (0, 0), &policy, now, false);
        let mut deleted = detector.begin_delete(1, 2, (0, 0), 100, 10).unwrap();
        assert!(matches!(
            detector.admit(
                1,
                2,
                post(10, 100, now, &[2, 3]),
                (0, 0),
                &policy,
                now,
                true
            ),
            AdmitOutcome::Matched(_)
        ));
        detector.deleted(1, 100, 10);
        deleted.finish(true);
        assert!(
            !detector.enforcement_still_proving(
                1,
                2,
                MatchProof {
                    token: (0, 0),
                    messages: &[(100, 10)],
                    evidence: &evidence(&[(100, 10)], 2, &[1]),
                    policy: &policy,
                    now,
                },
                &[deleted]
            ),
            "an attachment edit before the echo must revoke the original proof"
        );
    }

    #[test]
    fn known_bad_hashes_never_match_missing_or_different_data() {
        let now = Instant::now();
        let mut p = policy();
        p.known_bad_hashes.insert([1; 32]);
        let mut missing = post(1, 10, now, &[1]);
        missing.hashes = None;
        assert_eq!(common_images(&[&missing], &p, 100_000), Ok(None));
        let other = post(2, 11, now, &[2]);
        assert_eq!(common_images(&[&other], &p, 100_000), Ok(None));
        let extra = post(3, 12, now, &[2, 1]);
        let found = common_images(&[&extra], &p, 100_000).unwrap().unwrap();
        assert_eq!(found.proving, vec![[1; 32]]);
        assert!(match_reason(&found, &p).contains("known-bad"));
        assert!(common_images(&[&extra], &p, 0).is_err());
        assert_eq!(common_images(&[&extra], &policy(), 100_000), Ok(None));
        assert!(jmodguard::image_spam::parse_hash("").is_none());
        assert!(jmodguard::image_spam::parse_hash("unknown").is_none());
    }
}
