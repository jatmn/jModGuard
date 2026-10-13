//! Shared Discord REST helpers.

use anyhow::{Result, anyhow};
use parking_lot::Mutex;
use serenity::model::channel::Channel;
use serenity::model::id::ChannelId;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Bounds every reconciliation REST attempt so one half-open Discord request
/// cannot stall durable work for every guild indefinitely.
pub const DISCORD_REST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Outcome of a Discord call raced against writer cutoff.
pub(crate) enum DiscordCall<T> {
    Finished(T),
    Cancelled,
}

/// Cancel the Discord future if cutoff wins. The waiter is registered inside
/// `until_cutoff` before the flag is observed, so a cutoff between the flag
/// read and the wait is not missed. Dropping `call` is the cancellation.
pub(crate) async fn race_discord_call<F>(
    activation: &crate::activation::Activation,
    call: F,
) -> DiscordCall<F::Output>
where
    F: std::future::Future,
{
    tokio::pin!(call);
    tokio::select! {
        biased;
        _ = activation.until_cutoff() => DiscordCall::Cancelled,
        value = &mut call => DiscordCall::Finished(value),
    }
}

/// A non-4xx error so [`crate::cases::classify_discord`] reports [`crate::db::guild_data::CaseOutcome::Uncertain`].
pub(crate) fn cutoff_discord_error() -> serenity::Error {
    serenity::Error::Io(std::io::Error::new(
        std::io::ErrorKind::Interrupted,
        "cutoff",
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryClass {
    Unauthorized,
    Permanent,
    Transient,
}

pub fn classify_retry(error: &serenity::Error) -> RetryClass {
    if let Some((status, code)) = http_error_response(error) {
        return classify_retry_response(status, code);
    }
    match classify_nickname_failure(error).0 {
        NicknameFailureClass::Transient => RetryClass::Transient,
        _ => RetryClass::Permanent,
    }
}

/// Provider fields for structured diagnostics, without parsing error text.
pub fn http_error_response(error: &serenity::Error) -> Option<(u16, isize)> {
    if let serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response)) = error {
        Some((response.status_code.as_u16(), response.error.code))
    } else {
        None
    }
}

/// Only use at already-fatal process boundaries. Serenity maps gateway close
/// 4004 to InvalidAuthentication; HTTP 401 is the startup discovery equivalent.
pub fn fatal_auth_exit_code(error: &serenity::Error) -> Option<i32> {
    (classify_retry(error) == RetryClass::Unauthorized
        || matches!(
            error,
            serenity::Error::Gateway(serenity::gateway::GatewayError::InvalidAuthentication)
        ))
    .then_some(jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE)
}

pub fn classify_retry_error(error: &anyhow::Error) -> RetryClass {
    error
        .downcast_ref::<serenity::Error>()
        .map_or(RetryClass::Transient, classify_retry)
}

pub(crate) fn classify_retry_response(status: u16, code: isize) -> RetryClass {
    match status {
        401 => RetryClass::Unauthorized,
        _ if code == 50001 => RetryClass::Permanent,
        408 | 429 | 500..=599 => RetryClass::Transient,
        _ => RetryClass::Permanent,
    }
}

/// How long a definite 403/404 (or similar permanent REST rejection) is remembered
/// before another attempt is allowed without a config or role change.
pub(crate) const REMEMBERED_REST_TARGET_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_REMEMBERED_REST_TARGETS: usize = 4096;

/// A Discord REST destination that may be skipped after a permanent failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum DiscordRestTarget {
    GuildChannel {
        guild: u64,
        channel: u64,
    },
    AutomodAction {
        guild: u64,
        action: AutomodRestAction,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AutomodRestAction {
    Delete,
    Timeout,
    Kick,
    Ban,
}

impl AutomodRestAction {
    pub(crate) fn from_action(action: &str) -> Option<Self> {
        match action {
            "delete" => Some(Self::Delete),
            "timeout" => Some(Self::Timeout),
            "kick" => Some(Self::Kick),
            "ban" => Some(Self::Ban),
            _ => None,
        }
    }
}

/// Process-local memory of permanent Discord REST rejections (invalid-request budget).
#[derive(Default)]
pub(crate) struct RememberedDiscordTargets {
    expires: Mutex<HashMap<DiscordRestTarget, Instant>>,
}

impl RememberedDiscordTargets {
    pub(crate) fn blocked(&self, target: DiscordRestTarget, now: Instant) -> bool {
        let mut map = self.expires.lock();
        map.retain(|_, until| *until > now);
        map.get(&target).is_some_and(|until| *until > now)
    }

    pub(crate) fn remember_permanent(&self, target: DiscordRestTarget, now: Instant) {
        let mut map = self.expires.lock();
        map.retain(|_, until| *until > now);
        if map.len() >= MAX_REMEMBERED_REST_TARGETS
            && !map.contains_key(&target)
            && let Some(oldest) = map.iter().min_by_key(|(_, until)| *until).map(|(k, _)| *k)
        {
            map.remove(&oldest);
        }
        map.insert(target, now + REMEMBERED_REST_TARGET_TTL);
    }

    pub(crate) fn observe_discord_error(
        &self,
        error: &serenity::Error,
        target: DiscordRestTarget,
        now: Instant,
    ) {
        if classify_retry(error) == RetryClass::Permanent {
            self.remember_permanent(target, now);
        }
    }

    pub(crate) fn observe_anyhow_error(
        &self,
        error: &anyhow::Error,
        target: DiscordRestTarget,
        now: Instant,
    ) {
        if classify_retry_error(error) == RetryClass::Permanent {
            self.remember_permanent(target, now);
        }
    }

    pub(crate) fn clear_guild(&self, guild: u64) {
        self.expires.lock().retain(|target, _| match target {
            DiscordRestTarget::GuildChannel { guild: g, .. } => *g != guild,
            DiscordRestTarget::AutomodAction { guild: g, .. } => *g != guild,
        });
    }

    pub(crate) fn clear_all(&self) {
        self.expires.lock().clear();
    }
}

/// Process-local REST retry deadline. Permanent errors wait for an explicit
/// reset; unauthorized errors must never be reset by registry/config changes.
#[derive(Default)]
pub struct RestRetry {
    failures: u32,
    pub(crate) next: Option<std::time::Instant>,
    pub(crate) stopped: bool,
}

impl RestRetry {
    pub fn due(&self, now: std::time::Instant) -> bool {
        !self.stopped && self.next.is_none_or(|next| now >= next)
    }

    pub fn fail(&mut self, class: RetryClass, now: std::time::Instant, base_secs: u64) {
        if class != RetryClass::Transient {
            self.stopped = true;
            return;
        }
        let base = base_secs
            .saturating_mul(1_u64 << self.failures.min(12))
            .min(3600);
        self.failures = self.failures.saturating_add(1);
        // Time-derived jitter avoids aligning all guild retries to the poll.
        let jitter = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as u64
            % (base / 4 + 1);
        self.next = Some(now + std::time::Duration::from_secs((base + jitter).min(3600)));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NicknameFailureClass {
    Transient,
    Dormant,
    Terminal,
}

/// Classify a Discord nickname failure into the durable outbox lifecycle.
/// Transport failures, rate limits, and server failures may recover. A guild
/// the bot cannot currently see is dormant until `guild_create` reactivates
/// it. Other client rejections require operator action and are terminal.
pub fn classify_nickname_failure(error: &serenity::Error) -> (NicknameFailureClass, String) {
    if let serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response)) = error {
        return classify_nickname_response(response.status_code.as_u16(), response.error.code);
    }
    if matches!(
        error,
        serenity::Error::Http(
            serenity::http::HttpError::Request(_)
                | serenity::http::HttpError::RateLimitI64F64
                | serenity::http::HttpError::RateLimitUtf8
        ) | serenity::Error::Io(_)
            | serenity::Error::Json(_)
            | serenity::Error::Decode(_, _)
    ) {
        return (
            NicknameFailureClass::Transient,
            "transport_or_decode".into(),
        );
    }
    (NicknameFailureClass::Terminal, "client_error".into())
}

fn classify_nickname_response(status: u16, discord_code: isize) -> (NicknameFailureClass, String) {
    let code = format!("discord_{discord_code}");
    if matches!(discord_code, 10004 | 50001) {
        return (NicknameFailureClass::Dormant, code);
    }
    if classify_retry_response(status, discord_code) == RetryClass::Transient {
        return (NicknameFailureClass::Transient, code);
    }
    (NicknameFailureClass::Terminal, code)
}

/// Ensures a configured log destination belongs to the guild whose events it
/// will receive. A channel snowflake alone is not a safe ownership boundary
/// when the bot is installed in multiple guilds.
pub async fn validate_log_channel_guild(
    http: impl AsRef<serenity::http::Http>,
    guild_id: u64,
    channel_id: ChannelId,
) -> Result<()> {
    let channel = http
        .as_ref()
        .get_channel(channel_id)
        .await
        .map_err(|e| anyhow!("failed to resolve log channel {channel_id}: {e}"))?;
    match channel {
        Channel::Guild(channel)
            if channel.guild_id.get() == guild_id && channel.is_text_based() =>
        {
            Ok(())
        }
        Channel::Guild(channel) if channel.guild_id.get() == guild_id => {
            anyhow::bail!("log channel {channel_id} cannot receive messages")
        }
        Channel::Guild(channel) => anyhow::bail!(
            "log channel {channel_id} belongs to guild {}, not {guild_id}",
            channel.guild_id
        ),
        Channel::Private(_) => anyhow::bail!("log channel {channel_id} is not a guild channel"),
        _ => anyhow::bail!("log channel {channel_id} is not a guild channel"),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AutomodRestAction, DiscordRestTarget, NicknameFailureClass, REMEMBERED_REST_TARGET_TTL,
        RememberedDiscordTargets, RetryClass, classify_nickname_failure,
        classify_nickname_response, classify_retry_response,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn permanent_targets_block_until_ttl_and_transient_statuses_do_not() {
        let cache = RememberedDiscordTargets::default();
        let now = Instant::now();
        let channel = DiscordRestTarget::GuildChannel {
            guild: 10,
            channel: 20,
        };
        assert!(!cache.blocked(channel, now));
        cache.remember_permanent(channel, now);
        assert!(cache.blocked(channel, now));
        assert!(!cache.blocked(
            channel,
            now + REMEMBERED_REST_TARGET_TTL + Duration::from_secs(1)
        ));
        assert_eq!(classify_retry_response(403, 50013), RetryClass::Permanent);
        assert_eq!(classify_retry_response(404, 10003), RetryClass::Permanent);
        assert_eq!(classify_retry_response(429, 0), RetryClass::Transient);
        assert_eq!(classify_retry_response(503, 0), RetryClass::Transient);
        let automod = DiscordRestTarget::AutomodAction {
            guild: 10,
            action: AutomodRestAction::Delete,
        };
        assert!(!cache.blocked(automod, now));
    }

    #[test]
    fn remembered_targets_clear_per_guild_and_on_config_reset() {
        let cache = RememberedDiscordTargets::default();
        let now = Instant::now();
        let a = DiscordRestTarget::GuildChannel {
            guild: 1,
            channel: 2,
        };
        let b = DiscordRestTarget::GuildChannel {
            guild: 3,
            channel: 4,
        };
        cache.remember_permanent(a, now);
        cache.remember_permanent(b, now);
        cache.clear_guild(1);
        assert!(!cache.blocked(a, now));
        assert!(cache.blocked(b, now));
        cache.clear_all();
        assert!(!cache.blocked(b, now));
    }

    #[test]
    fn transient_deadlines_grow_with_jitter_and_stop_at_one_hour() {
        let mut retry = super::RestRetry::default();
        let now = std::time::Instant::now();
        let mut previous = 0;
        for _ in 0..16 {
            retry.fail(super::RetryClass::Transient, now, 30);
            let wait = retry.next.unwrap().duration_since(now).as_secs();
            assert!((30..=3600).contains(&wait));
            assert!(wait >= previous);
            previous = wait;
            assert!(!retry.due(now + std::time::Duration::from_secs(5)));
            assert!(retry.due(now + std::time::Duration::from_secs(3601)));
        }
        assert_eq!(previous, 3600);
        retry.fail(super::RetryClass::Unauthorized, now, 30);
        assert!(!retry.due(now + std::time::Duration::from_secs(7200)));
    }

    #[test]
    fn nickname_failure_lifecycle_classification() {
        assert_eq!(
            classify_nickname_response(404, 10004).0,
            NicknameFailureClass::Dormant
        );
        assert_eq!(
            classify_nickname_response(403, 50001).0,
            NicknameFailureClass::Dormant
        );
        assert_eq!(
            classify_nickname_response(403, 50013).0,
            NicknameFailureClass::Terminal
        );
        assert_eq!(
            classify_nickname_response(404, 10007).0,
            NicknameFailureClass::Terminal
        );
        assert_eq!(
            classify_nickname_response(401, 0).0,
            NicknameFailureClass::Terminal
        );
        assert_eq!(
            classify_nickname_response(400, 50035).0,
            NicknameFailureClass::Terminal
        );
        assert_eq!(
            classify_nickname_response(408, 0).0,
            NicknameFailureClass::Transient
        );
        assert_eq!(
            classify_nickname_response(429, 0).0,
            NicknameFailureClass::Transient
        );
        assert_eq!(
            classify_nickname_response(503, 0).0,
            NicknameFailureClass::Transient
        );
        assert_eq!(
            classify_nickname_failure(&serenity::Error::Http(
                serenity::http::HttpError::RateLimitUtf8,
            ))
            .0,
            NicknameFailureClass::Transient
        );
        let json_error = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        assert_eq!(
            classify_nickname_failure(&serenity::Error::Json(json_error)).0,
            NicknameFailureClass::Transient
        );
    }
}
