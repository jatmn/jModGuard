//! The built-in trap listens to member events and uses the normal automod kick.
#[cfg(test)]
mod tests;

use crate::automod::{action_vars, automod_member_blocked, kick_with_recovery};
use crate::commands::{PermLevel, permission_level};
use crate::discord::{DiscordCall, race_discord_call};
use crate::state::BotState;
use crate::{cases, db, logging};
use jmodguard::onboarding_trap::{RULE_ID, cleanup_window_minutes, join_window_minutes, role_id};
use jmodguard::types::RuleDef;
use parking_lot::Mutex;
use serenity::all::{
    Cache, GuildId, GuildMemberUpdateEvent, Http, Member, RoleId, Timestamp, User, UserId,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Notify, Semaphore};

const MATCH_LIMIT: usize = 4096;
const MATCH_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

type Subject = (GuildId, UserId);
type MatchKey = (Subject, Timestamp);
// The cache generation orders primary rebaselines; the durable version orders
// snapshots within a generation.
type ConfigRevision = (u64, u64);

pub(crate) struct RecentMatches {
    claims: Mutex<Claims>,
    preflights: [Semaphore; 2],
}

#[derive(Default)]
struct Claims {
    active: [HashMap<Subject, ActiveMatch>; 2],
    history: [ReplayHistory; 2],
}

#[derive(Default)]
struct ReplayHistory {
    completed: HashSet<MatchKey>,
    order: VecDeque<(Instant, MatchKey)>,
}

struct ActiveMatch {
    token: Arc<Notify>,
    joined_at: Timestamp,
    role: RoleId,
    revision: ConfigRevision,
    phase: ClaimPhase,
    // Gateway evidence belongs to the stay, even when the claim is replaced.
    // An empty observation also vetoes dispatch after a leave or rejoin.
    latest_roles: Option<Vec<RoleId>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClaimPhase {
    Preflight,
    Invalidated,
    Dispatched,
}

impl Default for RecentMatches {
    fn default() -> Self {
        Self {
            claims: Mutex::default(),
            preflights: std::array::from_fn(|_| Semaphore::new(MATCH_LIMIT)),
        }
    }
}

impl ActiveMatch {
    fn can_replace_same_stay(&self, role: RoleId, revision: ConfigRevision) -> bool {
        self.phase != ClaimPhase::Dispatched
            && revision >= self.revision
            && (self.phase == ClaimPhase::Invalidated
                || role != self.role
                || revision > self.revision)
    }

    fn invalidate(&mut self) {
        if self.phase == ClaimPhase::Preflight {
            self.phase = ClaimPhase::Invalidated;
            // The single runner might not have started waiting yet.
            self.token.notify_one();
        }
    }
}

impl ReplayHistory {
    fn prune(&mut self, now: Instant) {
        while self
            .order
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) >= MATCH_RETENTION)
        {
            self.evict_oldest();
        }
    }

    fn evict_oldest(&mut self) {
        if let Some((_, key)) = self.order.pop_front() {
            self.completed.remove(&key);
        }
    }
}

struct MatchClaim<'a> {
    recent: &'a RecentMatches,
    dry_run: bool,
    token: Arc<Notify>,
    key: MatchKey,
}

impl MatchClaim<'_> {
    async fn preflight<T>(&self, work: impl Future<Output = Option<T>>) -> Option<T> {
        tokio::select! {
            biased;
            _ = self.token.notified() => None,
            result = async {
                // Superseded work counts until its future is dropped. A
                // replacement may wait for that permit even at capacity.
                let _permit = self.recent.preflights[usize::from(self.dry_run)]
                    .acquire().await.ok()?;
                work.await
            } => result,
        }
    }

    fn complete(&self, dry_run: bool, roles_admit: impl FnOnce(&[RoleId]) -> bool) -> bool {
        let Claims { active, history } = &mut *self.recent.claims.lock();
        let Some(active) = active[usize::from(self.dry_run)].get_mut(&self.key.0) else {
            return false;
        };
        if !Arc::ptr_eq(&active.token, &self.token)
            || active.phase != ClaimPhase::Preflight
            || !active.latest_roles.as_deref().is_none_or(roles_admit)
        {
            return false;
        }
        // Record the dispatched mode, which can differ from the mode selected
        // by the Gateway matcher if an admin enabled dry-run in the meantime.
        let mode = &mut history[usize::from(dry_run)];
        let now = Instant::now();
        mode.prune(now);
        if mode.completed.contains(&self.key) {
            return false;
        }
        if mode.completed.len() >= MATCH_LIMIT {
            mode.evict_oldest();
        }
        mode.completed.insert(self.key);
        mode.order.push_back((now, self.key));
        active.phase = ClaimPhase::Dispatched;
        true
    }
}

impl Drop for MatchClaim<'_> {
    fn drop(&mut self) {
        let subject = self.key.0;
        let mut claims = self.recent.claims.lock();
        let active = &mut claims.active[usize::from(self.dry_run)];
        if active
            .get(&subject)
            .is_some_and(|active| Arc::ptr_eq(&active.token, &self.token))
        {
            active.remove(&subject);
        }
    }
}

impl RecentMatches {
    fn claim(
        &self,
        subject: Subject,
        joined_at: Timestamp,
        role: RoleId,
        dry_run: bool,
        revision: ConfigRevision,
    ) -> Option<MatchClaim<'_>> {
        let mut claims = self.claims.lock();
        let mode = usize::from(dry_run);
        let key = (subject, joined_at);
        claims.history[mode].prune(Instant::now());
        if claims.history[mode].completed.contains(&key) {
            return None;
        }
        let active = &mut claims.active[mode];
        let latest_roles = if let Some(previous) = active.get_mut(&subject) {
            if previous.joined_at > joined_at
                || previous.joined_at == joined_at
                    && !previous.can_replace_same_stay(role, revision)
            {
                return None;
            }
            previous.invalidate();
            if previous.joined_at == joined_at {
                previous.latest_roles.take()
            } else {
                None
            }
        } else if active.len() >= MATCH_LIMIT {
            return None;
        } else {
            None
        };
        let token = Arc::new(Notify::new());
        active.insert(
            subject,
            ActiveMatch {
                token: token.clone(),
                joined_at,
                role,
                revision,
                phase: ClaimPhase::Preflight,
                latest_roles,
            },
        );
        Some(MatchClaim {
            recent: self,
            dry_run,
            token,
            key,
        })
    }

    pub(crate) fn observe(
        &self,
        guild: GuildId,
        user: UserId,
        membership: Option<(Timestamp, &[RoleId])>,
    ) {
        for mode in &mut self.claims.lock().active {
            if let Some(active) = mode.get_mut(&(guild, user))
                && active.phase != ClaimPhase::Dispatched
            {
                let roles = membership
                    .filter(|(joined_at, _)| *joined_at == active.joined_at)
                    .map_or(&[][..], |(_, roles)| roles);
                active.latest_roles = Some(roles.to_vec());
                if !roles.contains(&active.role) {
                    active.invalidate();
                }
            }
        }
    }

    pub(crate) fn prune(&self) {
        let now = Instant::now();
        for mode in &mut self.claims.lock().history {
            mode.prune(now);
        }
    }
}

struct RoleEvent<'a> {
    guild: GuildId,
    user: &'a User,
    display_name: &'a str,
    roles: &'a [RoleId],
    old_roles: Option<&'a [RoleId]>,
    joined_at: Timestamp,
}

fn matches_role(event: &RoleEvent<'_>, role: RoleId, minutes: u32, now: Timestamp) -> bool {
    let age = *now - *event.joined_at;
    role.get() != event.guild.get()
        && event.roles.contains(&role)
        && event.old_roles.is_none_or(|roles| !roles.contains(&role))
        && age >= chrono::Duration::zero()
        && (minutes == 0 || age <= chrono::Duration::minutes(i64::from(minutes)))
}

pub(crate) async fn on_join(app: &Arc<BotState>, http: &Http, cache: &Cache, member: &Member) {
    if let Some(joined_at) = member.joined_at {
        check_event(
            app,
            http,
            cache,
            RoleEvent {
                guild: member.guild_id,
                user: &member.user,
                display_name: member.display_name(),
                roles: &member.roles,
                old_roles: None,
                joined_at,
            },
        )
        .await;
    }
}

pub(crate) async fn on_update(
    app: &Arc<BotState>,
    http: &Http,
    cache: &Cache,
    old: Option<&Member>,
    event: &GuildMemberUpdateEvent,
) {
    check_event(
        app,
        http,
        cache,
        RoleEvent {
            guild: event.guild_id,
            user: &event.user,
            display_name: event
                .nick
                .as_deref()
                .unwrap_or_else(|| event.user.display_name()),
            roles: &event.roles,
            old_roles: old
                .filter(|old| old.joined_at == Some(event.joined_at))
                .map(|old| old.roles.as_slice()),
            joined_at: event.joined_at,
        },
    )
    .await;
}

async fn check_event(app: &Arc<BotState>, http: &Http, cache: &Cache, event: RoleEvent<'_>) {
    if event.roles.is_empty() {
        return;
    }
    let now = Timestamp::now();
    let Ok(cfg) = app.configs.get(&app.db, event.guild.get()).await else {
        return;
    };
    let Some(rule) = cfg.rules.get(RULE_ID).filter(|rule| rule.enabled) else {
        return;
    };
    let (Some(role), Some(minutes), [action]) = (
        role_id(rule),
        join_window_minutes(rule),
        rule.actions.as_slice(),
    ) else {
        return;
    };
    let role = RoleId::new(role);
    if action.action != "kick" || !matches_role(&event, role, minutes, now) {
        return;
    }
    let Some(claim) = app.automod.onboarding_matches.claim(
        (event.guild, event.user.id),
        event.joined_at,
        role,
        rule.dry_run,
        (cfg.version, cfg.durable_version),
    ) else {
        return;
    };
    let (mut vars, reason) =
        action_vars(event.user, event.display_name, &rule.name, action, 1, None);
    let cleanup_window = cleanup_window_minutes(rule).filter(|minutes| *minutes > 0);

    // Supersession cancels enrollment and preflight, but the cutoff race also
    // covers the committed kick. Claim completion is synchronous at handoff.
    let dispatch = async {
        let (enrollment, member, dry_run, role_exemptions) = claim
            .preflight(async {
                let enrollment = cases::capture(app, event.guild.get()).await?;
                let (member, dry_run, role_exemptions) =
                    member_at_dispatch(app, http, cache, &event, &enrollment, rule, &claim).await?;
                Some((enrollment, member, dry_run, role_exemptions))
            })
            .await?;
        let dispatched = if dry_run {
            Dispatch::DryRun
        } else {
            let started_at_ms = Timestamp::now().timestamp_millis();
            let error = kick_with_recovery(app, http, event.guild, &member, &reason).await;
            Dispatch::Kick {
                started_at_ms,
                error,
                role_exemptions,
            }
        };
        Some((enrollment, dispatched))
    };
    let DiscordCall::Finished(Some((enrollment, dispatched))) =
        race_discord_call(&app.activation, dispatch).await
    else {
        return;
    };
    drop(claim);
    if !app.activation.may_record() {
        return;
    }
    match dispatched {
        Dispatch::DryRun => {
            let mut action_text = format!("[DRY RUN] would kick: {}", action.reason);
            if let Some(minutes) = cleanup_window {
                action_text.push_str(&format!(
                    " and would clean tracked messages from the last {minutes} minutes"
                ));
            }
            vars.insert("action".into(), action_text);
        }
        Dispatch::Kick {
            started_at_ms,
            error,
            role_exemptions,
        } => {
            let outcome = cases::classify_discord(error.as_ref());
            let case_id = cases::record_onboarding_kick(
                app,
                Some(enrollment.clone()),
                outcome.clone(),
                event.user.id.get(),
                reason.clone(),
            )
            .await;
            match outcome {
                db::guild_data::CaseOutcome::Succeeded => {
                    if let Some(case_id) = case_id {
                        let mut case_vars = vars.clone();
                        case_vars.insert(
                            "actor".into(),
                            format!("Automod · case `{}`", case_id.as_str()),
                        );
                        logging::emit_to(app, http, event.guild, "member_kick", case_vars).await;
                    }
                    if let Some(minutes) = cleanup_window {
                        let cleanup = logging::begin_cleanup(
                            app,
                            event.guild.get(),
                            event.user.id.get(),
                            logging::CleanupRule {
                                id: RULE_ID,
                                name: &rule.name,
                                reason: &reason,
                                action: "kick",
                                join_only: false,
                                sample: None,
                                hash: None,
                            },
                        )
                        .await;
                        if let Some(cleanup) = &cleanup {
                            // The private kick case belongs in the Member kick log.
                            cleanup.outcome(db::guild_data::CaseOutcome::Succeeded, None);
                        }
                        let cutoff = (started_at_ms - i64::from(minutes) * 60_000)
                            .max(event.joined_at.timestamp_millis());
                        let deleted = crate::recent_messages::sweep_tracked_messages(
                            app,
                            http,
                            &enrollment,
                            event.user.id.get(),
                            cutoff..=started_at_ms,
                            "automod onboarding trap cleanup",
                            cleanup.as_ref(),
                            || async {
                                let mut dry_run = false;
                                refresh_rule(app, &enrollment, rule, &mut dry_run)
                                    .await
                                    .is_some_and(|current| {
                                        // The kick removed the member, so any change to the
                                        // admitted role exemptions stops cleanup.
                                        !dry_run
                                            && cleanup_window_minutes(&current) == Some(minutes)
                                            && current.whitelist_roles == role_exemptions
                                            && !jmodguard::custom_rules::rule_scope_skips(
                                                &current,
                                                event.user.id.get(),
                                                0,
                                                &[],
                                                false,
                                                false,
                                            )
                                    })
                            },
                        )
                        .await;
                        vars.insert(
                            "action".into(),
                            format!("kick, tracked messages deleted ({deleted})"),
                        );
                    }
                }
                db::guild_data::CaseOutcome::Failed => {
                    vars.insert("action".into(), "kick failed".into());
                }
                _ => {
                    vars.insert("action".into(), "kick outcome uncertain".into());
                }
            }
            if !app.activation.may_record() {
                return;
            }
            logging::log_persistence_result(
                "automod event",
                db::insert_enrolled_event(
                    &app.db,
                    Some(enrollment),
                    "automod",
                    Some(event.user.id.get() as i64),
                    None,
                    &serde_json::json!({
                        "rule": RULE_ID, "action": "kick", "reason": reason, "evidence": {},
                    }),
                )
                .await,
            );
        }
    }
    vars.insert("content".into(), String::new());
    logging::emit_to(app, http, event.guild, "automod", vars).await;
}

enum Dispatch {
    DryRun,
    Kick {
        started_at_ms: i64,
        error: Option<serenity::Error>,
        role_exemptions: Vec<u64>,
    },
}

/// Refresh after each potentially slow pre-dispatch operation. Once a match
/// becomes dry-run it stays log-only, even if the rule changes again.
async fn refresh_rule(
    app: &BotState,
    enrollment: &db::GuildEnrollment,
    matched: &RuleDef,
    dry_run: &mut bool,
) -> Option<RuleDef> {
    let (current, fenced) = db::get_onboarding_trap_for_dispatch(&app.db, enrollment.clone())
        .await
        .ok()?;
    *dry_run |= current.dry_run;
    (current.enabled
        && (*dry_run || !fenced)
        && current.skip_moderators
        && current.open_case
        && current.log_automod
        && role_id(&current) == role_id(matched)
        && join_window_minutes(&current) == join_window_minutes(matched)
        && matches!(current.actions.as_slice(), [action]
            if action.action == "kick" && action.at == 1 && action.reason == matched.actions[0].reason))
    .then_some(current)
}

async fn member_at_dispatch(
    app: &BotState,
    http: &Http,
    cache: &Cache,
    event: &RoleEvent<'_>,
    enrollment: &db::GuildEnrollment,
    matched: &RuleDef,
    claim: &MatchClaim<'_>,
) -> Option<(Member, bool, Vec<u64>)> {
    let (guild_id, user_id) = (event.guild, event.user.id);
    let joined_at = event.joined_at;
    let role = RoleId::new(role_id(matched)?);
    let mut dry_run = matched.dry_run;

    // Enrollment capture and both REST lookups are awaited. Check the
    // primary rule after each one, with the final read as the last await
    // before validating recent membership evidence and dispatching the kick.
    refresh_rule(app, enrollment, matched, &mut dry_run).await?;
    let guild = http.get_guild(guild_id).await.ok()?;
    if guild.owner_id == user_id {
        return None;
    }
    refresh_rule(app, enrollment, matched, &mut dry_run).await?;
    let member = http.get_member(guild_id, user_id).await.ok()?;
    let rule = refresh_rule(app, enrollment, matched, &mut dry_run).await?;

    let roles_admit = |assigned: &[RoleId]| {
        let roles: Vec<_> = assigned.iter().map(|role| role.get()).collect();
        let effective = roles.iter().copied().chain([guild_id.get()]);
        assigned.contains(&role)
            && !effective
                .clone()
                .any(|role| !guild.roles.contains_key(&RoleId::new(role)))
            && permission_level(app.cfg.bot.owner_id, &guild.roles, user_id.get(), effective)
                < PermLevel::Mod
            && !jmodguard::custom_rules::rule_scope_skips(
                &rule,
                user_id.get(),
                0,
                &roles,
                false,
                false,
            )
    };
    let cached_contradicts = cache.guild(guild_id).is_some_and(|cached| {
        let staff = |assigned: &[RoleId]| {
            let roles = assigned
                .iter()
                .map(|role| role.get())
                .chain([guild_id.get()]);
            permission_level(app.cfg.bot.owner_id, &cached.roles, user_id.get(), roles)
                >= PermLevel::Mod
        };
        cached.owner_id == user_id
            || staff(event.roles)
            || staff(&member.roles)
            || cached.members.get(&user_id).is_some_and(|cached_member| {
                cached_member.joined_at != Some(joined_at)
                    || !roles_admit(&cached_member.roles)
                    || staff(&cached_member.roles)
            })
    });
    if member.user.id != user_id
        || member.joined_at != Some(joined_at)
        || !roles_admit(event.roles)
        || !roles_admit(&member.roles)
        || cached_contradicts
        || !app.activation.may_record()
        || (!dry_run && automod_member_blocked(app, guild_id.get(), user_id.get(), "kick"))
        || !claim.complete(dry_run, roles_admit)
    {
        return None;
    }
    Some((member, dry_run, rule.whitelist_roles))
}
