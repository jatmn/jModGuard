//! Shared Discord REST helpers.

use anyhow::{Result, anyhow};
use parking_lot::Mutex;
use serenity::model::channel::Channel;
use serenity::model::id::ChannelId;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
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
    member: u64,
}

#[derive(Clone, Copy)]
struct RecoverySlot {
    generation: u64,
    touched: Instant,
}

#[derive(Default)]
struct RememberedDiscordState {
    expires: HashMap<DiscordRestTarget, Instant>,
    global_recovery: u64,
    // Unique guild recovery generations and the fallback for reclaimed slots.
    // Neither resets on clear_all; ordinary clears leave unrelated guilds alone.
    guild_generation: u64,
    guild_generation_floor: u64,
    guild_recovery: HashMap<u64, RecoverySlot>,
    member_moderation_recovery: HashMap<(u64, u64), RecoverySlot>,
}

/// Process-local memory of permanent Discord REST rejections (invalid-request budget).
pub(crate) struct RememberedDiscordTargets {
    state: Mutex<RememberedDiscordState>,
    /// Monotonic member generation assigned on role recovery; never reset.
    member_moderation_generation: AtomicU64,
    /// Lower bound for members without a slot; raised when slots expire or are evicted.
    member_moderation_generation_floor: AtomicU64,
}

impl Default for RememberedDiscordTargets {
    fn default() -> Self {
        Self {
            state: Mutex::new(RememberedDiscordState::default()),
            member_moderation_generation: AtomicU64::new(0),
            member_moderation_generation_floor: AtomicU64::new(0),
        }
    }
}

impl RememberedDiscordTargets {
    fn raise_member_generation_floor(&self, generation: u64) {
        self.member_moderation_generation_floor
            .fetch_max(generation, Ordering::SeqCst);
        self.member_moderation_generation_floor.fetch_max(
            self.member_moderation_generation.load(Ordering::SeqCst),
            Ordering::SeqCst,
        );
    }

    fn recovery_epoch_matches(
        &self,
        state: &RememberedDiscordState,
        target: DiscordRestTarget,
        epoch: RecoveryEpoch,
    ) -> bool {
        let guild = target.guild_id();
        if state.global_recovery != epoch.global
            || Self::guild_generation(state, guild) != epoch.guild
        {
            return false;
        }
        match target {
            DiscordRestTarget::AutomodMember {
                guild: g,
                user: u,
                action:
                    AutomodRestAction::Kick | AutomodRestAction::Ban | AutomodRestAction::Timeout,
            } => self.member_moderation_generation(state, g, u) == epoch.member,
            _ => true,
        }
    }

    fn guild_generation(state: &RememberedDiscordState, guild: u64) -> u64 {
        state
            .guild_recovery
            .get(&guild)
            .map(|slot| slot.generation)
            .unwrap_or(state.guild_generation_floor)
    }

    fn member_moderation_generation(
        &self,
        state: &RememberedDiscordState,
        guild: u64,
        user: u64,
    ) -> u64 {
        state
            .member_moderation_recovery
            .get(&(guild, user))
            .map(|slot| slot.generation)
            .unwrap_or_else(|| {
                self.member_moderation_generation_floor
                    .load(Ordering::SeqCst)
            })
    }

    fn prune_recovery_metadata(&self, state: &mut RememberedDiscordState, now: Instant) {
        state.guild_recovery.retain(|_, slot| {
            if slot.touched + REMEMBERED_REST_TARGET_TTL > now {
                return true;
            }
            state.guild_generation_floor = state.guild_generation_floor.max(slot.generation);
            false
        });
        let expired_members: Vec<_> = state
            .member_moderation_recovery
            .iter()
            .filter(|(_, slot)| slot.touched + REMEMBERED_REST_TARGET_TTL <= now)
            .map(|(_, slot)| slot.generation)
            .collect();
        for generation in expired_members {
            self.raise_member_generation_floor(generation);
        }
        state
            .member_moderation_recovery
            .retain(|_, slot| slot.touched + REMEMBERED_REST_TARGET_TTL > now);
    }

    fn evict_oldest_recovery_slot<K: Copy + Eq + std::hash::Hash>(
        map: &mut HashMap<K, RecoverySlot>,
        generation_floor: &mut u64,
    ) {
        if let Some(key) = map
            .iter()
            .min_by_key(|(_, slot)| slot.touched)
            .map(|(key, _)| *key)
            && let Some(slot) = map.remove(&key)
        {
            *generation_floor = (*generation_floor).max(slot.generation);
        }
    }

    fn evict_oldest_member_recovery_slot(&self, state: &mut RememberedDiscordState) {
        let key = state
            .member_moderation_recovery
            .iter()
            .min_by_key(|(_, slot)| slot.touched)
            .map(|(key, _)| *key);
        if let Some(key) = key
            && let Some(slot) = state.member_moderation_recovery.remove(&key)
        {
            self.raise_member_generation_floor(slot.generation);
        }
    }

    fn insert_recovery_slot<K: Copy + Eq + std::hash::Hash>(
        map: &mut HashMap<K, RecoverySlot>,
        key: K,
        generation: u64,
        now: Instant,
        generation_floor: &mut u64,
    ) {
        if map.len() >= MAX_REMEMBERED_REST_TARGETS && !map.contains_key(&key) {
            Self::evict_oldest_recovery_slot(map, generation_floor);
        }
        map.insert(
            key,
            RecoverySlot {
                generation,
                touched: now,
            },
        );
    }

    fn insert_member_recovery_slot(
        &self,
        state: &mut RememberedDiscordState,
        guild: u64,
        user: u64,
        generation: u64,
        now: Instant,
    ) {
        let key = (guild, user);
        if state.member_moderation_recovery.len() >= MAX_REMEMBERED_REST_TARGETS
            && !state.member_moderation_recovery.contains_key(&key)
        {
            self.evict_oldest_member_recovery_slot(state);
        }
        state.member_moderation_recovery.insert(
            key,
            RecoverySlot {
                generation,
                touched: now,
            },
        );
    }

    fn bump_guild_recovery(&self, state: &mut RememberedDiscordState, guild: u64, now: Instant) {
        self.prune_recovery_metadata(state, now);
        state.guild_generation += 1;
        Self::insert_recovery_slot(
            &mut state.guild_recovery,
            guild,
            state.guild_generation,
            now,
            &mut state.guild_generation_floor,
        );
    }

    fn bump_member_moderation_recovery(
        &self,
        state: &mut RememberedDiscordState,
        guild: u64,
        user: u64,
        now: Instant,
    ) {
        self.prune_recovery_metadata(state, now);
        let next = self
            .member_moderation_generation
            .fetch_add(1, Ordering::SeqCst)
            + 1;
        self.raise_member_generation_floor(next);
        self.insert_member_recovery_slot(state, guild, user, next, now);
    }

    fn touch_member_moderation_slot(
        &self,
        state: &mut RememberedDiscordState,
        guild: u64,
        user: u64,
        now: Instant,
    ) {
        self.prune_recovery_metadata(state, now);
        let key = (guild, user);
        if let Some(slot) = state.member_moderation_recovery.get_mut(&key) {
            slot.touched = now;
            return;
        }
        let generation = self
            .member_moderation_generation_floor
            .load(Ordering::SeqCst);
        self.insert_member_recovery_slot(state, guild, user, generation, now);
    }

    fn insert_permanent(
        &self,
        state: &mut RememberedDiscordState,
        target: DiscordRestTarget,
        now: Instant,
    ) {
        self.prune_recovery_metadata(state, now);
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
        if let DiscordRestTarget::AutomodMember {
            guild,
            user,
            action: AutomodRestAction::Kick | AutomodRestAction::Ban | AutomodRestAction::Timeout,
        } = target
        {
            self.touch_member_moderation_slot(state, guild, user, now);
        }
    }

    pub(crate) fn try_remember_permanent(
        &self,
        since: RecoveryEpoch,
        target: DiscordRestTarget,
        now: Instant,
    ) -> bool {
        let mut state = self.state.lock();
        if !self.recovery_epoch_matches(&state, target, since) {
            return false;
        }
        self.insert_permanent(&mut state, target, now);
        true
    }

    pub(crate) fn capture_recovery_epoch(&self, target: DiscordRestTarget) -> RecoveryEpoch {
        let guild = target.guild_id();
        let state = self.state.lock();
        let member = match target {
            DiscordRestTarget::AutomodMember {
                guild: g,
                user: u,
                action:
                    AutomodRestAction::Kick | AutomodRestAction::Ban | AutomodRestAction::Timeout,
            } => self.member_moderation_generation(&state, g, u),
            _ => 0,
        };
        RecoveryEpoch {
            global: state.global_recovery,
            guild: Self::guild_generation(&state, guild),
            member,
        }
    }

    pub(crate) fn blocked(&self, target: DiscordRestTarget, now: Instant) -> bool {
        let mut state = self.state.lock();
        self.prune_recovery_metadata(&mut state, now);
        state.expires.retain(|_, until| *until > now);
        state.expires.get(&target).is_some_and(|until| *until > now)
    }

    pub(crate) fn forget(&self, target: DiscordRestTarget) {
        self.state.lock().expires.remove(&target);
    }

    /// Role or permission changes can unblock kick, ban, or timeout for a member.
    pub(crate) fn clear_member_moderation_rejections(&self, guild: u64, user: u64) {
        let now = Instant::now();
        let mut state = self.state.lock();
        self.prune_recovery_metadata(&mut state, now);
        state.expires.retain(|target, _| {
            !matches!(
                target,
                DiscordRestTarget::AutomodMember {
                    guild: g,
                    user: u,
                    action: AutomodRestAction::Kick
                        | AutomodRestAction::Ban
                        | AutomodRestAction::Timeout,
                } if *g == guild && *u == user
            )
        });
        self.bump_member_moderation_recovery(&mut state, guild, user, now);
    }

    pub(crate) fn remember_rest_rejection(
        &self,
        since: RecoveryEpoch,
        error: &serenity::Error,
        target: DiscordRestTarget,
        now: Instant,
    ) {
        if should_remember_rest_rejection(error, target) {
            self.try_remember_permanent(since, target, now);
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
        let remember = if let Some(error) = error.downcast_ref::<serenity::Error>() {
            should_remember_rest_rejection(error, target)
        } else {
            classify_retry_error(error) == RetryClass::Permanent
                && matches!(target, DiscordRestTarget::AutomodMember { .. })
        };
        if remember {
            self.try_remember_permanent(since, target, now);
        }
    }

    pub(crate) fn clear_guild(&self, guild: u64) {
        let now = Instant::now();
        let mut state = self.state.lock();
        self.bump_guild_recovery(&mut state, guild, now);
        state
            .member_moderation_recovery
            .retain(|(g, _), _| *g != guild);
        state.expires.retain(|target, _| match target {
            DiscordRestTarget::GuildChannel { guild: g, .. } => *g != guild,
            DiscordRestTarget::AutomodDelete { guild: g, .. } => *g != guild,
            DiscordRestTarget::AutomodMember { guild: g, .. } => *g != guild,
        });
    }

    pub(crate) fn clear_all(&self) {
        let mut state = self.state.lock();
        state.global_recovery += 1;
        state.guild_recovery.clear();
        state.member_moderation_recovery.clear();
        state.expires.clear();
    }

    #[cfg(test)]
    fn recovery_metadata_counts(&self) -> (usize, usize) {
        let state = self.state.lock();
        (
            state.guild_recovery.len(),
            state.member_moderation_recovery.len(),
        )
    }

    #[cfg(test)]
    fn seed_guild_recovery_slot(&self, guild: u64, generation: u64, touched: Instant) {
        let mut state = self.state.lock();
        let state = &mut *state;
        Self::insert_recovery_slot(
            &mut state.guild_recovery,
            guild,
            generation,
            touched,
            &mut state.guild_generation_floor,
        );
    }

    #[cfg(test)]
    fn seed_member_recovery_slot(&self, guild: u64, user: u64, generation: u64, touched: Instant) {
        let mut state = self.state.lock();
        self.insert_member_recovery_slot(&mut state, guild, user, generation, touched);
    }

    #[cfg(test)]
    fn prune_recovery_metadata_for_test(&self, now: Instant) {
        let mut state = self.state.lock();
        self.prune_recovery_metadata(&mut state, now);
    }

    #[cfg(test)]
    fn member_moderation_generation_floor(&self) -> u64 {
        self.member_moderation_generation_floor
            .load(Ordering::SeqCst)
    }

    #[cfg(test)]
    fn member_recovery_slot_present(&self, guild: u64, user: u64) -> bool {
        self.state
            .lock()
            .member_moderation_recovery
            .contains_key(&(guild, user))
    }

    #[cfg(test)]
    pub(crate) fn remember_permanent_for_test(&self, target: DiscordRestTarget, now: Instant) {
        let mut state = self.state.lock();
        self.insert_permanent(&mut state, target, now);
    }

    #[cfg(test)]
    pub(crate) fn try_remember_permanent_with_recheck_after_gap(
        &self,
        since: RecoveryEpoch,
        target: DiscordRestTarget,
        now: Instant,
        during_gap: impl FnOnce(&Self),
    ) -> bool {
        {
            let state = self.state.lock();
            if !self.recovery_epoch_matches(&state, target, since) {
                return false;
            }
        }
        during_gap(self);
        let mut state = self.state.lock();
        if !self.recovery_epoch_matches(&state, target, since) {
            return false;
        }
        self.insert_permanent(&mut state, target, now);
        true
    }

    #[cfg(test)]
    pub(crate) fn insert_permanent_after_stale_epoch_check_for_test(
        &self,
        since: RecoveryEpoch,
        target: DiscordRestTarget,
        now: Instant,
        during_gap: impl FnOnce(&Self),
    ) {
        {
            let state = self.state.lock();
            if !self.recovery_epoch_matches(&state, target, since) {
                return;
            }
        }
        during_gap(self);
        let mut state = self.state.lock();
        self.insert_permanent(&mut state, target, now);
    }
}

fn destination_level_channel_failure(error: &serenity::Error) -> bool {
    if matches!(error, serenity::Error::Model(_)) {
        return false;
    }
    http_error_response(error).is_some_and(|(_, code)| matches!(code, 50001 | 50013 | 10003))
}

fn destination_level_automod_delete_failure(error: &serenity::Error) -> bool {
    http_error_response(error).is_some_and(|(_, code)| matches!(code, 50001 | 50013 | 10003))
}

/// Permanent errors that apply only to one message/member must not park unrelated work.
pub(crate) fn should_remember_rest_rejection(
    error: &serenity::Error,
    target: DiscordRestTarget,
) -> bool {
    if classify_retry(error) != RetryClass::Permanent {
        return false;
    }
    match target {
        DiscordRestTarget::GuildChannel { .. } => destination_level_channel_failure(error),
        DiscordRestTarget::AutomodDelete { .. } => destination_level_automod_delete_failure(error),
        DiscordRestTarget::AutomodMember { .. } => {
            let code = http_error_response(error)
                .map(|(_, code)| code)
                .unwrap_or(0);
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
        AutomodRestAction, DiscordRestTarget, NicknameFailureClass, REMEMBERED_REST_TARGET_TTL,
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
        };
        assert!(!cache.blocked(delete, now));
    }

    #[tokio::test]
    async fn resource_specific_automod_errors_are_not_remembered() {
        use super::should_remember_rest_rejection;
        let delete = DiscordRestTarget::AutomodDelete {
            guild: 10,
            channel: 1,
        };
        let other_channel = DiscordRestTarget::AutomodDelete {
            guild: 10,
            channel: 9,
        };
        let cache = RememberedDiscordTargets::default();
        let now = Instant::now();
        let missing = fixture_discord_http_error(404, 10008).await;
        let epoch = cache.capture_recovery_epoch(delete);
        cache.remember_rest_rejection(epoch, &missing, delete, now);
        assert!(!cache.blocked(delete, now));
        assert!(!cache.blocked(other_channel, now));
        assert!(!should_remember_rest_rejection(&missing, delete));
        let denied = fixture_discord_http_error(403, 50013).await;
        assert!(should_remember_rest_rejection(&denied, delete));
    }

    #[tokio::test]
    async fn automod_delete_permission_failure_blocks_the_whole_channel() {
        let cache = RememberedDiscordTargets::default();
        let now = Instant::now();
        let channel = DiscordRestTarget::AutomodDelete {
            guild: 10,
            channel: 20,
        };
        let other_channel = DiscordRestTarget::AutomodDelete {
            guild: 10,
            channel: 21,
        };
        let denied = fixture_discord_http_error(403, 50013).await;
        let epoch = cache.capture_recovery_epoch(channel);
        cache.remember_rest_rejection(epoch, &denied, channel, now);
        assert!(cache.blocked(channel, now));
        assert!(!cache.blocked(other_channel, now));
    }

    #[tokio::test]
    async fn invalid_form_body_does_not_block_guild_channel() {
        use super::should_remember_rest_rejection;
        let cache = RememberedDiscordTargets::default();
        let now = Instant::now();
        let target = DiscordRestTarget::GuildChannel {
            guild: 90,
            channel: 91,
        };
        let invalid = fixture_discord_http_error(400, 50035).await;
        assert!(!should_remember_rest_rejection(&invalid, target));
        let epoch = cache.capture_recovery_epoch(target);
        cache.remember_rest_rejection(epoch, &invalid, target, now);
        assert!(!cache.blocked(target, now));
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
    async fn late_member_rejection_after_role_recovery_does_not_remember() {
        let cache = RememberedDiscordTargets::default();
        let now = Instant::now();
        let target = DiscordRestTarget::AutomodMember {
            guild: 9,
            user: 8,
            action: AutomodRestAction::Kick,
        };
        let epoch = cache.capture_recovery_epoch(target);
        let deny = fixture_discord_http_error(403, 50013).await;
        cache.clear_member_moderation_rejections(9, 8);
        cache.remember_rest_rejection(epoch, &deny, target, now);
        assert!(!cache.blocked(target, now));
    }

    #[tokio::test]
    async fn stale_in_flight_rejection_fails_after_role_clear_and_slot_eviction() {
        const CAP: usize = 4096;
        let cache = RememberedDiscordTargets::default();
        let now = Instant::now();
        let stale_touch = now - Duration::from_secs(3600);
        let victim = DiscordRestTarget::AutomodMember {
            guild: 5,
            user: 1,
            action: AutomodRestAction::Kick,
        };
        let epoch = cache.capture_recovery_epoch(victim);
        assert_eq!(epoch.member, 0);
        cache.clear_member_moderation_rejections(5, 1);
        cache.seed_member_recovery_slot(5, 1, 1, stale_touch);
        for user in 2..(CAP + 2) {
            cache.seed_member_recovery_slot(5, user as u64, 1, now);
        }
        assert!(!cache.member_recovery_slot_present(5, 1));
        assert!(cache.member_moderation_generation_floor() >= 1);
        let deny = fixture_discord_http_error(403, 50013).await;
        cache.remember_rest_rejection(epoch, &deny, victim, now);
        assert!(!cache.blocked(victim, now));
    }

    #[test]
    fn member_capacity_eviction_alone_raises_generation_floor() {
        let cache = RememberedDiscordTargets::default();
        let now = Instant::now();
        let victim = DiscordRestTarget::AutomodMember {
            guild: 3,
            user: 1,
            action: AutomodRestAction::Kick,
        };
        let epoch = cache.capture_recovery_epoch(victim);
        // Seed without a role clear: only eviction can raise the floor.
        cache.seed_member_recovery_slot(3, 1, 7, now - Duration::from_secs(1));
        for user in 2..=super::MAX_REMEMBERED_REST_TARGETS {
            cache.seed_member_recovery_slot(3, user as u64, 1, now);
        }
        assert_eq!(cache.member_moderation_generation_floor(), 0);
        cache.seed_member_recovery_slot(3, 5000, 1, now);
        assert!(!cache.member_recovery_slot_present(3, 1));
        assert_eq!(cache.member_moderation_generation_floor(), 7);
        assert!(!cache.try_remember_permanent(epoch, victim, now));
        assert!(!cache.blocked(victim, now));
    }

    #[test]
    fn member_ttl_expiry_alone_raises_generation_floor() {
        let cache = RememberedDiscordTargets::default();
        let now = Instant::now();
        let victim = DiscordRestTarget::AutomodMember {
            guild: 3,
            user: 1,
            action: AutomodRestAction::Kick,
        };
        let epoch = cache.capture_recovery_epoch(victim);
        // Below capacity, with no role clear or insertion to raise the floor.
        cache.seed_member_recovery_slot(3, 1, 7, now - REMEMBERED_REST_TARGET_TTL);
        assert_eq!(cache.member_moderation_generation_floor(), 0);
        assert!(!cache.blocked(victim, now));
        assert!(!cache.member_recovery_slot_present(3, 1));
        assert_eq!(cache.member_moderation_generation_floor(), 7);
        assert!(!cache.try_remember_permanent(epoch, victim, now));
        assert!(!cache.blocked(victim, now));
    }

    #[tokio::test]
    async fn guild_slot_reclamation_does_not_restore_stale_rejections() {
        let deny = fixture_discord_http_error(403, 50013).await;
        for expire in [false, true] {
            let cache = RememberedDiscordTargets::default();
            let target = DiscordRestTarget::GuildChannel {
                guild: 1,
                channel: 2,
            };
            let original = cache.capture_recovery_epoch(target);
            cache.clear_guild(1);
            let stale = cache.capture_recovery_epoch(target);
            cache.clear_guild(1);
            let now = Instant::now();
            if expire {
                // Advance cleanup time, without sleeping for the 30-minute TTL.
                cache.prune_recovery_metadata_for_test(now + REMEMBERED_REST_TARGET_TTL);
                assert_eq!(cache.recovery_metadata_counts().0, 0);
            } else {
                for guild in 2..=(super::MAX_REMEMBERED_REST_TARGETS + 1) {
                    cache.clear_guild(guild as u64);
                }
                assert_eq!(
                    cache.recovery_metadata_counts().0,
                    super::MAX_REMEMBERED_REST_TARGETS
                );
            }
            assert!(!cache.state.lock().guild_recovery.contains_key(&1));
            cache.remember_rest_rejection(original, &deny, target, now);
            assert!(!cache.blocked(target, now), "missing slot, expiry={expire}");
            cache.clear_guild(1);
            cache.remember_rest_rejection(stale, &deny, target, now);
            assert!(!cache.blocked(target, now), "rebuilt slot, expiry={expire}");
            let fresh = cache.capture_recovery_epoch(target);
            assert!(fresh.guild > stale.guild);
            cache.remember_rest_rejection(fresh, &deny, target, now);
            assert!(
                cache.blocked(target, now),
                "fresh rejection, expiry={expire}"
            );
        }
    }

    #[tokio::test]
    async fn evicted_member_recovery_slot_does_not_restore_stale_generation() {
        const CAP: usize = 4096;
        let cache = RememberedDiscordTargets::default();
        let now = Instant::now();
        let stale = now - Duration::from_secs(3600);
        let victim = DiscordRestTarget::AutomodMember {
            guild: 3,
            user: 1,
            action: AutomodRestAction::Kick,
        };
        cache.seed_member_recovery_slot(3, 1, 4, stale);
        let epoch = cache.capture_recovery_epoch(victim);
        assert_eq!(epoch.member, 4);
        for user in 2..(CAP + 2) {
            cache.seed_member_recovery_slot(3, user as u64, 1, now);
        }
        assert!(!cache.member_recovery_slot_present(3, 1));
        let deny = fixture_discord_http_error(403, 50013).await;
        cache.clear_member_moderation_rejections(3, 1);
        cache.remember_rest_rejection(epoch, &deny, victim, now);
        assert!(!cache.blocked(victim, now));
    }

    #[tokio::test]
    async fn recovery_metadata_maps_are_bounded_at_capacity() {
        const CAP: usize = 4096;
        let cache = RememberedDiscordTargets::default();
        let now = Instant::now();
        for guild in 0..(CAP + 64) {
            cache.clear_guild(guild as u64);
        }
        let (guilds, _) = cache.recovery_metadata_counts();
        assert!(guilds <= CAP);
        let deny = fixture_discord_http_error(403, 50013).await;
        for user in 0..(CAP + 64) {
            let member = DiscordRestTarget::AutomodMember {
                guild: 2,
                user: user as u64,
                action: AutomodRestAction::Ban,
            };
            let epoch = cache.capture_recovery_epoch(member);
            cache.remember_rest_rejection(epoch, &deny, member, now);
        }
        let (_, members) = cache.recovery_metadata_counts();
        assert!(members <= CAP);
    }

    #[test]
    fn recovery_metadata_pruned_after_ttl() {
        let cache = RememberedDiscordTargets::default();
        let touched = Instant::now() - REMEMBERED_REST_TARGET_TTL - Duration::from_secs(1);
        cache.seed_guild_recovery_slot(7, 3, touched);
        cache.seed_member_recovery_slot(8, 9, 2, touched);
        cache.prune_recovery_metadata_for_test(Instant::now());
        assert_eq!(cache.recovery_metadata_counts(), (0, 0));
    }

    #[test]
    fn many_distinct_member_role_clears_keep_recovery_maps_bounded() {
        const CAP: usize = 4096;
        let cache = RememberedDiscordTargets::default();
        for user in 0..10_000u64 {
            cache.clear_member_moderation_rejections(1, user);
        }
        let (guilds, members) = cache.recovery_metadata_counts();
        assert_eq!(guilds, 0);
        assert_eq!(members, CAP);
    }

    #[test]
    fn clear_all_resets_guild_recovery_metadata() {
        let cache = RememberedDiscordTargets::default();
        cache.clear_guild(99);
        assert_eq!(cache.recovery_metadata_counts().0, 1);
        cache.clear_all();
        assert_eq!(cache.recovery_metadata_counts(), (0, 0));
        let target = DiscordRestTarget::GuildChannel {
            guild: 99,
            channel: 1,
        };
        assert_eq!(cache.capture_recovery_epoch(target).guild, 0);
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
    fn atomic_try_remember_holds_lock_between_epoch_check_and_insert() {
        let cache = RememberedDiscordTargets::default();
        let now = Instant::now();
        let target = DiscordRestTarget::GuildChannel {
            guild: 61,
            channel: 62,
        };
        let epoch = cache.capture_recovery_epoch(target);
        assert!(cache.try_remember_permanent(epoch, target, now));
        assert!(cache.blocked(target, now));
    }

    #[test]
    fn clear_between_epoch_check_and_insert_does_not_remember_with_atomic_path() {
        let cache = RememberedDiscordTargets::default();
        let now = Instant::now();
        let target = DiscordRestTarget::GuildChannel {
            guild: 71,
            channel: 72,
        };
        let epoch = cache.capture_recovery_epoch(target);
        let inserted =
            cache.try_remember_permanent_with_recheck_after_gap(epoch, target, now, |cache| {
                cache.clear_guild(71)
            });
        assert!(!inserted);
        assert!(!cache.blocked(target, now));
    }

    #[test]
    fn clear_between_epoch_check_and_insert_would_remember_without_recheck() {
        let cache = RememberedDiscordTargets::default();
        let now = Instant::now();
        let target = DiscordRestTarget::GuildChannel {
            guild: 81,
            channel: 82,
        };
        let epoch = cache.capture_recovery_epoch(target);
        cache.insert_permanent_after_stale_epoch_check_for_test(epoch, target, now, |cache| {
            cache.clear_guild(81)
        });
        assert!(cache.blocked(target, now));
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
