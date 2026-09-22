//! Event routing: renders a template for an event type and posts it to the
//! configured guild-scoped log channel. Routine delivery never archives an event.

use serenity::builder::{CreateAllowedMentions, CreateMessage};
use serenity::client::Context;
use serenity::model::channel::MessageFlags;
use serenity::model::event::MessageUpdateEvent;
use serenity::model::guild::audit_log::{Action, AuditLogs, MemberAction};
use serenity::model::id::{ChannelId, GuildId, MessageId};
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::state::BotState;
use crate::{db, templates};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

const SNIPPET_KEEP: Duration = Duration::from_secs(600);
const SNIPPETS_PER_GUILD: usize = 64;
const SNIPPET_CHARS: usize = 1000;
const EDIT_REPORT_KEEP: Duration = Duration::from_secs(10);
const EDIT_REPORT_LIMIT: usize = 4096;
const MEMBER_JOINS_LIMIT: usize = 4096;
const MEMBER_JOINS_KEEP: Duration = Duration::from_secs(3600);

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
                (
                    format!("<t:{}:F>", time.unix_timestamp()),
                    format!(
                        "{} days, {} hours, {} minutes (joined <t:{}:F>)",
                        seconds / 86400,
                        seconds % 86400 / 3600,
                        seconds % 3600 / 60,
                        time.unix_timestamp()
                    ),
                )
            }
            None => ("unavailable".into(), "unavailable".into()),
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
        let content = event.content.as_ref().map(fingerprint);
        let edited_at_ms = event.edited_timestamp.map(|time| time.timestamp_millis());
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
    text: String,
}

pub fn remember_message(
    app: &BotState,
    guild: u64,
    channel: u64,
    message: u64,
    author: Option<u64>,
    text: &str,
) {
    let mut cache = app.log_snippets.inner.lock();
    cache.retain(|_, s| s.at.elapsed() < SNIPPET_KEEP);
    let key = (guild, channel, message);
    let author = author
        .filter(|id| *id != 0)
        .or_else(|| cache.get(&key).map(|s| s.author).filter(|id| *id != 0));
    if author.is_none() {
        return;
    }
    let author = author.unwrap();
    if cache.values().filter(|s| s.guild == guild).count() >= SNIPPETS_PER_GUILD
        && !cache.contains_key(&key)
        && let Some(oldest) = cache
            .iter()
            .filter(|(_, s)| s.guild == guild)
            .min_by_key(|(_, s)| s.at)
            .map(|(k, _)| *k)
    {
        cache.remove(&oldest);
    }
    cache.insert(
        key,
        Snippet {
            at: Instant::now(),
            guild,
            author,
            text: text.chars().take(SNIPPET_CHARS).collect(),
        },
    );
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

fn take_snippet(app: &BotState, guild: u64, channel: u64, message: u64) -> Option<(u64, String)> {
    let mut cache = app.log_snippets.inner.lock();
    cache.retain(|_, s| s.at.elapsed() < SNIPPET_KEEP);
    cache
        .remove(&(guild, channel, message))
        .map(|s| (s.author, s.text))
}

/// Resolves the log channel for an event type from the cached config.
fn channel_for(
    cfg: &crate::state::CachedGuildConfig,
    event: &str,
) -> Option<serenity::model::id::ChannelId> {
    cfg.log_channels.get(event).copied()
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
}

pub(crate) async fn emit_to(
    app: &Arc<BotState>,
    sink: &impl LogDelivery,
    guild_id: GuildId,
    event: &str,
    mut vars: BTreeMap<String, String>,
) -> Delivery {
    // Admission never waits or creates a backlog holding raw event content.
    let Ok(_permit) = app.log_delivery_slots.try_acquire() else {
        record_delivery_failure(app);
        return Delivery::Dropped;
    };
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(app.cfg.log.discord_timeout_secs),
        async {
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
            if let Some(action) = member_audit_action(event) {
                // Lookup is part of the admitted report, never an unbounded
                // Gateway task or a history-recovery queue. Reserve delivery time.
                let observed = vars
                    .get("time")
                    .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                    .map(|time| time.timestamp_millis())
                    .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
                let target = vars.get("user_id").and_then(|id| id.parse::<u64>().ok());
                vars.insert("actor".into(), "unavailable".into());
                vars.insert("actor_id".into(), "unavailable".into());
                vars.insert("reason".into(), "unavailable".into());
                let budget = Duration::from_secs(app.cfg.log.discord_timeout_secs)
                    .div_f64(2.0)
                    .min(Duration::from_secs(3));
                if let Some(target) = target
                    && let Ok(Some(context)) = tokio::time::timeout(budget, async {
                        for attempt in 0..3 {
                            if attempt > 0 {
                                tokio::time::sleep(Duration::from_millis(500)).await;
                            }
                            let logs = sink.member_audit(guild_id, action).await.ok()?;
                            if let Some(context) =
                                member_audit_vars(&logs, action, target, observed)
                            {
                                return Some(context);
                            }
                        }
                        None
                    })
                    .await
                {
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
            let rendered = templates::render_bounded(template, &vars, 2000);
            sink.send(channel_id, log_message(&rendered)).await?;
            Ok::<_, anyhow::Error>(Delivery::Sent)
        },
    )
    .await;
    match result {
        Ok(Ok(delivery)) => delivery,
        _ => {
            record_delivery_failure(app);
            Delivery::Failed
        }
    }
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
) -> Option<BTreeMap<String, String>> {
    let mut matches = logs.entries.iter().filter(|entry| {
        let delta = entry.id.created_at().timestamp_millis() - observed_ms;
        entry.action.num() == action.num()
            && entry.target_id.is_some_and(|id| id.get() == target)
            && (-10_000..=3_000).contains(&delta)
    });
    let entry = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    let actor = logs
        .users
        .get(&entry.user_id)
        .map(|user| format!("{} (<@{}>)", user.name, user.id))
        .unwrap_or_else(|| format!("<@{}>", entry.user_id));
    Some(BTreeMap::from([
        ("actor".into(), actor),
        ("actor_id".into(), entry.user_id.to_string()),
        (
            "reason".into(),
            entry
                .reason
                .clone()
                .filter(|reason| !reason.trim().is_empty())
                .unwrap_or_else(|| "No reason provided".into()),
        ),
    ]))
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
        || source
            .and_then(|source| source.parse::<u64>().ok())
            .filter(|id| *id != 0)
            .is_some_and(|source| filter.channels.iter().any(|id| id == &source.to_string()))
}

fn record_delivery_failure(app: &BotState) {
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
        ("before".into(), "not retained".into()),
        ("content".into(), "not retained".into()),
    ])
}

/// Use only fields in the delivered update; cache-filled values are not edit history.
/// Embed/pin-only refreshes do not prove a content edit and produce no report.
pub(crate) fn message_update_report(
    event: &MessageUpdateEvent,
    known_author_id: Option<u64>,
) -> Option<MessageReport> {
    let guild_id = event.guild_id?;
    if event.content.is_none() && event.attachments.is_none() {
        return None;
    }
    let mut vars = message_vars(guild_id, event.channel_id, event.id);
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
    if let Some((author, text)) = take_snippet(app, guild_id.get(), channel.get(), id.get()) {
        vars.extend(templates::user_vars(
            author,
            &author.to_string(),
            &author.to_string(),
        ));
        vars.insert(
            "content".into(),
            if text.is_empty() {
                "empty".into()
            } else {
                clip(&text, 1000)
            },
        );
    } else {
        vars.insert("content".into(), "not supplied to this process".into());
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
    let mut vars = templates::user_vars(target_id, target_name, target_display);
    vars.insert("actor".into(), format!("{} (<@{}>)", actor_name, actor_id));
    vars.insert("actor_id".into(), actor_id.to_string());
    vars.insert("source_channel_id".into(), source_channel_id.to_string());
    vars.insert("action".into(), action.to_string());
    vars.insert("reason".into(), reason.to_string());
    emit(app, ctx, guild_id, "mod_action", vars).await;

    let _ = db::insert_enrolled_event(
        &app.db,
        enrollment.clone(),
        "mod_action",
        Some(target_id as i64),
        Some(actor_id as i64),
        &serde_json::json!({"action": action, "reason": reason}),
    )
    .await;
}

/// Emits a member event (join/leave/ban/unban/kick) with full member context.
pub async fn emit_member_event(
    app: &Arc<BotState>,
    ctx: &Context,
    guild_id: GuildId,
    event: &str,
    user: &serenity::model::user::User,
    extra: BTreeMap<String, String>,
) {
    let account_created = user.created_at();
    let account_age_days =
        (chrono::Utc::now().timestamp() - account_created.unix_timestamp()).max(0) as u64 / 86400;

    let mut vars = templates::user_vars(
        user.id.get(),
        &user.name,
        user.global_name.as_deref().unwrap_or(&user.name),
    );
    vars.insert("account_created".into(), account_created.to_string());
    vars.insert("account_age".into(), format!("{account_age_days} days"));
    vars.extend(extra);
    emit(app, ctx, guild_id, event, vars).await;
}

fn default_template(event: &str) -> &'static str {
    crate::defaults::default_templates()
        .into_iter()
        .find(|(e, _)| *e == event)
        .map(|(_, t)| t)
        .unwrap_or("**{event}**\n{content}")
}

#[cfg(test)]
mod tests;
