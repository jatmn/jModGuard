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
    AutomodDelete {
        guild: u64,
        channel: u64,
        message: u64,
    },
    AutomodMember {
        guild: u64,
        user: u64,
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

impl DiscordRestTarget {
    pub(crate) fn guild_id(self) -> u64 {
        match self {
            DiscordRestTarget::GuildChannel { guild, .. }
            | DiscordRestTarget::AutomodDelete { guild, .. }
            | DiscordRestTarget::AutomodMember { guild, .. } => guild,
        }
    }
}

/// Recovery invalidation captured before an outbound Discord call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RecoveryEpoch {
    global: u64,
    guild: u64,
}

#[derive(Default)]
struct RememberedDiscordState {
    expires: HashMap<DiscordRestTarget, Instant>,
    global_recovery: u64,
    guild_recovery: HashMap<u64, u64>,
}

/// Process-local memory of permanent Discord REST rejections (invalid-request budget).
#[derive(Default)]
pub(crate) struct RememberedDiscordTargets {
    state: Mutex<RememberedDiscordState>,
}

impl RememberedDiscordTargets {
    pub(crate) fn capture_recovery_epoch(&self, target: DiscordRestTarget) -> RecoveryEpoch {
        let guild = target.guild_id();
        let state = self.state.lock();
        RecoveryEpoch {
            global: state.global_recovery,
            guild: state.guild_recovery.get(&guild).copied().unwrap_or(0),
        }
    }

    fn recovery_epoch_unchanged(&self, target: DiscordRestTarget, epoch: RecoveryEpoch) -> bool {
        let guild = target.guild_id();
        let state = self.state.lock();
        state.global_recovery == epoch.global
            && state.guild_recovery.get(&guild).copied().unwrap_or(0) == epoch.guild
    }

    pub(crate) fn blocked(&self, target: DiscordRestTarget, now: Instant) -> bool {
        let mut state = self.state.lock();
        state.expires.retain(|_, until| *until > now);
        state.expires.get(&target).is_some_and(|until| *until > now)
    }

    fn remember_permanent(&self, target: DiscordRestTarget, now: Instant) {
        let mut state = self.state.lock();
        state.expires.retain(|_, until| *until > now);
        if state.expires.len() >= MAX_REMEMBERED_REST_TARGETS
            && !state.expires.contains_key(&target)
            && let Some(oldest) = state
                .expires
                .iter()
                .min_by_key(|(_, until)| *until)
                .map(|(k, _)| *k)
        {
            state.expires.remove(&oldest);
        }
        state
            .expires
            .insert(target, now + REMEMBERED_REST_TARGET_TTL);
    }

    pub(crate) fn forget(&self, target: DiscordRestTarget) {
        self.state.lock().expires.remove(&target);
    }

    pub(crate) fn remember_rest_rejection(
        &self,
        since: RecoveryEpoch,
        error: &serenity::Error,
        target: DiscordRestTarget,
        now: Instant,
    ) {
        if !self.recovery_epoch_unchanged(target, since) {
            return;
        }
        if should_remember_rest_rejection(error, target) {
            self.remember_permanent(target, now);
        }
    }

    pub(crate) fn observe_discord_error(
        &self,
        since: RecoveryEpoch,
        error: &serenity::Error,
        target: DiscordRestTarget,
        now: Instant,
    ) {
        self.remember_rest_rejection(since, error, target, now);
    }

    pub(crate) fn observe_anyhow_error(
        &self,
        since: RecoveryEpoch,
        error: &anyhow::Error,
        target: DiscordRestTarget,
        now: Instant,
    ) {
        if !self.recovery_epoch_unchanged(target, since) {
            return;
        }
        if let Some(error) = error.downcast_ref::<serenity::Error>() {
            if should_remember_rest_rejection(error, target) {
                self.remember_permanent(target, now);
            }
        } else if classify_retry_error(error) == RetryClass::Permanent {
            self.remember_permanent(target, now);
        }
    }

    pub(crate) fn clear_guild(&self, guild: u64) {
        let mut state = self.state.lock();
        let next = state.guild_recovery.get(&guild).copied().unwrap_or(0) + 1;
        state.guild_recovery.insert(guild, next);
        state.expires.retain(|target, _| match target {
            DiscordRestTarget::GuildChannel { guild: g, .. } => *g != guild,
            DiscordRestTarget::AutomodDelete { guild: g, .. } => *g != guild,
            DiscordRestTarget::AutomodMember { guild: g, .. } => *g != guild,
        });
    }

    pub(crate) fn clear_all(&self) {
        let mut state = self.state.lock();
        state.global_recovery += 1;
        state.expires.clear();
    }

    #[cfg(test)]
    pub(crate) fn remember_permanent_for_test(&self, target: DiscordRestTarget, now: Instant) {
        self.remember_permanent(target, now);
    }
}

/// Permanent errors that apply only to one message/member must not park unrelated work.
pub(crate) fn should_remember_rest_rejection(
    error: &serenity::Error,
    target: DiscordRestTarget,
) -> bool {
    if classify_retry(error) != RetryClass::Permanent {
        return false;
    }
    let code = http_error_response(error)
        .map(|(_, code)| code)
        .unwrap_or(0);
    match target {
        DiscordRestTarget::GuildChannel { .. } => !matches!(code, 10008),
        DiscordRestTarget::AutomodDelete { .. } => {
            !matches!(code, 10007 | 10008 | 10026 | 10029 | 50035)
        }
        DiscordRestTarget::AutomodMember { .. } => {
            !matches!(code, 10007 | 10008 | 10026 | 10029 | 50035)
        }
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
        DiscordRestTarget, NicknameFailureClass, REMEMBERED_REST_TARGET_TTL,
        RememberedDiscordTargets, RetryClass, classify_nickname_failure,
        classify_nickname_response, classify_retry_response,
    };
    use serenity::builder::CreateMessage;
    use serenity::model::id::ChannelId;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    async fn fixture_discord_http_error(status: u16, code: isize) -> serenity::Error {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let server = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut request = Vec::new();
                loop {
                    let mut buf = [0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                    if request.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                counter.fetch_add(1, Ordering::SeqCst);
                let body = format!(r#"{{"code":{code},"message":"fixture"}}"#);
                let response = format!(
                    "HTTP/1.1 {status} Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        let http = serenity::http::HttpBuilder::new("fixture")
            .proxy(format!("http://{address}"))
            .application_id(serenity::model::id::ApplicationId::new(1))
            .ratelimiter_disabled(true)
            .build();
        let err = ChannelId::new(1)
            .send_message(&http, CreateMessage::new().content("x"))
            .await
            .unwrap_err();
        server.abort();
        err
    }

    #[test]
    fn forget_clears_remembered_target_before_ttl() {
        let cache = RememberedDiscordTargets::default();
        let now = Instant::now();
        let channel = DiscordRestTarget::GuildChannel {
            guild: 1,
            channel: 2,
        };
        cache.remember_permanent_for_test(channel, now);
        assert!(cache.blocked(channel, now));
        cache.forget(channel);
        assert!(!cache.blocked(channel, now));
    }

    #[test]
    fn permanent_targets_block_until_ttl_and_transient_statuses_do_not() {
        let cache = RememberedDiscordTargets::default();
        let now = Instant::now();
        let channel = DiscordRestTarget::GuildChannel {
            guild: 10,
            channel: 20,
        };
        assert!(!cache.blocked(channel, now));
        cache.remember_permanent_for_test(channel, now);
        assert!(cache.blocked(channel, now));
        assert!(!cache.blocked(
            channel,
            now + REMEMBERED_REST_TARGET_TTL + Duration::from_secs(1)
        ));
        assert_eq!(classify_retry_response(403, 50013), RetryClass::Permanent);
        assert_eq!(classify_retry_response(404, 10003), RetryClass::Permanent);
        assert_eq!(classify_retry_response(429, 0), RetryClass::Transient);
        assert_eq!(classify_retry_response(503, 0), RetryClass::Transient);
        let delete = DiscordRestTarget::AutomodDelete {
            guild: 10,
            channel: 1,
            message: 2,
        };
        assert!(!cache.blocked(delete, now));
    }

    #[tokio::test]
    async fn resource_specific_automod_errors_are_not_remembered() {
        use super::should_remember_rest_rejection;
        let delete = DiscordRestTarget::AutomodDelete {
            guild: 10,
            channel: 1,
            message: 2,
        };
        let other = DiscordRestTarget::AutomodDelete {
            guild: 10,
            channel: 1,
            message: 3,
        };
        let cache = RememberedDiscordTargets::default();
        let now = Instant::now();
        let missing = fixture_discord_http_error(404, 10008).await;
        let epoch = cache.capture_recovery_epoch(delete);
        cache.remember_rest_rejection(epoch, &missing, delete, now);
        assert!(!cache.blocked(delete, now));
        assert!(!cache.blocked(other, now));
        assert!(!should_remember_rest_rejection(&missing, delete));
        let denied = fixture_discord_http_error(403, 50013).await;
        assert!(should_remember_rest_rejection(&denied, delete));
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
        cache.remember_permanent_for_test(a, now);
        cache.remember_permanent_for_test(b, now);
        cache.clear_guild(1);
        assert!(!cache.blocked(a, now));
        assert!(cache.blocked(b, now));
        cache.clear_all();
        assert!(!cache.blocked(b, now));
    }

    #[tokio::test]
    async fn clear_guild_races_ahead_of_late_delivery_forget() {
        use super::should_remember_rest_rejection;
        let cache = RememberedDiscordTargets::default();
        let now = Instant::now();
        let target = DiscordRestTarget::GuildChannel {
            guild: 5,
            channel: 6,
        };
        let deny = fixture_discord_http_error(403, 50013).await;
        assert!(should_remember_rest_rejection(&deny, target));
        let epoch = cache.capture_recovery_epoch(target);
        cache.remember_rest_rejection(epoch, &deny, target, now);
        assert!(cache.blocked(target, now));
        cache.clear_guild(5);
        assert!(!cache.blocked(target, now));
        cache.forget(target);
        assert!(!cache.blocked(target, now));
    }

    #[tokio::test]
    async fn recovery_clear_guild_during_in_flight_skips_remember() {
        let cache = RememberedDiscordTargets::default();
        let now = Instant::now();
        let target = DiscordRestTarget::GuildChannel {
            guild: 11,
            channel: 12,
        };
        let epoch = cache.capture_recovery_epoch(target);
        let deny = fixture_discord_http_error(403, 50013).await;
        cache.clear_guild(11);
        cache.remember_rest_rejection(epoch, &deny, target, now);
        assert!(!cache.blocked(target, now));
    }

    #[tokio::test]
    async fn recovery_clear_all_during_in_flight_skips_remember() {
        let cache = RememberedDiscordTargets::default();
        let now = Instant::now();
        let target = DiscordRestTarget::GuildChannel {
            guild: 21,
            channel: 22,
        };
        let epoch = cache.capture_recovery_epoch(target);
        let deny = fixture_discord_http_error(403, 50013).await;
        cache.clear_all();
        cache.remember_rest_rejection(epoch, &deny, target, now);
        assert!(!cache.blocked(target, now));
    }

    #[tokio::test]
    async fn recovery_unchanged_epoch_still_remembers_permanent_failure() {
        let cache = RememberedDiscordTargets::default();
        let now = Instant::now();
        let target = DiscordRestTarget::GuildChannel {
            guild: 31,
            channel: 32,
        };
        let epoch = cache.capture_recovery_epoch(target);
        let deny = fixture_discord_http_error(403, 50013).await;
        cache.remember_rest_rejection(epoch, &deny, target, now);
        assert!(cache.blocked(target, now));
    }

    #[tokio::test]
    async fn recovery_clear_one_guild_does_not_invalidate_another_guild_epoch() {
        let cache = RememberedDiscordTargets::default();
        let now = Instant::now();
        let guild_a = DiscordRestTarget::GuildChannel {
            guild: 41,
            channel: 1,
        };
        let guild_b = DiscordRestTarget::GuildChannel {
            guild: 42,
            channel: 2,
        };
        let epoch_b = cache.capture_recovery_epoch(guild_b);
        let deny = fixture_discord_http_error(403, 50013).await;
        cache.clear_guild(41);
        cache.remember_rest_rejection(epoch_b, &deny, guild_b, now);
        assert!(!cache.blocked(guild_a, now));
        assert!(cache.blocked(guild_b, now));
    }

    #[tokio::test]
    async fn recovery_double_clear_guild_still_skips_stale_epoch() {
        let cache = RememberedDiscordTargets::default();
        let now = Instant::now();
        let target = DiscordRestTarget::GuildChannel {
            guild: 51,
            channel: 52,
        };
        let epoch = cache.capture_recovery_epoch(target);
        let deny = fixture_discord_http_error(403, 50013).await;
        cache.clear_guild(51);
        cache.clear_guild(51);
        cache.remember_rest_rejection(epoch, &deny, target, now);
        assert!(!cache.blocked(target, now));
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
