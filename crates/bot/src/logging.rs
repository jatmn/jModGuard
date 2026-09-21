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
        let attachments = event
            .attachments
            .as_ref()
            .map(|items| fingerprint(&items.iter().map(|a| a.id.get()).collect::<Vec<_>>()));
        let key = (guild.get(), event.channel_id.get(), event.id.get());
        let mut entries = self.inner.lock();
        entries.retain(|_, seen| seen.at.elapsed() < EDIT_REPORT_KEEP);
        if let Some(seen) = entries.get_mut(&key) {
            // Gateway updates may supply only one field at a time. A newly
            // supplied field completes this observation; it is not a new edit.
            let changed = content.is_some_and(|value| seen.content.is_some_and(|old| old != value))
                || attachments
                    .is_some_and(|value| seen.attachments.is_some_and(|old| old != value));
            if content.is_some() {
                seen.content = content;
            }
            if attachments.is_some() {
                seen.attachments = attachments;
            }
            seen.at = Instant::now();
            return changed;
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
                content,
                attachments,
                at: Instant::now(),
            },
        );
        true
    }
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
    let known_user = vars.get("user_id").is_none_or(|id| id != "unavailable");
    let user_match = ["user_id", "actor_id"]
        .iter()
        .filter_map(|key| vars.get(*key))
        .filter(|value| value.parse::<u64>().is_ok_and(|id| id != 0))
        .chain(
            ["username", "user"]
                .iter()
                .filter_map(|key| vars.get(*key))
                .filter(|_| known_user),
        )
        .any(|value| {
            filter
                .users
                .iter()
                .any(|ignored| ignored == &value.to_lowercase())
        });
    let source = vars
        .get("source_channel_id")
        .or_else(|| vars.get("channel_id"))
        .map(String::as_str)
        .or_else(|| {
            vars.get("channel")
                .and_then(|value| value.strip_prefix("<#")?.strip_suffix('>'))
        });
    user_match || source.is_some_and(|source| filter.channels.iter().any(|id| id == source))
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
pub(crate) fn message_update_report(event: &MessageUpdateEvent) -> Option<MessageReport> {
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
