//! Bounded Gateway message identities for automod's ban purge.
use crate::db::GuildEnrollment;
use crate::state::BotState;
use parking_lot::Mutex;
use serenity::http::Http;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::time::Instant;

const MAX_TRACKED_MESSAGES: usize = 32_768;
const MAX_DELETE_WINDOW_SECS: i64 = 7 * 86_400;
const MAX_SWEEP_TIME: Duration = Duration::from_secs(30);

#[derive(Default)]
pub(crate) struct RecentMessages {
    // Discord snowflakes sort by original creation time. Guild is part of the
    // key as a safeguard against acting on identities from another guild.
    entries: Mutex<BTreeMap<(u64, u64), MessageIdentity>>,
}

struct MessageIdentity {
    author: u64,
    channel: u64,
    created_ms: i64,
    purge_echo_until: Option<Instant>,
}

impl RecentMessages {
    pub(crate) fn remember(
        &self,
        guild: u64,
        author: u64,
        channel: u64,
        message: u64,
        created_ms: i64,
    ) {
        let mut entries = self.entries.lock();
        entries.entry((message, guild)).or_insert(MessageIdentity {
            author,
            channel,
            created_ms,
            purge_echo_until: None,
        });
        if entries.len() > MAX_TRACKED_MESSAGES {
            entries.pop_first();
        }
    }

    pub(crate) fn identity(&self, guild: u64, channel: u64, message: u64) -> Option<(u64, i64)> {
        let entries = self.entries.lock();
        let entry = entries.get(&(message, guild)).filter(|entry| {
            entry.channel == channel
                && entry
                    .purge_echo_until
                    .is_none_or(|until| Instant::now() < until)
        })?;
        Some((entry.author, entry.created_ms))
    }

    /// Discord says the message is gone. Keep its identity for a late echo,
    /// and keep it out of future REST sweeps.
    pub(crate) fn defer_purge_echo(&self, guild: u64, channel: u64, message: u64, until: Instant) {
        if let Some(entry) = self.entries.lock().get_mut(&(message, guild))
            && entry.channel == channel
        {
            entry.purge_echo_until = Some(until);
        }
    }

    pub(crate) fn forget(&self, guild: u64, channel: u64, message: u64) {
        let mut entries = self.entries.lock();
        let key = (message, guild);
        if entries
            .get(&key)
            .is_some_and(|entry| entry.channel == channel)
        {
            entries.remove(&key);
        }
    }

    pub(crate) fn forget_guild(&self, guild: u64) {
        self.entries.lock().retain(|(_, id), _| *id != guild);
    }

    pub(crate) fn prune(&self) {
        let cutoff = chrono::Utc::now().timestamp_millis() - MAX_DELETE_WINDOW_SECS * 1000;
        let now = Instant::now();
        self.entries.lock().retain(|_, item| {
            item.created_ms >= cutoff && item.purge_echo_until.is_none_or(|until| now < until)
        });
    }

    fn still_tracked(&self, guild: u64, author: u64, channel: u64, message: u64) -> bool {
        self.entries
            .lock()
            .get(&(message, guild))
            .is_some_and(|entry| {
                entry.author == author
                    && entry.channel == channel
                    && entry.purge_echo_until.is_none()
            })
    }

    fn for_ban(&self, guild: u64, author: u64, seconds: u32, now_ms: i64) -> Vec<(u64, u64)> {
        let cutoff = now_ms - i64::from(seconds) * 1000;
        self.entries
            .lock()
            .iter()
            .rev()
            .filter(|((_, id), item)| {
                *id == guild
                    && item.author == author
                    && item.purge_echo_until.is_none()
                    && (cutoff..=now_ms).contains(&item.created_ms)
            })
            .map(|((message, _), item)| (item.channel, *message))
            .collect()
    }
}

/// A successful ban supplies Discord's server-side deletion window. Sweep any
/// remaining tracked messages for the same enrollment without fetching history.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn sweep_after_ban(
    app: &std::sync::Arc<BotState>,
    http: &Http,
    enrollment: &GuildEnrollment,
    author: u64,
    seconds: u32,
    sweep_at_ms: i64,
    cleanup: Option<&crate::logging::Cleanup<'_>>,
    still_admitted: impl Fn() -> bool,
) -> usize {
    let guild = enrollment.guild_id as u64;
    if seconds == 0
        || !app.activation.may_record()
        || crate::db::guild_stores::guild_is_write_fenced(&app.db, enrollment.guild_id)
            .await
            .unwrap_or(true)
        || crate::cases::capture(app, guild).await.as_ref() != Some(enrollment)
    {
        return 0;
    }
    let messages = app
        .recent_messages
        .for_ban(guild, author, seconds, sweep_at_ms);
    let mut deleted = 0;
    let deadline = Instant::now() + MAX_SWEEP_TIME;
    for (channel, message) in messages {
        if !still_admitted() {
            break;
        }
        if !app
            .recent_messages
            .still_tracked(guild, author, channel, message)
        {
            continue;
        }
        let prepared = cleanup.map(|cleanup| cleanup.prepare(app, channel, message));
        let result = tokio::time::timeout_at(
            deadline,
            crate::discord::race_discord_call(
                &app.activation,
                http.delete_message(channel.into(), message.into(), Some("automod ban purge")),
            ),
        )
        .await;
        match result {
            Ok(crate::discord::DiscordCall::Finished(Ok(()))) => {
                deleted += 1;
                app.recent_messages.forget(guild, channel, message);
                if let Some((cleanup, prepared)) = cleanup.zip(prepared)
                    && !cleanup
                        .deleted(app, channel, message, prepared, false)
                        .await
                {
                    cleanup
                        .complete_ungrouped(app, http, channel, message, prepared)
                        .await;
                }
            }
            Ok(crate::discord::DiscordCall::Finished(Err(error))) => {
                let gone = crate::discord::http_error_response(&error)
                    .is_some_and(|(_, code)| code == 10008);
                if let Some((cleanup, prepared)) = cleanup.zip(prepared) {
                    if gone
                        && cleanup
                            .purge_sweep_gone(app, channel, message, prepared)
                            .await
                    {
                        continue;
                    }
                    if !gone {
                        cleanup.failed(
                            channel,
                            message,
                            crate::logging::cleanup_delete_failure(&error),
                        );
                    }
                    cleanup
                        .release_ungrouped(app, http, channel, message, prepared)
                        .await;
                } else if gone {
                    app.recent_messages.forget(guild, channel, message);
                }
                if !gone {
                    tracing::debug!(guild, channel, message, %error, "automod ban purge could not delete tracked message");
                }
            }
            Ok(crate::discord::DiscordCall::Cancelled) | Err(_) => {
                if result.is_err() {
                    tracing::warn!(
                        guild,
                        author,
                        deleted,
                        "automod ban purge reached its deadline"
                    );
                }
                if let Some((cleanup, prepared)) = cleanup.zip(prepared)
                    && !cleanup.deleted(app, channel, message, prepared, true).await
                {
                    cleanup.failed(channel, message, "delete outcome unknown");
                    cleanup
                        .release_ungrouped(app, http, channel, message, prepared)
                        .await;
                }
                break;
            }
        }
    }
    deleted
}
