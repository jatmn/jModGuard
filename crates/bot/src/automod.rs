//! Automod engine: sliding-window state per (guild, user) with per-rule
//! strike buckets, 6 generic detection rules with per-rule action escalation,
//! whitelists, and dry-run mode.
//!
//! All state is bounded: buckets are pruned on a timer and every event has a
//! window, so memory stays flat regardless of user count. Lookups are O(1)
//! per message (per-user bucket), never a scan of all tracked users.

use dashmap::DashMap;
use parking_lot::Mutex;
use regex::Regex;
use serenity::client::Context;
use serenity::model::guild::Member;
use serenity::model::id::{GuildId, UserId};
use serenity::model::prelude::Timestamp;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::normalize;
use crate::state::{BotState, CachedGuildConfig};
use crate::types::{ActionDef, RULE_IDS, RuleDef};
use crate::{db, logging};

pub struct AutomodEngine {
    /// (guild, user) -> per-rule strike buckets
    state: DashMap<(u64, u64), HashMap<&'static str, RuleState>>,
    member_joins: DashMap<(u64, u64), Instant>,
    last_prune: Mutex<Instant>,
}

#[derive(Default)]
struct RuleState {
    events: VecDeque<WindowEvent>,
    strikes: u32,
    fired: HashSet<u32>,
    last_event: Option<Instant>,
}

#[derive(Clone, Copy)]
struct WindowEvent {
    at: Instant,
}

/// Everything the engine needs to know about an incoming message.
pub struct MsgCtx<'a> {
    pub guild_id: u64,
    pub message_id: u64,
    pub channel_id: u64,
    pub author_id: u64,
    pub content: &'a str,
    pub attachment_count: usize,
    pub everyone_mentions: usize,
    pub here_mentions: usize,
    pub role_mentions: usize,
    pub joined_secs_ago: Option<u64>,
    pub account_age_days: Option<u64>,
    pub member_roles: &'a [u64],
    pub is_moderator: bool,
    pub can_mention_everyone: bool,
}

/// A rule fired and an action decided (highest severity across rules wins).
pub struct PendingAction {
    pub rule_id: String,
    pub rule_name: String,
    pub action: ActionDef,
    pub strikes: u32,
    pub evidence: serde_json::Value,
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
            last_prune: Mutex::new(Instant::now()),
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
    pub fn release_fired(&self, guild: u64, user: u64, rule_id: &str, at: u32) {
        let Some(mut bucket) = self.state.get_mut(&(guild, user)) else {
            return;
        };
        if let Some(state) = bucket.get_mut(rule_id) {
            state.fired.remove(&at);
        }
    }

    /// Removes stale state. Runs on a timer from the cleanup task.
    pub fn prune(&self) {
        let now = Instant::now();
        let keep = Duration::from_secs(60 * 10);
        self.state.retain(|_, rules| {
            rules.retain(|_, s| {
                s.events.retain(|e| now.duration_since(e.at) < keep);
                s.last_event
                    .map(|t| now.duration_since(t) < keep)
                    .unwrap_or(false)
                    || !s.events.is_empty()
            });
            !rules.is_empty()
        });
        self.member_joins
            .retain(|_, t| now.duration_since(*t) < Duration::from_secs(24 * 3600));
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
        let now = Instant::now();
        let normalized = normalize::normalize_lenient(ctx.content);
        let compact = normalize::normalize_compact(ctx.content);
        let has_url = URL_RE.is_match(ctx.content);
        let has_invite = INVITE_RE.is_match(ctx.content);

        let mut best: Option<PendingAction> = None;
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
                "invite" => has_invite && !self.invite_allowed(rule, ctx.content),
                "velocity" => self.trigger_velocity(rule, ctx, now),
                "account_age" => ctx
                    .account_age_days
                    .map(|d| d < param_u64(rule, "min_age_days", 3))
                    .unwrap_or(false),
                _ => false,
            };

            if !triggered {
                continue;
            }

            let mut bucket = self.state.entry((ctx.guild_id, ctx.author_id)).or_default();
            let state = bucket.entry(rule_id).or_default();
            state.strikes += 1;
            state.last_event = Some(now);
            state.events.push_back(WindowEvent { at: now });

            let strikes = state.strikes;
            let fire = rule
                .action_for_strike(strikes)
                .filter(|a| !state.fired.contains(&a.at));
            let Some(action) = fire else { continue };
            state.fired.insert(action.at);

            let pending = PendingAction {
                rule_id: rule_id.to_string(),
                rule_name: rule.name.clone(),
                action: action.clone(),
                strikes,
                evidence: serde_json::json!({
                    "content": truncate(ctx.content, 2000),
                    "channel_id": ctx.channel_id,
                    "message_id": ctx.message_id,
                    "attachments": ctx.attachment_count,
                    "mentions": ctx.everyone_mentions + ctx.here_mentions + ctx.role_mentions,
                }),
            };
            if is_more_severe(&pending, &best) {
                best = Some(pending);
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
                    let links = rule.params.get("links").and_then(|v| v.as_bool()) == Some(true);
                    let attachments =
                        rule.params.get("attachments").and_then(|v| v.as_bool()) == Some(true);
                    (links && has_url) || (attachments && ctx.attachment_count > 0)
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
            let Some(action) = jmodguard::custom_rules::custom_rule_action(rule) else {
                continue;
            };
            let pending = PendingAction {
                rule_id: rule_id.clone(),
                rule_name: rule.name.clone(),
                action,
                strikes: 1,
                evidence: serde_json::json!({
                    "content": truncate(ctx.content, 2000),
                    "channel_id": ctx.channel_id,
                    "message_id": ctx.message_id,
                    "attachments": ctx.attachment_count,
                }),
            };
            if is_more_severe(&pending, &best) {
                best = Some(pending);
            }
        }
        best
    }

    /// Counts this message's mentions plus recent mentions in the window.
    fn trigger_mention_spam(&self, rule: &RuleDef, ctx: &MsgCtx<'_>, now: Instant) -> bool {
        let window = Duration::from_secs(param_u64(rule, "window_secs", 10));
        let max_total = param_u64(rule, "max_mentions_in_window", 5) as usize;

        let mut total = ctx.everyone_mentions + ctx.here_mentions + ctx.role_mentions;
        if let Some(bucket) = self.state.get(&(ctx.guild_id, ctx.author_id)) {
            for (rid, s) in bucket.iter() {
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

    fn trigger_velocity(&self, rule: &RuleDef, ctx: &MsgCtx<'_>, now: Instant) -> bool {
        let max = param_u64(rule, "max_messages", 8) as usize;
        let window = Duration::from_secs(param_u64(rule, "window_secs", 10));
        let mut count = 1usize;
        if let Some(bucket) = self.state.get(&(ctx.guild_id, ctx.author_id)) {
            for s in bucket.values() {
                for e in s.events.iter() {
                    if now.duration_since(e.at) < window {
                        count += 1;
                    }
                }
            }
        }
        count > max
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

fn is_more_severe(cand: &PendingAction, best: &Option<PendingAction>) -> bool {
    fn rank(a: &str) -> u8 {
        match a {
            "ban" => 6,
            "kick" => 5,
            "timeout" => 4,
            "warn" => 3,
            "delete" => 2,
            _ => 1,
        }
    }
    best.as_ref()
        .map(|b| rank(&cand.action.action) > rank(&b.action.action))
        .unwrap_or(true)
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

/// Executes a decided automod action (or logs it in dry-run mode).
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
) {
    let enrollment = crate::cases::capture(app, guild_id.get()).await;
    let rule = &pending.action;
    let mut vars = crate::templates::user_vars(target.id.get(), &target.name, display_name);
    if let Some(channel) = msg_channel {
        vars.insert("source_channel_id".into(), channel.to_string());
    }
    vars.insert("rule".into(), pending.rule_name.clone());
    vars.insert("action".into(), rule.action.clone());
    vars.insert("strikes".into(), pending.strikes.to_string());
    vars.insert("time".into(), chrono::Utc::now().to_rfc3339());
    let reason = crate::templates::render(&rule.reason, &vars);
    vars.insert("reason".into(), reason.clone());

    let (dry, log_automod, open_case) = app
        .configs
        .get(&app.db, guild_id.get())
        .await
        .ok()
        .and_then(|c| {
            c.rules
                .get(&pending.rule_id)
                .map(|r| (r.dry_run, r.log_automod, r.open_case))
        })
        .unwrap_or((false, true, true));

    if dry {
        let mut v = vars.clone();
        v.insert(
            "action".into(),
            format!("[DRY RUN] would {}: {}", rule.action, rule.reason),
        );
        v.insert(
            "content".into(),
            pending.evidence["content"]
                .as_str()
                .unwrap_or("")
                .to_string(),
        );
        logging::emit(app, ctx, guild_id, "automod", v).await;
        return;
    }

    if matches!(rule.action.as_str(), "delete" | "timeout" | "kick" | "ban")
        && db::guild_stores::guild_is_write_fenced(&app.db, guild_id.get() as i64)
            .await
            .unwrap_or(true)
    {
        tracing::warn!(
            guild_id = guild_id.get(),
            action = rule.action,
            "automod Discord mutation skipped because the guild is write-fenced"
        );
        return;
    }

    let mut publish_report = true;
    match rule.action.as_str() {
        "log" => {}
        "delete" => {
            if let (Some(ch), Some(id)) = (msg_channel, msg_id) {
                if !app.activation.may_record() {
                    return;
                }
                let _ = crate::discord::race_discord_call(
                    &app.activation,
                    ctx.http.delete_message(ch.into(), id.into(), None),
                )
                .await;
            }
        }
        "warn" => {
            if !app.activation.may_record() {
                return;
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
                return;
            }
            let outcome = crate::cases::classify_db_commit(&warn_result);
            if open_case {
                crate::cases::record(
                    app,
                    enrollment.clone(),
                    crate::db::guild_data::CaseAction::Warning,
                    outcome,
                    target.id.get(),
                    None,
                    Some(reason.clone()),
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
        }
        "timeout" => {
            let minutes = rule.duration_minutes.max(1) as i64;
            let member = member_for_selected_action(
                provided_member.clone(),
                ctx.cache
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
                        return;
                    }
                    match crate::discord::race_discord_call(
                        &app.activation,
                        member.disable_communication_until_datetime(&ctx.http, until),
                    )
                    .await
                    {
                        crate::discord::DiscordCall::Cancelled => {
                            discord_err = Some(crate::discord::cutoff_discord_error());
                        }
                        crate::discord::DiscordCall::Finished(result) => {
                            discord_err = result.err();
                        }
                    }
                    called = true;
                } else {
                    tracing::warn!("automod timeout: could not build Discord timeout timestamp");
                }
            } else {
                tracing::warn!("automod timeout: no guild member for {}", target.id);
            }
            if !app.activation.may_record() {
                return;
            }
            if open_case
                && let Some(outcome) =
                    crate::cases::discord_attempt_outcome(called, discord_err.as_ref())
            {
                crate::cases::record(
                    app,
                    enrollment.clone(),
                    crate::db::guild_data::CaseAction::Timeout,
                    outcome,
                    target.id.get(),
                    None,
                    Some(reason.clone()),
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
        }
        "kick" => {
            let member = member_for_selected_action(
                provided_member.clone(),
                ctx.cache
                    .guild(guild_id)
                    .and_then(|g| g.members.get(&target.id).cloned()),
                target.id,
            );
            let (called, discord_err) = if let Some(member) = member {
                if !app.activation.may_record() {
                    return;
                }
                let discord_err = match crate::discord::race_discord_call(
                    &app.activation,
                    member.kick_with_reason(&ctx.http, &reason),
                )
                .await
                {
                    crate::discord::DiscordCall::Cancelled => {
                        Some(crate::discord::cutoff_discord_error())
                    }
                    crate::discord::DiscordCall::Finished(result) => result.err(),
                };
                (true, discord_err)
            } else {
                tracing::warn!("automod kick: no guild member for {}", target.id);
                (false, None)
            };
            if !app.activation.may_record() {
                return;
            }
            if open_case
                && let Some(outcome) =
                    crate::cases::discord_attempt_outcome(called, discord_err.as_ref())
            {
                crate::cases::record_application(
                    app,
                    enrollment.clone(),
                    crate::db::guild_data::CaseAction::Kick,
                    outcome,
                    target.id.get(),
                    None,
                    Some(reason.clone()),
                    crate::cases::text_excerpt(
                        msg_channel.unwrap_or(0),
                        msg_id.unwrap_or(0),
                        target.id.get(),
                        pending.evidence["content"].as_str().unwrap_or(""),
                    ),
                )
                .await;
            }
        }
        "ban" => {
            let decision = crate::member_directory::observe_ban_handoff(
                app,
                ctx.http.as_ref(),
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
                    if !app.activation.may_record() {
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
                                            Some(reason.clone()),
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
                            let operation_id = pending_ban.operation_id().to_owned();
                            let suppression = app.suppress_next_gateway_case(
                                crate::state::GatewayCaseAction::Ban,
                                guild_id.get(),
                                target.id.get(),
                                Some(&operation_id),
                            );
                            let raced = crate::discord::race_discord_call(
                                &app.activation,
                                guild_id.ban_with_reason(
                                    &ctx.http,
                                    target.id,
                                    rule.delete_days,
                                    &reason,
                                ),
                            )
                            .await;
                            match raced {
                                crate::discord::DiscordCall::Cancelled => {
                                    pending_ban
                                        .finish(app, crate::db::guild_data::CaseOutcome::Uncertain)
                                        .await;
                                    publish_report = false;
                                }
                                crate::discord::DiscordCall::Finished(discord) => {
                                    let outcome =
                                        crate::cases::classify_discord(discord.err().as_ref());
                                    if matches!(outcome, crate::db::guild_data::CaseOutcome::Failed)
                                    {
                                        app.release_gateway_case_suppression(suppression);
                                    }
                                    if app.activation.may_record() {
                                        if let Some(attempt) = attempted {
                                            attempt.finish(app, outcome.clone()).await;
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
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        other => {
            tracing::warn!("unknown automod action `{other}`");
        }
    }

    if !publish_report {
        return;
    }
    // Log-only/delete-only observations are routine Discord reports. Retain
    // selected moderation records until the typed case model replaces this table.
    if open_case && matches!(rule.action.as_str(), "warn" | "timeout" | "kick" | "ban") {
        if !app.activation.may_record() {
            return;
        }
        let evidence = serde_json::json!({
            "rule": pending.rule_id, "action": rule.action,
            "reason": reason, "evidence": pending.evidence,
        });
        let _ = db::insert_enrolled_event(
            &app.db,
            enrollment.clone(),
            "automod",
            Some(target.id.get() as i64),
            None,
            &evidence,
        )
        .await;
    }

    if !log_automod {
        return;
    }
    let mut v = vars;
    v.insert(
        "content".into(),
        pending.evidence["content"]
            .as_str()
            .unwrap_or("")
            .to_string(),
    );
    logging::emit(app, ctx, guild_id, "automod", v).await;
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

    use crate::cases::discord_attempt_outcome;
    use crate::db::guild_data::CaseOutcome;
    use crate::state::CachedGuildConfig;
    use crate::types::{ActionDef, GuildSettings, RuleDef};

    use super::{AutomodEngine, MsgCtx};

    #[test]
    fn selected_kick_timeout_cases_require_a_discord_attempt() {
        assert_eq!(discord_attempt_outcome(false, None), None);
        assert_eq!(
            discord_attempt_outcome(true, None),
            Some(CaseOutcome::Succeeded)
        );
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
            everyone_mentions: 0,
            here_mentions: 0,
            role_mentions: 0,
            joined_secs_ago: None,
            account_age_days: Some(30),
            member_roles: &roles,
            is_moderator: false,
            can_mention_everyone: false,
        };
        let first = engine.evaluate(&cfg, &message).expect("first ban");
        assert_eq!(first.action.action, "ban");
        assert!(engine.evaluate(&cfg, &message).is_none());
        engine.release_fired(1, 2, &first.rule_id, first.action.at);
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
            everyone_mentions: 0,
            here_mentions: 0,
            role_mentions: 0,
            joined_secs_ago: None,
            account_age_days: Some(30),
            member_roles: &[],
            is_moderator: is_mod,
            can_mention_everyone: can_everyone,
        }
    }

    #[test]
    fn custom_create_helper_starts_dry_run() {
        let rule = custom_rule(
            serde_json::json!({ "kind": "pattern", "pattern": "x" }),
            "delete",
        );
        assert!(rule.dry_run);
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
        assert!(
            engine
                .evaluate(&cfg, &msg(1, "scam", 0, false, false))
                .is_some()
        );
        assert!(
            engine
                .evaluate(&cfg, &msg(1, "scamper", 0, false, false))
                .is_none()
        );
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
    fn honeypot_role_added_helper() {
        let id = "custom:0123456789abcdef".to_string();
        let mut rules = HashMap::new();
        let mut honeypot = custom_rule(
            serde_json::json!({ "kind": "honeypot_role", "role_id": "99" }),
            "kick",
        );
        honeypot.dry_run = false;
        rules.insert(id.clone(), honeypot);
        let hit = jmodguard::custom_rules::honeypot_role_added(&rules, &[99], 42, &[], false);
        assert_eq!(
            hit.as_ref()
                .map(|(rid, a)| (rid.as_str(), a.action.as_str())),
            Some((id.as_str(), "kick"))
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
}
