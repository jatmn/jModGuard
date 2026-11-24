//! Automod engine: sliding-window state per (guild, user) with per-rule
//! strike buckets, 6 generic detection rules with per-rule action escalation,
//! whitelists, and dry-run mode.
//!
//! Buckets are pruned on a timer, and velocity history is bounded by time and
//! count. Lookups are O(1) per message (per-user bucket), never a scan of all
//! tracked users.

use dashmap::DashMap;
use parking_lot::Mutex;
use regex::Regex;
use serenity::client::Context;
use serenity::model::guild::Member;
use serenity::model::id::{ChannelId, GuildId, MessageId, UserId};
use serenity::model::prelude::Timestamp;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::Instant as AutomodInstant;

use crate::db::guild_data::CaseOutcome;
use crate::normalize;
use crate::state::{BotState, CachedGuildConfig};
use crate::types::{ActionDef, RULE_IDS, RuleDef};
use crate::{db, logging};

mod channel_reply;

pub struct AutomodEngine {
    /// (guild, user) -> window history and per-rule strike buckets
    state: DashMap<(u64, u64), AuthorState>,
    member_joins: DashMap<(u64, u64), Instant>,
    pub(crate) onboarding_matches: crate::onboarding_trap::RecentMatches,
    last_prune: Mutex<AutomodInstant>,
    replies: channel_reply::Replies,
}

#[derive(Default)]
struct AuthorState {
    /// Strike ladders keyed by (rule, dry-run). Dry-run strikes are simulated,
    /// so each mode keeps its own ladder: simulated strikes never use up live
    /// escalation, and live strikes never count toward dry-run.
    rules: HashMap<(&'static str, bool), RuleState>,
    // Normal traffic keeps the velocity window alive without refreshing strikes.
    velocity_events: VecDeque<AutomodInstant>,
}

impl AuthorState {
    /// Records a message at its arrival time. Evaluations can finish out of
    /// arrival order, so the history stays sorted rather than clamping a late
    /// message forward.
    fn record_velocity(&mut self, rule: &RuleDef, arrived: AutomodInstant) -> bool {
        // Retain enough history for the highest supported threshold so a
        // config update can raise the threshold without losing observations.
        if self.velocity_events.len() == crate::types::AUTOMOD_VELOCITY_MAX_MESSAGES + 1 {
            if self
                .velocity_events
                .front()
                .is_some_and(|oldest| arrived < *oldest)
            {
                // Delayed behind the whole retained history: too old to count.
                return false;
            }
            self.velocity_events.pop_front();
        }
        let index = self.velocity_events.partition_point(|at| *at <= arrived);
        self.velocity_events.insert(index, arrived);
        // Each message is judged on the messages that arrived before it, so a
        // strike always lands on a message that is itself over the limit. If
        // an earlier arrival is evaluated after a later one, the later
        // message's strike can be missed, but none is charged to the wrong one.
        self.velocity_over_limit(rule, index)
    }

    /// Counts the messages that arrived in the window ending at the message at
    /// `index`, including it. If the (max + 1)th of them is still inside the
    /// window, the rule is over its limit. Legacy thresholds above the
    /// supported limit have no such observation.
    fn velocity_over_limit(&self, rule: &RuleDef, index: usize) -> bool {
        let max = crate::types::velocity_max_messages(&rule.params);
        let window = velocity_window(rule);
        let arrived = self.velocity_events[index];
        usize::try_from(max)
            .ok()
            .and_then(|max| index.checked_sub(max))
            .and_then(|first| self.velocity_events.get(first))
            .is_some_and(|first| arrived.duration_since(*first) < window)
    }
}

fn velocity_window(rule: &RuleDef) -> Duration {
    Duration::from_secs(crate::types::velocity_window_secs(&rule.params))
}

#[derive(Default)]
struct RuleState {
    events: VecDeque<WindowEvent>,
    strikes: u32,
    fired: ThresholdReservations,
    last_velocity_fallback_delete: Option<AutomodInstant>,
    last_event: Option<AutomodInstant>,
}

/// Reservation is independent of whether the selected Discord action runs.
#[derive(Default)]
struct ThresholdReservations(HashMap<u32, String>);

#[derive(Clone, Copy, PartialEq, Eq)]
enum ThresholdState {
    Available,
    Reserved,
}

impl ThresholdReservations {
    fn state(&self, threshold: u32) -> ThresholdState {
        if self.0.contains_key(&threshold) {
            ThresholdState::Reserved
        } else {
            ThresholdState::Available
        }
    }

    fn reserve(&mut self, step: &ActionDef) {
        self.0.insert(step.at, step.action.clone());
    }

    fn reserved_for(&self, step: &ActionDef) -> bool {
        self.0.get(&step.at) == Some(&step.action)
    }

    fn release(&mut self, threshold: u32) {
        self.0.remove(&threshold);
    }
}

#[derive(PartialEq, Eq)]
enum RuleRetention {
    Retained,
    Expired,
}

/// Strike state lives for 10 minutes after the last strike. Cleanup and
/// strike recording both apply this, so expiry never waits for a cleanup tick.
const STRIKE_RETENTION: Duration = Duration::from_secs(crate::types::AUTOMOD_WINDOW_MAX_SECS);
const VELOCITY_FALLBACK_DELETE_INTERVAL: Duration = Duration::from_secs(1);

impl RuleState {
    fn velocity_fallback_available(&self, step: &ActionDef, now: AutomodInstant) -> bool {
        self.fired.reserved_for(step)
            && self
                .last_velocity_fallback_delete
                .is_none_or(|last| now.duration_since(last) >= VELOCITY_FALLBACK_DELETE_INTERVAL)
    }

    /// Starts a new ladder when the previous one expired.
    fn record_strike(&mut self, now: AutomodInstant, record_event: bool) -> u32 {
        // Events never postdate the last strike, so it alone decides expiry.
        if self
            .last_event
            .is_some_and(|last| now.duration_since(last) >= STRIKE_RETENTION)
        {
            *self = RuleState::default();
        }
        self.strikes += 1;
        // Concurrent evaluations can record strikes out of clock order; the
        // retention deadline never moves backwards.
        self.last_event = Some(self.last_event.map_or(now, |last| last.max(now)));
        if record_event {
            self.events.push_back(WindowEvent { at: now });
        }
        self.strikes
    }

    fn prune(&mut self, now: AutomodInstant, keep: Duration) -> RuleRetention {
        self.events.retain(|e| now.duration_since(e.at) < keep);
        if self
            .last_event
            .is_some_and(|t| now.duration_since(t) < keep)
            || !self.events.is_empty()
        {
            RuleRetention::Retained
        } else {
            RuleRetention::Expired
        }
    }
}

#[derive(Clone, Copy)]
struct WindowEvent {
    at: AutomodInstant,
}

/// Everything the engine needs to know about an incoming message.
pub struct MsgCtx<'a> {
    pub guild_id: u64,
    pub message_id: u64,
    pub channel_id: u64,
    pub author_id: u64,
    pub content: &'a str,
    pub attachment_count: usize,
    pub sticker_count: usize,
    pub everyone_mentions: usize,
    pub here_mentions: usize,
    pub role_mentions: usize,
    pub joined_secs_ago: Option<u64>,
    pub account_age_days: Option<u64>,
    pub member_roles: &'a [u64],
    pub is_moderator: bool,
    pub can_mention_everyone: bool,
    /// Set when the channel, active thread, or thread parent was not cached.
    /// The everyone-attempt rule still matches. Kick and ban are withheld.
    pub mention_permission_unknown: bool,
    /// When the bot received the message, before any configuration read.
    /// Velocity counts by it; callers without one use the evaluation time.
    pub received: Option<AutomodInstant>,
}

/// A rule fired and an action decided (highest severity across rules wins).
pub struct PendingAction {
    pub rule_id: String,
    pub rule_name: String,
    pub action: ActionDef,
    pub strikes: u32,
    pub evidence: serde_json::Value,
    /// The winning action is an everyone-attempt whose channel permission
    /// could not be resolved. Kick and ban delete the message and stop.
    pub mention_permission_unknown: bool,
    /// The rule as it was when the message matched. For a built-in rule, if its
    /// configuration cannot be re-read before acting, these settings still
    /// decide dry-run and exemptions. Custom rules snapshot the ordered actions
    /// here, but still require current configuration before acting.
    pub matched_rule: Option<RuleDef>,
    /// Whether the rule was in dry-run when this action was selected. Its
    /// strike ladder belongs to that mode, so execution never runs it live.
    pub dry_run: bool,
}

impl PendingAction {
    pub fn from_custom(
        rule_id: &str,
        rule: &RuleDef,
        primary: &ActionDef,
        evidence: serde_json::Value,
        mention_permission_unknown: bool,
    ) -> Self {
        Self {
            rule_id: rule_id.to_owned(),
            rule_name: rule.name.clone(),
            action: primary.clone(),
            strikes: 1,
            evidence,
            mention_permission_unknown,
            matched_rule: Some(rule.clone()),
            dry_run: rule.dry_run,
        }
    }

    fn actions(&self) -> &[ActionDef] {
        if jmodguard::custom_rules::is_custom_id(&self.rule_id)
            && let Some(rule) = &self.matched_rule
        {
            &rule.actions
        } else {
            std::slice::from_ref(&self.action)
        }
    }
}

static URL_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:https?://|www\.)[^\s<>]+|(?:discord\.gg|discord(?:app)?\.com/invite)/[^\s<>]+",
    )
    .unwrap()
});

static INVITE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?i)(?:discord\.gg/|discord(?:app)?\.com/invite/)([a-z0-9-]+)").unwrap()
});

impl Default for AutomodEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl AutomodEngine {
    pub fn new() -> Self {
        Self {
            state: DashMap::new(),
            member_joins: DashMap::new(),
            onboarding_matches: Default::default(),
            last_prune: Mutex::new(AutomodInstant::now()),
            replies: channel_reply::Replies::default(),
        }
    }

    pub fn member_joined(&self, guild: u64, user: u64) {
        self.member_joins.insert((guild, user), Instant::now());
    }

    pub fn member_joined_at(&self, guild: u64, user: u64) -> Option<Instant> {
        self.member_joins.get(&(guild, user)).map(|e| *e.value())
    }

    /// `evaluate` marks a strike fired before Discord runs. Release it only
    /// when this process did not attempt the ban, so a later message can try
    /// again. A retention-marker failure still bans and must not release.
    pub fn release_fired(&self, guild: u64, user: u64, rule_id: &str, dry_run: bool, at: u32) {
        let Some(mut bucket) = self.state.get_mut(&(guild, user)) else {
            return;
        };
        let Some(rule_id) = RULE_IDS.into_iter().find(|id| *id == rule_id) else {
            return;
        };
        if let Some(state) = bucket.rules.get_mut(&(rule_id, dry_run)) {
            state.fired.release(at);
        }
    }

    /// Removes stale state. Runs on a timer from the cleanup task.
    pub fn prune(&self) {
        let now = AutomodInstant::now();
        let keep = STRIKE_RETENTION;
        self.state.retain(|_, bucket| {
            bucket
                .rules
                .retain(|_, s| s.prune(now, keep) == RuleRetention::Retained);
            while bucket
                .velocity_events
                .front()
                .is_some_and(|at| now.duration_since(*at) >= keep)
            {
                bucket.velocity_events.pop_front();
            }
            if bucket.velocity_events.is_empty() {
                bucket.velocity_events = VecDeque::new();
            }
            !bucket.rules.is_empty() || !bucket.velocity_events.is_empty()
        });
        let joined_now = Instant::now();
        self.member_joins
            .retain(|_, t| joined_now.duration_since(*t) < Duration::from_secs(24 * 3600));
        self.onboarding_matches.prune();
        self.replies.prune(now);
        *self.last_prune.lock() = now;
    }

    /// Cheap periodic prune: only runs at most once a minute.
    pub fn maybe_prune(&self) {
        if self.last_prune.lock().elapsed() > Duration::from_secs(60) {
            self.prune();
        }
    }

    /// Evaluates all enabled rules for a message. Returns the pending action
    pub fn evaluate(&self, cfg: &CachedGuildConfig, ctx: &MsgCtx<'_>) -> Option<PendingAction> {
        let now = AutomodInstant::now();
        // Only velocity counts by arrival; other windows and strikes keep the
        // evaluation clock, which never runs backwards for one author.
        let arrived = ctx.received.unwrap_or(now);
        let normalized = normalize::normalize_lenient(ctx.content);
        let compact = normalize::normalize_compact(ctx.content);
        let has_url = URL_RE.is_match(ctx.content);
        let has_invite = INVITE_RE.is_match(ctx.content);

        let mut best: Option<PendingAction> = None;
        let mut velocity_fallback = None;
        for rule_id in RULE_IDS {
            let Some(rule) = cfg.rules.get(rule_id) else {
                continue;
            };
            if !rule.enabled {
                continue;
            }
            if jmodguard::custom_rules::rule_scope_skips(
                rule,
                ctx.author_id,
                ctx.channel_id,
                ctx.member_roles,
                ctx.is_moderator,
                true,
            ) {
                continue;
            }

            let triggered = match rule_id {
                "mention_spam" => self.trigger_mention_spam(rule, ctx, now),
                // Exact-image observations have a separate bounded log-only path.
                "attachment_flood" => false,
                "repeat_text" => false,
                "new_member_url" => {
                    ctx.joined_secs_ago
                        .map(|s| s <= param_u64(rule, "grace_secs", 120))
                        .unwrap_or(false)
                        && (has_url || has_invite)
                }
                "badword" => {
                    !cfg.regexes.is_empty()
                        && cfg.regexes.iter().any(|re| {
                            re.is_match(ctx.content)
                                || re.is_match(&normalized)
                                || re.is_match(&compact)
                        })
                }
                "known_spam" => cfg
                    .known_spam
                    .matches_forms(ctx.content, &normalized, &compact),
                "invite" => has_invite && !self.invite_allowed(rule, ctx.content),
                // Velocity observes each eligible message under the strike bucket lock.
                "velocity" => true,
                "account_age" => ctx
                    .account_age_days
                    .map(|d| d < param_u64(rule, "min_age_days", 3))
                    .unwrap_or(false),
                _ => false,
            };

            if !triggered {
                continue;
            }

            if rule_id == "known_spam" {
                let pending = PendingAction {
                    rule_id: rule_id.into(),
                    rule_name: rule.name.clone(),
                    action: jmodguard::known_spam::action(),
                    strikes: 1,
                    mention_permission_unknown: false,
                    evidence: serde_json::json!({ "content": ctx.content,
                        "channel_id": ctx.channel_id, "message_id": ctx.message_id }),
                    matched_rule: Some(rule.clone()),
                    dry_run: rule.dry_run,
                };
                // This list promises the same fixed one-hour action even when
                // another detector would also ban with a different window.
                if !rule.dry_run {
                    return Some(pending);
                }
                // A dry-run match is only logged, so it must not stop a live
                // rule from acting on the same message.
                if is_more_severe(&pending, best.as_ref()) {
                    best = Some(pending);
                }
                continue;
            }

            let mut bucket = self.state.entry((ctx.guild_id, ctx.author_id)).or_default();
            if rule_id == "velocity" && !bucket.record_velocity(rule, arrived) {
                continue;
            }
            let state = bucket.rules.entry((rule_id, rule.dry_run)).or_default();
            // Only mention spam reads past strike timestamps.
            let strikes = state.record_strike(now, rule_id == "mention_spam");
            let Some(step) = rule.action_for_strike(strikes) else {
                continue;
            };
            let mut fallback_step = None;
            let action = if state.fired.state(step.at) == ThresholdState::Available {
                state.fired.reserve(step);
                step.clone()
            } else {
                // Keep deliberate log/warn-only terminal steps and gaps in the ladder.
                if rule_id != "velocity"
                    || !matches!(step.action.as_str(), "delete" | "timeout" | "kick" | "ban")
                    || rule.actions.iter().any(|action| action.at > step.at)
                    || !state.velocity_fallback_available(step, AutomodInstant::now())
                {
                    continue;
                }
                fallback_step = Some(step);
                ActionDef {
                    at: step.at,
                    action: "delete".into(),
                    duration_minutes: 0,
                    delete_days: 0,
                    purge_last_hour: false,
                    reason: "Message velocity".into(),
                }
            };

            let pending = PendingAction {
                rule_id: rule_id.to_string(),
                rule_name: rule.name.clone(),
                action,
                strikes,
                mention_permission_unknown: false,
                evidence: serde_json::json!({
                    "content": truncate(ctx.content, 2000),
                    "channel_id": ctx.channel_id,
                    "message_id": ctx.message_id,
                    "attachments": ctx.attachment_count,
                    "mentions": ctx.everyone_mentions + ctx.here_mentions + ctx.role_mentions,
                }),
                matched_rule: Some(rule.clone()),
                dry_run: rule.dry_run,
            };
            if is_more_severe(&pending, best.as_ref()) {
                if let Some(step) = fallback_step {
                    // Keep the best alternative in case another message takes the cooldown.
                    velocity_fallback = Some((pending, step));
                } else {
                    best = Some(pending);
                }
            }
        }

        for (rule_id, rule) in &cfg.rules {
            if !jmodguard::custom_rules::is_custom_id(rule_id) || !rule.enabled {
                continue;
            }
            if jmodguard::custom_rules::rule_scope_skips(
                rule,
                ctx.author_id,
                ctx.channel_id,
                ctx.member_roles,
                ctx.is_moderator,
                true,
            ) {
                continue;
            }
            let kind = rule
                .params
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let triggered = match kind {
                "pattern" => cfg
                    .custom_patterns
                    .get(rule_id)
                    .map(|re| normalize::matches_badword(ctx.content, &normalized, re))
                    .unwrap_or(false),
                "word" => {
                    let words = jmodguard::custom_rules::custom_words(rule);
                    jmodguard::custom_rules::word_rule_matches(ctx.content, &words)
                }
                "channel_content" => {
                    use jmodguard::custom_rules::{channel_link_blocked, content_toggle};
                    (content_toggle(rule, "attachments") && ctx.attachment_count > 0)
                        || (content_toggle(rule, "stickers") && ctx.sticker_count > 0)
                        || (has_url
                            && URL_RE
                                .find_iter(ctx.content)
                                .any(|m| channel_link_blocked(rule, m.as_str())))
                }
                "everyone_attempt" => {
                    !ctx.can_mention_everyone
                        && jmodguard::custom_rules::everyone_attempt_matches(
                            ctx.content,
                            ctx.everyone_mentions > 0,
                        )
                }
                "honeypot_role" => false,
                _ => false,
            };
            if !triggered {
                continue;
            }
            let Some(action) = jmodguard::custom_rules::primary_custom_action(rule) else {
                continue;
            };
            let pending = PendingAction::from_custom(
                rule_id,
                rule,
                action,
                serde_json::json!({
                    "content": truncate(ctx.content, 2000),
                    "channel_id": ctx.channel_id,
                    "message_id": ctx.message_id,
                    "attachments": ctx.attachment_count,
                    "stickers": ctx.sticker_count,
                }),
                kind == "everyone_attempt" && ctx.mention_permission_unknown,
            );
            if is_more_severe(&pending, best.as_ref()) {
                best = Some(pending);
            }
        }
        // A fallback stored above already beat the earlier rules. It wins ties
        // with later rules, just as it would in the normal traversal.
        if let Some((pending, step)) = velocity_fallback
            && best
                .as_ref()
                .is_none_or(|best| !is_more_severe(best, Some(&pending)))
            && self.reserve_velocity_fallback((ctx.guild_id, ctx.author_id), pending.dry_run, step)
        {
            return Some(pending);
        }
        best
    }

    fn reserve_velocity_fallback(&self, key: (u64, u64), dry_run: bool, step: &ActionDef) -> bool {
        let Some(mut bucket) = self.state.get_mut(&key) else {
            return false;
        };
        let Some(state) = bucket.rules.get_mut(&("velocity", dry_run)) else {
            return false;
        };
        let now = AutomodInstant::now();
        if !state.velocity_fallback_available(step, now) {
            return false;
        }
        state.last_velocity_fallback_delete = Some(now);
        true
    }

    /// Counts this message's mentions plus recent mentions in the window.
    fn trigger_mention_spam(&self, rule: &RuleDef, ctx: &MsgCtx<'_>, now: AutomodInstant) -> bool {
        let window = Duration::from_secs(
            param_u64(rule, "window_secs", 10).clamp(1, crate::types::AUTOMOD_WINDOW_MAX_SECS),
        );
        let max_total = param_u64(rule, "max_mentions_in_window", 5) as usize;

        let mut total = ctx.everyone_mentions + ctx.here_mentions + ctx.role_mentions;
        if let Some(bucket) = self.state.get(&(ctx.guild_id, ctx.author_id)) {
            // Mention counts are detection history, shared by both modes.
            for ((rid, _), s) in bucket.rules.iter() {
                if *rid == "mention_spam" {
                    total += s
                        .events
                        .iter()
                        .filter(|e| now.duration_since(e.at) < window)
                        .count();
                }
            }
        }
        total > max_total
    }

    fn invite_allowed(&self, rule: &RuleDef, content: &str) -> bool {
        let allow: Vec<String> = rule
            .params
            .get("allowlist_codes")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        if allow.is_empty() {
            return false;
        }
        INVITE_RE.captures_iter(content).any(|c| {
            c.get(1)
                .map(|m| allow.iter().any(|a| a == m.as_str()))
                .unwrap_or(false)
        })
    }
}

// ---------------------------------------------------------------------------
// action execution
// ---------------------------------------------------------------------------

/// A live action outranks any dry-run one, so testing a rule in dry-run never
/// displaces another rule's enforcement on the same message.
fn is_more_severe(cand: &PendingAction, best: Option<&PendingAction>) -> bool {
    use jmodguard::custom_rules::action_rank;
    best.is_none_or(|b| {
        (!cand.dry_run, action_rank(&cand.action.action))
            > (!b.dry_run, action_rank(&b.action.action))
    })
}

fn param_u64(rule: &RuleDef, key: &str, default: u64) -> u64 {
    rule.params
        .get(key)
        .and_then(|v| v.as_u64())
        .unwrap_or(default)
}

fn truncate(s: &str, max: usize) -> String {
    let t: String = s.chars().take(max).collect();
    if t.chars().count() >= max {
        format!("{t}…")
    } else {
        t
    }
}

fn automod_delete_target(guild: u64, channel: u64) -> crate::discord::DiscordRestTarget {
    crate::discord::DiscordRestTarget::AutomodDelete { guild, channel }
}

fn automod_member_target(
    guild: u64,
    user: u64,
    action: &str,
) -> Option<crate::discord::DiscordRestTarget> {
    crate::discord::AutomodRestAction::from_action(action).map(|kind| {
        crate::discord::DiscordRestTarget::AutomodMember {
            guild,
            user,
            action: kind,
        }
    })
}

#[cfg(test)]
struct AutomodDeleteCancelProbe {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    /// When set, uncertain settlement declines so the removal arm runs.
    skip_uncertain: bool,
}

#[cfg(test)]
tokio::task_local! {
    static AUTOMOD_DELETE_CANCEL_PROBE: std::sync::Arc<AutomodDeleteCancelProbe>;
}

#[allow(clippy::too_many_arguments)]
async fn automod_try_delete_message(
    app: &std::sync::Arc<BotState>,
    http: &serenity::http::Http,
    guild_id: u64,
    channel: u64,
    message: u64,
    rule_id: &str,
    rule_name: &str,
    reason: &str,
) -> Option<CaseOutcome> {
    let target = automod_delete_target(guild_id, channel);
    if app
        .remembered_discord_targets
        .blocked(target, Instant::now())
    {
        return None;
    }
    let prepared = app
        .automod_delete_marks
        .prepare_delete(guild_id, channel, message, rule_id, rule_name, reason);
    let delete_attempt = prepared.attempt;
    let recovery = app
        .remembered_discord_targets
        .capture_recovery_epoch(target);
    let forced_cancel = {
        #[cfg(test)]
        {
            if let Ok(probe) = AUTOMOD_DELETE_CANCEL_PROBE.try_with(std::sync::Arc::clone) {
                probe.entered.notify_one();
                probe.release.notified().await;
                true
            } else {
                false
            }
        }
        #[cfg(not(test))]
        {
            false
        }
    };
    let raced = if forced_cancel {
        Ok(crate::discord::DiscordCall::Cancelled)
    } else {
        tokio::time::timeout(
            Duration::from_secs(10),
            crate::discord::race_discord_call(
                &app.activation,
                http.delete_message(channel.into(), message.into(), None),
            ),
        )
        .await
    };
    match raced {
        Ok(crate::discord::DiscordCall::Finished(Ok(_))) => {
            app.recent_messages.forget(guild_id, channel, message);
            logging::complete_prepared_automod_delete(
                app, http, guild_id, channel, message, &prepared, rule_id, rule_name, reason,
            )
            .await;
            Some(CaseOutcome::Succeeded)
        }
        Ok(crate::discord::DiscordCall::Finished(Err(error))) => {
            let removal = app.automod_delete_marks.remove_unconfirmed(
                guild_id,
                channel,
                message,
                delete_attempt,
            );
            if let Some(settlement) = removal.released_settlement.as_ref() {
                logging::emit_reconciled_automod_delete(
                    app,
                    http,
                    guild_id,
                    channel,
                    message,
                    &settlement.rule_id,
                    &settlement.rule_name,
                    &settlement.reason,
                    settlement.retained.as_ref(),
                    settlement.fallback_to_bulk,
                )
                .await;
            }
            if removal.action
                == crate::automod_delete_marks::UnconfirmedMarkRemoval::ReplayPlainDelete
            {
                logging::replay_unconfirmed_plain_delete(
                    app,
                    http,
                    GuildId::new(guild_id),
                    ChannelId::new(channel),
                    MessageId::new(message),
                    &removal,
                )
                .await;
            }
            app.remembered_discord_targets.remember_rest_rejection(
                recovery,
                &error,
                target,
                Instant::now(),
            );
            Some(crate::cases::classify_discord(Some(&error)))
        }
        Ok(crate::discord::DiscordCall::Cancelled) | Err(_) => {
            let skip_uncertain = {
                #[cfg(test)]
                {
                    AUTOMOD_DELETE_CANCEL_PROBE
                        .try_with(|probe| probe.skip_uncertain)
                        .unwrap_or(false)
                }
                #[cfg(not(test))]
                {
                    false
                }
            };
            if !skip_uncertain
                && let Some(settlement) = app
                    .automod_delete_marks
                    .take_uncertain_gateway_settlement(guild_id, channel, message, delete_attempt)
            {
                logging::emit_reconciled_automod_delete(
                    app,
                    http,
                    guild_id,
                    channel,
                    message,
                    &settlement.rule_id,
                    &settlement.rule_name,
                    &settlement.reason,
                    settlement.retained.as_ref(),
                    settlement.fallback_to_bulk,
                )
                .await;
            } else {
                let removal = app.automod_delete_marks.remove_unconfirmed(
                    guild_id,
                    channel,
                    message,
                    delete_attempt,
                );
                if let Some(settlement) = removal.released_settlement.as_ref() {
                    logging::emit_reconciled_automod_delete(
                        app,
                        http,
                        guild_id,
                        channel,
                        message,
                        &settlement.rule_id,
                        &settlement.rule_name,
                        &settlement.reason,
                        settlement.retained.as_ref(),
                        settlement.fallback_to_bulk,
                    )
                    .await;
                }
                if removal.action
                    == crate::automod_delete_marks::UnconfirmedMarkRemoval::ReplayPlainDelete
                {
                    logging::replay_unconfirmed_plain_delete(
                        app,
                        http,
                        GuildId::new(guild_id),
                        ChannelId::new(channel),
                        MessageId::new(message),
                        &removal,
                    )
                    .await;
                }
            }
            Some(CaseOutcome::Uncertain)
        }
    }
}

pub(crate) fn automod_member_blocked(app: &BotState, guild: u64, user: u64, action: &str) -> bool {
    automod_member_target(guild, user, action).is_some_and(|target| {
        app.remembered_discord_targets
            .blocked(target, Instant::now())
    })
}

fn automod_remember_member_failure(
    app: &BotState,
    since: crate::discord::RecoveryEpoch,
    guild: u64,
    user: u64,
    action: &str,
    error: &serenity::Error,
) {
    if let Some(target) = automod_member_target(guild, user, action) {
        app.remembered_discord_targets.remember_rest_rejection(
            since,
            error,
            target,
            Instant::now(),
        );
    }
}

pub(crate) async fn kick_with_recovery(
    app: &BotState,
    http: &serenity::http::Http,
    guild_id: GuildId,
    member: &Member,
    reason: &str,
) -> Option<serenity::Error> {
    let guild = guild_id.get();
    let user = member.user.id.get();
    let recovery = automod_member_target(guild, user, "kick").map(|target| {
        app.remembered_discord_targets
            .capture_recovery_epoch(target)
    });
    let result = member.kick_with_reason(http, reason).await;
    if let (Some(recovery), Err(error)) = (recovery, &result) {
        automod_remember_member_failure(app, recovery, guild, user, "kick", error);
    }
    result.err()
}

pub(crate) fn action_vars(
    target: &serenity::model::user::User,
    display_name: &str,
    rule_name: &str,
    rule: &ActionDef,
    strikes: u32,
    msg_channel: Option<u64>,
) -> (std::collections::BTreeMap<String, String>, String) {
    let mut vars = crate::templates::user_vars(target.id.get(), &target.name, display_name);
    if let Some(channel) = msg_channel {
        vars.insert("source_channel_id".into(), channel.to_string());
    }
    vars.insert("rule".into(), rule_name.to_owned());
    vars.insert("action".into(), rule.action.clone());
    vars.insert("strikes".into(), strikes.to_string());
    vars.insert("time".into(), chrono::Utc::now().to_rfc3339());
    let rendered_reason = crate::templates::render(&rule.reason, &vars);
    let reason = jmodguard::ban_reason::without_tag(&rendered_reason).to_owned();
    vars.insert("reason".into(), reason.clone());
    (vars, reason)
}

/// Executes a decided automod action. Returns true if the action entered the
/// live path, even when an execution guard later skips the Discord action.
#[allow(clippy::too_many_arguments)]
pub async fn execute_pending(
    app: &Arc<BotState>,
    ctx: &Context,
    guild_id: GuildId,
    target: &serenity::model::user::User,
    provided_member: Option<Member>,
    display_name: &str,
    msg_id: Option<u64>,
    msg_channel: Option<u64>,
    pending: &PendingAction,
) -> bool {
    execute_pending_with(
        app,
        &ctx.http,
        ctx.cache.as_ref(),
        guild_id,
        target,
        provided_member,
        display_name,
        msg_id,
        msg_channel,
        pending,
    )
    .await
}

/// [`execute_pending`] for a caller that already holds the HTTP client and cache.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_pending_with(
    app: &Arc<BotState>,
    http: &Arc<serenity::http::Http>,
    cache: &serenity::cache::Cache,
    guild_id: GuildId,
    target: &serenity::model::user::User,
    provided_member: Option<Member>,
    display_name: &str,
    msg_id: Option<u64>,
    msg_channel: Option<u64>,
    pending: &PendingAction,
) -> bool {
    let enrollment = crate::cases::capture(app, guild_id.get()).await;
    let loaded = app.configs.get(&app.db, guild_id.get()).await;
    if pending.rule_id == "known_spam" {
        let Ok(cfg) = loaded.as_ref() else {
            return false;
        };
        let Some(current) = cfg.rules.get("known_spam") else {
            return false;
        };
        let content = pending.evidence["content"].as_str().unwrap_or("");
        if !current.enabled || !cfg.known_spam.matches(content) {
            return false;
        }
        if jmodguard::custom_rules::rule_scope_skips(
            current,
            target.id.get(),
            msg_channel.unwrap_or(0),
            &[],
            false,
            true,
        ) {
            return false;
        }
    }
    let current = loaded
        .as_ref()
        .ok()
        .and_then(|cfg| cfg.rules.get(&pending.rule_id));
    let config_unknown = current.is_none();
    let custom = jmodguard::custom_rules::is_custom_id(&pending.rule_id);
    let settings = if custom {
        current
    } else {
        current.or(pending.matched_rule.as_ref())
    };
    let Some(settings) = settings else {
        tracing::warn!(
            guild_id = guild_id.get(),
            rule_id = pending.rule_id.as_str(),
            "{}",
            if custom {
                jmodguard::custom_rules::CUSTOM_RULE_UNAVAILABLE_WARNING
            } else {
                "rule configuration unavailable; skipping the action"
            }
        );
        return false;
    };
    let media_preset = jmodguard::custom_rules::requires_include_channels(settings)
        || pending
            .matched_rule
            .as_ref()
            .is_some_and(jmodguard::custom_rules::requires_include_channels);
    if media_preset
        && (pending.matched_rule.as_ref() != Some(settings)
            || !preset_scope_allows(settings, target.id, msg_channel, &[], false))
    {
        return false;
    }
    let mut vars = crate::templates::user_vars(target.id.get(), &target.name, display_name);
    if let Some(channel) = msg_channel {
        vars.insert("source_channel_id".into(), channel.to_string());
    }
    vars.insert("rule".into(), pending.rule_name.clone());
    vars.insert("strikes".into(), pending.strikes.to_string());
    vars.insert("time".into(), chrono::Utc::now().to_rfc3339());
    let actions = pending.actions();

    // Either the selection or this configuration check can keep a rule log-only.
    if pending.dry_run || settings.dry_run {
        let summary = actions
            .iter()
            .map(|action| format!("{}: {}", action.action, action.reason))
            .collect::<Vec<_>>()
            .join(", ");
        vars.insert("action".into(), pending.action.action.clone());
        let rendered_reason = crate::templates::render(&pending.action.reason, &vars);
        vars.insert(
            "reason".into(),
            jmodguard::ban_reason::without_tag(&rendered_reason).to_owned(),
        );
        vars.insert("action".into(), format!("[DRY RUN] would {summary}"));
        vars.insert(
            "content".into(),
            pending.evidence["content"]
                .as_str()
                .unwrap_or("")
                .to_owned(),
        );
        logging::emit_to(app, http, guild_id, "automod", vars).await;
        return false;
    }

    let explicit_log = custom && actions.iter().any(|action| action.action == "log");
    let enforces = actions.iter().any(|action| action.action != "log");
    let resolved_member = resolve_rule_member(
        app,
        http,
        cache,
        guild_id,
        target.id,
        provided_member,
        settings,
        enforces,
    )
    .await;
    let mut preset_dispatch = None;
    if media_preset {
        let Some(dispatch) = recheck_preset(
            app,
            cache,
            guild_id,
            target.id,
            msg_channel,
            pending,
            resolved_member.as_ref().ok().and_then(Option::as_ref),
        )
        .await
        else {
            return false;
        };
        preset_dispatch = Some(dispatch);
    }
    let provided_member = match resolved_member {
        Ok(member) => member,
        Err(RuleExemption::Exempt) => return true,
        Err(RuleExemption::Unknown(reason)) => {
            tracing::warn!(
                guild_id = guild_id.get(),
                user_id = target.id.get(),
                rule_id = pending.rule_id.as_str(),
                reason,
                "automod enforcement skipped"
            );
            if explicit_log || settings.log_automod {
                let skipped = actions
                    .iter()
                    .filter(|action| action.action != "log")
                    .map(|action| format!("{} (skipped)", action.action))
                    .collect::<Vec<_>>()
                    .join(", ");
                vars.insert("action".into(), skipped);
                vars.insert("reason".into(), reason.into());
                vars.insert(
                    "content".into(),
                    pending.evidence["content"].as_str().unwrap_or("").into(),
                );
                logging::emit_to(app, http, guild_id, "automod", vars).await;
            }
            return true;
        }
    };
    let mut earlier_actions = Vec::new();
    let mut earlier_reasons = Vec::new();
    let mut explicit_log_pending = explicit_log;
    let mut has_action_outcome = false;
    for (index, action) in actions.iter().enumerate() {
        // The previous action may have awaited Discord, logging or persistence.
        if media_preset && index != 0 {
            let Some(dispatch) = recheck_preset(
                app,
                cache,
                guild_id,
                target.id,
                msg_channel,
                pending,
                provided_member.as_ref(),
            )
            .await
            else {
                if explicit_log_pending && has_action_outcome {
                    apply_action_outcomes(&mut vars, &earlier_actions, &earlier_reasons, "");
                    emit_action_report(app, http, guild_id, pending, vars).await;
                }
                break;
            };
            preset_dispatch = Some(dispatch);
        }
        let mut vars = vars.clone();
        vars.insert("action".into(), action.action.clone());
        let rendered_reason = crate::templates::render(&action.reason, &vars);
        let reason = jmodguard::ban_reason::without_tag(&rendered_reason).to_owned();
        vars.insert("reason".into(), reason.clone());
        if explicit_log_pending && action.action == "log" {
            apply_action_outcomes(&mut vars, &earlier_actions, &earlier_reasons, &reason);
            emit_action_report(app, http, guild_id, pending, vars).await;
            explicit_log_pending = false;
            continue;
        }
        let outcome = execute_live(
            app,
            http,
            cache,
            guild_id,
            target,
            provided_member.as_ref(),
            msg_id,
            msg_channel,
            pending,
            action,
            pending.matched_rule.as_ref().unwrap_or(settings),
            enrollment.clone(),
            vars,
            &reason,
            preset_dispatch.as_ref(),
            config_unknown,
            !explicit_log && settings.log_automod,
            settings.open_case,
        )
        .await;
        if explicit_log_pending {
            has_action_outcome |= outcome.is_some();
            let (name, status) = match outcome {
                Some((name, CaseOutcome::Succeeded)) => (name, "succeeded"),
                Some((name, CaseOutcome::Failed)) => (name, "failed"),
                Some((name, CaseOutcome::Uncertain)) => (name, "unconfirmed"),
                Some((name, CaseOutcome::Attempted)) => (name, "attempted"),
                None => (action.action.as_str(), "skipped"),
            };
            earlier_actions.push(format!("{name} ({status})"));
            if !reason.is_empty() {
                earlier_reasons.push(format!("{name}: {reason}"));
            }
        }
    }
    true
}

fn apply_action_outcomes(
    vars: &mut std::collections::BTreeMap<String, String>,
    earlier_actions: &[String],
    earlier_reasons: &[String],
    log_reason: &str,
) {
    if !earlier_actions.is_empty() {
        vars.insert("action".into(), earlier_actions.join(", "));
    }
    if !earlier_reasons.is_empty() {
        let mut reason = earlier_reasons.join("; ");
        if !log_reason.is_empty() {
            reason.push_str("; log: ");
            reason.push_str(log_reason);
        }
        vars.insert("reason".into(), reason);
    }
}

async fn emit_action_report(
    app: &Arc<BotState>,
    http: &serenity::http::Http,
    guild_id: GuildId,
    pending: &PendingAction,
    mut vars: std::collections::BTreeMap<String, String>,
) {
    vars.insert(
        "content".into(),
        pending.evidence["content"]
            .as_str()
            .unwrap_or("")
            .to_owned(),
    );
    logging::emit_to(app, http, guild_id, "automod", vars).await;
}

fn preset_scope_allows(
    rule: &RuleDef,
    user_id: UserId,
    channel: Option<u64>,
    roles: &[u64],
    moderator: bool,
) -> bool {
    rule.enabled
        && channel.is_some_and(|channel| {
            rule.include_channels.contains(&channel)
                && !jmodguard::custom_rules::rule_scope_skips(
                    rule,
                    user_id.get(),
                    channel,
                    roles,
                    moderator,
                    true,
                )
        })
}

/// A scope seen after asynchronous admission. The version checks are
/// synchronous so a later I/O wait cannot authorize a stale dispatch.
struct PresetDispatch {
    guild_id: u64,
    cache_version: u64,
    durable_version: u64,
    write_generation: u64,
}

impl PresetDispatch {
    fn is_current(&self, app: &BotState) -> bool {
        self.write_generation & 1 == 0
            && self.write_generation == app.db.config_write_generation()
            && self.durable_version >= app.db.required_config_version()
            && app.configs.last_known(self.guild_id).is_some_and(|cfg| {
                cfg.version == self.cache_version && cfg.durable_version == self.durable_version
            })
    }
}

#[allow(clippy::too_many_arguments)]
async fn recheck_preset(
    app: &BotState,
    cache: &serenity::cache::Cache,
    guild_id: GuildId,
    user: UserId,
    channel: Option<u64>,
    pending: &PendingAction,
    member: Option<&Member>,
) -> Option<PresetDispatch> {
    let write_generation = app.db.config_write_generation();
    // The cache honors the bot writer's committed-version floor and falls
    // back to the primary when a replica is behind an acknowledged save.
    let cfg = app.configs.get(&app.db, guild_id.get()).await.ok()?;
    let current = cfg.rules.get(&pending.rule_id)?;
    // A change to the rule invalidates both the ordered plan and the exemption
    // decision made from it. A later message can be judged under the edit.
    if current != pending.matched_rule.as_ref()? || current.dry_run {
        return None;
    }

    let roles: Vec<_> = member
        .map(|member| member.roles.iter().map(|role| role.get()).collect())
        .unwrap_or_default();
    let mut moderator = false;
    if let Some(guild) = cache.guild(guild_id) {
        if (current.skip_moderators || !current.whitelist_roles.is_empty())
            && let Some(cached) = guild.members.get(&user)
            && member.is_some_and(|member| member.roles != cached.roles)
        {
            // The membership changed since the exemption lookup.
            return None;
        }
        moderator = current.skip_moderators
            && (guild.owner_id == user
                || crate::commands::permission_level(
                    app.cfg.bot.owner_id,
                    &guild.roles,
                    user.get(),
                    roles.iter().copied().chain([guild_id.get()]),
                ) >= crate::commands::PermLevel::Mod);
    }
    if !preset_scope_allows(current, user, channel, &roles, moderator) {
        return None;
    }
    let dispatch = PresetDispatch {
        guild_id: guild_id.get(),
        cache_version: cfg.version,
        durable_version: cfg.durable_version,
        write_generation,
    };
    dispatch.is_current(app).then_some(dispatch)
}

enum RuleExemption {
    Exempt,
    Unknown(&'static str),
}

#[allow(clippy::too_many_arguments)]
async fn resolve_rule_member(
    app: &BotState,
    http: &serenity::http::Http,
    cache: &serenity::cache::Cache,
    guild_id: GuildId,
    user_id: UserId,
    provided: Option<Member>,
    settings: &RuleDef,
    enforces: bool,
) -> Result<Option<Member>, RuleExemption> {
    use crate::commands::{ModeratorStatus, PermLevel, permission_level};

    if !settings.skip_moderators && settings.whitelist_roles.is_empty() {
        return Ok(provided);
    }
    let (cached_owner, cached_member) = cache
        .guild(guild_id)
        .map(|guild| (Some(guild.owner_id), guild.members.get(&user_id).cloned()))
        .unwrap_or_default();
    if settings.skip_moderators
        && (cached_owner == Some(user_id) || app.cfg.bot.owner_id == user_id.get())
    {
        return Err(RuleExemption::Exempt);
    }
    let fetched_guild = if settings.skip_moderators && cached_owner.is_none() && enforces {
        if let Ok(crate::discord::DiscordCall::Finished(Ok(guild))) = tokio::time::timeout(
            Duration::from_secs(2),
            crate::discord::race_discord_call(&app.activation, http.get_guild(guild_id)),
        )
        .await
        {
            if guild.owner_id == user_id {
                return Err(RuleExemption::Exempt);
            }
            Some(guild)
        } else {
            return Err(RuleExemption::Unknown(
                "moderator status could not be confirmed",
            ));
        }
    } else {
        None
    };

    // Gateway PartialMember roles are useful even when the user field was
    // omitted. The author ID comes from the event, not the partial member.
    let mut member = provided.or(cached_member);
    if member.is_none()
        && enforces
        && let Ok(crate::discord::DiscordCall::Finished(Ok(fetched))) = tokio::time::timeout(
            Duration::from_secs(2),
            crate::discord::race_discord_call(&app.activation, http.get_member(guild_id, user_id)),
        )
        .await
        && fetched.user.id == user_id
    {
        member = Some(fetched);
    }
    let Some(roles) = member.as_ref().map(|member| &member.roles) else {
        return if enforces {
            Err(RuleExemption::Unknown(
                "member roles could not be confirmed",
            ))
        } else {
            Ok(member)
        };
    };
    if roles
        .iter()
        .any(|role| settings.whitelist_roles.contains(&role.get()))
    {
        return Err(RuleExemption::Exempt);
    }
    if !settings.skip_moderators {
        return Ok(member);
    }

    // Include the implicit @everyone role. A missing definition is unknown.
    let role_ids: Vec<_> = roles
        .iter()
        .map(|role| role.get())
        .chain([guild_id.get()])
        .collect();
    let check_roles = |roles: &HashMap<_, _>| {
        if permission_level(
            app.cfg.bot.owner_id,
            roles,
            user_id.get(),
            role_ids.iter().copied(),
        ) >= PermLevel::Mod
        {
            ModeratorStatus::Moderator
        } else if role_ids.iter().all(|id| roles.contains_key(&(*id).into())) {
            ModeratorStatus::Member
        } else {
            ModeratorStatus::Unknown
        }
    };
    let mut status = fetched_guild.as_ref().map_or_else(
        || {
            cache
                .guild(guild_id)
                .map_or(ModeratorStatus::Unknown, |guild| check_roles(&guild.roles))
        },
        |guild| check_roles(&guild.roles),
    );
    if status == ModeratorStatus::Unknown
        && cached_owner.is_some()
        && enforces
        && let Ok(crate::discord::DiscordCall::Finished(Ok(roles))) = tokio::time::timeout(
            Duration::from_secs(2),
            crate::discord::race_discord_call(&app.activation, http.get_guild_roles(guild_id)),
        )
        .await
    {
        status = check_roles(&roles.into_iter().map(|role| (role.id, role)).collect());
    }
    match status {
        ModeratorStatus::Moderator => Err(RuleExemption::Exempt),
        ModeratorStatus::Unknown if enforces => Err(RuleExemption::Unknown(
            "moderator status could not be confirmed",
        )),
        _ => Ok(member),
    }
}

/// The enforcement path of [`execute_pending_with`], after the dry-run decision.
#[allow(clippy::too_many_arguments)]
async fn execute_live<'a>(
    app: &Arc<BotState>,
    http: &Arc<serenity::http::Http>,
    cache: &serenity::cache::Cache,
    guild_id: GuildId,
    target: &serenity::model::user::User,
    provided_member: Option<&Member>,
    msg_id: Option<u64>,
    msg_channel: Option<u64>,
    pending: &PendingAction,
    rule: &'a ActionDef,
    matched_settings: &RuleDef,
    enrollment: Option<db::GuildEnrollment>,
    mut vars: std::collections::BTreeMap<String, String>,
    reason: &str,
    preset_dispatch: Option<&PresetDispatch>,
    config_unknown: bool,
    log_automod: bool,
    open_case: bool,
) -> Option<(&'a str, CaseOutcome)> {
    let warning = if config_unknown {
        Some("rule configuration unavailable; deleting the message and skipping kick or ban")
    } else if pending.mention_permission_unknown {
        Some(jmodguard::custom_rules::UNKNOWN_MENTION_PERMISSION_WARNING)
    } else {
        None
    };

    if matches!(
        rule.action.as_str(),
        "delete" | "timeout" | "kick" | "ban" | "channel_reply"
    ) && db::guild_stores::guild_is_write_fenced(&app.db, guild_id.get() as i64)
        .await
        .unwrap_or(true)
    {
        tracing::warn!(
            guild_id = guild_id.get(),
            action = rule.action,
            "automod Discord mutation skipped because the guild is write-fenced"
        );
        return None;
    }

    let dispatch_is_current = || preset_dispatch.is_none_or(|dispatch| dispatch.is_current(app));
    if !dispatch_is_current() {
        return None;
    }

    if let Some(warning) = warning
        && matches!(rule.action.as_str(), "kick" | "ban")
    {
        tracing::warn!(
            guild_id = guild_id.get(),
            user_id = target.id.get(),
            rule_id = pending.rule_id.as_str(),
            "{warning}"
        );
        let outcome = if let (Some(ch), Some(id)) = (msg_channel, msg_id) {
            if !app.activation.may_record() {
                return None;
            }
            automod_try_delete_message(
                app,
                http,
                guild_id.get(),
                ch,
                id,
                pending.rule_id.as_str(),
                pending.rule_name.as_str(),
                reason,
            )
            .await
        } else {
            None
        };
        // This return precedes the ban handoff and operation identity creation.
        if log_automod {
            let mut v = vars;
            v.insert("action".into(), format!("delete; {warning}"));
            v.insert(
                "content".into(),
                pending.evidence["content"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
            );
            logging::emit_to(app, http, guild_id, "automod", v).await;
        }
        return outcome.map(|outcome| ("delete", outcome));
    }

    let mut publish_report = true;
    let outcome = match rule.action.as_str() {
        "log" => None,
        "delete" => {
            if let (Some(ch), Some(id)) = (msg_channel, msg_id) {
                if !app.activation.may_record() {
                    return None;
                }
                automod_try_delete_message(
                    app,
                    http,
                    guild_id.get(),
                    ch,
                    id,
                    pending.rule_id.as_str(),
                    pending.rule_name.as_str(),
                    reason,
                )
                .await
            } else {
                None
            }
        }
        "channel_reply" => {
            let outcome = if let Some(channel) = msg_channel
                && let Ok((template, delete_after)) =
                    jmodguard::custom_rules::channel_reply(matched_settings)
            {
                app.automod
                    .replies
                    .send(
                        &app.activation,
                        http,
                        guild_id,
                        ChannelId::new(channel),
                        target.id,
                        template,
                        delete_after,
                    )
                    .await
            } else {
                None
            };
            publish_report = outcome == Some(CaseOutcome::Succeeded);
            outcome
        }
        "warn" => {
            if !app.activation.may_record() {
                return None;
            }
            let warn_result = db::add_enrolled_warn(
                &app.db,
                enrollment.clone(),
                target.id.get() as i64,
                0,
                &format!("[automod:{}] {}", pending.rule_id, reason),
            )
            .await;
            if !app.activation.may_record() {
                return None;
            }
            let outcome = crate::cases::classify_db_commit(&warn_result);
            if open_case {
                crate::cases::record(
                    app,
                    enrollment.clone(),
                    crate::db::guild_data::CaseAction::Warning,
                    outcome.clone(),
                    target.id.get(),
                    None,
                    Some(reason.to_owned()),
                    crate::cases::text_excerpt(
                        msg_channel.unwrap_or(0),
                        msg_id.unwrap_or(0),
                        target.id.get(),
                        pending.evidence["content"].as_str().unwrap_or(""),
                    ),
                    crate::cases::application_source(),
                )
                .await;
            }
            Some(outcome)
        }
        "timeout" => {
            let minutes = rule.duration_minutes.max(1) as i64;
            let member = member_for_selected_action(
                provided_member.cloned(),
                cache
                    .guild(guild_id)
                    .and_then(|g| g.members.get(&target.id).cloned()),
                target.id,
            );
            let mut called = false;
            let mut discord_err = None;
            if let Some(mut member) = member {
                if let Ok(until) =
                    Timestamp::from_unix_timestamp(chrono::Utc::now().timestamp() + minutes * 60)
                {
                    if !app.activation.may_record() {
                        return None;
                    }
                    if automod_member_blocked(app, guild_id.get(), target.id.get(), "timeout") {
                        called = false;
                    } else {
                        let recovery =
                            automod_member_target(guild_id.get(), target.id.get(), "timeout").map(
                                |member_target| {
                                    app.remembered_discord_targets
                                        .capture_recovery_epoch(member_target)
                                },
                            );
                        match crate::discord::race_discord_call(
                            &app.activation,
                            member.disable_communication_until_datetime(http, until),
                        )
                        .await
                        {
                            crate::discord::DiscordCall::Cancelled => {
                                discord_err = Some(crate::discord::cutoff_discord_error());
                            }
                            crate::discord::DiscordCall::Finished(result) => {
                                if let (Some(recovery), Err(error)) = (recovery, &result) {
                                    automod_remember_member_failure(
                                        app,
                                        recovery,
                                        guild_id.get(),
                                        target.id.get(),
                                        "timeout",
                                        error,
                                    );
                                }
                                discord_err = result.err();
                            }
                        }
                        called = true;
                    }
                } else {
                    tracing::warn!("automod timeout: could not build Discord timeout timestamp");
                }
            } else {
                tracing::warn!("automod timeout: no guild member for {}", target.id);
            }
            if !app.activation.may_record() {
                return None;
            }
            let outcome = crate::cases::discord_attempt_outcome(called, discord_err.as_ref());
            if open_case && let Some(outcome) = &outcome {
                crate::cases::record(
                    app,
                    enrollment.clone(),
                    crate::db::guild_data::CaseAction::Timeout,
                    outcome.clone(),
                    target.id.get(),
                    None,
                    Some(reason.to_owned()),
                    crate::cases::text_excerpt(
                        msg_channel.unwrap_or(0),
                        msg_id.unwrap_or(0),
                        target.id.get(),
                        pending.evidence["content"].as_str().unwrap_or(""),
                    ),
                    crate::cases::application_source(),
                )
                .await;
            }
            outcome
        }
        "kick" => {
            let member = member_for_selected_action(
                provided_member.cloned(),
                cache
                    .guild(guild_id)
                    .and_then(|g| g.members.get(&target.id).cloned()),
                target.id,
            );
            let (called, discord_err) = if let Some(member) = member {
                if !app.activation.may_record() {
                    return None;
                }
                if automod_member_blocked(app, guild_id.get(), target.id.get(), "kick") {
                    (false, None)
                } else {
                    let discord_err = match crate::discord::race_discord_call(
                        &app.activation,
                        kick_with_recovery(app, http, guild_id, &member, reason),
                    )
                    .await
                    {
                        crate::discord::DiscordCall::Cancelled => {
                            Some(crate::discord::cutoff_discord_error())
                        }
                        crate::discord::DiscordCall::Finished(error) => error,
                    };
                    (true, discord_err)
                }
            } else {
                tracing::warn!("automod kick: no guild member for {}", target.id);
                (false, None)
            };
            if !app.activation.may_record() {
                return None;
            }
            let outcome = crate::cases::discord_attempt_outcome(called, discord_err.as_ref());
            if open_case && let Some(outcome) = &outcome {
                crate::cases::record_application(
                    app,
                    enrollment.clone(),
                    crate::db::guild_data::CaseAction::Kick,
                    outcome.clone(),
                    target.id.get(),
                    None,
                    Some(reason.to_owned()),
                    crate::cases::text_excerpt(
                        msg_channel.unwrap_or(0),
                        msg_id.unwrap_or(0),
                        target.id.get(),
                        pending.evidence["content"].as_str().unwrap_or(""),
                    ),
                )
                .await;
            }
            outcome
        }
        "ban" => {
            let mut ban_outcome = None;
            let decision = crate::member_directory::observe_ban_handoff(
                app,
                http,
                guild_id.get(),
                target.id.get(),
                crate::member_directory::MarkerSave::Missing,
            )
            .await;
            match decision {
                crate::member_directory::BanContinuation::Stop => {
                    publish_report = false;
                }
                crate::member_directory::BanContinuation::AlreadyFinished { reason } => {
                    if open_case {
                        crate::cases::settle_present_application_ban(
                            app,
                            enrollment.clone(),
                            target.id.get(),
                            reason.as_deref(),
                        )
                        .await;
                    }
                    publish_report = false;
                }
                crate::member_directory::BanContinuation::RecordOnly {
                    reason: discord_reason,
                } => {
                    if open_case {
                        crate::cases::settle_present_application_ban(
                            app,
                            enrollment.clone(),
                            target.id.get(),
                            discord_reason.as_deref(),
                        )
                        .await;
                    }
                    publish_report = app.activation.may_record()
                        && db::record_enrolled_ban_if_absent(
                            &app.db,
                            enrollment.clone(),
                            target.id.get() as i64,
                            Some(&target.name),
                            target.global_name.as_deref(),
                            None,
                            None,
                            &format!("[automod:{}] {}", pending.rule_id, reason),
                            None,
                            db::BanRecord::Retry,
                            None,
                        )
                        .await
                        .ok()
                        .flatten()
                        .is_some();
                }
                crate::member_directory::BanContinuation::BanThenRecord => {
                    if !app.activation.may_record() || !dispatch_is_current() {
                        publish_report = false;
                    } else {
                        let pending_ban = crate::member_directory::begin_ban_attempt(
                            app,
                            guild_id.get(),
                            target.id.get(),
                        )
                        .await;
                        if !app.activation.may_record() {
                            publish_report = false;
                        } else if !dispatch_is_current() {
                            pending_ban.finish(app, CaseOutcome::Failed).await;
                            publish_report = false;
                        } else {
                            let attempted = if open_case {
                                match crate::db::guild_data::CaseId::try_from(
                                    pending_ban.operation_id().to_owned(),
                                ) {
                                    Ok(operation_id) => {
                                        crate::cases::begin_application_attempt(
                                            app,
                                            enrollment.clone(),
                                            crate::db::guild_data::CaseAction::Ban,
                                            target.id.get(),
                                            None,
                                            Some(reason.to_owned()),
                                            crate::cases::text_excerpt(
                                                msg_channel.unwrap_or(0),
                                                msg_id.unwrap_or(0),
                                                target.id.get(),
                                                pending.evidence["content"].as_str().unwrap_or(""),
                                            ),
                                            operation_id,
                                        )
                                        .await
                                    }
                                    Err(_) => None,
                                }
                            } else {
                                None
                            };
                            let discord_reason =
                                crate::cases::ban_dispatch_reason(reason, attempted.as_ref());
                            let operation_id = pending_ban.operation_id().to_owned();
                            let suppression = app.suppress_next_gateway_case(
                                crate::state::GatewayCaseAction::Ban,
                                guild_id.get(),
                                target.id.get(),
                                Some(&operation_id),
                            );
                            let purge_seconds = rule.ban_delete_message_seconds();
                            let cleanup = if purge_seconds > 0 {
                                logging::begin_cleanup(
                                    app,
                                    guild_id.get(),
                                    target.id.get(),
                                    logging::CleanupRule {
                                        id: &pending.rule_id,
                                        name: &pending.rule_name,
                                        reason,
                                        action: "ban",
                                        sample: msg_channel.zip(msg_id).map(
                                            |(channel, message)| {
                                                (
                                                    channel,
                                                    message,
                                                    pending.evidence["content"]
                                                        .as_str()
                                                        .unwrap_or(""),
                                                )
                                            },
                                        ),
                                        hash: None,
                                    },
                                )
                                .await
                            } else {
                                None
                            };
                            let raced = if !dispatch_is_current()
                                || automod_member_blocked(
                                    app,
                                    guild_id.get(),
                                    target.id.get(),
                                    "ban",
                                ) {
                                None
                            } else {
                                let recovery =
                                    automod_member_target(guild_id.get(), target.id.get(), "ban")
                                        .map(|member_target| {
                                            app.remembered_discord_targets
                                                .capture_recovery_epoch(member_target)
                                        });
                                if let Some(cleanup) = &cleanup {
                                    cleanup.begin_purge(purge_seconds);
                                }
                                Some((
                                    recovery,
                                    crate::discord::race_discord_call(
                                        &app.activation,
                                        crate::discord::automod_ban(
                                            http,
                                            guild_id,
                                            target.id,
                                            rule,
                                            &discord_reason,
                                        ),
                                    )
                                    .await,
                                ))
                            };
                            match raced {
                                None => {
                                    app.release_gateway_case_suppression(suppression);
                                    if app.activation.may_record() {
                                        if let Some(attempt) = attempted {
                                            attempt
                                                .finish(
                                                    app,
                                                    crate::db::guild_data::CaseOutcome::Failed,
                                                )
                                                .await;
                                        }
                                        pending_ban
                                            .finish(app, crate::db::guild_data::CaseOutcome::Failed)
                                            .await;
                                    }
                                    publish_report = false;
                                }
                                Some((_, crate::discord::DiscordCall::Cancelled)) => {
                                    ban_outcome = Some(CaseOutcome::Uncertain);
                                    pending_ban
                                        .finish(app, crate::db::guild_data::CaseOutcome::Uncertain)
                                        .await;
                                    publish_report = false;
                                }
                                Some((
                                    recovery,
                                    crate::discord::DiscordCall::Finished(discord),
                                )) => {
                                    if let (Some(recovery), Err(error)) = (recovery, &discord) {
                                        automod_remember_member_failure(
                                            app,
                                            recovery,
                                            guild_id.get(),
                                            target.id.get(),
                                            "ban",
                                            error,
                                        );
                                    }
                                    let outcome =
                                        crate::cases::classify_discord(discord.err().as_deref());
                                    ban_outcome = Some(outcome.clone());
                                    if matches!(outcome, CaseOutcome::Failed) {
                                        app.release_gateway_case_suppression(suppression);
                                    }
                                    if app.activation.may_record() {
                                        let case = if let Some(attempt) = attempted {
                                            attempt.finish(app, outcome.clone()).await
                                        } else {
                                            None
                                        };
                                        if let Some(cleanup) = &cleanup {
                                            cleanup.outcome(outcome.clone(), case);
                                        }
                                        pending_ban.finish(app, outcome.clone()).await;
                                    }
                                    if !app.activation.may_record() {
                                        publish_report = false;
                                    } else if matches!(
                                        outcome,
                                        crate::db::guild_data::CaseOutcome::Succeeded
                                    ) {
                                        publish_report = db::record_enrolled_ban_if_absent(
                                            &app.db,
                                            enrollment.clone(),
                                            target.id.get() as i64,
                                            Some(&target.name),
                                            target.global_name.as_deref(),
                                            None,
                                            None,
                                            &format!("[automod:{}] {}", pending.rule_id, reason),
                                            None,
                                            db::BanRecord::Fresh,
                                            Some(&operation_id),
                                        )
                                        .await
                                        .ok()
                                        .flatten()
                                        .is_some();
                                        if let Some(enrollment) = &enrollment
                                            && rule.ban_delete_message_seconds() > 0
                                            && dispatch_is_current()
                                        {
                                            let deleted = crate::recent_messages::sweep_after_ban(
                                                app,
                                                http,
                                                enrollment,
                                                target.id.get(),
                                                purge_seconds,
                                                cleanup.as_ref(),
                                                dispatch_is_current,
                                            )
                                            .await;
                                            vars.insert(
                                                "action".into(),
                                                format!("ban, deleted {deleted} tracked messages"),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            ban_outcome
        }
        other => {
            tracing::warn!("unknown automod action `{other}`");
            None
        }
    };
    let report = outcome.map(|outcome| (rule.action.as_str(), outcome));

    if !publish_report {
        return report;
    }
    // Log-only/delete-only observations are routine Discord reports. Retain
    // selected moderation records until the typed case model replaces this table.
    if open_case && matches!(rule.action.as_str(), "warn" | "timeout" | "kick" | "ban") {
        if !app.activation.may_record() {
            return report;
        }
        let evidence = serde_json::json!({
            "rule": pending.rule_id, "action": rule.action,
            "reason": reason, "evidence": pending.evidence,
        });
        crate::logging::log_persistence_result(
            "automod event",
            db::insert_enrolled_event(
                &app.db,
                enrollment.clone(),
                "automod",
                Some(target.id.get() as i64),
                None,
                &evidence,
            )
            .await,
        );
    }

    if !log_automod {
        return report;
    }
    emit_action_report(app, http, guild_id, pending, vars).await;
    report
}

fn member_for_selected_action(
    provided: Option<Member>,
    cached: Option<Member>,
    target: UserId,
) -> Option<Member> {
    provided
        .filter(|member| member.user.id == target)
        .or_else(|| cached.filter(|member| member.user.id == target))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Instant;

    use crate::state::{BotState, CachedGuildConfig};
    use crate::types::{ActionDef, GuildSettings, RuleDef};

    use super::{AutomodEngine, MsgCtx, PendingAction, execute_pending_with};

    #[tokio::test(start_paused = true)]
    async fn default_velocity_counts_plain_messages_and_escalates() {
        let cfg = default_automod_config();
        let engine = AutomodEngine::new();
        for id in 1..=16 {
            if id == 15 {
                tokio::time::advance(std::time::Duration::from_millis(999)).await;
            } else if id == 16 {
                tokio::time::advance(std::time::Duration::from_millis(1)).await;
            }
            let text = format!("ordinary message {id}");
            let channel = if id == 14 { 2 } else { 1 };
            let mut message = msg(channel, &text, 0, false, false);
            message.message_id = id;
            let action = engine.evaluate(&cfg, &message);
            let expected = match id {
                9 => Some((1, "delete")),
                10 => Some((2, "timeout")),
                12 => Some((4, "kick")),
                13 => Some((5, "delete")),
                16 => Some((8, "delete")),
                _ => None,
            };
            assert_eq!(
                action.as_ref().map(|action| {
                    assert_eq!(action.rule_id, "velocity");
                    (action.strikes, action.action.action.as_str())
                }),
                expected,
                "message {id}",
            );
        }
    }

    #[test]
    fn velocity_does_not_delete_after_a_terminal_log_or_warn() {
        for terminal in ["log", "warn"] {
            let mut cfg = default_automod_config();
            let rule = cfg.rules.get_mut("velocity").unwrap();
            rule.params["max_messages"] = serde_json::json!(0);
            rule.actions.truncate(1);
            rule.actions[0].action = terminal.into();
            let engine = AutomodEngine::new();
            let message = msg(1, "ordinary message", 0, false, false);
            let step = engine.evaluate(&cfg, &message).unwrap();
            assert_eq!(step.action.action, terminal);
            assert!(engine.evaluate(&cfg, &message).is_none());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn velocity_fallback_requires_the_reserved_terminal_action_kind() {
        for original in ["log", "timeout"] {
            let mut cfg = default_automod_config();
            let rule = cfg.rules.get_mut("velocity").unwrap();
            rule.params["max_messages"] = serde_json::json!(0);
            rule.actions.truncate(1);
            rule.actions[0].action = original.into();
            let engine = AutomodEngine::new();
            let message = msg(1, "ordinary message", 0, false, false);
            assert_eq!(
                engine.evaluate(&cfg, &message).unwrap().action.action,
                original
            );

            cfg.rules.get_mut("velocity").unwrap().actions[0].action = "kick".into();
            assert!(
                engine.evaluate(&cfg, &message).is_none(),
                "the old {original} reservation can neither fire the kick nor unlock deletes",
            );
            tokio::time::advance(super::STRIKE_RETENTION).await;
            assert_eq!(
                engine.evaluate(&cfg, &message).unwrap().action.action,
                "kick"
            );
            assert_eq!(
                engine.evaluate(&cfg, &message).unwrap().action.action,
                "delete"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn velocity_fallback_cooldown_is_charged_only_to_the_winner() {
        let mut cfg = default_automod_config();
        let rule = cfg.rules.get_mut("velocity").unwrap();
        rule.params["max_messages"] = serde_json::json!(0);
        rule.actions.truncate(1);
        rule.actions[0].action = "kick".into();
        let custom_id = "custom:0123456789abcdef";
        let mut custom = custom_rule(
            serde_json::json!({ "kind": "channel_content", "links": true }),
            "warn",
        );
        custom.dry_run = false;
        cfg.rules.insert(custom_id.into(), custom);
        let engine = AutomodEngine::new();
        let plain = msg(1, "ordinary message", 0, false, false);
        assert_eq!(engine.evaluate(&cfg, &plain).unwrap().action.action, "kick");

        let link = msg(1, "https://example.com", 0, false, false);
        let warning = engine.evaluate(&cfg, &link).unwrap();
        assert_eq!(warning.rule_id, custom_id);
        assert_eq!(warning.action.action, "warn");
        let delete = engine
            .evaluate(&cfg, &plain)
            .expect("the warning used no cooldown");
        assert_eq!(delete.rule_id, "velocity");
        assert_eq!(delete.action.action, "delete");
        assert!(engine.evaluate(&cfg, &plain).is_none());

        cfg.rules.get_mut(custom_id).unwrap().actions[0].action = "delete".into();
        assert_eq!(engine.evaluate(&cfg, &link).unwrap().rule_id, custom_id);
        tokio::time::advance(super::VELOCITY_FALLBACK_DELETE_INTERVAL).await;
        assert_eq!(engine.evaluate(&cfg, &link).unwrap().rule_id, "velocity");
    }

    #[tokio::test]
    async fn velocity_fallback_after_rejoin_dispatches_only_when_live() {
        use serenity::all::{GuildId, Member, User, UserId};
        use std::sync::{Arc, atomic::AtomicBool};

        let path = std::env::temp_dir().join(format!(
            "velocity_rejoin_{:032x}.db",
            rand::random::<u128>()
        ));
        let app =
            BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap();
        assert!(
            crate::db::claim_active_bot(
                &app.db,
                app.activation.instance(),
                jmodguard::control::now_ms(),
                Arc::new(AtomicBool::new(false))
            )
            .await
            .unwrap()
        );
        app.activation.mark_lock_held(true);
        let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment).await.unwrap();

        let mut rule = default_automod_config().rules.remove("velocity").unwrap();
        rule.whitelist_channels = vec![71];
        // Leave room for real SQLite and loopback Discord I/O between bursts.
        rule.params["window_secs"] = serde_json::json!(600);

        for (dry_run, author) in [(false, 99), (true, 100)] {
            rule.dry_run = dry_run;
            crate::db::upsert_rule(&app.db, 10, "velocity", &rule)
                .await
                .unwrap();
            app.invalidate_configs();
            let cfg = app.configs.get(&app.db, 10).await.unwrap();

            let mut user = User::default();
            user.id = UserId::new(author);
            user.name = "member".into();
            let guild = GuildId::new(10);
            let mut member = Member::default();
            member.guild_id = guild;
            member.user = user.clone();
            let stub = DiscordStub::start();
            let mut message = word_message(&[]);
            message.author_id = author;
            message.content = "ordinary message";

            for id in 1..12 {
                message.message_id = id;
                app.automod.evaluate(&cfg, &message);
            }
            message.message_id = 12;
            let kick = app.automod.evaluate(&cfg, &message).unwrap();
            assert_eq!(kick.action.action, "kick");
            assert_eq!(
                execute_pending_with(
                    &app,
                    &stub.http,
                    &stub.cache,
                    guild,
                    &user,
                    Some(member.clone()),
                    "member",
                    Some(12),
                    Some(70),
                    &kick,
                )
                .await,
                !dry_run,
            );

            app.automod.member_joined(10, author);
            message.message_id = 13;
            message.channel_id = 71;
            assert!(app.automod.evaluate(&cfg, &message).is_none());

            message.message_id = 14;
            message.channel_id = 70;
            let delete = app
                .automod
                .evaluate(&cfg, &message)
                .expect("post-rejoin fallback");
            assert_eq!(delete.rule_id, "velocity");
            assert_eq!(delete.action.action, "delete");
            assert_eq!(delete.dry_run, dry_run);
            assert_eq!(
                execute_pending_with(
                    &app,
                    &stub.http,
                    &stub.cache,
                    guild,
                    &user,
                    Some(member),
                    "member",
                    Some(14),
                    Some(70),
                    &delete,
                )
                .await,
                !dry_run,
            );
            let seen = stub.finish();
            if dry_run {
                assert!(seen.is_empty(), "{seen:?}");
            } else {
                assert_eq!(
                    seen,
                    [
                        "DELETE /api/v10/guilds/10/members/99",
                        "DELETE /api/v10/channels/70/messages/14",
                    ]
                );
            }
        }
        drop(app);
        let _ = std::fs::remove_file(&path);
        crate::tenant::remove_sidecars(&path);
    }

    #[tokio::test(start_paused = true)]
    async fn velocity_history_is_bounded_and_supports_threshold_changes() {
        use std::time::Duration;

        let mut cfg = default_automod_config();
        let engine = AutomodEngine::new();
        let mut message = msg(1, "ordinary message", 0, false, false);
        // Seed history through a legacy rule whose threshold exceeds the supported range.
        cfg.rules.get_mut("velocity").unwrap().params["max_messages"] = serde_json::json!(u64::MAX);
        for _ in 0..20 {
            message.message_id += 1;
            assert!(engine.evaluate(&cfg, &message).is_none());
        }
        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..1023 {
            message.message_id += 1;
            assert!(engine.evaluate(&cfg, &message).is_none());
        }
        let key = (message.guild_id, message.author_id);
        assert_eq!(engine.state.get(&key).unwrap().velocity_events.len(), 1024);

        // A legacy threshold one past the maximum must never trigger, even
        // when the author has sent enough messages to exceed it.
        cfg.rules.get_mut("velocity").unwrap().params["max_messages"] = serde_json::json!(1024);
        message.message_id += 1;
        assert!(engine.evaluate(&cfg, &message).is_none());

        // At 10s the first batch is exactly outside the window. The newest
        // observations must survive both the cap and the configuration reload.
        cfg.rules.get_mut("velocity").unwrap().params["max_messages"] = serde_json::json!(1023);
        tokio::time::advance(Duration::from_secs(9)).await;
        message.message_id += 1;
        let action = engine.evaluate(&cfg, &message).expect("over the limit");
        assert_eq!(action.rule_id, "velocity");
        assert_eq!(action.strikes, 1);
        assert_eq!(action.action.action, "delete");

        tokio::time::advance(Duration::from_secs(1)).await;
        message.message_id += 1;
        assert!(engine.evaluate(&cfg, &message).is_none());

        tokio::time::advance(Duration::from_secs(590)).await;
        engine.prune();
        assert_eq!(engine.state.get(&key).unwrap().velocity_events.len(), 2);
        tokio::time::advance(Duration::from_secs(10)).await;
        engine.prune();
        assert!(!engine.state.contains_key(&key));
    }

    #[tokio::test(start_paused = true)]
    async fn velocity_excludes_expired_and_exempt_messages_and_isolates_guilds_and_authors() {
        let mut cfg = default_automod_config();
        let rule = cfg.rules.get_mut("velocity").unwrap();
        rule.whitelist_users = vec![99];
        rule.whitelist_channels = vec![3];
        let engine = AutomodEngine::new();
        let mut message = msg(1, "ordinary message", 0, false, false);
        for id in 1..=8 {
            message.message_id = id;
            assert!(engine.evaluate(&cfg, &message).is_none());
        }
        tokio::time::advance(std::time::Duration::from_secs(10)).await;

        for (guild, author, channel, count) in [
            (2, 42, 1, 8),
            (1, 50, 1, 8),
            (1, 99, 1, 9),
            (1, 42, 3, 9),
            (1, 42, 1, 8),
        ] {
            message.guild_id = guild;
            message.author_id = author;
            message.channel_id = channel;
            for _ in 0..count {
                message.message_id += 1;
                assert!(
                    engine.evaluate(&cfg, &message).is_none(),
                    "guild {guild}, author {author}, channel {channel}"
                );
            }
        }
        message.message_id += 1;
        let action = engine
            .evaluate(&cfg, &message)
            .expect("ninth eligible message");
        assert_eq!(action.rule_id, "velocity");
        assert_eq!(action.action.action, "delete");
    }

    #[tokio::test(start_paused = true)]
    async fn velocity_counts_messages_once_when_other_rules_match() {
        let cfg = default_automod_config();
        let engine = AutomodEngine::new();
        let mut message = msg(1, "ordinary message", 0, false, false);
        message.account_age_days = Some(1);
        for id in 1..=9 {
            message.message_id = id;
            let action = engine.evaluate(&cfg, &message);
            assert_eq!(
                action.as_ref().map(|action| action.rule_id.as_str()),
                match id {
                    1 => Some("account_age"),
                    9 => Some("velocity"),
                    _ => None,
                },
                "message {id}",
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ordinary_messages_do_not_keep_expired_velocity_strikes() {
        let mut cfg = default_automod_config();
        cfg.rules.get_mut("velocity").unwrap().params["max_messages"] = serde_json::json!(1);
        let engine = AutomodEngine::new();
        let mut message = msg(1, "ordinary message", 0, false, false);
        assert!(engine.evaluate(&cfg, &message).is_none());
        message.message_id += 1;
        assert_eq!(engine.evaluate(&cfg, &message).unwrap().strikes, 1);

        let retention = crate::types::AUTOMOD_WINDOW_MAX_SECS;
        tokio::time::advance(std::time::Duration::from_secs(retention - 1)).await;
        message.message_id += 1;
        assert!(engine.evaluate(&cfg, &message).is_none());
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        engine.prune();

        message.message_id += 1;
        let action = engine
            .evaluate(&cfg, &message)
            .expect("new velocity strike");
        assert_eq!(action.strikes, 1);
        assert_eq!(action.action.action, "delete");
    }

    #[tokio::test(start_paused = true)]
    async fn dry_run_velocity_does_not_displace_a_live_action() {
        let mut cfg = default_automod_config();
        cfg.rules.get_mut("velocity").unwrap().dry_run = true;
        cfg.regexes = vec![jmodguard::normalize::compile_user_regex("scam").unwrap()];
        let engine = AutomodEngine::new();
        let mut message = msg(1, "ordinary message", 0, false, false);
        for id in 1..=9 {
            message.message_id = id;
            let action = engine.evaluate(&cfg, &message);
            // A dry-run velocity match alone is still selected so it is logged.
            assert_eq!(
                action.map(|action| action.rule_id),
                (id == 9).then(|| "velocity".to_owned()),
                "message {id}",
            );
        }
        // Velocity would time out (dry-run); the live badword delete must win.
        message.message_id = 10;
        message.content = "scam";
        let action = engine.evaluate(&cfg, &message).expect("live badword");
        assert_eq!(action.rule_id, "badword");
        assert_eq!(action.action.action, "delete");
    }

    #[tokio::test(start_paused = true)]
    async fn expired_strikes_reset_without_waiting_for_cleanup() {
        let mut cfg = default_automod_config();
        cfg.rules.get_mut("velocity").unwrap().params["max_messages"] = serde_json::json!(1);
        let engine = AutomodEngine::new();
        let mut message = msg(1, "ordinary message", 0, false, false);
        assert!(engine.evaluate(&cfg, &message).is_none());
        message.message_id += 1;
        assert_eq!(engine.evaluate(&cfg, &message).unwrap().strikes, 1);

        // Cleanup runs just before the strike expires, so it keeps the bucket.
        let retention = crate::types::AUTOMOD_WINDOW_MAX_SECS;
        tokio::time::advance(std::time::Duration::from_secs(retention - 1)).await;
        engine.prune();
        tokio::time::advance(std::time::Duration::from_secs(2)).await;

        // The next burst starts a new ladder before the next cleanup tick.
        message.message_id += 1;
        assert!(engine.evaluate(&cfg, &message).is_none());
        message.message_id += 1;
        let action = engine.evaluate(&cfg, &message).expect("new burst");
        assert_eq!(action.strikes, 1);
        assert_eq!(action.action.action, "delete");
    }

    #[test]
    fn dry_run_known_spam_does_not_displace_a_live_action() {
        let mut cfg = default_automod_config();
        cfg.rules.get_mut("known_spam").unwrap().dry_run = true;
        cfg.regexes = vec![jmodguard::normalize::compile_user_regex("scam").unwrap()];
        cfg.known_spam =
            jmodguard::known_spam::CompiledList::new(&[jmodguard::known_spam::Entry::new(
                "url",
                "tinyurl.com/*",
                None,
            )
            .unwrap()]);
        let engine = AutomodEngine::new();
        // Known spam alone in dry-run is still selected so it is logged.
        let alone = engine
            .evaluate(&cfg, &msg(1, "link tinyurl.com/abc", 0, false, false))
            .expect("dry-run known spam");
        assert_eq!(alone.rule_id, "known_spam");
        // A live badword match on the same message wins.
        let both = engine
            .evaluate(&cfg, &msg(2, "scam tinyurl.com/abc", 0, false, false))
            .expect("live badword");
        assert_eq!(both.rule_id, "badword");
        assert_eq!(both.action.action, "delete");
    }

    #[tokio::test(start_paused = true)]
    async fn dry_run_strikes_do_not_carry_into_live_enforcement() {
        let mut cfg = default_automod_config();
        cfg.rules.get_mut("velocity").unwrap().dry_run = true;
        let engine = AutomodEngine::new();
        let mut message = msg(1, "ordinary message", 0, false, false);
        // Reach the final dry-run threshold (kick at strike 4).
        for id in 1..=12 {
            message.message_id = id;
            let action = engine.evaluate(&cfg, &message);
            if id == 12 {
                assert_eq!(action.unwrap().action.action, "kick");
            }
        }
        // Switching to live keeps the message history but starts a new ladder.
        cfg.rules.get_mut("velocity").unwrap().dry_run = false;
        message.message_id = 13;
        let action = engine.evaluate(&cfg, &message).expect("first live action");
        assert_eq!(action.rule_id, "velocity");
        assert_eq!(action.strikes, 1);
        assert_eq!(action.action.action, "delete");
    }

    #[test]
    fn only_mention_spam_keeps_strike_timestamps() {
        let mut cfg = default_automod_config();
        cfg.regexes = vec![jmodguard::normalize::compile_user_regex("scam").unwrap()];
        let engine = AutomodEngine::new();
        let mut message = msg(1, "scam", 0, false, false);
        for id in 1..=3 {
            message.message_id = id;
            engine.evaluate(&cfg, &message);
        }
        let bucket = engine
            .state
            .get(&(message.guild_id, message.author_id))
            .unwrap();
        let badword = &bucket.rules[&("badword", false)];
        assert_eq!(badword.strikes, 3);
        assert!(badword.events.is_empty());
    }

    // The mode can change while the executor awaits enrollment and reloads
    // configuration. The action must never run more live than it was selected.
    #[tokio::test]
    async fn a_mode_switch_before_execution_never_makes_an_action_live() {
        use serenity::model::id::{GuildId, UserId};
        use serenity::model::user::User;
        use std::sync::{Arc, atomic::AtomicBool};

        for selected_dry in [true, false] {
            let path = std::env::temp_dir()
                .join(format!("mode_switch_{:032x}.db", rand::random::<u128>()));
            let app =
                BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap();
            assert!(
                crate::db::claim_active_bot(
                    &app.db,
                    app.activation.instance(),
                    jmodguard::control::now_ms(),
                    Arc::new(AtomicBool::new(false))
                )
                .await
                .unwrap()
            );
            app.activation.mark_lock_held(true);
            let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
                .await
                .unwrap()
                .unwrap();
            app.tenant.activate(enrollment).await.unwrap();
            let store = |dry_run: bool| {
                let mut rule = crate::defaults::default_rules().remove(0);
                rule.dry_run = dry_run;
                let data = serde_json::to_string(&rule).unwrap();
                let db = app.db.clone();
                async move {
                    db.with_conn(move |conn| {
                        use crate::db::SqlExecutor;
                        conn.execute_bounded(
                            "INSERT OR REPLACE INTO rules(guild_id,rule_id,data) VALUES (10,'mention_spam',?1)",
                            crate::db::params![data],
                            1,
                        )?;
                        Ok(())
                    })
                    .await
                    .unwrap();
                }
            };
            store(selected_dry).await;
            app.invalidate_configs();
            let cfg = app.configs.get(&app.db, 10).await.unwrap();
            let mut message = word_message(&[]);
            message.everyone_mentions = 6;
            let pending = AutomodEngine::new()
                .evaluate(&cfg, &message)
                .expect("mention spam ban");
            assert_eq!(pending.action.action, "ban");
            assert_eq!(pending.dry_run, selected_dry);

            store(!selected_dry).await;
            app.invalidate_configs();
            let mut user = User::default();
            user.id = UserId::new(99);
            user.name = "member".into();
            let stub = DiscordStub::start();
            let live = execute_pending_with(
                &app,
                &stub.http,
                &stub.cache,
                GuildId::new(10),
                &user,
                None,
                "member",
                Some(42),
                Some(70),
                &pending,
            )
            .await;
            let seen = stub.finish();
            assert!(!live, "selected dry-run={selected_dry}");
            assert!(
                seen.is_empty(),
                "selected dry-run={selected_dry}: no Discord action: {seen:?}"
            );
            drop(app);
            let _ = std::fs::remove_file(&path);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn velocity_counts_messages_by_arrival_time() {
        use super::AutomodInstant;
        use std::time::Duration;
        let mut cfg = default_automod_config();
        let velocity = cfg.rules.get_mut("velocity").unwrap();
        velocity.params["max_messages"] = serde_json::json!(1);
        velocity.params["window_secs"] = serde_json::json!(1);
        let start = AutomodInstant::now();
        let message = |id: u64, after_ms: u64| {
            let mut message = msg(1, "ordinary message", 0, false, false);
            message.message_id = id;
            message.received = Some(start + Duration::from_millis(after_ms));
            message
        };
        // A slow configuration read makes both evaluations finish together.
        tokio::time::advance(Duration::from_secs(3)).await;
        for order in [[0, 1500], [1500, 0]] {
            let engine = AutomodEngine::new();
            for (id, after_ms) in order.into_iter().enumerate() {
                assert!(
                    engine
                        .evaluate(&cfg, &message(id as u64, after_ms))
                        .is_none(),
                    "arrivals 1.5 s apart, evaluated in order {order:?}"
                );
            }
        }
        // Arrivals inside the window still count when evaluated in order.
        let engine = AutomodEngine::new();
        assert!(engine.evaluate(&cfg, &message(1, 0)).is_none());
        let action = engine
            .evaluate(&cfg, &message(2, 500))
            .expect("over the limit");
        assert_eq!(action.strikes, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_dry_run_trial_neither_adds_to_nor_erases_live_strikes() {
        use std::time::Duration;
        let mut cfg = default_automod_config();
        cfg.rules.get_mut("velocity").unwrap().params["max_messages"] = serde_json::json!(1);
        let engine = AutomodEngine::new();
        let mut message = msg(1, "ordinary message", 0, false, false);
        assert!(engine.evaluate(&cfg, &message).is_none());
        message.message_id += 1;
        assert_eq!(engine.evaluate(&cfg, &message).unwrap().strikes, 1);

        // Dry-run strikes go to their own ladder.
        cfg.rules.get_mut("velocity").unwrap().dry_run = true;
        tokio::time::advance(Duration::from_secs(20)).await;
        for strikes in [None, Some(1), Some(2)] {
            message.message_id += 1;
            assert_eq!(engine.evaluate(&cfg, &message).map(|a| a.strikes), strikes);
        }
        cfg.rules.get_mut("velocity").unwrap().dry_run = false;
        tokio::time::advance(Duration::from_secs(20)).await;
        message.message_id += 1;
        assert!(engine.evaluate(&cfg, &message).is_none());
        message.message_id += 1;
        let action = engine.evaluate(&cfg, &message).expect("live escalation");
        assert_eq!(action.strikes, 2);
        assert_eq!(action.action.action, "timeout");
    }

    #[tokio::test(start_paused = true)]
    async fn out_of_order_velocity_never_strikes_a_message_under_the_limit() {
        use super::AutomodInstant;
        use std::time::Duration;
        let mut cfg = default_automod_config();
        let velocity = cfg.rules.get_mut("velocity").unwrap();
        velocity.params["max_messages"] = serde_json::json!(1);
        velocity.params["window_secs"] = serde_json::json!(1);
        let start = AutomodInstant::now();
        tokio::time::advance(Duration::from_secs(5)).await;
        // Message IDs are the arrival times in milliseconds. In receipt order
        // 750 and 1500 are over the limit; 0 never is.
        let run = |order: &[u64]| {
            let engine = AutomodEngine::new();
            order
                .iter()
                .filter_map(|&after_ms| {
                    let mut message = msg(1, "ordinary message", 0, false, false);
                    message.message_id = after_ms;
                    message.received = Some(start + Duration::from_millis(after_ms));
                    engine.evaluate(&cfg, &message).map(|action| {
                        assert_eq!(action.evidence["message_id"], after_ms);
                        (after_ms, action.action.action)
                    })
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            run(&[0, 750, 1500]),
            [(750, "delete".to_owned()), (1500, "timeout".to_owned())]
        );
        for order in [
            [750, 0, 1500],
            [0, 1500, 750],
            [1500, 750, 0],
            [750, 1500, 0],
        ] {
            for (struck, _) in run(&order) {
                assert_ne!(
                    struck, 0,
                    "order {order:?} struck a message under the limit"
                );
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn mention_spam_expires_strikes_by_evaluation_time() {
        use super::AutomodInstant;
        use std::time::Duration;
        let mut cfg = default_automod_config();
        // An escalating ladder makes an extra strike visible.
        let ladder = cfg.rules["velocity"].actions.clone();
        let mention = cfg.rules.get_mut("mention_spam").unwrap();
        mention.actions = ladder;
        mention.params["window_secs"] = serde_json::json!(1);
        mention.params["max_mentions_in_window"] = serde_json::json!(5);
        let engine = AutomodEngine::new();
        let start = AutomodInstant::now();
        // The later message is evaluated first and strikes.
        tokio::time::advance(Duration::from_secs(2)).await;
        let mut later = msg(1, "ping", 0, false, false);
        later.message_id = 2;
        later.role_mentions = 6;
        later.received = Some(start + Duration::from_secs(2));
        assert_eq!(
            engine.evaluate(&cfg, &later).unwrap().rule_id,
            "mention_spam"
        );
        // The older message, at the limit, is evaluated two seconds later.
        tokio::time::advance(Duration::from_secs(2)).await;
        let mut older = msg(1, "ping", 0, false, false);
        older.message_id = 1;
        older.role_mentions = 5;
        older.received = Some(start);
        assert!(engine.evaluate(&cfg, &older).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_late_dry_run_evaluation_cannot_reset_the_live_ladder() {
        let mut live = default_automod_config();
        live.rules.get_mut("velocity").unwrap().params["max_messages"] = serde_json::json!(1);
        let mut stale = default_automod_config();
        let velocity = stale.rules.get_mut("velocity").unwrap();
        velocity.params["max_messages"] = serde_json::json!(1);
        velocity.dry_run = true;
        let engine = AutomodEngine::new();
        let mut message = msg(1, "ordinary message", 0, false, false);
        assert!(engine.evaluate(&live, &message).is_none());
        message.message_id += 1;
        assert_eq!(engine.evaluate(&live, &message).unwrap().strikes, 1);
        // A late evaluation still holding the older dry-run snapshot only adds
        // a log-only strike to the dry-run ladder.
        message.message_id += 1;
        assert!(engine.evaluate(&stale, &message).unwrap().dry_run);
        message.message_id += 1;
        let action = engine.evaluate(&live, &message).expect("escalation");
        assert_eq!(action.strikes, 2);
        assert_eq!(action.action.action, "timeout");
    }

    #[tokio::test(start_paused = true)]
    async fn an_older_strike_timestamp_cannot_shorten_retention() {
        use super::{AutomodInstant, RuleRetention, RuleState, STRIKE_RETENTION};
        use std::time::Duration;
        let older = AutomodInstant::now();
        let newer = older + Duration::from_secs(10);
        let mut state = RuleState::default();
        // Concurrent evaluations record the newer strike first.
        assert_eq!(state.record_strike(newer, false), 1);
        assert_eq!(state.record_strike(older, false), 2);
        // The older strike has expired, the newer one has not.
        let check = older + STRIKE_RETENTION + Duration::from_secs(5);
        assert!(state.prune(check, STRIKE_RETENTION) == RuleRetention::Retained);
        assert_eq!(state.record_strike(check, false), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn velocity_reads_legacy_limits_stored_as_json_floats() {
        for (limit, first_strike) in [
            (serde_json::json!(5000.0), None),
            (serde_json::json!(1e30), None),
            (serde_json::json!(1023.5), None),
            // In-range non-integers keep their old default of 8, never stricter.
            (serde_json::json!(8.0), Some(9)),
            (serde_json::json!(1.0), Some(9)),
            (serde_json::json!(8.5), Some(9)),
            (serde_json::json!(0.5), Some(9)),
            (serde_json::json!("3"), Some(9)),
        ] {
            let mut cfg = default_automod_config();
            cfg.rules.get_mut("velocity").unwrap().params["max_messages"] = limit.clone();
            let engine = AutomodEngine::new();
            let mut message = msg(1, "ordinary message", 0, false, false);
            let struck = (1..=12).find(|id| {
                message.message_id = *id;
                engine.evaluate(&cfg, &message).is_some()
            });
            assert_eq!(struck, first_strike, "max_messages {limit}");
        }
    }

    fn default_automod_config() -> CachedGuildConfig {
        let rules = crate::types::RULE_IDS
            .into_iter()
            .zip(crate::defaults::default_rules())
            .map(|(id, rule)| (id.into(), rule))
            .collect();
        test_cfg(&rules, &HashMap::new())
    }

    #[test]
    fn default_badword_ladder_escalates_on_distinct_matches() {
        let mut cfg = default_automod_config();
        cfg.regexes = vec![jmodguard::normalize::compile_user_regex("scam").unwrap()];
        let engine = AutomodEngine::new();
        let mut message = msg(1, "scam", 0, false, false);
        for (strike, expected) in [
            (1, Some(("badword", "delete", 0, 1))),
            (2, Some(("badword", "warn", 0, 2))),
            (3, None),
            (4, Some(("badword", "timeout", 60, 4))),
        ] {
            message.message_id = strike;
            let action = engine.evaluate(&cfg, &message);
            let actual = action.as_ref().map(|action| {
                (
                    action.rule_id.as_str(),
                    action.action.action.as_str(),
                    action.action.duration_minutes,
                    action.strikes,
                )
            });
            assert_eq!(actual, expected, "badword match {strike}");
        }
    }

    #[test]
    fn stronger_live_action_wins_in_either_rule_order() {
        let mut cfg = default_automod_config();
        cfg.regexes = vec![jmodguard::normalize::compile_user_regex("scam").unwrap()];
        let message = msg(1, "scam https://discord.gg/test", 0, false, false);

        // Badword is visited first. Keep both rules live and move the ban
        // between them so choosing the first or last match cannot pass.
        for (stronger, weaker) in [("invite", "badword"), ("badword", "invite")] {
            cfg.rules.get_mut(weaker).unwrap().actions[0].action = "delete".into();
            cfg.rules.get_mut(stronger).unwrap().actions[0].action = "ban".into();
            let action = AutomodEngine::new()
                .evaluate(&cfg, &message)
                .expect("both live rules match");
            assert_eq!(action.rule_id, stronger);
            assert_eq!(action.action.action, "ban");
            assert!(!action.dry_run);
        }
    }

    #[test]
    fn builtin_legacy_windows_clamp_to_retained_history() {
        assert_legacy_windows(&[(0, 500, true), (0, 2000, false)]);
    }

    #[test]
    fn builtin_legacy_long_windows_clamp_to_retained_history() {
        assert_legacy_windows(&[
            (601, 599_000, true),
            (601, 600_500, false),
            (u64::MAX, 599_000, true),
            (u64::MAX, 601_000, false),
        ]);
    }

    fn assert_legacy_windows(cases: &[(u64, u64, bool)]) {
        use super::{AuthorState, AutomodInstant, RuleState, WindowEvent};
        use std::time::Duration;
        for id in ["mention_spam", "velocity"] {
            for &(window, age_ms, expected) in cases {
                let engine = AutomodEngine::new();
                let now = AutomodInstant::now();
                let mut rule = crate::defaults::default_rules()
                    [if id == "mention_spam" { 0 } else { 6 }]
                .clone();
                rule.params = serde_json::json!({"window_secs": window, "max_mentions_in_window": 1, "max_messages": 1});
                let at = now - Duration::from_millis(age_ms);
                let mut bucket = AuthorState::default();
                if id == "velocity" {
                    bucket.velocity_events.extend([at, now]);
                } else {
                    bucket.rules.insert(
                        (id, false),
                        RuleState {
                            events: std::collections::VecDeque::from([WindowEvent { at }]),
                            last_event: Some(at),
                            ..Default::default()
                        },
                    );
                }
                engine.state.insert((1, 42), bucket);
                let mut ctx = msg(1, "", 0, false, false);
                ctx.role_mentions = 1;
                let triggered = || {
                    if id == "mention_spam" {
                        engine.trigger_mention_spam(&rule, &ctx, now)
                    } else {
                        // The newest observation is the current message.
                        let bucket = engine.state.get(&(1, 42)).unwrap();
                        bucket.velocity_over_limit(&rule, bucket.velocity_events.len() - 1)
                    }
                };
                assert_eq!(triggered(), expected, "{id}: window {window}, age {age_ms}");
                engine.prune();
                assert_eq!(
                    triggered(),
                    expected,
                    "pruning must agree with the effective window"
                );
            }
        }
    }

    #[test]
    fn invite_code_form_params_match_without_reinterpreting_legacy_guild_ids() {
        let engine = AutomodEngine::new();
        let mut rule = crate::defaults::default_rules()
            .into_iter()
            .find(|rule| rule.name == "Invite links")
            .unwrap();
        assert_eq!(rule.params, serde_json::json!({"allowlist_codes": []}));
        assert!(!engine.invite_allowed(&rule, "https://discord.gg/Allowed-Code"));
        rule.params = serde_json::json!({"allowlist_codes": ["Allowed-Code", "second"]});
        assert!(engine.invite_allowed(&rule, "https://discord.gg/Allowed-Code"));
        assert!(engine.invite_allowed(&rule, "https://discord.com/invite/second"));
        assert!(!engine.invite_allowed(&rule, "https://discord.gg/unlisted"));
        rule.params = serde_json::json!({"allowlist_guilds": ["9007199254740993"]});
        assert!(!engine.invite_allowed(&rule, "https://discord.gg/9007199254740993"));
    }

    #[test]
    fn releasing_a_fired_ban_lets_the_next_message_select_it_again() {
        let engine = AutomodEngine::new();
        let mut rules = HashMap::new();
        rules.insert(
            "invite".into(),
            RuleDef {
                name: "invites".into(),
                description: String::new(),
                enabled: true,
                dry_run: false,
                params: serde_json::json!({}),
                actions: vec![ActionDef {
                    at: 1,
                    action: "ban".into(),
                    duration_minutes: 0,
                    delete_days: 0,
                    purge_last_hour: false,
                    reason: "invite".into(),
                }],
                whitelist_roles: Vec::new(),
                whitelist_channels: Vec::new(),
                whitelist_users: Vec::new(),
                skip_moderators: false,
                include_channels: vec![],
                log_automod: true,
                open_case: true,
            },
        );
        let cfg = CachedGuildConfig {
            version: 1,
            durable_version: 1,
            loaded_at: Instant::now(),
            settings: GuildSettings::default(),
            rules,
            commands: HashMap::new(),
            templates: HashMap::new(),
            log_channels: HashMap::new(),
            known_bad_hashes: Default::default(),
            known_spam: Default::default(),
            log_filters: HashMap::new(),
            regexes: Vec::new(),
            custom_patterns: HashMap::new(),
        };
        let roles = [];
        let message = MsgCtx {
            guild_id: 1,
            message_id: 9,
            channel_id: 8,
            author_id: 2,
            content: "discord.gg/spam",
            attachment_count: 0,
            sticker_count: 0,
            everyone_mentions: 0,
            here_mentions: 0,
            role_mentions: 0,
            joined_secs_ago: None,
            account_age_days: Some(30),
            member_roles: &roles,
            is_moderator: false,
            can_mention_everyone: false,
            mention_permission_unknown: false,
            received: None,
        };
        let first = engine.evaluate(&cfg, &message).expect("first ban");
        assert_eq!(first.action.action, "ban");
        assert!(engine.evaluate(&cfg, &message).is_none());
        engine.release_fired(1, 2, &first.rule_id, first.dry_run, first.action.at);
        let retry = engine.evaluate(&cfg, &message).expect("retried ban");
        assert_eq!(retry.action.action, "ban");
        assert_eq!(retry.action.at, 1);
    }

    fn test_cfg(
        rules: &HashMap<String, RuleDef>,
        patterns: &HashMap<String, regex::Regex>,
    ) -> CachedGuildConfig {
        CachedGuildConfig {
            version: 1,
            durable_version: 1,
            loaded_at: Instant::now(),
            settings: GuildSettings::default(),
            rules: rules.clone(),
            commands: HashMap::new(),
            templates: HashMap::new(),
            log_channels: HashMap::new(),
            known_bad_hashes: Default::default(),
            known_spam: Default::default(),
            log_filters: HashMap::new(),
            regexes: Vec::new(),
            custom_patterns: patterns.clone(),
        }
    }

    fn custom_rule(params: serde_json::Value, action: &str) -> RuleDef {
        RuleDef {
            name: "custom".into(),
            description: String::new(),
            enabled: true,
            dry_run: true,
            params,
            actions: vec![ActionDef {
                at: 1,
                action: action.into(),
                duration_minutes: 0,
                delete_days: 0,
                purge_last_hour: false,
                reason: "r".into(),
            }],
            whitelist_roles: vec![],
            whitelist_channels: vec![],
            whitelist_users: vec![],
            skip_moderators: false,
            include_channels: vec![],
            log_automod: true,
            open_case: true,
        }
    }

    fn msg(
        channel: u64,
        content: &str,
        attachments: usize,
        is_mod: bool,
        can_everyone: bool,
    ) -> MsgCtx<'_> {
        MsgCtx {
            guild_id: 1,
            message_id: 1,
            channel_id: channel,
            author_id: 42,
            content,
            attachment_count: attachments,
            sticker_count: 0,
            everyone_mentions: 0,
            here_mentions: 0,
            role_mentions: 0,
            joined_secs_ago: None,
            account_age_days: Some(30),
            member_roles: &[],
            is_moderator: is_mod,
            can_mention_everyone: can_everyone,
            mention_permission_unknown: false,
            received: None,
        }
    }

    #[test]
    fn custom_pattern_and_word_rules() {
        let id = "custom:0123456789abcdef".to_string();
        let mut rules = HashMap::new();
        rules.insert(
            id.clone(),
            custom_rule(
                serde_json::json!({ "kind": "pattern", "pattern": "scam" }),
                "delete",
            ),
        );
        let patterns = HashMap::from([(
            id.clone(),
            jmodguard::normalize::compile_user_regex("scam").unwrap(),
        )]);
        let engine = AutomodEngine::new();
        let cfg = test_cfg(&rules, &patterns);
        assert!(
            engine
                .evaluate(&cfg, &msg(1, "this is a scam", 0, false, false))
                .is_some()
        );
        assert!(
            engine
                .evaluate(&cfg, &msg(1, "safe text", 0, false, false))
                .is_none()
        );

        rules.clear();
        rules.insert(
            id.clone(),
            custom_rule(
                serde_json::json!({ "kind": "word", "words": ["scam"] }),
                "delete",
            ),
        );
        let cfg = test_cfg(&rules, &HashMap::new());
        for content in ["scam", "scam!", "scam.", "(scam)"] {
            assert!(
                engine
                    .evaluate(&cfg, &msg(1, content, 0, false, false))
                    .is_some(),
                "{content}"
            );
        }
        assert!(
            engine
                .evaluate(&cfg, &msg(1, "scamper", 0, false, false))
                .is_none()
        );
    }

    #[test]
    fn badword_setup_literals_use_builtin_rule_and_preserve_regex_behavior() {
        let rule = custom_rule(serde_json::json!({}), "delete");
        let rules = HashMap::from([("badword".to_owned(), rule)]);
        let mut cfg = test_cfg(&rules, &HashMap::new());
        cfg.regexes = jmodguard::normalize::badword_patterns("scam, pump and dump, c++")
            .unwrap()
            .iter()
            .map(|p| jmodguard::normalize::compile_user_regex(p).unwrap())
            .collect();
        cfg.regexes
            .push(jmodguard::normalize::compile_user_regex("^LEGACY$").unwrap());
        for (content, expected) in [
            ("5c@m", true),
            ("a s c a m here", true),
            ("pump  and dump", true),
            ("c++", true),
            ("scammer", false),
            ("LEGACY", true),
            ("legacy", false),
        ] {
            assert_eq!(
                AutomodEngine::new()
                    .evaluate(&cfg, &msg(1, content, 0, false, false))
                    .is_some(),
                expected,
                "{content}"
            );
        }
    }

    #[test]
    fn custom_scope_include_and_skip_lists() {
        let id = "custom:0123456789abcdef".to_string();
        let mut rule = custom_rule(
            serde_json::json!({ "kind": "word", "words": ["hit"] }),
            "delete",
        );
        let engine = AutomodEngine::new();

        let mut rules = HashMap::from([(id.clone(), rule.clone())]);
        let cfg = test_cfg(&rules, &HashMap::new());
        assert!(
            engine
                .evaluate(&cfg, &msg(5, "hit", 0, false, false))
                .is_some()
        );

        rule.include_channels = vec![10, 20];
        rules = HashMap::from([(id.clone(), rule.clone())]);
        let cfg = test_cfg(&rules, &HashMap::new());
        assert!(
            engine
                .evaluate(&cfg, &msg(10, "hit", 0, false, false))
                .is_some()
        );
        assert!(
            engine
                .evaluate(&cfg, &msg(20, "hit", 0, false, false))
                .is_some()
        );
        assert!(
            engine
                .evaluate(&cfg, &msg(30, "hit", 0, false, false))
                .is_none()
        );

        rule.whitelist_channels = vec![20];
        rules = HashMap::from([(id.clone(), rule.clone())]);
        let cfg = test_cfg(&rules, &HashMap::new());
        assert!(
            engine
                .evaluate(&cfg, &msg(10, "hit", 0, false, false))
                .is_some()
        );
        assert!(
            engine
                .evaluate(&cfg, &msg(20, "hit", 0, false, false))
                .is_none()
        );

        rule.whitelist_users = vec![42];
        rule.whitelist_channels = vec![];
        rules = HashMap::from([(id.clone(), rule.clone())]);
        let cfg = test_cfg(&rules, &HashMap::new());
        assert!(
            engine
                .evaluate(&cfg, &msg(10, "hit", 0, false, false))
                .is_none()
        );

        rule.whitelist_users = vec![];
        rule.whitelist_roles = vec![7];
        rules = HashMap::from([(id.clone(), rule.clone())]);
        let cfg = test_cfg(&rules, &HashMap::new());
        let roles = [7u64];
        let mut skipped = msg(10, "hit", 0, false, false);
        skipped.member_roles = &roles;
        assert!(engine.evaluate(&cfg, &skipped).is_none());
        rule.whitelist_roles = vec![];

        rule.whitelist_users = vec![];
        rule.skip_moderators = true;
        rules = HashMap::from([(id.clone(), rule.clone())]);
        let cfg = test_cfg(&rules, &HashMap::new());
        assert!(
            engine
                .evaluate(&cfg, &msg(10, "hit", 0, true, false))
                .is_none()
        );
        assert!(
            engine
                .evaluate(&cfg, &msg(10, "hit", 0, false, false))
                .is_some()
        );
    }

    const MEDIA_RULE_ID: &str = "custom:0123456789abcdef";

    fn media_rule() -> RuleDef {
        let mut rule = custom_rule(
            serde_json::json!({
                "kind": "channel_content",
                "preset": jmodguard::custom_rules::NO_LINKS_OR_MEDIA_PRESET,
                "links": true,
                "attachments": true,
                "stickers": true,
                "gifs": true,
                "allowed_domains": ["example.test"],
            }),
            "delete",
        );
        rule.include_channels = vec![70];
        rule.skip_moderators = true;
        rule.whitelist_roles = vec![7];
        rule
    }

    #[tokio::test]
    async fn media_preset_save_accepts_only_direct_chat_channels() {
        use serenity::all::{ChannelId, ChannelType, GuildChannel};

        let path =
            std::env::temp_dir().join(format!("media_channels_{:032x}.db", rand::random::<u128>()));
        let app =
            BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap();
        let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();
        let wire = crate::db::ScopeWire {
            global: false,
            guild_id: enrollment.guild_id,
            enrollment_id: enrollment.id.clone(),
            joined_at_ns: enrollment.joined_at_ns,
        };
        let (store, scope) = app.tenant.live(enrollment).await.unwrap();
        let channels = [
            (70, ChannelType::Text, true),
            (71, ChannelType::News, true),
            (72, ChannelType::Voice, true),
            (73, ChannelType::Stage, true),
            (74, ChannelType::Forum, false),
            (75, ChannelType::Unknown(16), false),
            (76, ChannelType::Category, false),
            (77, ChannelType::PublicThread, false),
        ];
        let discord_channels = channels.map(|(id, kind, _)| {
            let mut channel = GuildChannel::default();
            channel.id = ChannelId::new(id);
            channel.kind = kind;
            channel
        });
        let names = crate::server_names::snapshot([], &discord_channels);
        store.replace_server_names(scope, names, 1).await.unwrap();

        let mut last_saved = vec![];
        for (id, kind, allowed) in channels {
            let mut rule = media_rule();
            rule.include_channels = vec![id];
            let result = crate::db::apply_control(
                &app.db,
                Some(&app.tenant),
                crate::db::ControlOp::UpsertRule {
                    request: wire.clone(),
                    target: wire.clone(),
                    id: MEDIA_RULE_ID.into(),
                    rule,
                    clear_guild_override: false,
                    replace_velocity_limit: false,
                },
                None,
            )
            .await;
            if allowed {
                assert!(matches!(
                    result.unwrap(),
                    crate::db::ControlReply::Saved { .. }
                ));
                last_saved = vec![id];
            } else if kind == ChannelType::PublicThread {
                // Threads aren't in the snapshot. Pruning the unavailable ID
                // still prevents the rule from targeting a thread.
                let crate::db::ControlReply::Saved { body } = result.unwrap() else {
                    panic!("expected a saved rule");
                };
                assert_eq!(body["enabled"], false);
                last_saved.clear();
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains(&format!("Channel {id}"))
                );
            }
            let saved = crate::db::get_rule(&app.db, 10, MEDIA_RULE_ID)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(saved.include_channels, last_saved);
        }
        drop(app);
        let _ = std::fs::remove_file(&path);
        crate::tenant::remove_sidecars(&path);
    }

    #[test]
    fn media_preset_scopes_links_attachments_stickers_and_gifs() {
        let engine = AutomodEngine::new();
        let mut rule = media_rule();
        let cfg = |rule: &RuleDef| {
            test_cfg(
                &HashMap::from([(MEDIA_RULE_ID.to_owned(), rule.clone())]),
                &HashMap::new(),
            )
        };
        let check = |rule: &RuleDef, message: &MsgCtx<'_>| {
            engine
                .evaluate(&cfg(rule), message)
                .is_some_and(|hit| hit.rule_id == MEDIA_RULE_ID && hit.action.action == "delete")
        };
        for (content, blocked) in [
            ("plain text", false),
            ("see <https://EXAMPLE.test/path>", false),
            ("https://example.test https://other.test", true),
            ("https://example.test.other.test", true),
            ("https://sub.example.test", true),
            ("https://example.test_/path", true),
            ("https://example.test_", true),
            ("https://other.test@example.test", true),
        ] {
            assert_eq!(
                check(&rule, &msg(70, content, 0, false, false)),
                blocked,
                "{content}"
            );
        }
        let mut message = msg(70, "https://other.test", 0, false, false);
        assert!(check(&rule, &message));
        message.channel_id = 71;
        assert!(!check(&rule, &message));
        message.channel_id = 70;
        rule.include_channels.clear();
        assert!(!check(&rule, &message));
        rule.include_channels = vec![70];
        rule.enabled = false;
        assert!(!check(&rule, &message));
        rule.enabled = true;
        message.is_moderator = true;
        assert!(!check(&rule, &message));
        message.is_moderator = false;
        message.member_roles = &[7];
        assert!(!check(&rule, &message));
        message.member_roles = &[];

        message.content = "";
        message.attachment_count = 1;
        assert!(check(&rule, &message));
        rule.params["attachments"] = false.into();
        assert!(!check(&rule, &message));
        message.attachment_count = 0;
        message.sticker_count = 1;
        assert!(check(&rule, &message));
        rule.params["stickers"] = false.into();
        assert!(!check(&rule, &message));
        message.sticker_count = 0;

        for content in [
            "https://tenor.com/view/dancing",
            "https://media.giphy.com/media/clip/giphy.gif",
        ] {
            message.content = content;
            for (links, gifs, blocked) in [
                (true, false, true),
                (false, true, true),
                (false, false, false),
            ] {
                rule.params["links"] = links.into();
                rule.params["gifs"] = gifs.into();
                assert_eq!(
                    check(&rule, &message),
                    blocked,
                    "{content}, links={links}, gifs={gifs}"
                );
            }
        }
        rule.params["gifs"] = true.into();
        message.content = "https://tenor.com.other.test/gif";
        assert!(!check(&rule, &message));
        message.content = "https://tenor.com/view/dancing";
        rule.params["allowed_domains"] = serde_json::json!(["tenor.com"]);
        assert!(!check(&rule, &message));
    }

    #[test]
    fn custom_channel_content_and_everyone() {
        let id = "custom:0123456789abcdef".to_string();
        let engine = AutomodEngine::new();
        let mut rules = HashMap::new();
        rules.insert(
            id.clone(),
            custom_rule(
                serde_json::json!({ "kind": "channel_content", "links": true, "attachments": false }),
                "delete",
            ),
        );
        let cfg = test_cfg(&rules, &HashMap::new());
        assert!(
            engine
                .evaluate(&cfg, &msg(1, "see https://example.com", 0, false, false))
                .is_some()
        );
        assert!(
            engine
                .evaluate(&cfg, &msg(1, "no link", 0, false, false))
                .is_none()
        );

        rules.clear();
        rules.insert(
            id.clone(),
            custom_rule(
                serde_json::json!({ "kind": "channel_content", "links": false, "attachments": true }),
                "delete",
            ),
        );
        let cfg = test_cfg(&rules, &HashMap::new());
        assert!(
            engine
                .evaluate(&cfg, &msg(1, "file", 1, false, false))
                .is_some()
        );
        assert!(
            engine
                .evaluate(&cfg, &msg(1, "file", 0, false, false))
                .is_none()
        );

        rules.clear();
        rules.insert(
            id.clone(),
            custom_rule(
                serde_json::json!({ "kind": "channel_content", "links": true, "attachments": true }),
                "delete",
            ),
        );
        let cfg = test_cfg(&rules, &HashMap::new());
        assert!(
            engine
                .evaluate(&cfg, &msg(1, "see https://example.com", 0, false, false))
                .is_some()
        );
        assert!(
            engine
                .evaluate(&cfg, &msg(1, "discord.gg/spam", 0, false, false))
                .is_some()
        );
        assert!(
            engine
                .evaluate(&cfg, &msg(1, "file", 2, false, false))
                .is_some()
        );
        assert!(
            engine
                .evaluate(&cfg, &msg(1, "plain", 0, false, false))
                .is_none()
        );

        rules.clear();
        rules.insert(
            id.clone(),
            custom_rule(serde_json::json!({ "kind": "everyone_attempt" }), "delete"),
        );
        let cfg = test_cfg(&rules, &HashMap::new());
        let mut m = msg(1, "hello @everyone", 0, false, false);
        assert!(engine.evaluate(&cfg, &m).is_some());
        m.content = "HELLO @EVERYONE";
        assert!(engine.evaluate(&cfg, &m).is_some());
        m.content = "one @here attempt";
        assert!(engine.evaluate(&cfg, &m).is_some());
        m.can_mention_everyone = true;
        assert!(engine.evaluate(&cfg, &m).is_none());
    }

    #[test]
    fn everyone_attempt_punctuation_boundaries_with_denied_permission() {
        let id = "custom:0123456789abcdef".to_string();
        let rules = HashMap::from([(
            id,
            custom_rule(serde_json::json!({"kind": "everyone_attempt"}), "ban"),
        )]);
        let cfg = test_cfg(&rules, &HashMap::new());
        let engine = AutomodEngine::new();
        for (content, expected) in [
            ("@everyone", true),
            ("@here!", true),
            ("hello,@everyone", true),
            ("@everyone,@here", true),
            ("(@here);hello", true),
            ("@everyoneX", false),
            ("hello@everyone", false),
            ("@hereafter", false),
            ("@@everyone", false),
        ] {
            let message = msg(1, content, 0, false, false);
            assert_eq!(message.everyone_mentions, 0);
            assert!(!message.can_mention_everyone);
            assert!(!message.mention_permission_unknown);
            let hit = engine.evaluate(&cfg, &message);
            assert_eq!(hit.is_some(), expected, "{content}");
            if let Some(hit) = hit {
                assert!(!hit.mention_permission_unknown);
            }
        }
        let mut flagged = msg(1, "plain", 0, false, false);
        flagged.everyone_mentions = 1;
        assert!(
            engine.evaluate(&cfg, &flagged).is_some(),
            "mention flag shortcut"
        );
        flagged.can_mention_everyone = true;
        assert!(
            engine.evaluate(&cfg, &flagged).is_none(),
            "allowed mention is exempt"
        );
    }

    #[test]
    fn honeypot_role_added_helper() {
        let id = "custom:0123456789abcdef".to_string();
        let mut rules = HashMap::new();
        let mut honeypot = custom_rule(
            serde_json::json!({ "kind": "honeypot_role", "role_id": "99" }),
            "kick",
        );
        honeypot.dry_run = false;
        let mut log = honeypot.actions[0].clone();
        log.action = "log".into();
        honeypot.actions.insert(0, log);
        rules.insert(id.clone(), honeypot);
        let (matched_id, rule, primary) =
            jmodguard::custom_rules::honeypot_role_added(&rules, &[99], 42, &[], false).unwrap();
        assert_eq!(matched_id, id);
        assert_eq!(primary.action, "kick");
        let pending =
            PendingAction::from_custom(matched_id, rule, primary, serde_json::json!({}), false);
        assert_eq!(
            pending
                .actions()
                .iter()
                .map(|action| action.action.as_str())
                .collect::<Vec<_>>(),
            ["log", "kick"],
        );
        assert!(
            jmodguard::custom_rules::honeypot_role_added(&rules, &[], 42, &[], false).is_none()
        );
        rules.get_mut(&id).unwrap().whitelist_users = vec![42];
        assert!(
            jmodguard::custom_rules::honeypot_role_added(&rules, &[99], 42, &[], false).is_none()
        );
        rules.get_mut(&id).unwrap().whitelist_users.clear();
        rules.get_mut(&id).unwrap().dry_run = true;
        assert!(
            jmodguard::custom_rules::honeypot_role_added(&rules, &[99], 42, &[], false).is_some()
        );
        rules.get_mut(&id).unwrap().dry_run = false;
        rules.get_mut(&id).unwrap().skip_moderators = true;
        assert!(
            jmodguard::custom_rules::honeypot_role_added(&rules, &[99], 42, &[], true).is_none()
        );
    }

    #[tokio::test]
    async fn uncached_thread_deletes_everyone_attempt_without_kick_or_ban() {
        use std::io::{Read, Write};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use tracing::instrument::WithSubscriber;

        use serenity::model::guild::{Guild, PartialMember};
        use serenity::model::id::{ChannelId, GuildId, UserId};
        use serenity::model::user::User;

        use crate::automod::{AutomodEngine, MsgCtx, execute_pending_with};
        use crate::state::{BotState, CachedGuildConfig};
        use crate::types::{ActionDef, GuildSettings, RuleDef};

        #[derive(Clone)]
        struct CapturedLog(Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for CapturedLog {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().write(bytes)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        fn read_request(stream: &mut std::net::TcpStream) -> Option<(String, String)> {
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .ok()?;
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            loop {
                match stream.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            || error.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        break;
                    }
                    Err(_) => return None,
                }
                if buf.windows(4).any(|window| window == b"\r\n\r\n") || buf.len() > 65_536 {
                    break;
                }
            }
            let header_end = buf.windows(4).position(|window| window == b"\r\n\r\n")?;
            let header = String::from_utf8_lossy(&buf[..header_end]);
            let mut parts = header.lines().next()?.split_whitespace();
            let method = parts.next()?.to_string();
            let path = parts.next()?.to_string();
            Some((method, path))
        }

        let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        // Stopping wakes the blocking accept with one connection of its own.
        struct Stop(Arc<AtomicBool>, std::net::SocketAddr);
        impl Drop for Stop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
                let _ = std::net::TcpStream::connect(self.1);
            }
        }
        let stop_guard = Stop(Arc::clone(&stop), addr);
        let server_lines = Arc::clone(&lines);
        let server_stop = Arc::clone(&stop);
        let server = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if server_stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut stream) = stream else {
                    break;
                };
                let Some((method, path)) = read_request(&mut stream) else {
                    continue;
                };
                server_lines
                    .lock()
                    .unwrap()
                    .push(format!("{method} {path}"));
                let response =
                    "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        });

        const THREAD: u64 = 70;
        const AUTHOR: u64 = 99;
        const MESSAGE: u64 = 42;
        let mut guild = Guild::default();
        guild.id = GuildId::new(10);
        guild.owner_id = UserId::new(1);
        guild.name = "g".into();
        assert!(!guild.channels.contains_key(&ChannelId::new(THREAD)));
        assert!(
            guild
                .threads
                .iter()
                .all(|thread| thread.id != ChannelId::new(THREAD))
        );
        let partial: PartialMember = serde_json::from_value(serde_json::json!({
            "deaf": false,
            "joined_at": null,
            "mute": false,
            "nick": null,
            "roles": [],
            "pending": false,
            "premium_since": null,
            "guild_id": "10",
            "user": null,
            "permissions": null,
            "unusual_dm_activity_until": null,
            "avatar": null,
            "banner": null,
            "avatar_decoration_data": null,
        }))
        .unwrap();
        let scope = crate::commands::message_author_scope(
            0,
            &guild,
            ChannelId::new(THREAD),
            UserId::new(AUTHOR),
            &partial,
        );
        assert_eq!(scope.mention, crate::commands::MentionPermission::Unknown);

        let id = "custom:0123456789abcdef".to_string();
        let mut rules = HashMap::new();
        rules.insert(
            id,
            RuleDef {
                name: "custom".into(),
                description: String::new(),
                enabled: true,
                dry_run: false,
                params: serde_json::json!({ "kind": "everyone_attempt" }),
                actions: vec![ActionDef {
                    at: 1,
                    action: "ban".into(),
                    duration_minutes: 0,
                    delete_days: 0,
                    purge_last_hour: false,
                    reason: "r".into(),
                }],
                whitelist_roles: vec![],
                whitelist_channels: vec![],
                whitelist_users: vec![],
                skip_moderators: false,
                include_channels: vec![],
                log_automod: true,
                open_case: true,
            },
        );
        let cfg = CachedGuildConfig {
            version: 1,
            durable_version: 1,
            loaded_at: Instant::now(),
            settings: GuildSettings::default(),
            rules,
            commands: HashMap::new(),
            templates: HashMap::new(),
            log_channels: HashMap::new(),
            known_bad_hashes: Default::default(),
            known_spam: Default::default(),
            log_filters: HashMap::new(),
            regexes: Vec::new(),
            custom_patterns: HashMap::new(),
        };
        let roles = [];
        let message = MsgCtx {
            guild_id: 10,
            message_id: MESSAGE,
            channel_id: THREAD,
            author_id: AUTHOR,
            content: "@everyone",
            attachment_count: 0,
            sticker_count: 0,
            everyone_mentions: 1,
            here_mentions: 0,
            role_mentions: 0,
            joined_secs_ago: None,
            account_age_days: Some(30),
            member_roles: &roles,
            is_moderator: scope.is_moderator,
            can_mention_everyone: scope.mention == crate::commands::MentionPermission::Allowed,
            mention_permission_unknown: scope.mention
                == crate::commands::MentionPermission::Unknown,
            received: None,
        };
        let pending = AutomodEngine::new()
            .evaluate(&cfg, &message)
            .expect("unknown permission still matches");
        assert!(pending.mention_permission_unknown);
        assert_eq!(pending.action.action, "ban");

        let path = std::env::temp_dir().join(format!(
            "mention_unknown_{:032x}.db",
            rand::random::<u128>()
        ));
        let app =
            BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap();
        assert!(
            crate::db::claim_active_bot(
                &app.db,
                app.activation.instance(),
                jmodguard::control::now_ms(),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap()
        );
        app.activation.mark_lock_held(true);
        crate::db::upsert_rule(
            &app.db,
            10,
            "custom:0123456789abcdef",
            cfg.rules
                .get("custom:0123456789abcdef")
                .expect("everyone-attempt rule"),
        )
        .await
        .unwrap();
        app.poll_config_version().await.unwrap();
        app.configs.invalidate();
        let http = std::sync::Arc::new(
            serenity::http::HttpBuilder::new("token")
                .proxy(format!("http://{addr}"))
                .ratelimiter_disabled(true)
                .build(),
        );
        let cache = serenity::cache::Cache::new();
        let mut user = User::default();
        user.id = UserId::new(AUTHOR);
        user.name = "member".into();
        let log = CapturedLog(Arc::new(std::sync::Mutex::new(Vec::new())));
        let writer = log.clone();
        let subscriber = tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish(),
        );
        execute_pending_with(
            &app,
            &http,
            &cache,
            GuildId::new(10),
            &user,
            None,
            "member",
            Some(MESSAGE),
            Some(THREAD),
            &pending,
        )
        .with_subscriber(subscriber)
        .await;
        drop(stop_guard);
        let _ = server.join();
        let seen = lines.lock().unwrap().clone();
        assert!(
            seen.iter().any(|line| {
                line.starts_with("DELETE ") && line.contains(&format!("/messages/{MESSAGE}"))
            }),
            "the triggering message must be deleted: {seen:?}"
        );
        assert!(
            seen.iter().all(|line| !line.contains("/bans/")),
            "unknown mention permission must not ban: {seen:?}"
        );
        assert!(
            seen.iter()
                .all(|line| !(line.starts_with("DELETE ") && line.contains("/members/"))),
            "unknown mention permission must not kick: {seen:?}"
        );
        let output = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
        assert!(
            output.contains("WARN")
                && output.contains(jmodguard::custom_rules::UNKNOWN_MENTION_PERMISSION_WARNING),
            "{output}"
        );
        drop(app);
        let _ = std::fs::remove_file(&path);
        crate::tenant::remove_sidecars(&path);
    }

    fn gateway_partial(guild: u64) -> serenity::model::guild::PartialMember {
        serde_json::from_value(serde_json::json!({
            "deaf": false,
            "joined_at": null,
            "mute": false,
            "nick": null,
            "roles": [],
            "pending": false,
            "premium_since": null,
            "guild_id": guild.to_string(),
            "user": null,
            "permissions": null,
            "unusual_dm_activity_until": null,
            "avatar": null,
            "banner": null,
            "avatar_decoration_data": null,
        }))
        .unwrap()
    }

    fn everyone_ban_rule() -> RuleDef {
        RuleDef {
            name: "custom".into(),
            description: String::new(),
            enabled: true,
            dry_run: false,
            params: serde_json::json!({ "kind": "everyone_attempt" }),
            actions: vec![ActionDef {
                at: 1,
                action: "ban".into(),
                duration_minutes: 0,
                delete_days: 0,
                purge_last_hour: false,
                reason: "r".into(),
            }],
            whitelist_roles: vec![],
            whitelist_channels: vec![],
            whitelist_users: vec![],
            skip_moderators: false,
            include_channels: vec![],
            log_automod: true,
            open_case: true,
        }
    }

    async fn execute_scoped_everyone_ban(
        scope: crate::commands::MessageAuthorScope,
    ) -> (Vec<String>, String) {
        use std::io::{Read, Write};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use tracing::instrument::WithSubscriber;

        use serenity::model::id::{GuildId, UserId};
        use serenity::model::user::User;

        #[derive(Clone)]
        struct CapturedLog(Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for CapturedLog {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().write(bytes)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        fn read_request(stream: &mut std::net::TcpStream) -> Option<(String, String)> {
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .ok()?;
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            loop {
                match stream.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            || error.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        break;
                    }
                    Err(_) => return None,
                }
                if buf.windows(4).any(|window| window == b"\r\n\r\n") || buf.len() > 65_536 {
                    break;
                }
            }
            let header_end = buf.windows(4).position(|window| window == b"\r\n\r\n")?;
            let header = String::from_utf8_lossy(&buf[..header_end]);
            let mut parts = header.lines().next()?.split_whitespace();
            Some((parts.next()?.to_string(), parts.next()?.to_string()))
        }

        let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        struct Stop(Arc<AtomicBool>);
        impl Drop for Stop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let stop_guard = Stop(Arc::clone(&stop));
        let server_lines = Arc::clone(&lines);
        let server_stop = Arc::clone(&stop);
        let server = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            while !server_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let Some((method, path)) = read_request(&mut stream) else {
                            continue;
                        };
                        server_lines
                            .lock()
                            .unwrap()
                            .push(format!("{method} {path}"));
                        let response = "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                        let _ = stream.shutdown(std::net::Shutdown::Both);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        let id = "custom:0123456789abcdef".to_string();
        let mut rules = HashMap::new();
        rules.insert(id, everyone_ban_rule());
        let cfg = CachedGuildConfig {
            version: 1,
            durable_version: 1,
            loaded_at: Instant::now(),
            settings: GuildSettings::default(),
            rules,
            commands: HashMap::new(),
            templates: HashMap::new(),
            log_channels: HashMap::new(),
            known_bad_hashes: Default::default(),
            known_spam: Default::default(),
            log_filters: HashMap::new(),
            regexes: Vec::new(),
            custom_patterns: HashMap::new(),
        };
        let roles = [];
        let message = MsgCtx {
            guild_id: 10,
            message_id: 42,
            channel_id: 70,
            author_id: 99,
            content: "@everyone",
            attachment_count: 0,
            sticker_count: 0,
            everyone_mentions: 1,
            here_mentions: 0,
            role_mentions: 0,
            joined_secs_ago: None,
            account_age_days: Some(30),
            member_roles: &roles,
            is_moderator: scope.is_moderator,
            can_mention_everyone: scope.mention == crate::commands::MentionPermission::Allowed,
            mention_permission_unknown: scope.mention
                == crate::commands::MentionPermission::Unknown,
            received: None,
        };
        let pending = AutomodEngine::new()
            .evaluate(&cfg, &message)
            .expect("unknown permission still matches");
        assert!(pending.mention_permission_unknown);
        assert_eq!(pending.action.action, "ban");

        let path = std::env::temp_dir().join(format!(
            "mention_missing_{:032x}.db",
            rand::random::<u128>()
        ));
        let app =
            BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap();
        assert!(
            crate::db::claim_active_bot(
                &app.db,
                app.activation.instance(),
                jmodguard::control::now_ms(),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap()
        );
        app.activation.mark_lock_held(true);
        crate::db::upsert_rule(&app.db, 10, "custom:0123456789abcdef", &everyone_ban_rule())
            .await
            .unwrap();
        app.poll_config_version().await.unwrap();
        app.configs.invalidate();
        let http = std::sync::Arc::new(
            serenity::http::HttpBuilder::new("token")
                .proxy(format!("http://{addr}"))
                .ratelimiter_disabled(true)
                .build(),
        );
        let cache = serenity::cache::Cache::new();
        let mut user = User::default();
        user.id = UserId::new(99);
        user.name = "member".into();
        let log = CapturedLog(Arc::new(std::sync::Mutex::new(Vec::new())));
        let writer = log.clone();
        let subscriber = tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish(),
        );
        execute_pending_with(
            &app,
            &http,
            &cache,
            GuildId::new(10),
            &user,
            None,
            "member",
            Some(42),
            Some(70),
            &pending,
        )
        .with_subscriber(subscriber)
        .await;
        drop(stop_guard);
        let _ = server.join();
        let seen = lines.lock().unwrap().clone();
        let output = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
        drop(app);
        let _ = std::fs::remove_file(&path);
        crate::tenant::remove_sidecars(&path);
        (seen, output)
    }

    fn assert_deleted_without_kick_or_ban(seen: &[String], output: &str) {
        assert!(
            seen.iter()
                .any(|line| line.starts_with("DELETE ") && line.contains("/messages/42")),
            "the triggering message must be deleted: {seen:?}"
        );
        assert!(
            seen.iter().all(|line| !line.contains("/bans/")),
            "unknown mention permission must not ban: {seen:?}"
        );
        assert!(
            seen.iter()
                .all(|line| !(line.starts_with("DELETE ") && line.contains("/members/"))),
            "unknown mention permission must not kick: {seen:?}"
        );
        assert!(
            output.contains("WARN")
                && output.contains(jmodguard::custom_rules::UNKNOWN_MENTION_PERMISSION_WARNING),
            "{output}"
        );
    }

    #[tokio::test]
    async fn missing_guild_deletes_everyone_attempt_without_kick_or_ban() {
        use serenity::model::id::{ChannelId, UserId};

        let partial = gateway_partial(10);
        let scope = crate::commands::author_message_scope(
            0,
            None,
            ChannelId::new(70),
            UserId::new(99),
            Some(&partial),
        );
        assert_eq!(scope.mention, crate::commands::MentionPermission::Unknown);
        assert!(
            !scope.is_moderator,
            "a missing guild is not a known moderator"
        );
        let (seen, output) = execute_scoped_everyone_ban(scope).await;
        assert_deleted_without_kick_or_ban(&seen, &output);
    }

    #[tokio::test]
    async fn missing_member_deletes_everyone_attempt_without_kick_or_ban() {
        use serenity::model::guild::Guild;
        use serenity::model::id::{ChannelId, GuildId, UserId};

        let mut guild = Guild::default();
        guild.id = GuildId::new(10);
        guild.owner_id = UserId::new(1);
        guild.name = "g".into();
        let scope = crate::commands::author_message_scope(
            0,
            Some(&guild),
            ChannelId::new(70),
            UserId::new(99),
            None,
        );
        assert_eq!(scope.mention, crate::commands::MentionPermission::Unknown);
        assert!(
            !scope.is_moderator,
            "a missing member is not a known moderator"
        );
        let (seen, output) = execute_scoped_everyone_ban(scope).await;
        assert_deleted_without_kick_or_ban(&seen, &output);
    }

    fn dry_run_word_ban() -> RuleDef {
        RuleDef {
            name: "custom".into(),
            description: String::new(),
            enabled: true,
            dry_run: true,
            params: serde_json::json!({ "kind": "word", "words": ["spamword"] }),
            actions: vec![ActionDef {
                at: 1,
                action: "ban".into(),
                duration_minutes: 0,
                delete_days: 0,
                purge_last_hour: false,
                reason: "r".into(),
            }],
            whitelist_roles: vec![],
            whitelist_channels: vec![],
            whitelist_users: vec![],
            skip_moderators: false,
            include_channels: vec![],
            log_automod: true,
            open_case: true,
        }
    }

    fn word_message<'a>(roles: &'a [u64]) -> MsgCtx<'a> {
        MsgCtx {
            guild_id: 10,
            message_id: 42,
            channel_id: 70,
            author_id: 99,
            content: "spamword",
            attachment_count: 0,
            sticker_count: 0,
            everyone_mentions: 0,
            here_mentions: 0,
            role_mentions: 0,
            joined_secs_ago: None,
            account_age_days: Some(30),
            member_roles: roles,
            is_moderator: false,
            can_mention_everyone: true,
            mention_permission_unknown: false,
            received: None,
        }
    }

    // Exercise persisted action JSON -> matcher -> executor -> actual HTTP request.
    #[tokio::test]
    async fn ban_purge_generic_and_custom_requests_preserve_legacy_and_dry_run() {
        use serenity::model::id::{GuildId, UserId};
        use serenity::model::user::User;
        use std::sync::{Arc, atomic::AtomicBool};

        for id in ["mention_spam", "custom:0123456789abcdef"] {
            for (stored_purge, days, dry, seconds) in [
                (None, 0, false, 0),
                (Some(false), 0, false, 0),
                (Some(true), 0, false, 3600),
                (None, 1, false, 86400),
                (None, 7, false, 604800),
                (Some(true), 1, false, 3600),
                (Some(true), 0, true, 3600),
            ] {
                let path = std::env::temp_dir()
                    .join(format!("ban_purge_{:032x}.db", rand::random::<u128>()));
                let app = BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap())
                    .unwrap();
                assert!(
                    crate::db::claim_active_bot(
                        &app.db,
                        app.activation.instance(),
                        jmodguard::control::now_ms(),
                        Arc::new(AtomicBool::new(false))
                    )
                    .await
                    .unwrap()
                );
                app.activation.mark_lock_held(true);
                let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
                    .await
                    .unwrap()
                    .unwrap();
                app.tenant.activate(enrollment).await.unwrap();
                let mut rule = if id == "mention_spam" {
                    let rule = crate::defaults::default_rules().remove(0);
                    assert_eq!(
                        rule.actions[0].delete_days, 1,
                        "shipped default remains one day"
                    );
                    rule
                } else {
                    dry_run_word_ban()
                };
                rule.dry_run = dry;
                rule.actions[0].delete_days = days;
                // Insert the actual legacy JSON without the additive field, rather
                // than relying only on a newly serialized false value.
                let mut json = serde_json::to_value(&rule).unwrap();
                json["actions"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("purge_last_hour");
                if let Some(value) = stored_purge {
                    json["actions"][0]["purge_last_hour"] = value.into();
                }
                let data = json.to_string();
                let rule_id = id.to_owned();
                app.db
                    .with_conn(move |conn| {
                        use crate::db::SqlExecutor;
                        conn.execute_bounded(
                            "INSERT OR REPLACE INTO rules(guild_id,rule_id,data) VALUES (10,?1,?2)",
                            crate::db::params![rule_id, data],
                            1,
                        )?;
                        Ok(())
                    })
                    .await
                    .unwrap();
                let cfg = app.configs.get(&app.db, 10).await.unwrap();
                let mut message = word_message(&[]);
                message.everyone_mentions = if id == "mention_spam" { 6 } else { 0 };
                let pending = AutomodEngine::new()
                    .evaluate(&cfg, &message)
                    .expect("real rule match");
                assert_eq!(pending.rule_id, id);
                let mut user = User::default();
                user.id = UserId::new(99);
                user.name = "member".into();
                app.recent_messages.remember(
                    10,
                    99,
                    71,
                    41,
                    chrono::Utc::now().timestamp_millis() - 1_800_000,
                );
                let stub = DiscordStub::start();
                execute_pending_with(
                    &app,
                    &stub.http,
                    &stub.cache,
                    GuildId::new(10),
                    &user,
                    None,
                    "member",
                    Some(42),
                    Some(70),
                    &pending,
                )
                .await;
                let seen = stub.finish();
                let bans: Vec<_> = seen
                    .iter()
                    .filter(|line| line.starts_with("PUT ") && line.contains("/bans/99"))
                    .collect();
                if dry {
                    assert!(
                        seen.is_empty(),
                        "dry-run must not ban, purge or fetch history: {seen:?}"
                    );
                } else {
                    assert_eq!(
                        bans.len(),
                        1,
                        "{id} purge={stored_purge:?}, days={days}: {seen:?}"
                    );
                    crate::spam_actions::tests::assert_ban_request(bans[0], 10, 99, seconds);
                }
                assert_eq!(
                    seen.iter()
                        .any(|line| line == "DELETE /api/v10/channels/71/messages/41"),
                    !dry && seconds > 0,
                    "{seen:?}"
                );
                assert!(
                    !seen
                        .iter()
                        .any(|line| line.starts_with("GET ") && line.contains("/messages")),
                    "no history scan: {seen:?}"
                );
                drop(app);
                let _ = std::fs::remove_file(&path);
                crate::tenant::remove_sidecars(&path);
            }
        }
    }

    #[test]
    fn known_spam_first_hit_and_exemptions_preserve_other_rules() {
        let rule = crate::defaults::default_rules()
            .into_iter()
            .find(|r| r.name == "Known spam")
            .unwrap();
        let mut cfg = test_cfg(
            &HashMap::from([("known_spam".into(), rule)]),
            &HashMap::new(),
        );
        cfg.known_spam = jmodguard::known_spam::CompiledList::new(&[
            jmodguard::known_spam::Entry::new("sample", "Full Stack AI developer pitch", None)
                .unwrap(),
            jmodguard::known_spam::Entry::new("url", "tinyurl.com/*", None).unwrap(),
        ]);
        let engine = AutomodEngine::new();
        for content in [
            "FULL ST@CK AI d3v3lop3r pitch",
            "fullstackaideveloperpitch",
            "link tinyurl.com/abc",
            "https://ti\u{200c}nyurl.com/a\u{200b}b",
        ] {
            for _ in 0..2 {
                let pending = engine
                    .evaluate(&cfg, &msg(1, content, 0, false, false))
                    .unwrap();
                assert_eq!(pending.rule_id, "known_spam");
                assert_eq!(pending.strikes, 1);
                assert_eq!(pending.action.ban_delete_message_seconds(), 3600);
            }
        }
        for content in [
            "a different message",
            "https://\u{200b}@tinyurl.com/a",
            "https://:\u{200b}@tinyurl.com/a",
            "ht\u{200b}tps://tinyurl.com/a",
            "https://tinyurl.com:\u{200b}443/a",
        ] {
            assert!(
                engine
                    .evaluate(&cfg, &msg(1, content, 0, false, false))
                    .is_none(),
                "must not schedule a ban: {content:?}"
            );
        }
        let message = msg(1, "tinyurl.com/a", 0, false, false);
        for skip in 0..5 {
            let rule = cfg.rules.get_mut("known_spam").unwrap();
            rule.enabled = skip != 0;
            rule.whitelist_users = if skip == 1 {
                vec![message.author_id]
            } else {
                vec![]
            };
            rule.whitelist_channels = if skip == 2 {
                vec![message.channel_id]
            } else {
                vec![]
            };
            rule.include_channels = if skip == 3 {
                vec![message.channel_id + 1]
            } else {
                vec![]
            };
            rule.skip_moderators = skip == 4;
            let mut message = msg(1, "tinyurl.com/a", 0, false, false);
            message.is_moderator = skip == 4;
            assert!(engine.evaluate(&cfg, &message).is_none(), "skip {skip}");
        }
        let rule = cfg.rules.get_mut("known_spam").unwrap();
        rule.skip_moderators = false;
        rule.whitelist_roles = vec![42];
        let roles = [42];
        let mut message = msg(1, "tinyurl.com/a", 0, false, false);
        message.member_roles = &roles;
        assert!(engine.evaluate(&cfg, &message).is_none());
        // An empty known list never turns unrelated text/repeat rules into a ban.
        cfg.known_spam = Default::default();
        cfg.rules = crate::types::RULE_IDS
            .into_iter()
            .zip(crate::defaults::default_rules())
            .map(|(id, r)| (id.into(), r))
            .collect();
        cfg.regexes = vec![jmodguard::normalize::compile_user_regex("scam").unwrap()];
        let mut message = msg(2, "scam", 0, false, false);
        message.account_age_days = Some(30);
        let action = AutomodEngine::new().evaluate(&cfg, &message).unwrap();
        assert_eq!(action.rule_id, "badword");
        assert_eq!(action.action.action, "delete");
        assert_eq!(cfg.rules["repeat_text"].params["mode"], "exact_text");
        assert!(cfg.rules["repeat_text"].dry_run);
    }

    #[tokio::test]
    async fn known_spam_saved_entries_reach_existing_hour_ban_helper_and_dry_run() {
        use serenity::model::id::{GuildId, UserId};
        use std::sync::{Arc, atomic::AtomicBool};
        for dry in [false, true] {
            for (kind, value, content) in [
                (
                    "sample",
                    "Full Stack AI developer pitch",
                    "fullst@ckaid3v3lop3rpitch",
                ),
                ("url", "tinyurl.com/*", "join https://TINYURL.COM/abc"),
            ] {
                let path = std::env::temp_dir()
                    .join(format!("known_spam_{:032x}.db", rand::random::<u128>()));
                let app = BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap())
                    .unwrap();
                assert!(
                    crate::db::claim_active_bot(
                        &app.db,
                        app.activation.instance(),
                        jmodguard::control::now_ms(),
                        Arc::new(AtomicBool::new(false))
                    )
                    .await
                    .unwrap()
                );
                app.activation.mark_lock_held(true);
                let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
                    .await
                    .unwrap()
                    .unwrap();
                app.tenant.activate(enrollment.clone()).await.unwrap();
                let scope = crate::db::ScopeWire {
                    global: false,
                    guild_id: enrollment.guild_id,
                    enrollment_id: enrollment.id,
                    joined_at_ns: enrollment.joined_at_ns,
                };
                let added = crate::db::apply_control(
                    &app.db,
                    None,
                    crate::db::ControlOp::AddKnownSpam {
                        request: scope.clone(),
                        target: scope.clone(),
                        kind: kind.into(),
                        value: value.into(),
                        note: None,
                    },
                    None,
                )
                .await
                .unwrap();
                let crate::db::ControlReply::Saved { body } = added else {
                    panic!("known spam control write was not saved");
                };
                let id = body.as_i64().unwrap();
                let mut rule = crate::defaults::default_rules().pop().unwrap();
                assert_eq!(rule.name, "Known spam");
                rule.dry_run = dry;
                crate::db::upsert_rule(&app.db, 10, "known_spam", &rule)
                    .await
                    .unwrap();
                let cfg = app.configs.get(&app.db, 10).await.unwrap();
                let mut message = word_message(&[]);
                message.content = content;
                let pending = AutomodEngine::new().evaluate(&cfg, &message).unwrap();
                let mut user = serenity::model::user::User::default();
                user.id = UserId::new(99);
                user.name = "member".into();
                let stub = DiscordStub::start();
                execute_pending_with(
                    &app,
                    &stub.http,
                    &stub.cache,
                    GuildId::new(10),
                    &user,
                    None,
                    "member",
                    Some(42),
                    Some(70),
                    &pending,
                )
                .await;
                let seen = stub.finish();
                if dry {
                    assert!(seen.is_empty(), "{seen:?}");
                } else {
                    let bans: Vec<_> = seen
                        .iter()
                        .filter(|s| s.starts_with("PUT ") && s.contains("/bans/99"))
                        .collect();
                    assert_eq!(bans.len(), 1, "{seen:?}");
                    crate::spam_actions::tests::assert_ban_request(bans[0], 10, 99, 3600);
                    assert!(
                        !seen
                            .iter()
                            .any(|s| s.starts_with("GET ") && s.contains("/messages")),
                        "{seen:?}"
                    );
                }
                // Removal after evaluation must withhold the saved pending ban.
                crate::db::apply_control(
                    &app.db,
                    None,
                    crate::db::ControlOp::DeleteKnownSpam {
                        request: scope.clone(),
                        target: scope,
                        id,
                    },
                    None,
                )
                .await
                .unwrap();
                app.invalidate_configs();
                let stub = DiscordStub::start();
                execute_pending_with(
                    &app,
                    &stub.http,
                    &stub.cache,
                    GuildId::new(10),
                    &user,
                    None,
                    "member",
                    Some(42),
                    Some(70),
                    &pending,
                )
                .await;
                assert!(stub.finish().is_empty());
                drop(app);
                let _ = std::fs::remove_file(&path);
                crate::tenant::remove_sidecars(&path);
            }
        }
    }

    struct DiscordRequestGate {
        method: &'static str,
        path: &'static str,
        entered: tokio::sync::Notify,
        release: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    }

    impl DiscordRequestGate {
        fn wait_if_selected(&self, method: &str, path: &str) {
            if method == self.method && path == self.path {
                let release = self.release.lock().unwrap().take();
                if let Some(release) = release {
                    self.entered.notify_one();
                    let _ = release.blocking_recv();
                }
            }
        }
    }

    #[derive(Default)]
    struct ReplyRequests {
        gate: Option<DiscordRequestGate>,
        member: Option<serenity::model::guild::Member>,
        posts: std::sync::Mutex<Vec<serde_json::Value>>,
        logs: std::sync::Mutex<Vec<serde_json::Value>>,
        sent_at: std::sync::OnceLock<Instant>,
        deleted_at: std::sync::OnceLock<Instant>,
        deleted: tokio::sync::Notify,
    }

    enum PresetEdit {
        Disable,
        RemoveChannel,
        DryRun,
        SkipRole,
        LogOnly,
        ReplyText,
    }

    #[derive(Clone, Copy)]
    enum DiscordWait {
        BeforeDispatch,
        Member,
        Delete,
        DeleteAfterLog,
        BanLookup,
    }

    #[tokio::test]
    async fn pending_media_preset_stops_after_disable_during_member_lookup() {
        edit_preset_during_discord_wait(PresetEdit::Disable, DiscordWait::Member).await;
    }

    #[tokio::test]
    async fn pending_media_preset_stops_after_channel_removal_during_member_lookup() {
        edit_preset_during_discord_wait(PresetEdit::RemoveChannel, DiscordWait::Member).await;
    }

    #[tokio::test]
    async fn pending_media_preset_rechecks_mode_and_exemptions_after_member_lookup() {
        for edit in [PresetEdit::DryRun, PresetEdit::SkipRole] {
            edit_preset_during_discord_wait(edit, DiscordWait::Member).await;
        }
    }

    #[tokio::test]
    async fn pending_media_preset_reports_completed_delete_after_disable_without_reply() {
        edit_preset_during_discord_wait(PresetEdit::Disable, DiscordWait::Delete).await;
    }

    #[tokio::test]
    async fn pending_media_preset_logs_once_when_disabled_during_delete_after_log() {
        edit_preset_during_discord_wait(PresetEdit::Disable, DiscordWait::DeleteAfterLog).await;
    }

    #[tokio::test]
    async fn pending_media_preset_stops_after_disable_during_ban_lookup() {
        edit_preset_during_discord_wait(PresetEdit::Disable, DiscordWait::BanLookup).await;
    }

    #[tokio::test]
    async fn pending_media_preset_discards_changed_action_plan_before_dispatch() {
        for edit in [PresetEdit::LogOnly, PresetEdit::ReplyText] {
            edit_preset_during_discord_wait(edit, DiscordWait::BeforeDispatch).await;
        }
    }

    async fn edit_preset_during_discord_wait(edit: PresetEdit, wait: DiscordWait) {
        use serenity::all::{Guild, GuildCreateEvent, GuildId, Member, Role, RoleId, User, UserId};
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;

        let path = std::env::temp_dir().join(format!(
            "preset_dispatch_{:032x}.db",
            rand::random::<u128>()
        ));
        let app =
            BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap();
        assert!(
            crate::db::claim_active_bot(
                &app.db,
                app.activation.instance(),
                jmodguard::control::now_ms(),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap()
        );
        app.activation.mark_lock_held(true);
        let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();
        let wire = crate::db::ScopeWire {
            global: false,
            guild_id: enrollment.guild_id,
            enrollment_id: enrollment.id.clone(),
            joined_at_ns: enrollment.joined_at_ns,
        };
        let (store, scope) = app.tenant.live(enrollment).await.unwrap();
        store
            .replace_server_names(
                scope,
                crate::db::guild_data::ServerNames {
                    channels: [70, 71]
                        .into_iter()
                        .map(|id| crate::db::guild_data::ServerChannelName {
                            id: id.to_string(),
                            name: format!("channel-{id}"),
                            kind: "text".into(),
                            parent_id: None,
                            position: 0,
                        })
                        .collect(),
                    ..Default::default()
                },
                1,
            )
            .await
            .unwrap();
        crate::db::upsert_log_channel(&app.db, 10, "automod", 90)
            .await
            .unwrap();
        let mut rule = media_rule();
        rule.include_channels.push(71);
        rule.log_automod = false;
        rule.actions[0].reason = "Links or media are blocked here".into();
        if matches!(wait, DiscordWait::BanLookup) {
            rule.actions[0].action = "ban".into();
        }
        for (action, reason) in [("log", ""), ("channel_reply", "Explain the channel rule")] {
            let mut extra = rule.actions[0].clone();
            extra.action = action.into();
            extra.reason = reason.into();
            rule.actions.push(extra);
        }
        if matches!(wait, DiscordWait::DeleteAfterLog) {
            rule.actions.swap(0, 1);
        }
        rule.params["channel_reply"] =
            serde_json::json!({ "template": "{user} No links", "delete_after_seconds": 0 });
        let save = |rule: RuleDef| {
            crate::db::apply_control(
                &app.db,
                Some(&app.tenant),
                crate::db::ControlOp::UpsertRule {
                    request: wire.clone(),
                    target: wire.clone(),
                    id: MEDIA_RULE_ID.into(),
                    rule,
                    clear_guild_override: false,
                    replace_velocity_limit: false,
                },
                None,
            )
        };
        assert!(matches!(
            save(rule.clone()).await.unwrap(),
            crate::db::ControlReply::Saved { .. }
        ));
        rule.dry_run = false;
        assert!(matches!(
            save(rule.clone()).await.unwrap(),
            crate::db::ControlReply::Saved { .. }
        ));

        let mut user = User::default();
        user.id = UserId::new(99);
        user.name = "member".into();
        let mut member = Member::default();
        member.guild_id = GuildId::new(10);
        member.user = user.clone();
        member.roles = vec![RoleId::new(77)];
        let (release, release_rx) = tokio::sync::oneshot::channel();
        let gate = match wait {
            DiscordWait::BeforeDispatch => None,
            DiscordWait::Member => Some(("GET", "/api/v10/guilds/10/members/99")),
            DiscordWait::Delete | DiscordWait::DeleteAfterLog => {
                Some(("DELETE", "/api/v10/channels/70/messages/42"))
            }
            DiscordWait::BanLookup => Some(("GET", "/api/v10/guilds/10/bans/99")),
        };
        let requests = Arc::new(ReplyRequests {
            member: Some(member),
            gate: gate.map(|(method, path)| DiscordRequestGate {
                method,
                path,
                entered: tokio::sync::Notify::new(),
                release: std::sync::Mutex::new(Some(release_rx)),
            }),
            ..Default::default()
        });
        let stub = DiscordStub::bind(Some(Arc::clone(&requests)));
        let mut guild = Guild::default();
        guild.id = GuildId::new(10);
        guild.owner_id = UserId::new(1);
        for id in [10, 77] {
            let mut role = Role::default();
            role.id = RoleId::new(id);
            guild.roles.insert(role.id, role);
        }
        let mut event: GuildCreateEvent =
            serde_json::from_value(serde_json::to_value(guild).unwrap()).unwrap();
        stub.cache.update(&mut event);
        let cfg = app.configs.get(&app.db, 10).await.unwrap();
        let mut message = msg(70, "https://other.test", 0, false, false);
        message.guild_id = 10;
        message.author_id = 99;
        message.message_id = 42;
        let pending = app.automod.evaluate(&cfg, &message).unwrap();
        assert_eq!(pending.rule_id, MEDIA_RULE_ID);
        let execution = execute_pending_with(
            &app,
            &stub.http,
            &stub.cache,
            GuildId::new(10),
            &user,
            None,
            "member",
            Some(42),
            Some(70),
            &pending,
        );
        let edit_and_save = async {
            if let Some(gate) = &requests.gate {
                tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
                    .await
                    .unwrap();
            }
            match edit {
                PresetEdit::Disable => rule.enabled = false,
                PresetEdit::RemoveChannel => {
                    rule.include_channels.remove(0);
                }
                PresetEdit::DryRun => rule.dry_run = true,
                PresetEdit::SkipRole => rule.whitelist_roles.push(77),
                PresetEdit::LogOnly => rule.actions.retain(|action| action.action == "log"),
                PresetEdit::ReplyText => {
                    rule.params["channel_reply"]["template"] = "Updated text".into()
                }
            }
            let result = save(rule).await;
            if requests.gate.is_some() {
                release.send(()).unwrap();
            }
            assert!(matches!(
                result.unwrap(),
                crate::db::ControlReply::Saved { .. }
            ));
        };
        let started = if matches!(wait, DiscordWait::BeforeDispatch) {
            edit_and_save.await;
            execution.await
        } else {
            tokio::join!(execution, edit_and_save).0
        };
        let seen = stub.finish();
        let member = "GET /api/v10/guilds/10/members/99";
        assert_eq!(
            started,
            matches!(
                wait,
                DiscordWait::Delete | DiscordWait::DeleteAfterLog | DiscordWait::BanLookup
            ),
            "{seen:?}"
        );
        let expected: &[&str] = match wait {
            DiscordWait::BeforeDispatch => &[],
            DiscordWait::Member => &[member],
            DiscordWait::Delete => &[
                member,
                "DELETE /api/v10/channels/70/messages/42",
                "POST /api/v10/channels/90/messages",
            ],
            DiscordWait::DeleteAfterLog => &[
                member,
                "POST /api/v10/channels/90/messages",
                "DELETE /api/v10/channels/70/messages/42",
            ],
            DiscordWait::BanLookup => &[member, "GET /api/v10/guilds/10/bans/99"],
        };
        assert_eq!(seen, expected);
        {
            let logs = requests.logs.lock().unwrap();
            if matches!(wait, DiscordWait::Delete | DiscordWait::DeleteAfterLog) {
                assert_eq!(logs.len(), 1);
                let fields = logs[0]["embeds"][0]["fields"].as_array().unwrap();
                let field = |name| {
                    fields
                        .iter()
                        .find(|field| field["name"].as_str().unwrap().ends_with(name))
                        .unwrap()["value"]
                        .as_str()
                        .unwrap()
                };
                if matches!(wait, DiscordWait::Delete) {
                    assert_eq!(field("Action"), "delete (succeeded)");
                    assert_eq!(field("Reason"), "delete: Links or media are blocked here");
                } else {
                    assert_eq!(field("Action"), "log");
                }
            } else {
                assert!(logs.is_empty());
            }
        }
        drop(app);
        let _ = std::fs::remove_file(&path);
        crate::tenant::remove_sidecars(&path);
    }

    #[tokio::test]
    async fn media_rule_dispatches_delete_log_reply_in_order_and_expires_only_the_reply() {
        use serenity::all::{Guild, GuildCreateEvent, GuildId, Member, Role, RoleId, User, UserId};
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;

        let path =
            std::env::temp_dir().join(format!("media_rule_{:032x}.db", rand::random::<u128>()));
        let app =
            BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap();
        assert!(
            crate::db::claim_active_bot(
                &app.db,
                app.activation.instance(),
                jmodguard::control::now_ms(),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap()
        );
        app.activation.mark_lock_held(true);
        let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment).await.unwrap();
        crate::db::upsert_log_channel(&app.db, 10, "automod", 90)
            .await
            .unwrap();
        let mut rule = media_rule();
        rule.log_automod = false;
        rule.actions[0].reason = "Links or media are blocked here".into();
        for (action, reason) in [("log", ""), ("channel_reply", "Explain the channel rule")] {
            let mut extra = rule.actions[0].clone();
            extra.action = action.into();
            extra.reason = reason.into();
            rule.actions.push(extra);
        }
        rule.params["channel_reply"] = serde_json::json!({
            "template": "{user} Use a different channel than {channel}. <:wave:12> @everyone @here <@55> <@&56>",
            "delete_after_seconds": 1,
        });
        crate::db::upsert_rule(&app.db, 10, MEDIA_RULE_ID, &rule)
            .await
            .unwrap();
        rule.dry_run = false;
        crate::db::upsert_rule(&app.db, 10, MEDIA_RULE_ID, &rule)
            .await
            .unwrap();
        app.configs.invalidate();
        let cfg = app.configs.get(&app.db, 10).await.unwrap();
        let replies = Arc::new(ReplyRequests::default());
        let stub = DiscordStub::bind(Some(Arc::clone(&replies)));
        let mut guild = Guild::default();
        guild.id = GuildId::new(10);
        guild.owner_id = UserId::new(1);
        let mut everyone = Role::default();
        everyone.id = RoleId::new(10);
        guild.roles.insert(everyone.id, everyone);
        for id in [42, 43] {
            let mut member = Member::default();
            member.guild_id = guild.id;
            member.user.id = UserId::new(id);
            guild.members.insert(member.user.id, member);
        }
        let mut event: GuildCreateEvent =
            serde_json::from_value(serde_json::to_value(guild).unwrap()).unwrap();
        stub.cache.update(&mut event);
        let mut user = User::default();
        user.id = UserId::new(42);
        user.name = "member".into();

        let evaluate = |message: &MsgCtx<'_>| {
            let pending = app.automod.evaluate(&cfg, message);
            assert!(
                pending
                    .as_ref()
                    .is_none_or(|hit| hit.rule_id == MEDIA_RULE_ID)
            );
            pending
        };
        let mut message = msg(71, "https://other.test", 0, false, false);
        message.guild_id = 10;
        assert!(evaluate(&message).is_none());
        message.channel_id = 70;
        for (id, content, attachments) in [(11, "https://other.test", 0), (12, "", 1)] {
            message.message_id = id;
            message.content = content;
            message.attachment_count = attachments;
            let pending = evaluate(&message).unwrap();
            assert!(
                execute_pending_with(
                    &app,
                    &stub.http,
                    &stub.cache,
                    GuildId::new(10),
                    &user,
                    None,
                    "member",
                    Some(id),
                    Some(70),
                    &pending,
                )
                .await
            );
        }
        {
            let posts = replies.posts.lock().unwrap();
            assert_eq!(posts.len(), 1);
            assert_eq!(
                posts[0]["content"],
                "<@42> Use a different channel than <#70>. <:wave:12> @everyone @here <@55> <@&56>",
            );
            assert_eq!(
                posts[0]["allowed_mentions"],
                serde_json::json!({ "parse": [], "users": ["42"], "roles": [], "replied_user": false }),
            );
        }

        // An expiry of zero posts for another member and schedules no delete.
        rule.params["channel_reply"]["delete_after_seconds"] = serde_json::json!(0);
        rule.actions.swap(1, 2);
        crate::db::upsert_rule(&app.db, 10, MEDIA_RULE_ID, &rule)
            .await
            .unwrap();
        app.configs.invalidate();
        let cfg = app.configs.get(&app.db, 10).await.unwrap();
        user.id = UserId::new(43);
        message.author_id = 43;
        message.message_id = 13;
        message.attachment_count = 0;
        message.sticker_count = 1;
        let pending = app.automod.evaluate(&cfg, &message).unwrap();
        assert!(
            execute_pending_with(
                &app,
                &stub.http,
                &stub.cache,
                GuildId::new(10),
                &user,
                None,
                "member",
                Some(13),
                Some(70),
                &pending,
            )
            .await
        );
        message.message_id = 14;
        let pending = app.automod.evaluate(&cfg, &message).unwrap();
        assert!(
            execute_pending_with(
                &app,
                &stub.http,
                &stub.cache,
                GuildId::new(10),
                &user,
                None,
                "member",
                Some(14),
                Some(70),
                &pending,
            )
            .await
        );

        // A pending match must stop if its channel selection is cleared before dispatch.
        rule.include_channels.clear();
        rule.enabled = false;
        crate::db::upsert_rule(&app.db, 10, MEDIA_RULE_ID, &rule)
            .await
            .unwrap();
        app.configs.invalidate();
        assert!(
            !execute_pending_with(
                &app,
                &stub.http,
                &stub.cache,
                GuildId::new(10),
                &user,
                None,
                "member",
                Some(15),
                Some(70),
                &pending,
            )
            .await
        );

        tokio::time::timeout(Duration::from_secs(5), replies.deleted.notified())
            .await
            .unwrap();
        assert!(
            replies
                .deleted_at
                .get()
                .unwrap()
                .duration_since(*replies.sent_at.get().unwrap())
                >= Duration::from_secs(1),
            "the reply must survive for at least its configured lifetime",
        );
        let lines = stub.finish();
        let ordered: Vec<_> = lines
            .iter()
            .filter(|line| !line.ends_with("/messages/81"))
            .map(String::as_str)
            .collect();
        assert_eq!(
            ordered,
            [
                "DELETE /api/v10/channels/70/messages/11",
                "POST /api/v10/channels/90/messages",
                "POST /api/v10/channels/70/messages",
                "DELETE /api/v10/channels/70/messages/12",
                "POST /api/v10/channels/90/messages",
                "DELETE /api/v10/channels/70/messages/13",
                "POST /api/v10/channels/70/messages",
                "POST /api/v10/channels/90/messages",
                "DELETE /api/v10/channels/70/messages/14",
                "POST /api/v10/channels/90/messages",
            ]
        );
        let logs = replies.logs.lock().unwrap();
        assert_eq!(logs.len(), 4);
        let field = |index: usize, name: &str| {
            logs[index]["embeds"][0]["fields"]
                .as_array()
                .unwrap()
                .iter()
                .find(|field| field["name"].as_str().unwrap().ends_with(name))
                .unwrap()["value"]
                .as_str()
                .unwrap()
        };
        assert_eq!(field(0, "Action"), "delete (succeeded)");
        assert_eq!(
            field(0, "Reason"),
            "delete: Links or media are blocked here"
        );
        assert_eq!(field(1, "Action"), "delete (succeeded)");
        assert_eq!(
            field(2, "Action"),
            "delete (succeeded), channel_reply (succeeded)"
        );
        assert!(field(2, "Reason").contains("channel_reply: Explain the channel rule"));
        assert_eq!(
            field(3, "Action"),
            "delete (failed), channel_reply (skipped)"
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.ends_with("/messages/81"))
                .count(),
            1
        );
        assert_eq!(replies.posts.lock().unwrap().len(), 2);
        drop(app);
        let _ = std::fs::remove_file(&path);
        crate::tenant::remove_sidecars(&path);
    }

    struct DiscordStub {
        http: std::sync::Arc<serenity::http::Http>,
        cache: serenity::cache::Cache,
        lines: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        ban_reasons: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        server: Option<std::thread::JoinHandle<()>>,
    }

    impl DiscordStub {
        fn start() -> Self {
            Self::bind(None)
        }

        fn finish(mut self) -> Vec<String> {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(server) = self.server.take() {
                let _ = server.join();
            }
            self.lines.lock().unwrap().clone()
        }
    }

    impl DiscordStub {
        fn bind(replies: Option<std::sync::Arc<ReplyRequests>>) -> Self {
            use crate::spam_actions::tests::read_http_request;
            use std::io::Write;
            use std::sync::Arc;
            use std::sync::atomic::{AtomicBool, Ordering};

            let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
            let ban_reasons = Arc::new(std::sync::Mutex::new(Vec::new()));
            let server_reasons = Arc::clone(&ban_reasons);
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let server_lines = Arc::clone(&lines);
            let server_stop = Arc::clone(&stop);
            let server = std::thread::spawn(move || {
                listener.set_nonblocking(true).unwrap();
                while !server_stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let _ = stream.set_nonblocking(false);
                            let Some(request) = read_http_request(&mut stream) else {
                                continue;
                            };
                            server_lines.lock().unwrap().push(request.summary());
                            if let Some(gate) =
                                replies.as_ref().and_then(|fixture| fixture.gate.as_ref())
                            {
                                gate.wait_if_selected(&request.method, &request.path);
                            }
                            if request.method == "PUT" && request.path.contains("/bans/") {
                                server_reasons.lock().unwrap().push(request.reason);
                            }
                            if let Some(replies) = &replies
                                && request.method == "DELETE"
                                && request.path == "/api/v10/channels/70/messages/81"
                            {
                                let _ = replies.deleted_at.set(Instant::now());
                                replies.deleted.notify_one();
                            }
                            if let Some(replies) = &replies
                                && request.method == "POST"
                                && request.path == "/api/v10/channels/90/messages"
                            {
                                replies
                                    .logs
                                    .lock()
                                    .unwrap()
                                    .push(serde_json::from_str(&request.body).unwrap());
                            }
                            let response = if request.method == "GET"
                                && request.path == "/api/v10/guilds/10/members/99"
                                && let Some(member) =
                                    replies.as_ref().and_then(|fixture| fixture.member.as_ref())
                            {
                                let body = serde_json::to_string(member).unwrap();
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                    body.len()
                                )
                            } else if let Some(replies) = &replies
                                && request.method == "POST"
                                && request.path == "/api/v10/channels/70/messages"
                            {
                                let _ = replies.sent_at.set(Instant::now());
                                let mut posts = replies.posts.lock().unwrap();
                                posts.push(serde_json::from_str(&request.body).unwrap());
                                let id = 80 + posts.len();
                                let body = serde_json::json!({
                                    "id": id.to_string(), "channel_id": "70",
                                    "author": { "id": "7", "username": "bot", "discriminator": "0" },
                                    "content": "", "timestamp": "2020-01-01T00:00:00Z",
                                    "tts": false, "mention_everyone": false, "mentions": [],
                                    "mention_roles": [], "attachments": [], "embeds": [],
                                    "pinned": false, "type": 0,
                                }).to_string();
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                    body.len()
                                )
                            } else if replies.is_some()
                                && request.method == "DELETE"
                                && request.path == "/api/v10/channels/70/messages/14"
                            {
                                let body = r#"{"message":"Missing Permissions","code":50013}"#;
                                format!(
                                    "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                    body.len()
                                )
                            } else if request.method == "GET" {
                                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
                            } else {
                                "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
                            };
                            let _ = stream.write_all(response.as_bytes());
                            let _ = stream.flush();
                            let _ = stream.shutdown(std::net::Shutdown::Both);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            let http = std::sync::Arc::new(
                serenity::http::HttpBuilder::new("token")
                    .proxy(format!("http://{addr}"))
                    .ratelimiter_disabled(true)
                    .build(),
            );
            let cache = serenity::cache::Cache::new();
            Self {
                http,
                cache,
                lines,
                ban_reasons,
                stop,
                server: Some(server),
            }
        }
    }

    // Omitted gateway membership still uses cached author roles at execution.
    async fn cached_author_case(action: &str, moderator: bool, skipped_role: bool) {
        use serenity::all::{
            ChannelId, Guild, GuildCreateEvent, GuildId, Member, Permissions, Role, RoleId, User,
            UserId,
        };
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        let path =
            std::env::temp_dir().join(format!("cached_author_{:032x}.db", rand::random::<u128>()));
        let app =
            BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap();
        assert!(
            crate::db::claim_active_bot(
                &app.db,
                app.activation.instance(),
                jmodguard::control::now_ms(),
                Arc::new(AtomicBool::new(false))
            )
            .await
            .unwrap()
        );
        app.activation.mark_lock_held(true);
        let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();
        let mut rule = dry_run_word_ban();
        rule.dry_run = false;
        rule.skip_moderators = true;
        rule.whitelist_roles = if skipped_role { vec![55] } else { vec![56] };
        rule.actions[0].action = action.into();
        rule.actions[0].duration_minutes = 5;
        crate::db::upsert_rule(&app.db, 10, "custom:0123456789abcdef", &rule)
            .await
            .unwrap();
        app.poll_config_version().await.unwrap();
        app.configs.invalidate();
        let cfg = app.configs.get(&app.db, 10).await.unwrap();

        let mut user = User::default();
        user.id = UserId::new(99);
        user.name = "member".into();
        let mut guild = Guild::default();
        guild.id = GuildId::new(10);
        guild.owner_id = UserId::new(1);
        let mut everyone = Role::default();
        everyone.id = RoleId::new(10);
        guild.roles.insert(everyone.id, everyone);
        let mut role = Role::default();
        role.id = RoleId::new(55);
        role.permissions = if moderator {
            Permissions::MANAGE_MESSAGES
        } else {
            Permissions::empty()
        };
        guild.roles.insert(role.id, role);
        let mut member = Member::default();
        member.guild_id = guild.id;
        member.user = user.clone();
        member.roles = vec![RoleId::new(55)];
        guild.members.insert(user.id, member);
        let scope = crate::commands::author_message_scope(
            app.cfg.bot.owner_id,
            Some(&guild),
            ChannelId::new(70),
            user.id,
            None,
        );
        assert!(!scope.is_moderator, "admission has no partial member");
        let mut message = word_message(&[]);
        message.is_moderator = scope.is_moderator;
        message.mention_permission_unknown =
            scope.mention == crate::commands::MentionPermission::Unknown;
        let pending = AutomodEngine::new()
            .evaluate(&cfg, &message)
            .expect("live custom word rule matches at admission");
        assert_eq!(pending.action.action, action);
        assert!(
            !pending.mention_permission_unknown,
            "word rules do not depend on mention permission"
        );
        let stub = DiscordStub::start();
        let mut event: GuildCreateEvent =
            serde_json::from_value(serde_json::to_value(guild).unwrap()).unwrap();
        stub.cache.update(&mut event);
        let entered_live_path = execute_pending_with(
            &app,
            &stub.http,
            &stub.cache,
            GuildId::new(10),
            &user,
            None,
            "member",
            Some(42),
            Some(70),
            &pending,
        )
        .await;
        assert!(
            entered_live_path,
            "the live path checks the cached author before deciding whether to dispatch"
        );
        let seen = stub.finish();
        if moderator || skipped_role {
            assert!(
                seen.iter().all(|line| !line.contains("/bans/")),
                "no ban lookup or dispatch for an exempt author: {seen:?}"
            );
            assert!(
                seen.is_empty(),
                "cached exempt author must receive no Discord call for {action}: {seen:?}"
            );
            assert!(
                crate::db::list_warns(&app.db, 10, 99)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(
                crate::db::list_bans(&app.db, 10, 100)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(app.gateway_ban_echo_operation(10, 99).is_none());
            let scope = app.tenant.scope(enrollment).await.unwrap();
            let store = app.tenant.store(&scope).await.unwrap();
            assert_eq!(
                store
                    .case_count(scope.clone(), Default::default())
                    .await
                    .unwrap(),
                0,
                "no moderation case"
            );
            assert!(
                store
                    .next_member_pending_ban_verification(scope, i64::MAX)
                    .await
                    .unwrap()
                    .is_none(),
                "no ban attempt"
            );
        } else {
            assert!(
                seen.iter()
                    .any(|line| line.starts_with("PUT ") && line.contains("/bans/99")),
                "cached non-moderator must be banned: {seen:?}"
            );
            assert_eq!(
                crate::db::list_bans(&app.db, 10, 100).await.unwrap().len(),
                1
            );
        }
        drop(app);
        let _ = std::fs::remove_file(&path);
        crate::tenant::remove_sidecars(&path);
    }

    #[tokio::test]
    async fn cached_moderator_without_gateway_member_skips_all_penalties() {
        for action in ["delete", "warn", "timeout", "kick", "ban"] {
            cached_author_case(action, true, false).await;
        }
    }

    #[tokio::test]
    async fn cached_skipped_role_without_gateway_member_skips_all_penalties() {
        for action in ["delete", "warn", "timeout", "kick", "ban"] {
            cached_author_case(action, false, true).await;
        }
    }

    #[tokio::test]
    async fn cached_non_moderator_without_gateway_member_still_bans() {
        cached_author_case("ban", false, false).await;
    }

    #[derive(Clone, Copy)]
    enum ConfigLookup {
        Available,
        MissingCustom,
        UnreadableBuiltin,
    }

    // Exercises the real matcher and executor with SQLite and a local Discord HTTP stub.
    async fn lookup_fallback_case(
        action: &str,
        skip_moderators: bool,
        skipped_roles: Vec<u64>,
        member_roles: Option<Vec<u64>>,
        missing_role: bool,
        expect_withheld: bool,
        config_lookup: ConfigLookup,
    ) {
        use serenity::all::{Guild, GuildCreateEvent, GuildId, Member, Role, RoleId, User, UserId};
        use std::io::Write;
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        use tracing::instrument::WithSubscriber;

        #[derive(Clone)]
        struct CapturedLog(Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for CapturedLog {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().write(bytes)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let exemption_unconfirmed =
            expect_withheld && !matches!(config_lookup, ConfigLookup::UnreadableBuiltin);
        let explicit_log = exemption_unconfirmed && action == "ban";
        let id = if matches!(config_lookup, ConfigLookup::UnreadableBuiltin) || action == "log" {
            "velocity"
        } else {
            "custom:0123456789abcdef"
        };
        let path = std::env::temp_dir().join(format!(
            "lookup_fallback_{:032x}.db",
            rand::random::<u128>()
        ));
        let app =
            BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap();
        assert!(
            crate::db::claim_active_bot(
                &app.db,
                app.activation.instance(),
                jmodguard::control::now_ms(),
                Arc::new(AtomicBool::new(false))
            )
            .await
            .unwrap()
        );
        app.activation.mark_lock_held(true);
        let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();
        let mut rule = dry_run_word_ban();
        rule.dry_run = false;
        rule.actions[0].action = action.into();
        rule.actions[0].duration_minutes = 5;
        rule.skip_moderators = skip_moderators;
        rule.whitelist_roles = skipped_roles;
        if explicit_log {
            let mut log = rule.actions[0].clone();
            log.action = "log".into();
            rule.actions.push(log);
            rule.log_automod = false;
        }
        if action == "channel_reply" {
            rule.params["channel_reply"] = serde_json::json!({ "template": "Explanation" });
        }
        if exemption_unconfirmed {
            crate::db::upsert_log_channel(&app.db, 10, "automod", 90)
                .await
                .unwrap();
        }
        if id == "velocity" {
            rule.params = serde_json::json!({ "max_messages": 0, "window_secs": 10 });
        }
        crate::db::upsert_rule(&app.db, 10, id, &rule)
            .await
            .unwrap();
        app.poll_config_version().await.unwrap();
        app.configs.invalidate();
        let cfg = app.configs.get(&app.db, 10).await.unwrap();
        let roles = member_roles.clone().unwrap_or_default();
        let pending = AutomodEngine::new()
            .evaluate(&cfg, &word_message(&roles))
            .expect("word match");
        match config_lookup {
            ConfigLookup::Available => {}
            ConfigLookup::MissingCustom => {
                crate::db::delete_rule(&app.db, 10, id).await.unwrap();
                app.configs.invalidate();
            }
            ConfigLookup::UnreadableBuiltin => {
                app.db
                    .with_conn(|conn| conn.execute_batch_bounded("DROP TABLE rules", 65_536))
                    .await
                    .unwrap();
                app.configs.invalidate();
            }
        }

        let mut user = User::default();
        user.id = UserId::new(99);
        user.name = "member".into();
        let mut guild = Guild::default();
        guild.id = GuildId::new(10);
        guild.owner_id = UserId::new(1);
        let mut everyone = Role::default();
        everyone.id = RoleId::new(10);
        guild.roles.insert(everyone.id, everyone);
        let member_roles_missing = member_roles.is_none();
        let member = member_roles.map(|roles| {
            let mut member = Member::default();
            member.guild_id = guild.id;
            member.user = user.clone();
            member.roles = roles.into_iter().map(RoleId::new).collect();
            if !missing_role {
                for id in &member.roles {
                    let mut role = Role::default();
                    role.id = *id;
                    guild.roles.insert(*id, role);
                }
            }
            member
        });
        let fetched_member =
            (skip_moderators && !expect_withheld && member_roles_missing && action == "delete")
                .then(|| {
                    let mut member = Member::default();
                    member.guild_id = guild.id;
                    member.user = user.clone();
                    member
                });
        let stub_with_payloads = exemption_unconfirmed || fetched_member.is_some();
        let requests = Arc::new(ReplyRequests {
            member: fetched_member,
            ..Default::default()
        });
        let stub = DiscordStub::bind(stub_with_payloads.then(|| Arc::clone(&requests)));
        let mut event: GuildCreateEvent =
            serde_json::from_value(serde_json::to_value(guild).unwrap()).unwrap();
        stub.cache.update(&mut event);
        let log = CapturedLog(Arc::new(std::sync::Mutex::new(Vec::new())));
        let writer = log.clone();
        let subscriber = tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish(),
        );
        execute_pending_with(
            &app,
            &stub.http,
            &stub.cache,
            GuildId::new(10),
            &user,
            member,
            "member",
            Some(42),
            Some(70),
            &pending,
        )
        .with_subscriber(subscriber)
        .await;
        let seen = stub.finish();
        if requests.member.is_some() {
            assert_eq!(
                seen.first().map(String::as_str),
                Some("GET /api/v10/guilds/10/members/99")
            );
        }
        let output = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
        let deletes = seen
            .iter()
            .filter(|line| line.starts_with("DELETE ") && line.contains("/messages/42"))
            .count();
        if matches!(config_lookup, ConfigLookup::MissingCustom) {
            assert!(
                seen.is_empty(),
                "missing custom config must not call Discord: {seen:?}"
            );
            assert!(
                crate::db::list_warns(&app.db, 10, 99)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(
                output.contains("WARN")
                    && output.contains(jmodguard::custom_rules::CUSTOM_RULE_UNAVAILABLE_WARNING),
                "{output}"
            );
        } else if exemption_unconfirmed {
            assert_eq!(
                deletes, 0,
                "an unresolved exemption prevents deletion: {seen:?}"
            );
            assert!(
                seen.iter()
                    .all(|line| line.starts_with("GET ")
                        || line == "POST /api/v10/channels/90/messages"),
                "only read-only lookups and the configured log may run: {seen:?}",
            );
            assert!(
                crate::db::list_warns(&app.db, 10, 99)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let reports = requests.logs.lock().unwrap();
            assert_eq!(reports.len(), 1);
            let fields = reports[0]["embeds"][0]["fields"].as_array().unwrap();
            let field = |name| {
                fields
                    .iter()
                    .find(|field| field["name"].as_str().unwrap().ends_with(name))
                    .unwrap()["value"]
                    .as_str()
                    .unwrap()
            };
            assert_eq!(field("Action"), format!("{action} (skipped)"));
            let reason = if missing_role {
                "moderator status could not be confirmed"
            } else {
                "member roles could not be confirmed"
            };
            assert_eq!(field("Reason"), reason);
            assert!(
                output.contains("automod enforcement skipped") && output.contains(reason),
                "{output}"
            );
        } else if expect_withheld || action == "delete" {
            assert_eq!(deletes, 1, "delete must run exactly once: {seen:?}");
        } else {
            assert_eq!(
                deletes, 0,
                "normal {action} must not gain a fallback delete: {seen:?}"
            );
        }
        if expect_withheld {
            assert!(
                seen.iter().all(|line| !(line.contains("/bans/")
                    || line.starts_with("DELETE ") && line.contains("/members/"))),
                "no ban handoff, ban, or kick: {seen:?}"
            );
            if !exemption_unconfirmed {
                assert!(
                    output.contains("WARN") && output.contains("skipping kick or ban"),
                    "{output}"
                );
            }
            assert!(app.gateway_ban_echo_operation(10, 99).is_none());
            let scope = app.tenant.scope(enrollment).await.unwrap();
            let store = app.tenant.store(&scope).await.unwrap();
            assert!(
                store
                    .next_member_pending_ban_verification(scope, i64::MAX)
                    .await
                    .unwrap()
                    .is_none(),
                "no ban operation may start"
            );
        } else if !matches!(config_lookup, ConfigLookup::MissingCustom) {
            match action {
                "ban" => assert!(
                    seen.iter()
                        .any(|line| line.starts_with("PUT ") && line.contains("/bans/99")),
                    "normal ban: {seen:?}"
                ),
                "timeout" => assert!(
                    seen.iter()
                        .any(|line| line.starts_with("PATCH ") && line.contains("/members/99")),
                    "timeout still runs: {seen:?}"
                ),
                "warn" => assert_eq!(
                    crate::db::list_warns(&app.db, 10, 99).await.unwrap().len(),
                    1
                ),
                "delete" | "log" => {}
                _ => panic!("unexpected test action"),
            }
        }
        drop(app);
        let _ = std::fs::remove_file(&path);
        crate::tenant::remove_sidecars(&path);
    }

    #[tokio::test]
    async fn unknown_membership_skips_all_enforcement_including_ban_then_log() {
        for action in ["ban", "kick", "delete", "timeout", "warn", "channel_reply"] {
            lookup_fallback_case(
                action,
                true,
                vec![],
                None,
                false,
                true,
                ConfigLookup::Available,
            )
            .await;
            lookup_fallback_case(
                action,
                false,
                vec![55],
                None,
                false,
                true,
                ConfigLookup::Available,
            )
            .await;
        }
    }

    #[tokio::test]
    async fn missing_gateway_member_can_be_recovered_before_enforcement() {
        lookup_fallback_case(
            "delete",
            true,
            vec![],
            None,
            false,
            false,
            ConfigLookup::Available,
        )
        .await;
    }

    #[tokio::test]
    async fn known_empty_roles_and_no_role_dependency_still_enforce() {
        lookup_fallback_case(
            "ban",
            true,
            vec![],
            Some(vec![]),
            false,
            false,
            ConfigLookup::Available,
        )
        .await;
        lookup_fallback_case(
            "ban",
            false,
            vec![],
            None,
            false,
            false,
            ConfigLookup::Available,
        )
        .await;
    }

    #[tokio::test]
    async fn missing_role_definition_withholds_dependent_ban() {
        lookup_fallback_case(
            "ban",
            true,
            vec![],
            Some(vec![55]),
            true,
            true,
            ConfigLookup::Available,
        )
        .await;
    }

    #[tokio::test]
    async fn unknown_role_definitions_skip_every_enforcement_action() {
        for action in ["delete", "timeout", "warn", "channel_reply"] {
            lookup_fallback_case(
                action,
                true,
                vec![],
                Some(vec![55]),
                true,
                true,
                ConfigLookup::Available,
            )
            .await;
        }
    }

    #[tokio::test]
    async fn unavailable_custom_rule_config_stops_all_actions() {
        for action in ["delete", "ban", "kick", "timeout", "warn"] {
            lookup_fallback_case(
                action,
                false,
                vec![],
                Some(vec![]),
                false,
                false,
                ConfigLookup::MissingCustom,
            )
            .await;
        }
    }

    #[tokio::test]
    async fn unreadable_builtin_config_withholds_only_kick_and_ban() {
        for action in ["delete", "ban", "kick", "timeout", "warn", "log"] {
            lookup_fallback_case(
                action,
                false,
                vec![],
                Some(vec![]),
                false,
                matches!(action, "ban" | "kick"),
                ConfigLookup::UnreadableBuiltin,
            )
            .await;
        }
    }

    #[tokio::test]
    async fn unknown_roles_do_not_add_delete_to_log_action() {
        for config in [ConfigLookup::Available, ConfigLookup::UnreadableBuiltin] {
            lookup_fallback_case("log", true, vec![], None, false, false, config).await;
        }
    }

    #[tokio::test]
    async fn missing_mention_role_definitions_delete_without_ban() {
        use serenity::all::{
            ChannelId, Guild, GuildChannel, GuildId, PartialMember, Role, RoleId, UserId,
        };
        for missing_everyone in [false, true] {
            let mut guild = Guild::default();
            guild.id = GuildId::new(10);
            guild.owner_id = UserId::new(1);
            let mut channel = GuildChannel::default();
            channel.id = ChannelId::new(70);
            channel.guild_id = guild.id;
            guild.channels.insert(channel.id, channel);
            if !missing_everyone {
                let mut role = Role::default();
                role.id = RoleId::new(10);
                guild.roles.insert(role.id, role);
            }
            let partial: PartialMember = serde_json::from_value(serde_json::json!({
                "deaf": false, "joined_at": null, "mute": false, "nick": null,
                "roles": if missing_everyone { vec![] } else { vec!["55"] },
                "pending": false, "premium_since": null, "guild_id": "10", "user": null,
                "permissions": null, "unusual_dm_activity_until": null,
                "avatar": null, "banner": null, "avatar_decoration_data": null,
            }))
            .unwrap();
            let scope = crate::commands::author_message_scope(
                0,
                Some(&guild),
                ChannelId::new(70),
                UserId::new(99),
                Some(&partial),
            );
            assert_eq!(scope.mention, crate::commands::MentionPermission::Unknown);
            let (seen, output) = execute_scoped_everyone_ban(scope).await;
            assert_deleted_without_kick_or_ban(&seen, &output);
        }
    }

    #[tokio::test]
    async fn deleted_dry_run_custom_rule_does_not_ban() {
        use std::io::Write;
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        use tracing::instrument::WithSubscriber;

        use serenity::model::id::{GuildId, UserId};
        use serenity::model::user::User;

        #[derive(Clone)]
        struct CapturedLog(Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for CapturedLog {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().write(bytes)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        const ID: &str = "custom:0123456789abcdef";
        let path =
            std::env::temp_dir().join(format!("custom_deleted_{:032x}.db", rand::random::<u128>()));
        let app =
            BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap();
        assert!(
            crate::db::claim_active_bot(
                &app.db,
                app.activation.instance(),
                jmodguard::control::now_ms(),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap()
        );
        app.activation.mark_lock_held(true);
        let enrollment = crate::db::enroll_guild(&app.db, 10, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment).await.unwrap();
        crate::db::upsert_rule(&app.db, 10, ID, &dry_run_word_ban())
            .await
            .unwrap();
        app.poll_config_version().await.unwrap();
        app.configs.invalidate();
        let cfg = app.configs.get(&app.db, 10).await.unwrap();
        let roles = [];
        let pending = AutomodEngine::new()
            .evaluate(&cfg, &word_message(&roles))
            .expect("dry-run custom rule still matches");
        assert_eq!(pending.action.action, "ban");
        crate::db::delete_rule(&app.db, 10, ID).await.unwrap();
        app.configs.invalidate();

        let log = CapturedLog(Arc::new(std::sync::Mutex::new(Vec::new())));
        let writer = log.clone();
        let subscriber = tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish(),
        );
        let mut user = User::default();
        user.id = UserId::new(99);
        user.name = "member".into();
        let stub = DiscordStub::start();
        execute_pending_with(
            &app,
            &stub.http,
            &stub.cache,
            GuildId::new(10),
            &user,
            None,
            "member",
            Some(42),
            Some(70),
            &pending,
        )
        .with_subscriber(subscriber)
        .await;
        let seen = stub.finish();
        let output = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
        assert!(
            seen.iter().all(|line| !line.contains("/bans/")),
            "a deleted dry-run custom rule must not ban: {seen:?}"
        );
        assert!(
            seen.iter()
                .all(|line| !(line.starts_with("DELETE ") && line.contains("/messages/"))),
            "a deleted custom rule must not act: {seen:?}"
        );
        assert!(
            output.contains("WARN")
                && output.contains(jmodguard::custom_rules::CUSTOM_RULE_UNAVAILABLE_WARNING),
            "{output}"
        );
        drop(app);
        let _ = std::fs::remove_file(&path);
        crate::tenant::remove_sidecars(&path);
    }

    #[tokio::test]
    async fn custom_rule_does_not_act_when_config_read_fails() {
        use std::io::Write;
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        use tracing::instrument::WithSubscriber;

        use serenity::model::id::{GuildId, UserId};
        use serenity::model::user::User;

        #[derive(Clone)]
        struct CapturedLog(Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for CapturedLog {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().write(bytes)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let path = std::env::temp_dir().join(format!(
            "custom_read_fail_{:032x}.db",
            rand::random::<u128>()
        ));
        let app =
            BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap();
        assert!(
            crate::db::claim_active_bot(
                &app.db,
                app.activation.instance(),
                jmodguard::control::now_ms(),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap()
        );
        app.activation.mark_lock_held(true);
        let mut rules = HashMap::new();
        rules.insert("custom:0123456789abcdef".to_string(), dry_run_word_ban());
        let cfg = CachedGuildConfig {
            version: 1,
            durable_version: 1,
            loaded_at: Instant::now(),
            settings: GuildSettings::default(),
            rules,
            commands: HashMap::new(),
            templates: HashMap::new(),
            log_channels: HashMap::new(),
            known_bad_hashes: Default::default(),
            known_spam: Default::default(),
            log_filters: HashMap::new(),
            regexes: Vec::new(),
            custom_patterns: HashMap::new(),
        };
        let roles = [];
        let pending = AutomodEngine::new()
            .evaluate(&cfg, &word_message(&roles))
            .expect("dry-run custom rule still matches");
        app.db
            .with_conn(|conn| conn.execute_batch_bounded("DROP TABLE rules", 65_536))
            .await
            .unwrap();
        app.configs.invalidate();
        let log = CapturedLog(Arc::new(std::sync::Mutex::new(Vec::new())));
        let writer = log.clone();
        let subscriber = tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish(),
        );
        let mut user = User::default();
        user.id = UserId::new(99);
        user.name = "member".into();
        let stub = DiscordStub::start();
        execute_pending_with(
            &app,
            &stub.http,
            &stub.cache,
            GuildId::new(10),
            &user,
            None,
            "member",
            Some(42),
            Some(70),
            &pending,
        )
        .with_subscriber(subscriber)
        .await;
        let seen = stub.finish();
        let output = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
        assert!(
            seen.iter().all(|line| !line.contains("/bans/")),
            "a failed custom-rule config read must not ban: {seen:?}"
        );
        assert!(
            seen.iter()
                .all(|line| !(line.starts_with("DELETE ") && line.contains("/messages/"))),
            "a failed custom-rule config read must not act: {seen:?}"
        );
        assert!(
            output.contains("WARN")
                && output.contains(jmodguard::custom_rules::CUSTOM_RULE_UNAVAILABLE_WARNING),
            "{output}"
        );
        drop(app);
        let _ = std::fs::remove_file(&path);
        crate::tenant::remove_sidecars(&path);
    }

    #[tokio::test]
    async fn builtin_rule_uses_matched_settings_when_config_read_fails() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        use serenity::model::id::{GuildId, UserId};
        use serenity::model::user::User;

        for dry_run in [false, true] {
            let path = std::env::temp_dir().join(format!(
                "builtin_read_fail_{:032x}.db",
                rand::random::<u128>()
            ));
            let app =
                BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap();
            assert!(
                crate::db::claim_active_bot(
                    &app.db,
                    app.activation.instance(),
                    jmodguard::control::now_ms(),
                    Arc::new(AtomicBool::new(false)),
                )
                .await
                .unwrap()
            );
            app.activation.mark_lock_held(true);
            app.db
                .with_conn(|conn| conn.execute_batch_bounded("DROP TABLE rules", 65_536))
                .await
                .unwrap();
            app.configs.invalidate();
            let badword = crate::types::RULE_IDS
                .iter()
                .position(|id| *id == "badword")
                .unwrap();
            let mut matched = crate::defaults::default_rules().remove(badword);
            matched.dry_run = dry_run;
            let pending = PendingAction {
                rule_id: "badword".into(),
                rule_name: "Badwords".into(),
                action: ActionDef {
                    at: 1,
                    action: "delete".into(),
                    duration_minutes: 0,
                    delete_days: 0,
                    purge_last_hour: false,
                    reason: "Banned word".into(),
                },
                strikes: 1,
                evidence: serde_json::json!({
                    "content": "spamword",
                    "channel_id": 70,
                    "message_id": 42,
                }),
                mention_permission_unknown: false,
                matched_rule: Some(matched),
                dry_run: false,
            };
            let mut user = User::default();
            user.id = UserId::new(99);
            user.name = "member".into();
            let stub = DiscordStub::start();
            execute_pending_with(
                &app,
                &stub.http,
                &stub.cache,
                GuildId::new(10),
                &user,
                None,
                "member",
                Some(42),
                Some(70),
                &pending,
            )
            .await;
            let seen = stub.finish();
            let deleted = seen
                .iter()
                .any(|line| line.starts_with("DELETE ") && line.contains("/messages/42"));
            assert_eq!(
                deleted, !dry_run,
                "dry_run={dry_run}: a failed config read must keep the matched rule settings: {seen:?}"
            );
            drop(app);
            let _ = std::fs::remove_file(&path);
            crate::tenant::remove_sidecars(&path);
        }
    }

    #[tokio::test]
    async fn custom_rule_ban_echo_leaves_one_active_row() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        use serenity::model::id::{GuildId, UserId};
        use serenity::model::user::User;

        const ID: &str = "custom:0123456789abcdef";
        let path = std::env::temp_dir().join(format!(
            "custom_ban_echo_{:032x}.db",
            rand::random::<u128>()
        ));
        let app =
            BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap();
        assert!(
            crate::db::claim_active_bot(
                &app.db,
                app.activation.instance(),
                jmodguard::control::now_ms(),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap()
        );
        app.activation.mark_lock_held(true);
        let enrollment = crate::db::enroll_guild(&app.db, 10_i64, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();
        let mut rule = dry_run_word_ban();
        rule.dry_run = false;
        rule.open_case = true;
        let copied_tag = "0123456789abcdef0123";
        rule.actions[0].reason = format!("[jmg:{copied_tag}] {}", "{username}".repeat(16));
        crate::db::upsert_rule(&app.db, 10, ID, &rule)
            .await
            .unwrap();
        app.poll_config_version().await.unwrap();
        app.configs.invalidate();
        let cfg = app.configs.get(&app.db, 10).await.unwrap();
        let roles = [];
        let pending = AutomodEngine::new()
            .evaluate(&cfg, &word_message(&roles))
            .expect("custom ban rule matches");
        assert_eq!(pending.action.action, "ban");
        let mut user = User::default();
        user.id = UserId::new(99);
        user.name = "r".repeat(32);
        let stub = DiscordStub::start();
        execute_pending_with(
            &app,
            &stub.http,
            &stub.cache,
            GuildId::new(10),
            &user,
            None,
            "member",
            Some(42),
            Some(70),
            &pending,
        )
        .await;
        let sent_reasons = stub.ban_reasons.clone();
        let seen = stub.finish();
        let (store, scope) = app.tenant.live(enrollment.clone()).await.unwrap();
        let ids = store
            .case_ids(scope.clone(), None, 10, crate::tenant::case_limits())
            .await
            .unwrap();
        assert_eq!(ids.len(), 1);
        let case = store
            .case(scope, ids[0].clone(), crate::tenant::case_limits())
            .await
            .unwrap()
            .unwrap();
        let tag = case
            .ban_attempt_tag
            .as_ref()
            .expect("persisted attempt identity");
        assert_ne!(tag.as_str(), copied_tag);
        assert_eq!(case.reason.as_deref(), Some("r".repeat(512).as_str()));
        {
            let sent = sent_reasons.lock().unwrap();
            assert_eq!(sent.len(), 1);
            assert_eq!(
                sent[0],
                format!("%5Bjmg%3A{}%5D%20{}", tag.as_str(), "r".repeat(485))
            );
        }
        assert!(
            seen.iter().any(|line| line.contains("/bans/")),
            "the custom-rule ban must be sent: {seen:?}"
        );

        let (row_id, operation_id, reason) = app
            .db
            .with_conn(|conn| {
                use crate::db::SqlExecutor;
                let (count, row_id, operation_id, reason): (i64, i64, String, String) = conn
                    .query_row(
                        "SELECT COUNT(*), id, operation_id, reason FROM bans
                         WHERE guild_id=?1 AND user_id=?2 AND active=1",
                        crate::db::params![10_i64, 99_i64],
                        |row| {
                            Ok((
                                row.get(0).map_err(|error| anyhow::anyhow!("{error}"))?,
                                row.get(1).map_err(|error| anyhow::anyhow!("{error}"))?,
                                row.get(2).map_err(|error| anyhow::anyhow!("{error}"))?,
                                row.get(3).map_err(|error| anyhow::anyhow!("{error}"))?,
                            ))
                        },
                    )?;
                anyhow::ensure!(count == 1, "expected one active ban before the echo");
                Ok((row_id, operation_id, reason))
            })
            .await
            .unwrap();
        assert!(
            !operation_id.is_empty(),
            "the ban row must carry operation_id"
        );
        assert!(
            reason.contains(ID),
            "the bot reason must be stored before the echo: {reason}"
        );

        assert!(
            !reason.contains("[jmg:"),
            "core mirror must hide tags: {reason}"
        );
        let suppressed =
            app.consume_gateway_case_suppression(crate::state::GatewayCaseAction::Ban, 10, 99);
        let echo_operation = app.gateway_ban_echo_operation(10, 99);
        let owned_echo = suppressed || echo_operation.is_some();
        let echoed_operation = echo_operation.flatten();
        assert!(owned_echo, "the custom-rule ban must own its gateway echo");
        assert_eq!(echoed_operation.as_deref(), Some(operation_id.as_str()));
        let write = crate::db::record_gateway_ban(
            &app.db,
            Some(enrollment),
            99,
            Some("member"),
            None,
            None,
            owned_echo,
            echoed_operation.as_deref(),
            true,
        )
        .await
        .unwrap();
        assert_eq!(write, crate::db::GatewayBanWrite::Existing { row_id });
        let (active, total) = app
            .db
            .with_conn(|conn| {
                use crate::db::SqlExecutor;
                let active: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM bans WHERE guild_id=?1 AND user_id=?2 AND active=1",
                    crate::db::params![10_i64, 99_i64],
                    |row| row.get(0).map_err(|error| anyhow::anyhow!("{error}")),
                )?;
                let total: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM bans WHERE guild_id=?1 AND user_id=?2",
                    crate::db::params![10_i64, 99_i64],
                    |row| row.get(0).map_err(|error| anyhow::anyhow!("{error}")),
                )?;
                Ok((active, total))
            })
            .await
            .unwrap();
        assert_eq!(
            (active, total),
            (1, 1),
            "the echo must not add or replace the active ban"
        );
        drop(app);
        let _ = std::fs::remove_file(&path);
        crate::tenant::remove_sidecars(&path);
    }

    #[tokio::test]
    async fn dry_run_delete_does_not_insert_automod_delete_mark() {
        use serenity::model::id::{GuildId, UserId};
        use serenity::model::user::User;
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        let path = std::env::temp_dir().join(format!(
            "dry_run_delete_mark_{:032x}.db",
            rand::random::<u128>()
        ));
        let app =
            BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap();
        assert!(
            crate::db::claim_active_bot(
                &app.db,
                app.activation.instance(),
                jmodguard::control::now_ms(),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap()
        );
        app.activation.mark_lock_held(true);
        let mut rule = dry_run_word_ban();
        rule.actions[0].action = "delete".into();
        crate::db::upsert_rule(&app.db, 10, "custom:0123456789abcdef", &rule)
            .await
            .unwrap();
        app.poll_config_version().await.unwrap();
        app.configs.invalidate();
        let cfg = app.configs.get(&app.db, 10).await.unwrap();
        let pending = AutomodEngine::new()
            .evaluate(&cfg, &word_message(&[]))
            .expect("dry-run delete rule matches");
        let mut user = User::default();
        user.id = UserId::new(99);
        let stub = DiscordStub::start();
        execute_pending_with(
            &app,
            &stub.http,
            &stub.cache,
            GuildId::new(10),
            &user,
            None,
            "member",
            Some(42),
            Some(70),
            &pending,
        )
        .await;
        let _ = stub.finish();
        assert!(!app.automod_delete_marks.inner.lock().contains(10, 70, 42));
        drop(app);
        let _ = std::fs::remove_file(&path);
        crate::tenant::remove_sidecars(&path);
    }

    #[tokio::test]
    async fn automod_delete_mark_exists_before_delete_rest_finishes() {
        use serenity::model::id::{GuildId, UserId};
        use serenity::model::user::User;
        use std::io::{Read, Write};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let path = std::env::temp_dir().join(format!(
            "delete_mark_order_{:032x}.db",
            rand::random::<u128>()
        ));
        let app =
            BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap();
        assert!(
            crate::db::claim_active_bot(
                &app.db,
                app.activation.instance(),
                jmodguard::control::now_ms(),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap()
        );
        app.activation.mark_lock_held(true);
        let mut rule = dry_run_word_ban();
        rule.dry_run = false;
        rule.actions[0].action = "delete".into();
        crate::db::upsert_rule(&app.db, 10, "custom:0123456789abcdef", &rule)
            .await
            .unwrap();
        app.poll_config_version().await.unwrap();
        app.configs.invalidate();
        let cfg = app.configs.get(&app.db, 10).await.unwrap();
        let pending = AutomodEngine::new()
            .evaluate(&cfg, &word_message(&[]))
            .expect("delete rule matches");
        // The server reports whether the mark existed when the DELETE arrived,
        // then holds the response until the test releases it.
        let (delete_seen, mut delete_seen_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release, release_rx) = std::sync::mpsc::channel::<()>();
        let app_for_check = Arc::clone(&app);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let server_app = Arc::clone(&app_for_check);
        let server = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if server_stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut stream) = stream else {
                    break;
                };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 1024];
                while let Ok(n) = stream.read(&mut tmp) {
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let header = String::from_utf8_lossy(&buf);
                let method = header
                    .lines()
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .next()
                    .unwrap_or("");
                if method == "DELETE" && header.contains("/messages/") {
                    let marked = server_app
                        .automod_delete_marks
                        .inner
                        .lock()
                        .contains(10, 70, 42);
                    let _ = delete_seen.send(marked);
                    // A send or the test dropping the sender releases the reply.
                    let _ = release_rx.recv();
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                let _ = stream.flush();
            }
        });
        let http = std::sync::Arc::new(
            serenity::http::HttpBuilder::new("token")
                .proxy(format!("http://{addr}"))
                .ratelimiter_disabled(true)
                .build(),
        );
        let cache = serenity::cache::Cache::new();
        let mut user = User::default();
        user.id = UserId::new(99);
        let worker_app = Arc::clone(&app);
        let task = tokio::spawn(async move {
            execute_pending_with(
                &worker_app,
                &http,
                &cache,
                GuildId::new(10),
                &user,
                None,
                "member",
                Some(42),
                Some(70),
                &pending,
            )
            .await;
        });
        let marked = delete_seen_rx
            .recv()
            .await
            .expect("the DELETE request reaches the server");
        assert!(
            marked,
            "the automod delete mark must exist before the DELETE request"
        );
        release.send(()).unwrap();
        task.await.unwrap();
        stop.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(addr);
        let _ = server.join();
        drop(app);
        let _ = std::fs::remove_file(&path);
        crate::tenant::remove_sidecars(&path);
    }

    #[tokio::test]
    async fn failed_automod_delete_clears_mark_and_later_delete_is_plain() {
        use crate::logging::observed_delete_report;
        use serenity::model::id::{ChannelId, GuildId, MessageId, UserId};
        use serenity::model::user::User;
        use std::io::{Read, Write};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let path = std::env::temp_dir().join(format!(
            "delete_mark_403_{:032x}.db",
            rand::random::<u128>()
        ));
        let app =
            BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap();
        assert!(
            crate::db::claim_active_bot(
                &app.db,
                app.activation.instance(),
                jmodguard::control::now_ms(),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap()
        );
        app.activation.mark_lock_held(true);
        let mut rule = dry_run_word_ban();
        rule.dry_run = false;
        rule.actions[0].action = "delete".into();
        crate::db::upsert_rule(&app.db, 10, "custom:0123456789abcdef", &rule)
            .await
            .unwrap();
        app.poll_config_version().await.unwrap();
        app.configs.invalidate();
        let cfg = app.configs.get(&app.db, 10).await.unwrap();
        let pending = AutomodEngine::new()
            .evaluate(&cfg, &word_message(&[]))
            .expect("delete rule matches");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let server = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if server_stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut stream) = stream else {
                    break;
                };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 1024];
                while let Ok(n) = stream.read(&mut tmp) {
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let response = b"HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: 25\r\nConnection: close\r\n\r\n{\"message\": \"forbidden\"}";
                let _ = stream.write_all(response);
                let _ = stream.flush();
            }
        });
        let http = std::sync::Arc::new(
            serenity::http::HttpBuilder::new("token")
                .proxy(format!("http://{addr}"))
                .ratelimiter_disabled(true)
                .build(),
        );
        let cache = serenity::cache::Cache::new();
        let mut user = User::default();
        user.id = UserId::new(99);
        execute_pending_with(
            &app,
            &http,
            &cache,
            GuildId::new(10),
            &user,
            None,
            "member",
            Some(42),
            Some(70),
            &pending,
        )
        .await;
        stop.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(addr);
        let _ = server.join();
        assert!(!app.automod_delete_marks.inner.lock().contains(10, 70, 42));
        let report = observed_delete_report(
            &app,
            GuildId::new(10),
            ChannelId::new(70),
            MessageId::new(42),
            None,
        )
        .expect("plain delete");
        assert_eq!(report.event, "message_delete");
        drop(app);
        let _ = std::fs::remove_file(&path);
        crate::tenant::remove_sidecars(&path);
    }

    #[tokio::test]
    async fn stalled_automod_delete_times_out_and_clears_its_mark() {
        use serenity::model::id::{ChannelId, GuildId, MessageId};
        use std::sync::Arc;
        use tokio::io::AsyncReadExt;

        let path = std::env::temp_dir().join(format!(
            "automod_timeout_{:032x}.db",
            rand::random::<u128>()
        ));
        let app =
            BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap();
        app.activation.mark_lock_held(true);
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let (entered, received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut bytes = [0; 1024];
                let n = stream.read(&mut bytes).await.unwrap();
                assert_ne!(n, 0);
                request.extend_from_slice(&bytes[..n]);
                if request.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
            }
            assert!(String::from_utf8_lossy(&request).starts_with("DELETE "));
            entered.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        let http = serenity::http::HttpBuilder::new("fixture")
            .proxy(format!("http://{address}"))
            .ratelimiter_disabled(true)
            .build();
        let task_app = Arc::clone(&app);
        let task = tokio::spawn(async move {
            super::automod_try_delete_message(
                &task_app,
                &http,
                10,
                70,
                42,
                "badword",
                "Bad words",
                "blocked term",
            )
            .await;
        });
        received.await.unwrap();
        assert!(app.automod_delete_marks.inner.lock().contains(10, 70, 42));
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(9)).await;
        assert!(!task.is_finished(), "delete timed out before ten seconds");
        tokio::time::advance(std::time::Duration::from_millis(1001)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        let finished = task.is_finished();
        if !finished {
            task.abort();
        }
        let _ = task.await;
        server.abort();
        let _ = server.await;
        assert!(
            finished,
            "stalled REST delete exceeded its ten-second deadline"
        );
        assert!(!app.automod_delete_marks.inner.lock().contains(10, 70, 42));
        let report = crate::logging::observed_delete_report(
            &app,
            GuildId::new(10),
            ChannelId::new(70),
            MessageId::new(42),
            None,
        )
        .expect("a later external deletion stays plain");
        assert_eq!(report.event, "message_delete");
        drop(app);
        let _ = std::fs::remove_file(&path);
        crate::tenant::remove_sidecars(&path);
    }

    #[tokio::test]
    async fn cancelled_automod_delete_replays_plain_message_delete() {
        use super::{
            AUTOMOD_DELETE_CANCEL_PROBE, AutomodDeleteCancelProbe, automod_try_delete_message,
        };
        use std::io::{Read, Write};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let path =
            std::env::temp_dir().join(format!("cancel_replay_{:032x}.db", rand::random::<u128>()));
        let app =
            BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap();
        assert!(
            crate::db::claim_active_bot(
                &app.db,
                app.activation.instance(),
                jmodguard::control::now_ms(),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap()
        );
        app.activation.mark_lock_held(true);
        crate::db::upsert_log_channel(&app.db, 10, "message_delete", 900)
            .await
            .unwrap();
        app.poll_config_version().await.unwrap();
        app.invalidate_configs();

        let bodies = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let server_bodies = Arc::clone(&bodies);
        let server = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if server_stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut stream) = stream else {
                    break;
                };
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                let mut buf = Vec::new();
                let mut tmp = [0u8; 2048];
                loop {
                    match stream.read(&mut tmp) {
                        Ok(0) => break,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                || error.kind() == std::io::ErrorKind::TimedOut =>
                        {
                            break;
                        }
                        Err(_) => break,
                    }
                    if buf.len() > 65_536 {
                        break;
                    }
                    if let Some(header_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&buf[..header_end]);
                        let length = header.lines().find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            if name.eq_ignore_ascii_case("content-length") {
                                value.trim().parse::<usize>().ok()
                            } else {
                                None
                            }
                        });
                        if let Some(length) = length {
                            if buf.len() >= header_end + 4 + length {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                }
                server_bodies
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf).into_owned());
                let _ = stream.write_all(
                    b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                let _ = stream.flush();
            }
        });
        let http = std::sync::Arc::new(
            serenity::http::HttpBuilder::new("token")
                .proxy(format!("http://{addr}"))
                .ratelimiter_disabled(true)
                .build(),
        );
        let probe = Arc::new(AutomodDeleteCancelProbe {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            skip_uncertain: true,
        });
        let task_app = Arc::clone(&app);
        let task_probe = Arc::clone(&probe);
        let task = tokio::spawn(async move {
            AUTOMOD_DELETE_CANCEL_PROBE
                .scope(task_probe, async move {
                    automod_try_delete_message(
                        &task_app,
                        &http,
                        10,
                        70,
                        42,
                        "badword",
                        "Bad words",
                        "blocked term",
                    )
                    .await;
                })
                .await;
        });
        probe.entered.notified().await;
        assert!(
            crate::logging::observed_delete_report(
                &app,
                serenity::model::id::GuildId::new(10),
                serenity::model::id::ChannelId::new(70),
                serenity::model::id::MessageId::new(42),
                None,
            )
            .is_none(),
            "gateway delete stays pending until REST settles"
        );
        probe.release.notify_one();
        task.await.unwrap();
        stop.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(addr);
        let _ = server.join();
        let logged = bodies.lock().unwrap().clone();
        assert!(
            logged.iter().any(|body| body.contains("Message deleted")),
            "cancelled delete must replay plain message_delete, got {logged:?}"
        );
        drop(app);
        let _ = std::fs::remove_file(&path);
        crate::tenant::remove_sidecars(&path);
    }
}
