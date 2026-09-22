//! Event routing: renders a template for an event type and posts it to the
//! configured guild-scoped log channel. Routine delivery never archives an event.

use serenity::builder::{CreateAllowedMentions, CreateMessage};
use serenity::client::Context;
use serenity::model::channel::MessageFlags;
use serenity::model::event::MessageUpdateEvent;
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
}
#[serenity::async_trait]
impl LogDelivery for serenity::http::Http {
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
            vars.insert("event".into(), event.to_owned());
            vars.insert("time".into(), chrono::Utc::now().to_rfc3339());
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
    event.edited_timestamp?;
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
