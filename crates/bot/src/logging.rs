//! Event routing: renders a template for an event type and posts it to the
//! configured guild-scoped log channel. Routine delivery never archives an event.

use serenity::builder::{CreateAllowedMentions, CreateAttachment, CreateMessage};
use serenity::client::Context;
use serenity::model::channel::MessageFlags;
use serenity::model::event::MessageUpdateEvent;
use serenity::model::guild::audit_log::{Action, AuditLogs, MemberAction};
use serenity::model::id::{ChannelId, GuildId, MessageId};
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::db::guild_data::MessageSnapshot;
use crate::state::BotState;
use crate::{db, templates};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

const SNIPPET_KEEP: Duration = Duration::from_secs(600);
const SNIPPETS_PER_GUILD: usize = 64;
const EDIT_REPORT_KEEP: Duration = Duration::from_secs(10);
const EDIT_REPORT_LIMIT: usize = 4096;
const MEMBER_JOINS_LIMIT: usize = 4096;
const MEMBER_JOINS_KEEP: Duration = Duration::from_secs(3600);
const MEMBER_AUDIT_KEEP: Duration = Duration::from_secs(30);
const MEMBER_AUDIT_LIMIT: usize = 4096;
const SHUTDOWN_NOTICE_PHASE: Duration = Duration::from_secs(3);

/// Display-only correlation state. No entry may explain two reports, and an
/// unresolved observation must not donate a delayed entry to a later action.
#[derive(Default)]
pub struct MemberAuditClaims {
    inner: Mutex<MemberAuditState>,
}

type MemberAuditKey = (u64, u64, u8);

#[derive(Default)]
struct MemberAuditState {
    claimed: HashMap<(u64, u64), Instant>,
    subjects: HashMap<MemberAuditKey, MemberAuditSubject>,
    next_generation: u64,
    saturated_until: Option<Instant>,
}

struct MemberAuditSubject {
    generation: u64,
    observed_ms: i64,
    uncertain_through: Option<i64>,
    matched: bool,
    at: Instant,
}

pub(crate) struct MemberAuditObservation {
    guild: u64,
    target: u64,
    action: Action,
    observed_ms: i64,
    generation: Option<u64>,
}

impl MemberAuditState {
    fn prune(&mut self) {
        self.claimed
            .retain(|_, at| at.elapsed() < MEMBER_AUDIT_KEEP);
        self.subjects
            .retain(|_, subject| subject.at.elapsed() < MEMBER_AUDIT_KEEP);
    }
}

impl MemberAuditClaims {
    pub(crate) fn observe_event(
        &self,
        guild: u64,
        target: u64,
        event: &str,
        observed_ms: i64,
    ) -> Option<MemberAuditObservation> {
        member_audit_action(event).map(|action| self.observe(guild, action, target, observed_ms))
    }

    fn observe(
        &self,
        guild: u64,
        action: Action,
        target: u64,
        observed_ms: i64,
    ) -> MemberAuditObservation {
        let mut state = self.inner.lock();
        state.prune();
        let key = (guild, target, action.num());
        let mut observation = MemberAuditObservation {
            guild,
            target,
            action,
            observed_ms,
            generation: None,
        };
        if state.subjects.len() >= MEMBER_AUDIT_LIMIT && !state.subjects.contains_key(&key) {
            // An untracked event must not later donate an apparently unique entry.
            state.saturated_until = Some(Instant::now() + MEMBER_AUDIT_KEEP);
            return observation;
        }
        let uncertain_through = state.subjects.get(&key).and_then(|previous| {
            let unresolved = (!previous.matched).then_some(previous.observed_ms + 3_000);
            previous.uncertain_through.max(unresolved)
        });
        state.next_generation = state.next_generation.wrapping_add(1);
        let generation = state.next_generation;
        state.subjects.insert(
            key,
            MemberAuditSubject {
                generation,
                observed_ms,
                uncertain_through,
                matched: false,
                at: Instant::now(),
            },
        );
        observation.generation = Some(generation);
        observation
    }

    fn claim(
        &self,
        observation: &MemberAuditObservation,
        logs: &AuditLogs,
    ) -> Option<BTreeMap<String, String>> {
        let mut state = self.inner.lock();
        state.prune();
        if state
            .saturated_until
            .is_some_and(|until| Instant::now() < until)
        {
            return None;
        }
        let generation = observation.generation?;
        let key = (
            observation.guild,
            observation.target,
            observation.action.num(),
        );
        let subject = state.subjects.get(&key)?;
        if subject.generation != generation || subject.matched {
            return None;
        }
        let uncertain_through = subject.uncertain_through;
        let (entry, vars) = member_audit_vars(
            logs,
            observation.action,
            observation.target,
            observation.observed_ms,
            |id| {
                state.claimed.contains_key(&(observation.guild, id))
                    || uncertain_through.is_some_and(|through| {
                        serenity::model::id::AuditLogEntryId::new(id)
                            .created_at()
                            .timestamp_millis()
                            <= through
                    })
            },
        )?;
        // Do not evict a live receipt and make an old entry reusable.
        if state.claimed.len() >= MEMBER_AUDIT_LIMIT {
            return None;
        }
        state
            .claimed
            .insert((observation.guild, entry), Instant::now());
        state.subjects.get_mut(&key)?.matched = true;
        Some(vars)
    }
}

/// Only join timestamps already supplied with recent messages, never a roster
/// or message archive. Serenity does not retain Message.member in its cache.
#[derive(Default)]
pub struct MemberJoinContext {
    inner: Mutex<HashMap<(u64, u64), (serenity::model::Timestamp, Instant)>>,
}

impl MemberJoinContext {
    pub fn remember(&self, guild: u64, user: u64, joined: Option<serenity::model::Timestamp>) {
        let Some(joined) = joined else { return };
        let mut entries = self.inner.lock();
        entries.retain(|_, (_, seen)| seen.elapsed() < MEMBER_JOINS_KEEP);
        if entries.len() >= MEMBER_JOINS_LIMIT
            && !entries.contains_key(&(guild, user))
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(key, _)| *key)
        {
            entries.remove(&oldest);
        }
        entries.insert((guild, user), (joined, Instant::now()));
    }

    pub fn take(&self, guild: u64, user: u64) -> Option<serenity::model::Timestamp> {
        self.inner
            .lock()
            .remove(&(guild, user))
            .filter(|(_, seen)| seen.elapsed() < MEMBER_JOINS_KEEP)
            .map(|(joined, _)| joined)
    }

    pub fn clear_guild(&self, guild: u64) {
        self.inner.lock().retain(|(id, _), _| *id != guild);
    }
}

pub(crate) fn member_join_vars(
    joined: Option<serenity::model::Timestamp>,
    observed: chrono::DateTime<chrono::Utc>,
) -> BTreeMap<String, String> {
    let (joined, duration) =
        match joined.filter(|time| time.unix_timestamp() <= observed.timestamp()) {
            Some(time) => {
                let seconds = observed.timestamp() - time.unix_timestamp();
                (time.to_string(), templates::format_duration(seconds as u64))
            }
            None => ("Unavailable".into(), "Unavailable".into()),
        };
    BTreeMap::from([
        ("joined".into(), joined),
        ("duration".into(), duration),
        ("time".into(), observed.to_rfc3339()),
    ])
}

/// A committed stay is preferred to the short-lived member cache for a leave
/// report. Unknown imported join dates remain visibly unavailable.
pub(crate) fn member_join_vars_ms(
    joined_at_ms: Option<i64>,
    observed: chrono::DateTime<chrono::Utc>,
) -> BTreeMap<String, String> {
    let valid = joined_at_ms
        .filter(|joined| *joined > 0 && *joined <= observed.timestamp_millis())
        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis);
    let (joined, duration) = match valid {
        Some(joined) => (
            joined.to_rfc3339(),
            templates::format_duration((observed - joined).num_seconds().max(0) as u64),
        ),
        None => ("Unavailable".into(), "Unavailable".into()),
    };
    BTreeMap::from([
        ("joined".into(), joined),
        ("duration".into(), duration),
        ("time".into(), observed.to_rfc3339()),
    ])
}
type EditKey = (u64, u64, u64);
struct EditObservation {
    edited_at_ms: Option<i64>,
    reported_timestamps: Vec<i64>,
    pending_without_timestamp: Option<(Option<u64>, Option<u64>)>,
    content: Option<u64>,
    attachments: Option<u64>,
    at: Instant,
}

#[derive(Default)]
pub struct EditReportDedup {
    inner: Mutex<HashMap<EditKey, EditObservation>>,
}

impl EditReportDedup {
    /// A Gateway update may repeat the same content/attachment observation.
    /// Keep only a bounded fingerprint, never a second copy of message text.
    pub fn should_report(&self, event: &MessageUpdateEvent) -> bool {
        let Some(guild) = event.guild_id else {
            return false;
        };
        // Discord sets edited_timestamp only for an actual edit. Embed, GIF/link
        // unfurl, reply-preview, pin, and flag refreshes omit it. Do not store
        // those as a reported observation, or a later timestamped edit of the
        // same text would be coalesced away.
        let Some(edited_at) = event.edited_timestamp else {
            return false;
        };
        let content = event.content.as_ref().map(fingerprint);
        let edited_at_ms = Some(edited_at.timestamp_millis());
        let attachments = event.attachments.as_ref().map(|items| {
            fingerprint(
                &items
                    .iter()
                    .map(|a| (a.id.get(), a.filename.as_str(), a.description.as_deref()))
                    .collect::<Vec<_>>(),
            )
        });
        let key = (guild.get(), event.channel_id.get(), event.id.get());
        let mut entries = self.inner.lock();
        entries.retain(|_, seen| seen.at.elapsed() < EDIT_REPORT_KEEP);
        if let Some(seen) = entries.get_mut(&key) {
            if let (Some(previous), Some(current)) = (seen.edited_at_ms, edited_at_ms) {
                if previous != current && seen.reported_timestamps.contains(&current) {
                    seen.at = Instant::now();
                    return false;
                }
                let distinct_edit = previous != current;
                if !distinct_edit && seen.pending_without_timestamp.is_some() {
                    // This is an older timestamp replay while a newer edit has
                    // only been observed without its timestamp. Keep the newer
                    // field fingerprints until its timestamp arrives.
                    seen.at = Instant::now();
                    return false;
                }
                if distinct_edit
                    && let Some((pending_content, pending_attachments)) =
                        seen.pending_without_timestamp
                    && content.is_none_or(|value| pending_content.is_none_or(|old| old == value))
                    && attachments
                        .is_none_or(|value| pending_attachments.is_none_or(|old| old == value))
                {
                    // The changed fields were already reported without a
                    // timestamp. This update supplies their edit identity.
                    seen.edited_at_ms = Some(current);
                    remember_edit_timestamp(seen, current);
                    seen.pending_without_timestamp = None;
                    if content.is_some() {
                        seen.content = content;
                    }
                    if attachments.is_some() {
                        seen.attachments = attachments;
                    }
                    seen.at = Instant::now();
                    return false;
                }
                // Discord's supplied edit timestamp groups partial updates.
                // A new timestamp starts a new observation even when its first
                // payload field was absent from the preceding edit.
                seen.edited_at_ms = edited_at_ms;
                if distinct_edit {
                    remember_edit_timestamp(seen, current);
                    seen.pending_without_timestamp = None;
                    seen.content = content;
                    seen.attachments = attachments;
                } else {
                    if content.is_some() {
                        seen.content = content;
                    }
                    if attachments.is_some() {
                        seen.attachments = attachments;
                    }
                }
                seen.at = Instant::now();
                return distinct_edit;
            }
            // Gateway updates may supply only one field at a time. A newly
            // supplied field completes this observation; it is not a new edit.
            let changed_content =
                content.is_some_and(|value| seen.content.is_some_and(|old| old != value));
            let changed_attachments =
                attachments.is_some_and(|value| seen.attachments.is_some_and(|old| old != value));
            let changed = changed_content || changed_attachments;
            let continuation = if edited_at_ms.is_none()
                && let Some((pending_content, pending_attachments)) =
                    seen.pending_without_timestamp.as_mut()
            {
                let continuation = if changed_content
                    && !changed_attachments
                    && pending_content.is_none()
                {
                    *pending_content = content;
                    true
                } else if changed_attachments && !changed_content && pending_attachments.is_none() {
                    *pending_attachments = attachments;
                    true
                } else {
                    false
                };
                // A field first supplied by a partial update is part of this
                // edit even when there was no earlier value to compare with.
                if pending_content.is_none() {
                    *pending_content = content;
                }
                if pending_attachments.is_none() {
                    *pending_attachments = attachments;
                }
                continuation
            } else {
                false
            };
            if changed && !continuation {
                seen.pending_without_timestamp =
                    edited_at_ms.is_none().then_some((content, attachments));
            }
            if content.is_some() {
                seen.content = content;
            }
            if attachments.is_some() {
                seen.attachments = attachments;
            }
            if edited_at_ms.is_some() {
                seen.edited_at_ms = edited_at_ms;
                seen.pending_without_timestamp = None;
                if let Some(current) = edited_at_ms
                    && !seen.reported_timestamps.contains(&current)
                {
                    remember_edit_timestamp(seen, current);
                }
            }
            seen.at = Instant::now();
            return changed && !continuation;
        }
        if entries.len() >= EDIT_REPORT_LIMIT
            && !entries.contains_key(&key)
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, seen)| seen.at)
                .map(|(key, _)| *key)
        {
            entries.remove(&oldest);
        }
        entries.insert(
            key,
            EditObservation {
                edited_at_ms,
                reported_timestamps: edited_at_ms.into_iter().collect(),
                pending_without_timestamp: edited_at_ms.is_none().then_some((content, attachments)),
                content,
                attachments,
                at: Instant::now(),
            },
        );
        true
    }
}

fn remember_edit_timestamp(seen: &mut EditObservation, timestamp: i64) {
    if seen.reported_timestamps.len() >= 64 {
        seen.reported_timestamps.remove(0);
    }
    seen.reported_timestamps.push(timestamp);
}

fn fingerprint<T: Hash>(value: &T) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

#[derive(Default)]
pub struct SnippetCache {
    inner: Mutex<HashMap<(u64, u64, u64), Snippet>>,
}
struct Snippet {
    at: Instant,
    guild: u64,
    author: u64,
    webhook: bool,
}

pub fn remember_message(
    app: &BotState,
    guild: u64,
    channel: u64,
    message: u64,
    author: Option<u64>,
    text: Option<&str>,
    attachments: Option<&[String]>,
) {
    remember_message_create(
        app,
        guild,
        channel,
        message,
        author,
        text,
        attachments,
        false,
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn remember_message_create(
    app: &BotState,
    guild: u64,
    channel: u64,
    message: u64,
    author: Option<u64>,
    _text: Option<&str>,
    _attachments: Option<&[String]>,
    webhook: bool,
) {
    let mut cache = app.log_snippets.inner.lock();
    cache.retain(|_, s| s.at.elapsed() < SNIPPET_KEEP);
    let key = (guild, channel, message);
    let Some(author) = author
        .filter(|id| *id != 0)
        .or_else(|| cache.get(&key).map(|s| s.author))
    else {
        return;
    };
    if cache.values().filter(|s| s.guild == guild).count() >= SNIPPETS_PER_GUILD
        && !cache.contains_key(&key)
        && let Some(oldest) = cache
            .iter()
            .filter(|(_, s)| s.guild == guild)
            .min_by_key(|(_, s)| s.at)
            .map(|(key, _)| *key)
    {
        cache.remove(&oldest);
    }
    let webhook = webhook || cache.get(&key).is_some_and(|old| old.webhook);
    cache.insert(
        key,
        Snippet {
            at: Instant::now(),
            guild,
            author,
            webhook,
        },
    );
}

/// Keep only identity for filtering; snapshot storage owns report content.
pub(crate) fn remember_message_update(
    app: &BotState,
    event: &MessageUpdateEvent,
    author: Option<u64>,
) {
    let Some(guild) = event.guild_id else {
        return;
    };
    if event.content.is_none() && event.attachments.is_none() {
        return;
    }
    remember_message_create(
        app,
        guild.get(),
        event.channel_id.get(),
        event.id.get(),
        author,
        None,
        None,
        matches!(event.webhook_id, Some(Some(_))),
    );
}

pub(crate) fn known_message_webhook(
    app: &BotState,
    guild: u64,
    channel: u64,
    message: u64,
) -> bool {
    let mut cache = app.log_snippets.inner.lock();
    cache.retain(|_, s| s.at.elapsed() < SNIPPET_KEEP);
    cache
        .get(&(guild, channel, message))
        .is_some_and(|snippet| snippet.webhook)
}

pub(crate) fn known_message_author(
    app: &BotState,
    guild: u64,
    channel: u64,
    message: u64,
) -> Option<u64> {
    app.log_snippets
        .inner
        .lock()
        .get(&(guild, channel, message))
        .filter(|snippet| snippet.at.elapsed() < SNIPPET_KEEP)
        .map(|snippet| snippet.author)
}

fn take_snippet(app: &BotState, guild: u64, channel: u64, message: u64) -> Option<Snippet> {
    let mut cache = app.log_snippets.inner.lock();
    cache.retain(|_, s| s.at.elapsed() < SNIPPET_KEEP);
    cache.remove(&(guild, channel, message))
}

/// Resolves the log channel for an event type from the cached config.
fn channel_for(
    cfg: &crate::state::CachedGuildConfig,
    event: &str,
) -> Option<serenity::model::id::ChannelId> {
    cfg.log_channels.get(event).copied()
}

/// Resolve one observed ban/unban against Discord's bounded recent audit window.
/// The claim is shared by persisted gateway-ban details and the Discord report.
pub(crate) async fn lookup_member_audit(
    app: &BotState,
    sink: &impl LogDelivery,
    guild_id: GuildId,
    observation: &MemberAuditObservation,
) -> Option<BTreeMap<String, String>> {
    let budget = Duration::from_secs(app.cfg.log.discord_timeout_secs)
        .div_f64(2.0)
        .min(Duration::from_secs(3));
    tokio::time::timeout(budget, async {
        for attempt in 0..3 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            let logs = sink.member_audit(guild_id, observation.action).await.ok()?;
            if let Some(context) = app.member_audits.claim(observation, &logs) {
                return Some(context);
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

/// Sends one best-effort Discord log report. No message history or retry queue.
pub async fn emit(
    app: &Arc<BotState>,
    ctx: &Context,
    guild_id: GuildId,
    event: &str,
    vars: BTreeMap<String, String>,
) {
    emit_to(app, ctx.http.as_ref(), guild_id, event, vars).await;
}

/// Sends a lifecycle notice through the configured system log channel.
pub async fn emit_system_notice(
    app: &Arc<BotState>,
    http: &serenity::http::Http,
    guild_id: GuildId,
    content: String,
) {
    emit_system_notice_to(app, http, guild_id, content, Admission::Immediate).await;
}

/// Attempts one notice per cached guild within the process shutdown budget.
pub async fn emit_shutdown_notices(
    app: &Arc<BotState>,
    http: Arc<serenity::http::Http>,
    guild_ids: Vec<GuildId>,
    content: String,
) -> usize {
    emit_shutdown_notices_to(app, http, guild_ids, content, SHUTDOWN_NOTICE_PHASE).await
}

async fn emit_shutdown_notices_to<S: LogDelivery + Send + Sync + 'static>(
    app: &Arc<BotState>,
    sink: Arc<S>,
    guild_ids: Vec<GuildId>,
    content: String,
    budget: Duration,
) -> usize {
    let deadline = tokio::time::Instant::now() + budget;
    let mut notices = tokio::task::JoinSet::new();
    for guild_id in guild_ids {
        let app = Arc::clone(app);
        let sink = Arc::clone(&sink);
        let content = content.clone();
        notices.spawn(async move {
            emit_system_notice_to(
                &app,
                sink.as_ref(),
                guild_id,
                content,
                Admission::WaitForShutdown,
            )
            .await
        });
    }
    let mut sent = 0;
    if tokio::time::timeout_at(deadline, async {
        while let Some(result) = notices.join_next().await {
            match result {
                Ok(Delivery::Sent) => sent += 1,
                Err(error) => tracing::warn!("shutdown notice task failed: {error}"),
                _ => {}
            }
        }
    })
    .await
    .is_err()
    {
        tracing::warn!(pending = notices.len(), "shutdown notice phase timed out");
        notices.abort_all();
    }
    sent
}

async fn emit_system_notice_to(
    app: &Arc<BotState>,
    sink: &impl LogDelivery,
    guild_id: GuildId,
    content: String,
    admission: Admission,
) -> Delivery {
    emit_to_observed(
        app,
        sink,
        guild_id,
        "system",
        BTreeMap::from([("content".into(), content)]),
        None,
        admission,
    )
    .await
}

pub fn shutdown_notice_content(uptime_seconds: u64) -> String {
    format!(
        "Bot shutting down (v{}). Uptime: {}",
        env!("CARGO_PKG_VERSION"),
        templates::format_duration(uptime_seconds)
    )
}

#[serenity::async_trait]
pub(crate) trait LogDelivery: Sync {
    async fn send(&self, channel: ChannelId, message: CreateMessage) -> anyhow::Result<()>;
    async fn member_audit(&self, _guild: GuildId, _action: Action) -> anyhow::Result<AuditLogs> {
        anyhow::bail!("audit context unavailable")
    }
}
#[serenity::async_trait]
impl LogDelivery for serenity::http::Http {
    async fn member_audit(&self, guild: GuildId, action: Action) -> anyhow::Result<AuditLogs> {
        Ok(self
            .get_audit_logs(guild, Some(action), None, None, Some(100))
            .await?)
    }
    async fn send(&self, channel: ChannelId, message: CreateMessage) -> anyhow::Result<()> {
        channel.send_message(self, message).await?;
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Delivery {
    Sent,
    Unconfigured,
    Dropped,
    Failed,
    /// Skipped because this guild/channel was recently rejected with a permanent error.
    Blocked,
}

#[derive(Clone, Copy)]
enum Admission {
    Immediate,
    WaitForShutdown,
}

pub(crate) async fn emit_to(
    app: &Arc<BotState>,
    sink: &impl LogDelivery,
    guild_id: GuildId,
    event: &str,
    vars: BTreeMap<String, String>,
) -> Delivery {
    emit_to_observed(app, sink, guild_id, event, vars, None, Admission::Immediate).await
}

async fn emit_to_observed(
    app: &Arc<BotState>,
    sink: &impl LogDelivery,
    guild_id: GuildId,
    event: &str,
    vars: BTreeMap<String, String>,
    observation: Option<MemberAuditObservation>,
    admission: Admission,
) -> Delivery {
    emit_to_observed_with_audit(app, sink, guild_id, event, vars, observation, admission)
        .await
        .0
}

async fn emit_to_observed_with_audit(
    app: &Arc<BotState>,
    sink: &impl LogDelivery,
    guild_id: GuildId,
    event: &str,
    mut vars: BTreeMap<String, String>,
    observation: Option<MemberAuditObservation>,
    admission: Admission,
) -> (Delivery, Option<BTreeMap<String, String>>) {
    // Gateway callers register before their DB awaits. Direct callers register
    // here, before admission/config/filter exits can discard the observation.
    let observation = observation.or_else(|| {
        let target = vars.get("user_id")?.parse::<u64>().ok()?;
        let observed = vars
            .get("time")
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|time| time.timestamp_millis())
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        app.member_audits
            .observe_event(guild_id.get(), target, event, observed)
    });
    let deadline = Duration::from_secs(app.cfg.log.discord_timeout_secs);
    let started = Instant::now();
    let _permit = match admission {
        Admission::Immediate => {
            // Routine and startup reports never queue raw event content.
            let Ok(permit) = app.log_delivery_slots.try_acquire() else {
                record_delivery_failure(app);
                return (Delivery::Dropped, None);
            };
            permit
        }
        Admission::WaitForShutdown => {
            // A stop notice gets the same total deadline for admission and
            // delivery. It does not retry a failed Discord request.
            let Ok(Ok(permit)) =
                tokio::time::timeout(deadline, app.log_delivery_slots.acquire()).await
            else {
                record_delivery_failure(app);
                return (Delivery::Failed, None);
            };
            permit
        }
    };
    let mut matched_audit = None;
    let result = tokio::time::timeout(deadline.saturating_sub(started.elapsed()), async {
        let cfg = app.configs.get(&app.db, guild_id.get()).await?;
        let Some(channel_id) = channel_for(&cfg, event) else {
            return Ok(Delivery::Unconfigured);
        };
        if cfg
            .log_filters
            .get(event)
            .is_some_and(|filter| excluded(filter, &vars))
        {
            return Ok(Delivery::Unconfigured);
        }
        if member_audit_action(event).is_some() {
            // Lookup is part of the admitted report, never an unbounded
            // Gateway task or a history-recovery queue. Reserve delivery time.
            let already_attributed = vars.contains_key("actor_id");
            vars.entry("actor".into())
                .or_insert_with(|| "unavailable".into());
            vars.entry("actor_id".into())
                .or_insert_with(|| "unavailable".into());
            vars.entry("reason".into())
                .or_insert_with(|| "unavailable".into());
            if !already_attributed
                && let Some(observation) = observation.as_ref()
                && let Some(context) = lookup_member_audit(app, sink, guild_id, observation).await
            {
                matched_audit = Some(context.clone());
                vars.extend(context);
            }
            // Moderator-ID exclusions can only be evaluated after attribution.
            if cfg
                .log_filters
                .get(event)
                .is_some_and(|filter| excluded(filter, &vars))
            {
                return Ok(Delivery::Unconfigured);
            }
        }
        vars.insert("event".into(), event.to_owned());
        vars.entry("time".into())
            .or_insert_with(|| chrono::Utc::now().to_rfc3339());
        vars.insert("guild".into(), guild_id.get().to_string());
        let template = cfg
            .templates
            .get(event)
            .map(String::as_str)
            .unwrap_or_else(|| default_template(event));
        templates::format_log_timestamps(&mut vars);
        let rendered = render_log_template(event, template, &mut vars);
        if !app.activation.may_record() {
            return Ok(Delivery::Dropped);
        }
        let rest_target = crate::discord::DiscordRestTarget::GuildChannel {
            guild: guild_id.get(),
            channel: channel_id.get(),
        };
        if app
            .remembered_discord_targets
            .blocked(rest_target, Instant::now())
        {
            return Ok(Delivery::Blocked);
        }
        let recovery = app
            .remembered_discord_targets
            .capture_recovery_epoch(rest_target);
        match crate::discord::race_discord_call(
            &app.activation,
            sink.send(channel_id, log_message(&rendered)),
        )
        .await
        {
            crate::discord::DiscordCall::Cancelled => return Ok(Delivery::Dropped),
            crate::discord::DiscordCall::Finished(Ok(())) => {
                app.remembered_discord_targets.forget(rest_target);
            }
            crate::discord::DiscordCall::Finished(Err(error)) => {
                app.remembered_discord_targets.observe_anyhow_error(
                    recovery,
                    &error,
                    rest_target,
                    Instant::now(),
                );
                return Err(error);
            }
        }
        Ok::<_, anyhow::Error>(Delivery::Sent)
    })
    .await;
    let delivery = match result {
        Ok(Ok(delivery)) => delivery,
        Ok(Err(_)) | Err(_) => {
            record_delivery_failure(app);
            Delivery::Failed
        }
    };
    (delivery, matched_audit)
}

fn member_audit_action(event: &str) -> Option<Action> {
    match event {
        "member_ban" => Some(Action::Member(MemberAction::BanAdd)),
        "member_unban" => Some(Action::Member(MemberAction::BanRemove)),
        _ => None,
    }
}

/// Correlate only this target/action near the observed Gateway event. An
/// ambiguous response is unavailable; this is display context, not case identity.
fn member_audit_vars(
    logs: &AuditLogs,
    action: Action,
    target: u64,
    observed_ms: i64,
    already_claimed: impl Fn(u64) -> bool,
) -> Option<(u64, BTreeMap<String, String>)> {
    let mut matches = logs.entries.iter().filter(|entry| {
        let delta = entry.id.created_at().timestamp_millis() - observed_ms;
        entry.action.num() == action.num()
            && entry.target_id.is_some_and(|id| id.get() == target)
            && (-10_000..=3_000).contains(&delta)
            && !already_claimed(entry.id.get())
    });
    let entry = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    let actor_name = logs.users.get(&entry.user_id).map(|user| user.name.clone());
    let actor = actor_name
        .as_ref()
        .map(|name| format!("{} (<@{}>)", name, entry.user_id))
        .unwrap_or_else(|| format!("<@{}>", entry.user_id));
    let mut vars = BTreeMap::from([
        ("actor".into(), actor),
        ("actor_id".into(), entry.user_id.to_string()),
        (
            "reason".into(),
            entry
                .reason
                .as_deref()
                .map(jmodguard::ban_reason::without_tag)
                .map(str::to_owned)
                .filter(|reason| !reason.trim().is_empty())
                .unwrap_or_else(|| "No reason provided".into()),
        ),
    ]);
    if let Some(name) = actor_name {
        vars.insert("actor_name".into(), name);
    }
    Some((entry.id.get(), vars))
}

fn excluded(filter: &db::LogFilter, vars: &BTreeMap<String, String>) -> bool {
    let user_match = ["user_id", "actor_id", "target_id"]
        .iter()
        .filter_map(|key| vars.get(*key))
        .filter_map(|value| value.parse::<u64>().ok())
        .filter(|id| *id != 0)
        .any(|id| {
            filter
                .users
                .iter()
                .any(|ignored| ignored == &id.to_string())
        });
    let source = vars
        .get("source_channel_id")
        .or_else(|| vars.get("channel_id"))
        .map(String::as_str)
        .or_else(|| {
            vars.get("channel")
                .and_then(|value| value.strip_prefix("<#")?.strip_suffix('>'))
        });
    user_match
        || channel_id_excluded(filter, source)
        || channel_id_excluded(filter, vars.get("before_channel_id").map(String::as_str))
}

fn channel_id_excluded(filter: &db::LogFilter, raw: Option<&str>) -> bool {
    raw.and_then(|source| source.parse::<u64>().ok())
        .filter(|id| *id != 0)
        .is_some_and(|source| filter.channels.iter().any(|id| id == &source.to_string()))
}

pub(crate) fn record_delivery_failure(app: &BotState) {
    let count = app
        .log_delivery_failures
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .wrapping_add(1);
    // Sparse diagnostics, without message bodies or one log entry per raid event.
    if count.is_power_of_two() {
        tracing::warn!(
            count,
            "Discord log reports dropped or failed; inspect log channel configuration and delivery capacity"
        );
    }
}

fn log_message(content: &str) -> CreateMessage {
    CreateMessage::new()
        .content(clip(content, 2000))
        .allowed_mentions(
            CreateAllowedMentions::new()
                .all_users(false)
                .all_roles(false)
                .everyone(false)
                .replied_user(false),
        )
        .flags(MessageFlags::SUPPRESS_EMBEDS)
}

/// Bound UTF-16 units too, so astral characters cannot exceed Discord's limit.
fn clip(text: &str, limit: usize) -> String {
    let mut out = String::new();
    let mut units = 0;
    for ch in text.chars() {
        if units + ch.len_utf16() > limit.saturating_sub(1) {
            out.push('…');
            break;
        }
        out.push(ch);
        units += ch.len_utf16();
    }
    out
}

#[derive(Clone)]
pub(crate) struct MessageReport {
    pub guild_id: GuildId,
    pub event: &'static str,
    pub vars: BTreeMap<String, String>,
}

fn message_vars(guild: GuildId, channel: ChannelId, id: MessageId) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("channel".into(), format!("<#{channel}>")),
        ("channel_id".into(), channel.to_string()),
        ("message_id".into(), id.to_string()),
        (
            "message_url".into(),
            format!("https://discord.com/channels/{guild}/{channel}/{id}"),
        ),
        ("mention".into(), "unavailable".into()),
        ("user".into(), "unavailable".into()),
        ("user_id".into(), "unavailable".into()),
        ("before".into(), "unavailable (not captured)".into()),
        ("after".into(), "not supplied in this update".into()),
        ("content".into(), "unavailable (not captured)".into()),
        ("attachments".into(), "unavailable (not captured)".into()),
        ("coverage".into(), "retained observation unavailable".into()),
    ])
}

fn retained_content(row: &MessageSnapshot) -> String {
    retained_content_with_limit(row, 1000)
}

fn retained_content_with_limit(row: &MessageSnapshot, limit: usize) -> String {
    match row.fields.content.as_deref() {
        Some("") => "empty".into(),
        Some(text) => {
            let mut shown = clip(text, limit);
            if shown != text {
                shown.push_str(" [display clipped]");
            }
            if row.fields.content_truncated {
                shown.push_str(" [retained text truncated]");
            }
            shown
        }
        None => "not supplied to capture".into(),
    }
}

fn retained_attachments(row: &MessageSnapshot) -> String {
    retained_attachments_with_limit(row, 1200)
}

fn retained_attachments_with_limit(row: &MessageSnapshot, limit: usize) -> String {
    match row.fields.attachments.as_ref() {
        None => "not supplied to capture".into(),
        Some(items) if items.is_empty() && !row.fields.attachments_truncated => "none".into(),
        Some(items) => {
            let mut shown = String::new();
            if row.fields.attachments_truncated {
                let omitted = row.fields.attachments_omitted.unwrap_or(0);
                if omitted > 0 {
                    shown.push_str(&format!(
                        "[attachment capture truncated; {omitted} attachment(s) omitted]\n"
                    ));
                } else {
                    shown.push_str("[attachment metadata truncated during capture]\n");
                }
            }
            for item in items {
                let next = format!("{} ({})\n", item.filename, item.url);
                if shown.len() + next.len() > limit {
                    shown.push_str("[display clipped]");
                    break;
                }
                shown.push_str(&next);
            }
            shown
        }
    }
}

fn bound_default_value(vars: &mut BTreeMap<String, String>, name: &str, units: usize) {
    let Some(value) = vars.get_mut(name) else {
        return;
    };
    if value.encode_utf16().count() <= units {
        return;
    }
    let capture_truncated = value.contains("[retained text truncated]");
    let marker = if capture_truncated {
        " [display clipped; retained text truncated]"
    } else {
        " [display clipped]"
    };
    *value = format!(
        "{}{}",
        clip(value, units.saturating_sub(marker.len())),
        marker
    );
}

fn bound_default_message_vars(event: &str, vars: &mut BTreeMap<String, String>) {
    match event {
        "message_edit" => {
            bound_default_value(vars, "before", 600);
            bound_default_value(vars, "after", 600);
            bound_default_value(vars, "attachments", 240);
        }
        "message_delete" => {
            bound_default_value(vars, "content", 900);
            bound_default_value(vars, "attachments", 400);
        }
        _ => {}
    }
}

const AUTOMOD_SHA256_PREFIX: &str = "sha256 ";

fn split_automod_proving_content(content: &str) -> (Option<String>, String) {
    if let Some(rest) = content.strip_prefix(AUTOMOD_SHA256_PREFIX) {
        let hash_value: String = rest
            .chars()
            .take_while(|ch| ch.is_ascii_hexdigit() || *ch == ',')
            .collect();
        let mut tail = rest[hash_value.len()..].trim_start();
        if let Some(stripped) = tail.strip_prefix(';') {
            tail = stripped.trim_start();
        }
        return (
            Some(format!("{AUTOMOD_SHA256_PREFIX}{hash_value}")),
            tail.to_string(),
        );
    }
    if let Some(idx) = content.rfind("; sha256 ") {
        let hashes = content[idx + 2..].trim().to_string();
        let head = content[..idx].trim().to_string();
        return (Some(hashes), head);
    }
    (None, content.to_string())
}

/// Discord log body after the same bounds delivery applies.
/// Automod `{content}` is bounded for every template, including a guild override.
pub(crate) fn render_log_template(
    event: &str,
    template: &str,
    vars: &mut BTreeMap<String, String>,
) -> String {
    if template == default_template(event) {
        bound_default_message_vars(event, vars);
    }
    if event == "automod" {
        bound_automod_content_var(template, vars, 2000);
    }
    templates::render_bounded(template, vars, 2000)
}

fn substituted_key_count(template: &str, key: &str) -> usize {
    template
        .split_inclusive('}')
        .filter(|chunk| {
            chunk.ends_with('}')
                && chunk
                    .rfind('{')
                    .is_some_and(|start| &chunk[start + 1..chunk.len() - 1] == key)
        })
        .count()
}

fn is_sha256_digest(digest: &str) -> bool {
    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Keep a prefix of complete digests. When some are omitted, say how many.
/// A budget that cannot hold one digest still reports the full omitted count
/// when that phrase fits, and never returns a cut digest.
fn bound_proving_hash_list(hashes: &str, budget: usize) -> String {
    if hashes.encode_utf16().count() <= budget {
        return hashes.to_string();
    }
    let Some(body) = hashes.strip_prefix(AUTOMOD_SHA256_PREFIX) else {
        return String::new();
    };
    if body.is_empty() {
        return String::new();
    }
    let digests: Vec<&str> = body.split(',').collect();
    if !digests.iter().copied().all(is_sha256_digest) {
        return String::new();
    }
    let total = digests.len();
    for shown in (0..total).rev() {
        let omitted = total - shown;
        let text = if shown == 0 {
            format!("and {omitted} more")
        } else {
            format!(
                "{AUTOMOD_SHA256_PREFIX}{} and {omitted} more",
                digests[..shown].join(",")
            )
        };
        if text.encode_utf16().count() <= budget {
            return text;
        }
    }
    String::new()
}

/// Clip a non-hash tail. Keep the display marker only when the marker and
/// `clip`'s ellipsis both fit; otherwise return a clip that fits, or nothing
/// when even the ellipsis would exceed the budget (a zero limit).
fn bound_plain_automod_tail(tail: &str, budget: usize) -> String {
    if tail.encode_utf16().count() <= budget {
        return tail.to_string();
    }
    let marker = " [display clipped]";
    let with_marker = format!(
        "{}{marker}",
        clip(tail, budget.saturating_sub(marker.encode_utf16().count()))
    );
    if with_marker.encode_utf16().count() <= budget {
        return with_marker;
    }
    let clipped = clip(tail, budget);
    if clipped.encode_utf16().count() <= budget {
        clipped
    } else {
        String::new()
    }
}

pub(crate) fn bound_automod_content_var(
    template: &str,
    vars: &mut BTreeMap<String, String>,
    max_units: usize,
) {
    let Some(content) = vars.remove("content") else {
        return;
    };
    if content.is_empty() {
        vars.insert("content".into(), content);
        return;
    }
    let copies = substituted_key_count(template, "content");
    if copies == 0 {
        vars.insert("content".into(), content);
        return;
    }
    let budget = templates::substitution_budget(template, vars, "content", max_units) / copies;
    let bounded = match split_automod_proving_content(&content) {
        (Some(hashes), tail) if tail.is_empty() => bound_proving_hash_list(&hashes, budget),
        (Some(hashes), tail) => {
            let hash_units = hashes.encode_utf16().count();
            let marker = " [display clipped]";
            let marker_units = marker.encode_utf16().count();
            let sep = "; ";
            let sep_units = sep.encode_utf16().count();
            if hash_units.saturating_add(sep_units) >= budget {
                bound_proving_hash_list(&hashes, budget)
            } else if tail.encode_utf16().count() <= budget.saturating_sub(hash_units + sep_units) {
                format!("{hashes}{sep}{tail}")
            } else {
                // `clip` still emits an ellipsis when the tail budget is zero, and the
                // marker is longer than that remainder. Drop the tail instead of
                // returning a string `render_bounded` would have to cut through a digest.
                let tail_budget = budget.saturating_sub(hash_units + sep_units + marker_units);
                let clipped = format!("{hashes}{sep}{}{marker}", clip(&tail, tail_budget));
                if clipped.encode_utf16().count() <= budget {
                    clipped
                } else {
                    bound_proving_hash_list(&hashes, budget)
                }
            }
        }
        (None, tail) => bound_plain_automod_tail(&tail, budget),
    };
    vars.insert("content".into(), bounded);
}

pub(crate) fn apply_retained_edit(
    report: &mut MessageReport,
    before: Option<&MessageSnapshot>,
    after: Option<&MessageSnapshot>,
) {
    if let Some(row) = before {
        report.vars.insert("before".into(), retained_content(row));
        report.vars.insert(
            "coverage".into(),
            if row.continuous {
                "last observed; Gateway order may be incomplete".into()
            } else {
                "last observed; observation gap or restart".into()
            },
        );
    } else {
        report
            .vars
            .insert("before".into(), "unavailable (not captured)".into());
    }
    if let Some(row) = after {
        report.vars.insert("after".into(), retained_content(row));
        report
            .vars
            .insert("attachments".into(), retained_attachments(row));
    }
    if let Some(id) = after
        .and_then(|row| row.fields.author_id)
        .or_else(|| before.and_then(|row| row.fields.author_id))
    {
        report.vars.insert("user_id".into(), id.to_string());
        report.vars.insert("mention".into(), format!("<@{id}>"));
    }
}

pub(crate) fn apply_retained_delete(report: &mut MessageReport, row: Option<&MessageSnapshot>) {
    if let Some(row) = row {
        report.vars.insert("content".into(), retained_content(row));
        report
            .vars
            .insert("attachments".into(), retained_attachments(row));
        report.vars.insert(
            "coverage".into(),
            if row.continuous {
                "last observed; later edits may be missing".into()
            } else {
                "last observed; observation gap or restart".into()
            },
        );
        if let Some(id) = row.fields.author_id {
            report.vars.insert("user_id".into(), id.to_string());
            report.vars.insert("mention".into(), format!("<@{id}>"));
        }
    } else {
        report
            .vars
            .insert("content".into(), "unavailable (not captured)".into());
        report
            .vars
            .insert("attachments".into(), "unavailable (not captured)".into());
        report
            .vars
            .insert("coverage".into(), "retained snapshot unavailable".into());
    }
}

pub(crate) struct BulkTranscript {
    pub total: usize,
    pub retained: usize,
    pub unavailable: usize,
    pub unknown: usize,
    pub omitted: usize,
    pub filtered: usize,
    included_authors: HashSet<i64>,
    bytes: usize,
    entries: Vec<String>,
}

impl BulkTranscript {
    pub fn new(total: usize) -> Self {
        Self {
            total,
            retained: 0,
            unavailable: 0,
            unknown: 0,
            omitted: 0,
            filtered: 0,
            included_authors: HashSet::new(),
            bytes: "jModGuard last-observed deleted messages\n\n".len(),
            entries: Vec::new(),
        }
    }

    pub fn push(&mut self, id: MessageId, row: Option<&MessageSnapshot>) {
        let line = if let Some(row) = row {
            self.retained += 1;
            if let Some(author) = row.fields.author_id {
                self.included_authors.insert(author);
            }
            format!(
                "Message {} | author {} | {}\nText: {}\nAttachments: {}\n\n",
                id,
                row.fields
                    .author_id
                    .map_or_else(|| "unavailable".into(), |id| id.to_string()),
                if row.continuous {
                    "last observed; later edits may be missing"
                } else {
                    "observation gap"
                },
                retained_content_with_limit(row, 16_000),
                retained_attachments_with_limit(row, 4000)
            )
        } else {
            self.unavailable += 1;
            format!("Message {} | retained content unavailable\n\n", id)
        };
        self.push_line(line);
    }

    pub fn push_unknown(&mut self, id: MessageId) {
        self.unknown += 1;
        self.push_line(format!("Message {} | snapshot storage unavailable\n\n", id));
    }

    fn push_line(&mut self, line: String) {
        if self.bytes.saturating_add(line.len()).saturating_add(200) <= 256 * 1024 {
            self.bytes += line.len();
            self.entries.push(line);
        } else {
            self.omitted += 1;
        }
    }

    pub fn push_filtered(&mut self) {
        self.filtered += 1;
    }

    fn finish(&self) -> Vec<u8> {
        let mut text = String::with_capacity(self.bytes.saturating_add(200));
        text.push_str("jModGuard last-observed deleted messages\n\n");
        for entry in &self.entries {
            text.push_str(entry);
        }
        if self.omitted > 0 {
            text.push_str(&format!(
                "\n{} entries omitted because transcript reached 256 KiB.\n",
                self.omitted
            ));
        }
        text.into_bytes()
    }
}

fn fallback_bulk_parts(
    template: &str,
    display: &BTreeMap<String, String>,
    transcript: &BulkTranscript,
) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut shown = 0usize;
    let mut clipped = 0usize;
    for entry in &transcript.entries {
        let slot = parts.len() - 1;
        let limit = if slot == 0 { 1100 } else { 1700 };
        if !parts[slot].is_empty()
            && parts[slot].encode_utf16().count() + entry.encode_utf16().count() > limit
        {
            if parts.len() == 3 {
                break;
            }
            parts.push(String::new());
        }
        let slot = parts.len() - 1;
        let available = (if slot == 0 { 1100usize } else { 1700usize })
            .saturating_sub(parts[slot].encode_utf16().count());
        let (prefix, rest) = take_utf16_prefix(
            entry,
            if entry.encode_utf16().count() <= available {
                available
            } else {
                available.saturating_sub(24)
            },
        );
        parts[slot].push_str(prefix);
        if !rest.is_empty() {
            parts[slot].push_str(" [entry clipped]\n");
            clipped += 1;
        }
        shown += 1;
    }
    let omitted = transcript.omitted + transcript.entries.len().saturating_sub(shown);
    let mut fallback_display = display.clone();
    fallback_display.insert("omitted".into(), omitted.to_string());
    fallback_display.insert(
        "details".into(),
        format!(
            "attachment unavailable; {} entries omitted from plain-message fallback",
            omitted
        ),
    );
    let rendered = templates::render_bounded(template, &fallback_display, 2000);
    let summary = format!(
        "Attachment unavailable. Unique deleted: {}; retained: {}; known missing: {}; storage unknown: {}; filtered: {}; entries omitted: {}; entries clipped: {}.\n",
        transcript.total,
        transcript.retained,
        transcript.unavailable,
        transcript.unknown,
        transcript.filtered,
        omitted,
        clipped
    );
    parts[0] = format!("{}\n{}\n{}", clip(&rendered, 450), summary, parts[0]);
    let part_count = parts.len();
    for (index, part) in parts.iter_mut().enumerate().skip(1) {
        *part = format!(
            "Deleted messages continued ({}/{}):\n{}",
            index + 1,
            part_count,
            part
        );
    }
    parts
}

#[allow(clippy::too_many_arguments)]
async fn deliver_bulk_text_fallback_parts(
    app: &Arc<BotState>,
    http: &serenity::http::Http,
    destination: ChannelId,
    rest_target: crate::discord::DiscordRestTarget,
    template: &str,
    display: &BTreeMap<String, String>,
    transcript: &BulkTranscript,
    deadline: Duration,
    started: Instant,
    mut failure_recovery: crate::discord::RecoveryEpoch,
) -> (bool, Option<serenity::Error>, crate::discord::RecoveryEpoch) {
    let parts = fallback_bulk_parts(template, display, transcript);
    let part_count = parts.len();
    let mut parts_sent = 0usize;
    let mut terminal_error = None;
    for details in parts {
        let left = deadline.saturating_sub(started.elapsed());
        if left.is_zero() {
            break;
        }
        failure_recovery = app
            .remembered_discord_targets
            .capture_recovery_epoch(rest_target);
        match tokio::time::timeout(
            left,
            crate::discord::race_discord_call(
                &app.activation,
                destination.send_message(http, log_message(&details)),
            ),
        )
        .await
        {
            Ok(crate::discord::DiscordCall::Finished(Ok(_))) => {
                parts_sent += 1;
            }
            Ok(crate::discord::DiscordCall::Finished(Err(error))) => {
                terminal_error = Some(error);
                break;
            }
            _ => break,
        }
    }
    let delivered = parts_sent == part_count && part_count > 0;
    (delivered, terminal_error, failure_recovery)
}

pub(crate) async fn emit_retained_bulk(
    app: &Arc<BotState>,
    ctx: &Context,
    guild: GuildId,
    channel: ChannelId,
    transcript: BulkTranscript,
) {
    let vars = BTreeMap::from([
        ("channel".into(), format!("<#{channel}>")),
        ("channel_id".into(), channel.to_string()),
        ("count".into(), transcript.total.to_string()),
        ("retained".into(), transcript.retained.to_string()),
        ("unavailable".into(), transcript.unavailable.to_string()),
        ("unknown".into(), transcript.unknown.to_string()),
        ("filtered".into(), transcript.filtered.to_string()),
        ("omitted".into(), transcript.omitted.to_string()),
        (
            "details".into(),
            format!(
                "transcript attached; {} omitted by 256 KiB limit; {} excluded by user filter",
                transcript.omitted, transcript.filtered
            ),
        ),
        ("time".into(), chrono::Utc::now().to_rfc3339()),
    ]);
    let Ok(_permit) = app.log_delivery_slots.try_acquire() else {
        record_delivery_failure(app);
        return;
    };
    let deadline = Duration::from_secs(app.cfg.log.discord_timeout_secs);
    let started = Instant::now();
    let cfg = match tokio::time::timeout(
        deadline.saturating_sub(started.elapsed()),
        app.configs.get(&app.db, guild.get()),
    )
    .await
    {
        Ok(Ok(cfg)) => cfg,
        _ => {
            record_delivery_failure(app);
            return;
        }
    };
    let Some(destination) = channel_for(&cfg, "message_bulk_delete") else {
        return;
    };
    let rest_target = crate::discord::DiscordRestTarget::GuildChannel {
        guild: guild.get(),
        channel: destination.get(),
    };
    if app
        .remembered_discord_targets
        .blocked(rest_target, Instant::now())
    {
        return;
    }
    if cfg
        .log_filters
        .get("message_bulk_delete")
        .is_some_and(|filter| excluded(filter, &vars))
    {
        return;
    }
    if cfg
        .log_filters
        .get("message_bulk_delete")
        .is_some_and(|filter| {
            transcript.included_authors.iter().any(|author| {
                filter
                    .users
                    .iter()
                    .any(|ignored| ignored == &author.to_string())
            })
        })
    {
        // The filter changed after transcript construction. Suppress all
        // retained payload rather than expose a newly ignored author's text.
        let mut display = vars;
        display.insert(
            "details".into(),
            "transcript unavailable after filter change".into(),
        );
        templates::format_log_timestamps(&mut display);
        let template = cfg
            .templates
            .get("message_bulk_delete")
            .map(String::as_str)
            .unwrap_or_else(|| default_template("message_bulk_delete"));
        let rendered = templates::render_bounded(template, &display, 2000);
        let left = deadline.saturating_sub(started.elapsed());
        if left.is_zero()
            || !matches!(
                tokio::time::timeout(
                    left,
                    crate::discord::race_discord_call(
                        &app.activation,
                        destination.send_message(&ctx.http, log_message(&rendered)),
                    ),
                )
                .await,
                Ok(crate::discord::DiscordCall::Finished(Ok(_)))
            )
        {
            record_delivery_failure(app);
        }
        return;
    }
    let template = cfg
        .templates
        .get("message_bulk_delete")
        .map(String::as_str)
        .unwrap_or_else(|| default_template("message_bulk_delete"));
    let mut display = vars;
    templates::format_log_timestamps(&mut display);
    let rendered = templates::render_bounded(template, &display, 2000);
    let body = transcript.finish();
    let message =
        log_message(&rendered).add_file(CreateAttachment::bytes(body, "deleted-messages.txt"));
    let mut delivered = false;
    let mut terminal_error: Option<serenity::Error> = None;
    let mut failure_recovery = app
        .remembered_discord_targets
        .capture_recovery_epoch(rest_target);
    match tokio::time::timeout(
        deadline.saturating_sub(started.elapsed()),
        crate::discord::race_discord_call(
            &app.activation,
            destination.send_message(&ctx.http, message),
        ),
    )
    .await
    {
        Ok(crate::discord::DiscordCall::Finished(Ok(_))) => delivered = true,
        Ok(crate::discord::DiscordCall::Finished(Err(discord_err))) => {
            if matches!(
                &discord_err,
                serenity::Error::Http(http_error)
                    if http_error
                        .status_code()
                        .is_some_and(|status| status.as_u16() == 403)
            ) {
                // Attach Files may be absent while Send Messages still works.
                let (all_parts_delivered, fallback_error, recovery) =
                    deliver_bulk_text_fallback_parts(
                        app,
                        &ctx.http,
                        destination,
                        rest_target,
                        template,
                        &display,
                        &transcript,
                        deadline,
                        started,
                        failure_recovery,
                    )
                    .await;
                failure_recovery = recovery;
                if let Some(error) = fallback_error {
                    terminal_error = Some(error);
                }
                delivered = all_parts_delivered;
            } else {
                terminal_error = Some(discord_err);
            }
        }
        _ => {
            record_delivery_failure(app);
            return;
        }
    }
    finish_bulk_channel_delivery(
        app,
        failure_recovery,
        rest_target,
        delivered,
        terminal_error.as_ref(),
    );
}

fn finish_bulk_channel_delivery(
    app: &BotState,
    recovery: crate::discord::RecoveryEpoch,
    rest_target: crate::discord::DiscordRestTarget,
    delivered: bool,
    terminal_error: Option<&serenity::Error>,
) {
    if delivered {
        app.remembered_discord_targets.forget(rest_target);
        return;
    }
    if let Some(error) = terminal_error {
        app.remembered_discord_targets.remember_rest_rejection(
            recovery,
            error,
            rest_target,
            Instant::now(),
        );
    }
    record_delivery_failure(app);
}

#[cfg(test)]
pub(crate) fn settle_bulk_channel_delivery(
    app: &BotState,
    recovery: crate::discord::RecoveryEpoch,
    rest_target: crate::discord::DiscordRestTarget,
    delivered: bool,
    terminal_error: Option<&serenity::Error>,
) {
    finish_bulk_channel_delivery(app, recovery, rest_target, delivered, terminal_error);
}

fn take_utf16_prefix(text: &str, max_units: usize) -> (&str, &str) {
    let mut units = 0;
    let mut boundary = 0;
    for (index, ch) in text.char_indices() {
        if units + ch.len_utf16() > max_units {
            break;
        }
        units += ch.len_utf16();
        boundary = index + ch.len_utf8();
    }
    text.split_at(boundary)
}

/// Use only fields in the delivered update; cache-filled values are not edit history.
/// Discord sets `edited_timestamp` only for an actual edit. Embed, GIF/link unfurl,
/// reply-preview, pin, and flag refreshes omit it and produce no report — even when
/// the Gateway repeats the original content, a `message_reference`, or an empty
/// attachment list. The bot never reports Gateway updates of its own messages, so a
/// log post cannot stack into another `message_edit` report.
pub(crate) fn message_update_report(
    event: &MessageUpdateEvent,
    known_author_id: Option<u64>,
    bot_user_id: Option<u64>,
) -> Option<MessageReport> {
    let guild_id = event.guild_id?;
    let edited_at = event.edited_timestamp?;
    if event.content.is_none() && event.attachments.is_none() {
        return None;
    }
    let author_id = event
        .author
        .as_ref()
        .map(|author| author.id.get())
        .or(known_author_id)
        .filter(|id| *id != 0);
    if let (Some(author_id), Some(bot_user_id)) = (author_id, bot_user_id.filter(|id| *id != 0))
        && author_id == bot_user_id
    {
        return None;
    }
    let mut vars = message_vars(guild_id, event.channel_id, event.id);
    vars.insert("time".into(), edited_at.to_string());
    vars.insert(
        "after".into(),
        event
            .content
            .as_deref()
            .map(|s| clip(s, 1000))
            .unwrap_or_else(|| "not supplied in this update".into()),
    );
    vars.insert(
        "attachments".into(),
        event
            .attachments
            .as_ref()
            .map(|a| a.len().to_string())
            .unwrap_or_else(|| "not supplied in this update".into()),
    );
    if let Some(author) = &event.author {
        vars.extend(templates::user_vars(
            author.id.get(),
            &author.name,
            author.global_name.as_deref().unwrap_or(&author.name),
        ));
    } else if let Some(author_id) = known_author_id.filter(|id| *id != 0) {
        // A message-create observation can identify an author without
        // supplying an author name or any earlier message content.
        vars.insert("user_id".into(), author_id.to_string());
    }
    Some(MessageReport {
        guild_id,
        event: "message_edit",
        vars,
    })
}

pub(crate) fn message_delete_report(
    app: &BotState,
    guild_id: GuildId,
    channel: ChannelId,
    id: MessageId,
) -> MessageReport {
    let mut vars = message_vars(guild_id, channel, id);
    if let Some(snippet) = take_snippet(app, guild_id.get(), channel.get(), id.get()) {
        vars.extend(templates::user_vars(
            snippet.author,
            &snippet.author.to_string(),
            &snippet.author.to_string(),
        ));
    }
    MessageReport {
        guild_id,
        event: "message_delete",
        vars,
    }
}

/// Emits a mod-action event with actor context (used by command behaviors).
#[allow(clippy::too_many_arguments)]
pub async fn emit_mod_action(
    app: &Arc<BotState>,
    enrollment: Option<db::GuildEnrollment>,
    ctx: &Context,
    guild_id: GuildId,
    action: &str,
    target_id: u64,
    target_name: &str,
    target_display: &str,
    actor_id: u64,
    actor_name: &str,
    source_channel_id: u64,
    reason: &str,
) {
    if !app.activation.may_record() {
        return;
    }
    let mut vars = templates::user_vars(target_id, target_name, target_display);
    vars.insert("actor".into(), format!("{} (<@{}>)", actor_name, actor_id));
    vars.insert("actor_id".into(), actor_id.to_string());
    vars.insert("source_channel_id".into(), source_channel_id.to_string());
    vars.insert("action".into(), action.to_string());
    vars.insert("reason".into(), reason.to_string());
    emit(app, ctx, guild_id, "mod_action", vars).await;

    crate::logging::log_persistence_result(
        "mod_action event",
        db::insert_enrolled_event(
            &app.db,
            enrollment.clone(),
            "mod_action",
            Some(target_id as i64),
            Some(actor_id as i64),
            &serde_json::json!({"action": action, "reason": reason}),
        )
        .await,
    );
}

/// Emits a member event (join/leave/ban/unban/kick) with full member context.
pub(crate) async fn emit_member_event(
    app: &Arc<BotState>,
    ctx: &Context,
    guild_id: GuildId,
    event: &str,
    user: &serenity::model::user::User,
    extra: BTreeMap<String, String>,
    observation: Option<MemberAuditObservation>,
) -> Option<BTreeMap<String, String>> {
    let account_created = user.created_at();
    let account_age_seconds =
        (chrono::Utc::now().timestamp() - account_created.unix_timestamp()).max(0) as u64;

    let mut vars = templates::user_vars(
        user.id.get(),
        &user.name,
        user.global_name.as_deref().unwrap_or(&user.name),
    );
    vars.insert("account_created".into(), account_created.to_string());
    vars.insert(
        "account_age".into(),
        templates::format_duration(account_age_seconds),
    );
    vars.extend(extra);
    emit_to_observed_with_audit(
        app,
        ctx.http.as_ref(),
        guild_id,
        event,
        vars,
        observation,
        Admission::Immediate,
    )
    .await
    .1
}

/// Emits a departure after the paced member check proved the captured stay is
/// absent. The Gateway handler has already returned, so use the saved profile
/// and the original event time instead of requiring another Discord lookup.
pub(crate) async fn emit_verified_member_departure(
    app: &Arc<BotState>,
    http: &impl LogDelivery,
    guild_id: GuildId,
    departure: &db::guild_data::MemberDeparture,
    profile: Option<&db::guild_data::MemberProfile>,
) {
    let Some(vars) = verified_member_departure_vars(departure, profile) else {
        return;
    };
    let _ = emit_to_observed_with_audit(
        app,
        http,
        guild_id,
        "member_leave",
        vars,
        None,
        Admission::Immediate,
    )
    .await;
}

fn verified_member_departure_vars(
    departure: &db::guild_data::MemberDeparture,
    profile: Option<&db::guild_data::MemberProfile>,
) -> Option<BTreeMap<String, String>> {
    let departed_at =
        chrono::DateTime::<chrono::Utc>::from_timestamp_millis(departure.departed_at_ms)?;
    let user_id = departure.user_id as u64;
    let id_text = user_id.to_string();
    let username = profile
        .and_then(|p| p.username.as_deref())
        .unwrap_or(&id_text);
    let display_name = profile.map(|p| p.display_name.as_str()).unwrap_or(username);
    let mut vars = templates::user_vars(user_id, username, display_name);
    let account_created = serenity::model::id::UserId::new(user_id).created_at();
    let account_age_seconds =
        (chrono::Utc::now().timestamp() - account_created.unix_timestamp()).max(0) as u64;
    vars.insert("account_created".into(), account_created.to_string());
    vars.insert(
        "account_age".into(),
        templates::format_duration(account_age_seconds),
    );
    vars.extend(member_join_vars_ms(departure.joined_at_ms, departed_at));
    Some(vars)
}

fn default_template(event: &str) -> &'static str {
    crate::defaults::rendered_log_templates()
        .into_iter()
        .find(|(e, _)| *e == event)
        .map(|(_, t)| t)
        .unwrap_or("**{event}**\n{content}")
}

#[cfg(test)]
mod tests;

/// Preserve the optional persistence result while making failed writes visible.
pub(crate) fn log_persistence_result<T>(
    kind: &'static str,
    result: anyhow::Result<T>,
) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::warn!("{kind} write failed: {error:#}");
            None
        }
    }
}

#[cfg(test)]
pub(crate) mod persistence_log_tests {
    use super::log_persistence_result;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    pub(crate) struct CapturedLog(pub(crate) Arc<Mutex<Vec<u8>>>);

    impl Write for CapturedLog {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl CapturedLog {
        pub(crate) fn dispatch(&self) -> tracing::Dispatch {
            let writer = self.clone();
            tracing::Dispatch::new(
                tracing_subscriber::fmt()
                    .without_time()
                    .with_ansi(false)
                    .with_writer(move || writer.clone())
                    .finish(),
            )
        }
        pub(crate) fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    #[test]
    fn failed_moderation_writes_log_each_operation_without_identifiers() {
        let log = CapturedLog::default();
        tracing::dispatcher::with_default(&log.dispatch(), || {
            for kind in [
                "automod event",
                "timeout_add event",
                "timeout_remove event",
                "mod_action event",
                "gateway ban row",
            ] {
                let result =
                    Err::<(), _>(anyhow::anyhow!("provider failure").context("write rejected"));
                assert!(log_persistence_result(kind, result).is_none());
            }
            assert_eq!(log_persistence_result("gateway ban row", Ok(7)), Some(7));
        });
        let text = log.text();
        assert_eq!(text.lines().count(), 5);
        for (line, kind) in text.lines().zip([
            "automod event",
            "timeout_add event",
            "timeout_remove event",
            "mod_action event",
            "gateway ban row",
        ]) {
            assert!(line.contains("WARN"));
            assert!(
                line.ends_with(&format!(
                    "{kind} write failed: write rejected: provider failure"
                )),
                "{line}"
            );
        }
    }
}
