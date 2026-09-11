//! Bounded, create-only exact-image observations. This module cannot punish.
use crate::state::{BotState, CachedGuildConfig};
use jmodguard::image_spam::{Attachment, Hash, Hasher, Limits, Policy};
use parking_lot::Mutex;
use serenity::{
    client::Context,
    model::{channel::Message, id::GuildId},
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
    time::{Duration, Instant},
};

const KEEP: Duration = Duration::from_secs(660);
type Key = (u64, u64, u64); // guild/channel/message
#[derive(Clone)]
struct Post {
    id: u64,
    channel: u64,
    serial: u64,
    at: Instant,
    files: Vec<Attachment>,
    hashes: Option<Vec<Hash>>,
    valid: bool,
    cost: usize,
    revision: u64,
}
struct Bucket {
    retired_before: u64,
    window: Duration,
    last_seen: Instant,
    token: (u64, u64),
    posts: Vec<Post>,
    budget: Budget,
    reported_until: Instant,
}
struct Guild {
    bytes: usize,
    authors: HashMap<u64, Bucket>,
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
pub struct Detector {
    limits: Limits,
    state: Mutex<State>,
    hasher: Hasher,
    slots: Arc<tokio::sync::Semaphore>,
    dropped: std::sync::atomic::AtomicU64,
}
struct Job {
    guild: u64,
    author: u64,
    token: (u64, u64),
    posts: Vec<Post>,
}
#[derive(Debug, PartialEq)]
enum Report {
    Verified(Vec<(u64, u64)>),
    Unverified,
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
        let State {
            guilds,
            index,
            authors,
            bytes,
            ..
        } = &mut *state;
        guilds.retain(|guild, g| {
            g.authors.retain(|author, b| {
                b.posts.retain(|p| {
                    if now.duration_since(p.at) >= b.window {
                        b.retired_before = b.retired_before.max(p.id);
                        index.remove(&(*guild, p.channel, p.id));
                        *bytes -= p.cost;
                        g.bytes -= p.cost;
                        false
                    } else {
                        true
                    }
                });
                let keep = now.duration_since(b.last_seen) < KEEP;
                if !keep {
                    *authors -= 1;
                }
                let _ = author;
                keep
            });
            !g.authors.is_empty() || now.duration_since(g.budget.start) < Duration::from_secs(60)
        });
    }
    /// Retain only a short tombstone: a duplicate create must not resurrect an edit/delete.
    pub fn revision(&self) -> u64 {
        self.state.lock().revision
    }

    pub fn invalidate(&self, guild: u64, channel: u64, message: u64) {
        let mut state = self.state.lock();
        state.revision = state.revision.wrapping_add(1);
        let Some(author) = state.index.get(&(guild, channel, message)).copied() else {
            return;
        };
        if let Some(b) = state
            .guilds
            .get_mut(&guild)
            .and_then(|g| g.authors.get_mut(&author))
        {
            for p in &mut b.posts {
                if p.id == message && p.channel == channel {
                    p.valid = false;
                    p.files.clear();
                    p.hashes = None;
                }
            }
        }
    }
    pub fn invalidate_author(&self, guild: u64, author: u64) {
        let mut state = self.state.lock();
        state.revision = state.revision.wrapping_add(1);
        if let Some(b) = state
            .guilds
            .get_mut(&guild)
            .and_then(|g| g.authors.get_mut(&author))
        {
            for p in &mut b.posts {
                p.valid = false;
                p.files.clear();
                p.hashes = None;
            }
        }
    }
    pub fn invalidate_guild(&self, guild: u64) {
        let mut state = self.state.lock();
        state.revision = state.revision.wrapping_add(1);
        if let Some(g) = state.guilds.get_mut(&guild) {
            for b in g.authors.values_mut() {
                for p in &mut b.posts {
                    p.valid = false;
                    p.files.clear();
                    p.hashes = None;
                }
            }
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
    ) -> Option<Job> {
        let mut state = self.state.lock();
        if post.revision != state.revision {
            return None;
        }
        let key = (guild, post.channel, post.id);
        // Expire this author in O(posts), keeping a bounded ID watermark
        // so expired/edited creates cannot return after a settings change.
        {
            let State {
                guilds,
                index,
                bytes,
                ..
            } = &mut *state;
            if let Some(g) = guilds.get_mut(&guild)
                && let Some(b) = g.authors.get_mut(&author)
            {
                b.posts.retain(|p| {
                    if now.duration_since(p.at) >= b.window {
                        b.retired_before = b.retired_before.max(p.id);
                        index.remove(&(guild, p.channel, p.id));
                        *bytes -= p.cost;
                        g.bytes -= p.cost;
                        false
                    } else {
                        true
                    }
                });
            }
        }
        if state.index.contains_key(&key)
            || state
                .guilds
                .get(&guild)
                .and_then(|g| g.authors.get(&author))
                .is_some_and(|b| post.id <= b.retired_before)
        {
            return None;
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
            return None;
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
            return None;
        }
        let cost = post.cost;
        let serial = state.serial.checked_add(1)?;
        state.serial = serial;
        post.serial = serial;
        let g = state.guilds.entry(guild).or_insert_with(|| Guild {
            bytes: 0,
            authors: HashMap::new(),
            budget: Budget::new(now),
        });
        let b = g.authors.entry(author).or_insert_with(|| Bucket {
            retired_before: 0,
            window: Duration::from_secs(policy.window_secs),
            last_seen: now,
            token,
            posts: vec![],
            budget: Budget::new(now),
            reported_until: now,
        });
        if b.posts.len() >= self.limits.max_posts_per_author {
            self.capacity_drop();
            return None;
        }
        if b.token != token {
            // Never reinterpret observations after configuration changes. Keep rate budgets/tombstones.
            b.token = token;
            for p in &mut b.posts {
                p.valid = false;
                p.files.clear();
                p.hashes = None;
            }
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
        if now < b.reported_until {
            return None;
        }
        let posts: Vec<_> = b
            .posts
            .iter()
            .filter(|p| live(p, policy, now))
            .cloned()
            .collect();
        // Metadata is only a cheap gate. It can never produce a verified report.
        if posts
            .iter()
            .map(|p| p.channel)
            .collect::<BTreeSet<_>>()
            .len()
            < policy.min_channels
            || !size_candidate(&posts, policy.min_attachments)
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
        if b.token != job.token || now < b.reported_until {
            return None;
        }
        // Every participating observation must still be the same, live create.
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
        let report = if failed {
            Report::Unverified
        } else {
            match common_images(&posts, policy, self.limits.comparison_steps) {
                Ok(Some(matched)) => Report::Verified(matched),
                Ok(None) => return None,
                Err(()) => Report::Unverified,
            }
        };
        b.reported_until = now + Duration::from_secs(policy.window_secs);
        Some(report)
    }
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
) -> Result<Option<Vec<(u64, u64)>>, ()> {
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
                .map(|(_, h)| h)
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
                return Ok(Some(matched));
            }
        }
    }
    Ok(None)
}
fn eligible(rule: &crate::types::RuleDef, author: u64, channel: u64, roles: &[u64]) -> bool {
    rule.enabled
        && !rule.whitelist_users.contains(&author)
        && !rule.whitelist_channels.contains(&channel)
        && !rule.whitelist_roles.iter().any(|r| roles.contains(r))
}

/// Called before the generic automod bot-author exemption. Unknown membership and
/// webhooks are skipped; no privileged role/bot exemption is inferred.
pub fn observe(
    app: &Arc<BotState>,
    ctx: &Context,
    cfg: &CachedGuildConfig,
    msg: &Message,
    received: Instant,
    revision: u64,
) {
    let Some(guild) = msg.guild_id else { return };
    if msg.author.id == ctx.cache.current_user().id || msg.webhook_id.is_some() {
        return;
    }
    let Some(rule) = cfg.rules.get("attachment_flood") else {
        return;
    };
    let Ok(policy) = Policy::parse(rule) else {
        return;
    };
    let Some(member) = &msg.member else { return };
    let roles: Vec<_> = member.roles.iter().map(|r| r.get()).collect();
    if !eligible(rule, msg.author.id.get(), msg.channel_id.get(), &roles) {
        return;
    }
    // A replay cannot acquire a new window. Use the Discord create time, clamped
    // to this callback's monotonic reception time; old/future timestamps are skipped.
    let age_ms = chrono::Utc::now().timestamp_millis() - msg.timestamp.timestamp_millis();
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
            a.width.is_some()
                && a.height.is_some()
                && a.content_type
                    .as_deref()
                    .is_some_and(|t| t.starts_with("image/"))
        })
        .collect();
    if images.len() < policy.min_attachments {
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
    let Some(job) = detector.admit(
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
            cost: 0,
            revision,
        },
        token,
        &policy,
        received,
    ) else {
        return;
    };
    // No wait queue and no task retaining Message/content. Capacity is acquired
    // before spawning; the permit bounds hashing, reconciliation and log delivery.
    let Ok(permit) = Arc::clone(&detector.slots).try_acquire_owned() else {
        detector.capacity_drop();
        return;
    };
    let app = Arc::clone(app);
    let ctx = ctx.clone();
    let channel = msg.channel_id.get();
    tokio::spawn(async move {
        let _permit = permit;
        let deadline = Duration::from_secs(app.image_spam.limits.timeout_secs + 15);
        if tokio::time::timeout(deadline, run_job(&app, &ctx, job, policy, channel))
            .await
            .is_err()
        {
            app.image_spam.capacity_drop();
        }
    });
}
async fn run_job(app: &Arc<BotState>, ctx: &Context, mut job: Job, policy: Policy, channel: u64) {
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
                    let mut hashes = BTreeSet::new();
                    for file in &post.files {
                        hashes.insert(
                            detector
                                .hasher
                                .hash(file, Duration::from_secs(detector.limits.timeout_secs))
                                .await?,
                        );
                    }
                    post.hashes = Some(hashes.into_iter().collect());
                }
                Ok::<_, anyhow::Error>(())
            })
            .await;
        failed = !matches!(outcome, Ok(Ok(())));
    }
    // Reconcile the normal bot configuration observation before logging. This is
    // best-effort test reporting, not an enforcement permission/freshness fence.
    if app.poll_config_version().await.is_err() {
        return;
    }
    let Ok(current) = app.configs.get(&app.db, guild).await else {
        return;
    };
    if (current.version, current.durable_version) != token {
        return;
    }
    let Some(report) = detector.complete(job, &policy, failed, Instant::now()) else {
        return;
    };
    let vars = report_vars(report, author, channel, &policy);
    crate::logging::emit(app, ctx, GuildId::new(guild), "automod", vars).await;
}
fn report_vars(
    report: Report,
    author: u64,
    channel: u64,
    policy: &Policy,
) -> BTreeMap<String, String> {
    let (reason, content) = match report {
        Report::Verified(posts) => (
            format!(
                "Verified identical image bytes: at least {} common distinct images across {} channels in {} seconds",
                policy.min_attachments, policy.min_channels, policy.window_secs
            ),
            posts
                .iter()
                .map(|(c, m)| format!("channel {c}, message {m}"))
                .collect::<Vec<_>>()
                .join("; "),
        ),
        Report::Unverified => (
            "Unverified image candidate: download or work budget unavailable; no match established"
                .into(),
            String::new(),
        ),
    };
    BTreeMap::from([
        ("rule".into(), "Exact image spam test".into()),
        ("mention".into(), format!("<@{author}>")),
        ("channel".into(), format!("<#{}>", channel)),
        ("action".into(), "log only; no punishment".into()),
        ("reason".into(), reason),
        ("content".into(), content),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy() -> Policy {
        Policy {
            mode: "exact_image_test".into(),
            window_secs: 60,
            min_channels: 3,
            min_attachments: 2,
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
        }
    }
    fn admit(d: &Detector, id: u64, channel: u64, at: Instant) -> Option<Job> {
        d.admit(1, 2, post(id, channel, at, &[1, 2]), (0, 0), &policy(), at)
    }
    fn job(d: &Detector, at: Instant) -> Job {
        assert!(admit(d, 10, 100, at).is_none());
        assert!(admit(d, 11, 101, at).is_none());
        admit(d, 12, 102, at).unwrap()
    }
    #[test]
    fn invalidation_during_config_load_prevents_late_admission() {
        let now = Instant::now();
        let d = Detector::new(Limits::default()).unwrap();
        let mut p = post(1, 1, now, &[1, 2]);
        p.revision = d.revision();
        d.invalidate(1, 1, 1); // The create callback has not reached admission yet.
        assert!(d.admit(1, 2, p, (0, 0), &policy(), now).is_none());
        assert!(d.state.lock().index.is_empty());
    }

    #[test]
    fn exact_common_set_allows_extras_order_and_rejects_overlap_chain() {
        let now = Instant::now();
        let a = post(1, 1, now, &[1, 2]);
        let b = post(2, 2, now, &[2, 1, 3]);
        let c = post(3, 3, now, &[1, 2]);
        assert_eq!(
            common_images(&[&a, &b, &c], &policy(), 100_000).unwrap(),
            Some(vec![(1, 1), (2, 2), (3, 3)])
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
        assert!(
            d.admit(2, 2, post(3, 11, now, &[1, 2]), (0, 0), &policy(), now)
                .is_none()
        );
        assert!(
            d.admit(1, 3, post(4, 12, now, &[1, 2]), (0, 0), &policy(), now)
                .is_none()
        );
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
        assert_eq!(s.guilds[&1].authors[&2].retired_before, 12);
    }
    #[test]
    fn settings_change_drops_old_observations_and_old_job() {
        let now = Instant::now();
        let d = Detector::new(Limits::default()).unwrap();
        let j = job(&d, now);
        assert!(
            d.admit(1, 2, post(13, 103, now, &[1, 2]), (1, 1), &policy(), now)
                .is_none()
        );
        assert_eq!(d.complete(j, &policy(), false, now), None);
        assert!(
            d.state.lock().guilds[&1].authors[&2].posts[..3]
                .iter()
                .all(|p| !p.valid)
        );
    }
    #[test]
    fn reports_are_throttled_and_failures_never_claim_a_match() {
        let now = Instant::now();
        let d = Detector::new(Limits::default()).unwrap();
        let j = job(&d, now);
        assert_eq!(
            d.complete(j, &policy(), true, now),
            Some(Report::Unverified)
        );
        assert!(admit(&d, 13, 103, now).is_none());
        let vars = report_vars(Report::Unverified, 2, 100, &policy());
        assert!(vars["action"].contains("no punishment"));
        assert!(vars["content"].is_empty());
        assert!(vars["reason"].contains("no match established"));
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
        assert!(
            d.admit(2, 2, post(13, 103, now, &[1, 2]), (0, 0), &policy(), now)
                .is_none()
        );
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
        assert!(eligible(&rule, 1, 2, &[3]));
        rule.whitelist_roles.push(3);
        assert!(!eligible(&rule, 1, 2, &[3]));
        assert!(eligible(&rule, 1, 2, &[]));
        rule.whitelist_channels.push(2);
        assert!(!eligible(&rule, 1, 2, &[]));
        rule.whitelist_channels.clear();
        rule.whitelist_users.push(1);
        assert!(!eligible(&rule, 1, 2, &[]));
    }
}
