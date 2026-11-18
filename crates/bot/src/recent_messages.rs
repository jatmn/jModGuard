//! Bounded Gateway message identities for automod's ban purge.
use crate::db::GuildEnrollment;
use crate::state::BotState;
use parking_lot::Mutex;
use serenity::http::Http;
use std::collections::BTreeMap;
use std::time::Duration;

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
        });
        if entries.len() > MAX_TRACKED_MESSAGES {
            entries.pop_first();
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
        self.entries
            .lock()
            .retain(|_, item| item.created_ms >= cutoff);
    }

    fn still_tracked(&self, guild: u64, author: u64, channel: u64, message: u64) -> bool {
        self.entries
            .lock()
            .get(&(message, guild))
            .is_some_and(|entry| entry.author == author && entry.channel == channel)
    }

    fn for_ban(&self, guild: u64, author: u64, seconds: u32) -> Vec<(u64, u64)> {
        let now = chrono::Utc::now().timestamp_millis();
        let cutoff = now - i64::from(seconds) * 1000;
        self.entries
            .lock()
            .iter()
            .rev()
            .filter(|((_, id), item)| {
                *id == guild && item.author == author && (cutoff..=now).contains(&item.created_ms)
            })
            .map(|((message, _), item)| (item.channel, *message))
            .collect()
    }
}

/// A successful ban supplies Discord's server-side deletion window. Sweep any
/// remaining tracked messages for the same enrollment without fetching history.
pub(crate) async fn sweep_after_ban(
    app: &BotState,
    http: &Http,
    enrollment: &GuildEnrollment,
    author: u64,
    seconds: u32,
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
    let messages = app.recent_messages.for_ban(guild, author, seconds);
    let mut deleted = 0;
    let sweep = async {
        for (channel, message) in messages {
            if !app
                .recent_messages
                .still_tracked(guild, author, channel, message)
            {
                continue;
            }
            let result = crate::discord::race_discord_call(
                &app.activation,
                http.delete_message(channel.into(), message.into(), Some("automod ban purge")),
            )
            .await;
            match result {
                crate::discord::DiscordCall::Finished(Ok(())) => {
                    deleted += 1;
                    app.recent_messages.forget(guild, channel, message);
                }
                crate::discord::DiscordCall::Finished(Err(error)) => {
                    if crate::discord::http_error_response(&error)
                        .is_some_and(|(_, code)| code == 10008)
                    {
                        // Discord's ban may already have removed it.
                        app.recent_messages.forget(guild, channel, message);
                    } else {
                        tracing::debug!(guild, channel, message, %error, "automod ban purge could not delete tracked message");
                    }
                }
                crate::discord::DiscordCall::Cancelled => break,
            }
        }
    };
    if tokio::time::timeout(MAX_SWEEP_TIME, sweep).await.is_err() {
        tracing::warn!(
            guild,
            author,
            deleted,
            "automod ban purge reached its deadline"
        );
    }
    deleted
}
