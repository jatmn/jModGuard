//! Process-local cooldown and bounded expiry for automod channel replies.

use parking_lot::Mutex;
use serenity::builder::{CreateAllowedMentions, CreateMessage};
use serenity::http::Http;
use serenity::model::id::{ChannelId, GuildId, UserId};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::time::Instant;

use crate::activation::Activation;
use crate::db::guild_data::CaseOutcome;
use crate::discord::{DiscordCall, race_discord_call};

const WINDOW: Duration = Duration::from_secs(30);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_COOLDOWNS: usize = 4096;
const MAX_PENDING_DELETES: usize = 4096;

type ReplyKey = (GuildId, ChannelId, UserId);

pub(super) struct Replies {
    cooldowns: Mutex<HashMap<ReplyKey, Instant>>,
    deletes: Arc<Semaphore>,
}

impl Default for Replies {
    fn default() -> Self {
        Self {
            cooldowns: Mutex::new(HashMap::new()),
            deletes: Arc::new(Semaphore::new(MAX_PENDING_DELETES)),
        }
    }
}

impl Replies {
    pub(super) fn prune(&self, now: Instant) {
        self.cooldowns
            .lock()
            .retain(|_, at| now.duration_since(*at) < WINDOW);
    }

    fn reserve(&self, key: ReplyKey, now: Instant) -> bool {
        let mut cooldowns = self.cooldowns.lock();
        if let Some(last) = cooldowns.get_mut(&key) {
            if now.duration_since(*last) < WINDOW {
                return false;
            }
            *last = now;
        } else if cooldowns.len() < MAX_COOLDOWNS {
            cooldowns.insert(key, now);
        } else {
            return false;
        }
        true
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn send(
        &self,
        activation: &Arc<Activation>,
        http: &Arc<Http>,
        guild: GuildId,
        channel: ChannelId,
        user: UserId,
        template: &str,
        delete_after_seconds: u64,
    ) -> Option<CaseOutcome> {
        if !activation.may_record() {
            return None;
        }
        let delete_permit = if delete_after_seconds == 0 {
            None
        } else {
            let Ok(permit) = Arc::clone(&self.deletes).try_acquire_owned() else {
                return None;
            };
            Some(permit)
        };
        let vars = BTreeMap::from([
            ("user".into(), format!("<@{user}>")),
            ("channel".into(), format!("<#{channel}>")),
        ]);
        let content = crate::templates::render_bounded(template, &vars, 2000);
        let message = CreateMessage::new().content(content).allowed_mentions(
            CreateAllowedMentions::new()
                .users([user])
                .replied_user(false),
        );
        if !self.reserve((guild, channel, user), Instant::now()) {
            return None;
        }
        let sent = match tokio::time::timeout(
            HTTP_TIMEOUT,
            race_discord_call(activation, channel.send_message(http.as_ref(), message)),
        )
        .await
        {
            Ok(DiscordCall::Finished(Ok(sent))) => sent,
            Ok(DiscordCall::Finished(Err(error))) => {
                return Some(crate::cases::classify_discord(Some(&error)));
            }
            Ok(DiscordCall::Cancelled) | Err(_) => return Some(CaseOutcome::Uncertain),
        };

        if let Some(permit) = delete_permit {
            let expires = Instant::now() + Duration::from_secs(delete_after_seconds);
            let http = Arc::clone(http);
            let activation = Arc::clone(activation);
            let id = sent.id;
            tokio::spawn(async move {
                let _permit = permit;
                tokio::select! {
                    _ = activation.until_cutoff() => return,
                    _ = tokio::time::sleep_until(expires) => {}
                }
                let _ = tokio::time::timeout(
                    HTTP_TIMEOUT,
                    race_discord_call(&activation, channel.delete_message(http.as_ref(), id)),
                )
                .await;
            });
        }
        Some(CaseOutcome::Succeeded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn reply_cooldown_is_per_user_channel_and_guild_at_window_boundary() {
        let replies = Replies::default();
        let now = Instant::now();
        let key = (GuildId::new(1), ChannelId::new(2), UserId::new(3));
        assert!(replies.reserve(key, now));
        assert!(!replies.reserve(key, now));
        assert!(!replies.reserve(key, now + WINDOW - Duration::from_millis(1)));
        assert!(replies.reserve((key.0, ChannelId::new(4), key.2), now));
        assert!(replies.reserve((key.0, key.1, UserId::new(4)), now));
        assert!(replies.reserve((GuildId::new(4), key.1, key.2), now));
        replies.prune(now + WINDOW);
        assert!(replies.cooldowns.lock().is_empty());
        assert!(replies.reserve(key, now + WINDOW));
        assert!(replies.reserve(key, now + WINDOW * 2));
    }
}
