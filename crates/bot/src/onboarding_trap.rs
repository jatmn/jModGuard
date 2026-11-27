//! The built-in trap listens to member events and uses the normal automod kick.
#[cfg(test)]
mod tests;

use crate::automod::{action_vars, automod_member_blocked, kick_with_recovery};
use crate::commands::{PermLevel, permission_level};
use crate::discord::{DISCORD_REST_TIMEOUT, DiscordCall, race_discord_call};
use crate::state::BotState;
use crate::{cases, db, logging};
use jmodguard::onboarding_trap::{
    RULE_ID, cleanup_window_minutes, join_window_minutes, removal_role_ids, role_id,
};
use jmodguard::types::RuleDef;
use parking_lot::Mutex;
use serenity::all::{
    Cache, GuildId, GuildMemberUpdateEvent, Http, Member, PartialGuild, Permissions, RoleId,
    Timestamp, User, UserId,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Notify, Semaphore};

const MATCH_LIMIT: usize = 4096;
const MATCH_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const DISPATCH_EVIDENCE_WINDOW: Duration = Duration::from_secs(5);

#[cfg(test)]
tokio::task_local! {
    static FIXTURE_NOW: Timestamp;
}

fn now() -> Timestamp {
    #[cfg(test)]
    if let Ok(now) = FIXTURE_NOW.try_with(|now| *now) {
        return now;
    }
    Timestamp::now()
}

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
    guild_counts: HashMap<GuildId, usize>,
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
            self.evict_oldest(None);
        }
    }

    fn evict_oldest(&mut self, guild: Option<GuildId>) {
        let index = guild
            .and_then(|guild| {
                let retained = self.guild_counts.get(&guild).copied().unwrap_or(0);
                let represented = self.guild_counts.len() + usize::from(retained == 0);
                let share = MATCH_LIMIT / represented;
                // Let a guild with a thin history grow by giving it a slot
                // from a guild above its share. Otherwise reuse its own slots.
                self.order.iter().position(|&(_, ((entry_guild, _), _))| {
                    if retained < share {
                        self.guild_counts.get(&entry_guild).copied().unwrap_or(0) > share
                    } else {
                        entry_guild == guild
                    }
                })
            })
            .unwrap_or(0);
        if let Some((_, key @ ((guild, _), _))) = self.order.remove(index) {
            self.completed.remove(&key);
            if let Some(count) = self.guild_counts.get_mut(&guild) {
                *count -= 1;
                if *count == 0 {
                    self.guild_counts.remove(&guild);
                }
            }
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
        let ((guild, _), _) = self.key;
        if mode.completed.len() >= MATCH_LIMIT {
            mode.evict_oldest(Some(guild));
        }
        mode.completed.insert(self.key);
        mode.order.push_back((now, self.key));
        *mode.guild_counts.entry(guild).or_default() += 1;
        active.phase = ClaimPhase::Dispatched;
        true
    }

    fn dispatched_roles_admit(&self, roles_admit: impl FnOnce(&[RoleId]) -> bool) -> bool {
        self.recent.claims.lock().active[usize::from(self.dry_run)]
            .get(&self.key.0)
            .is_some_and(|active| {
                Arc::ptr_eq(&active.token, &self.token)
                    && active.phase == ClaimPhase::Dispatched
                    && active.latest_roles.as_deref().is_none_or(roles_admit)
            })
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
            if let Some(active) = mode.get_mut(&(guild, user)) {
                // After a mutation, a missing stay or trap role ends this sequence.
                if active.phase == ClaimPhase::Dispatched
                    && active
                        .latest_roles
                        .as_ref()
                        .is_some_and(|roles| !roles.contains(&active.role))
                {
                    continue;
                }
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

pub(crate) async fn on_join(app: &Arc<BotState>, http: &Arc<Http>, cache: &Cache, member: &Member) {
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
    http: &Arc<Http>,
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

async fn check_event(app: &Arc<BotState>, http: &Arc<Http>, cache: &Cache, event: RoleEvent<'_>) {
    if event.roles.is_empty() {
        return;
    }
    let now = now();
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
    let guard = DispatchGuard {
        app,
        http,
        cache,
        event: &event,
        matched: rule,
        claim: &claim,
        role,
    };

    // Supersession cancels preflight. Writer cutoff also covers the committed sequence.
    let dispatch = async {
        let (enrollment, prepared) = claim
            .preflight(async {
                let enrollment = cases::capture(app, event.guild.get()).await?;
                let prepared = guard.prepare(&enrollment).await?;
                Some((enrollment, prepared))
            })
            .await?;
        let dispatched = guard.execute(&enrollment, prepared, &reason).await;
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
    let removals = match dispatched {
        Dispatch::DryRun { remove_roles } => {
            let mut action_text = String::from("[DRY RUN] ");
            if remove_roles {
                action_text.push_str("would attempt to remove selected roles, then ");
            }
            action_text.push_str(&format!("would kick: {}", action.reason));
            if let Some(minutes) = cleanup_window {
                action_text.push_str(&format!(
                    " and would clean tracked messages from the last {minutes} minutes"
                ));
            }
            vars.insert("action".into(), action_text);
            RoleRemovals::default()
        }
        Dispatch::KickSkipped(removals) => {
            let action = if removals == RoleRemovals::default() {
                "kick cancelled before dispatch"
            } else {
                "kick cancelled after role removal attempt"
            };
            vars.insert("action".into(), action.into());
            removals
        }
        Dispatch::Kick {
            started_at_ms,
            error,
            role_exemptions,
            removals,
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
                            crate::recent_messages::TrackedDelete::Onboarding {
                                rule_id: RULE_ID,
                                rule_name: &rule.name,
                                reason: &reason,
                                log_http: http,
                            },
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
            removals
        }
    };
    if removals != RoleRemovals::default() {
        let action = vars.get_mut("action").expect("dispatch includes an action");
        action.insert_str(
            0,
            &format!(
                "roles removed ({}), unconfirmed or skipped ({}); ",
                removals.confirmed, removals.unconfirmed
            ),
        );
    }
    vars.insert("content".into(), String::new());
    logging::emit_to(app, http, event.guild, "automod", vars).await;
}

enum Dispatch {
    DryRun {
        remove_roles: bool,
    },
    KickSkipped(RoleRemovals),
    Kick {
        started_at_ms: i64,
        error: Option<serenity::Error>,
        role_exemptions: Vec<u64>,
        removals: RoleRemovals,
    },
}

#[derive(Default, PartialEq, Eq)]
struct RoleRemovals {
    confirmed: usize,
    unconfirmed: usize,
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

struct DispatchGuard<'a> {
    app: &'a BotState,
    http: &'a Http,
    cache: &'a Cache,
    event: &'a RoleEvent<'a>,
    matched: &'a RuleDef,
    claim: &'a MatchClaim<'a>,
    role: RoleId,
}

struct PreparedMatch {
    evidence: DispatchEvidence,
    removal_roles: Vec<u64>,
}

struct DispatchEvidence {
    member: Member,
    guild: PartialGuild,
    rule: RuleDef,
    dry_run: bool,
    bot: Option<Member>,
    expires_at: tokio::time::Instant,
}

async fn before_evidence_expires<T>(
    expires_at: tokio::time::Instant,
    work: impl Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        _ = tokio::time::sleep_until(expires_at) => None,
        result = work => Some(result),
    }
}

impl DispatchGuard<'_> {
    async fn prepare(&self, enrollment: &db::GuildEnrollment) -> Option<PreparedMatch> {
        let mut dry_run = self.matched.dry_run;
        let removal_roles =
            removal_role_ids(self.event.guild.get(), self.matched).unwrap_or_default();

        refresh_rule(self.app, enrollment, self.matched, &mut dry_run).await?;
        let bot = if removal_roles.is_empty() {
            None
        } else {
            self.bot_member().await
        };
        let evidence = self.dispatch_evidence(enrollment, dry_run, bot).await?;
        if !self.claim.complete(evidence.dry_run, |roles| {
            self.roles_admit(&evidence.guild, &evidence.rule, roles)
        }) {
            return None;
        }
        Some(PreparedMatch {
            evidence,
            removal_roles,
        })
    }

    async fn execute(
        &self,
        enrollment: &db::GuildEnrollment,
        prepared: PreparedMatch,
        reason: &str,
    ) -> Dispatch {
        let (guild_id, user_id) = (self.event.guild, self.event.user.id);
        let PreparedMatch {
            mut evidence,
            removal_roles,
        } = prepared;
        let mut removals = RoleRemovals::default();
        let selection_matches = |rule: &RuleDef| {
            removal_role_ids(guild_id.get(), rule).as_ref() == Some(&removal_roles)
        };
        if evidence.dry_run {
            return Dispatch::DryRun {
                remove_roles: selection_matches(&evidence.rule)
                    && evidence.can_remove_roles()
                    && removal_roles
                        .iter()
                        .any(|id| evidence.holds_unmanaged_role(RoleId::new(*id))),
            };
        }
        for id in &removal_roles {
            if !selection_matches(&evidence.rule) {
                break;
            }
            let id = RoleId::new(*id);
            if !evidence.holds_unmanaged_role(id) {
                continue;
            }
            if !evidence.can_remove_roles() {
                removals.unconfirmed += 1;
                continue;
            }
            let result = before_evidence_expires(
                evidence.expires_at,
                self.http
                    .remove_member_role(guild_id, user_id, id, Some(reason)),
            )
            .await;
            if matches!(result, Some(Ok(()))) {
                removals.confirmed += 1;
            } else {
                removals.unconfirmed += 1;
            }

            // Recheck membership, bot capability and configuration after the request.
            let Some(current) = self.recheck(enrollment).await else {
                return Dispatch::KickSkipped(removals);
            };
            evidence = current;
        }

        let started_at_ms = now().timestamp_millis();
        let mut attempted = false;
        let error = match before_evidence_expires(evidence.expires_at, async {
            attempted = true;
            kick_with_recovery(self.app, self.http, guild_id, &evidence.member, reason).await
        })
        .await
        {
            Some(error) => error,
            None if attempted => Some(serenity::Error::Other("onboarding kick deadline elapsed")),
            None => return Dispatch::KickSkipped(removals),
        };
        Dispatch::Kick {
            started_at_ms,
            error,
            role_exemptions: evidence.rule.whitelist_roles,
            removals,
        }
    }

    async fn bot_member(&self) -> Option<Member> {
        let bot_id = self.cache.current_user().id;
        let member = tokio::time::timeout(
            DISCORD_REST_TIMEOUT,
            self.http.get_member(self.event.guild, bot_id),
        )
        .await
        .ok()?
        .ok()?;
        (member.user.id == bot_id).then_some(member)
    }

    async fn dispatch_evidence(
        &self,
        enrollment: &db::GuildEnrollment,
        mut dry_run: bool,
        bot: Option<Member>,
    ) -> Option<DispatchEvidence> {
        // Read the target after both the optional bot lookup and the guild
        // lookup. The final primary check can still veto a config change made
        // during REST reads. Bound that handoff and the ensuing REST call.
        let expires_at = tokio::time::Instant::now() + DISPATCH_EVIDENCE_WINDOW;
        let guild_id = self.event.guild;
        before_evidence_expires(expires_at, async {
            let guild = self.http.get_guild(guild_id).await.ok()?;
            if guild.owner_id == self.event.user.id {
                return None;
            }
            let member = self
                .http
                .get_member(guild_id, self.event.user.id)
                .await
                .ok()?;
            let rule = refresh_rule(self.app, enrollment, self.matched, &mut dry_run).await?;
            self.member_admits(&guild, &member, &rule, dry_run)
                .then_some(DispatchEvidence {
                    guild,
                    member,
                    rule,
                    dry_run,
                    bot,
                    expires_at,
                })
        })
        .await?
    }

    async fn recheck(&self, enrollment: &db::GuildEnrollment) -> Option<DispatchEvidence> {
        let bot = self.bot_member().await;
        let evidence = self.dispatch_evidence(enrollment, false, bot).await?;
        // Missing bot membership evidence skips later removals. A fresh member
        // without kick capability still vetoes the kick.
        (!evidence.dry_run
            && evidence.bot.as_ref().is_none_or(|bot| {
                bot_can_act(
                    &evidence.guild,
                    bot,
                    &evidence.member,
                    Permissions::KICK_MEMBERS,
                )
            })
            && self.claim.dispatched_roles_admit(|roles| {
                self.roles_admit(&evidence.guild, &evidence.rule, roles)
            }))
        .then_some(evidence)
    }

    fn roles_admit(&self, guild: &PartialGuild, rule: &RuleDef, assigned: &[RoleId]) -> bool {
        let user = self.event.user.id.get();
        let roles: Vec<_> = assigned.iter().map(|role| role.get()).collect();
        let effective = roles.iter().copied().chain([guild.id.get()]);
        assigned.contains(&self.role)
            && !effective
                .clone()
                .any(|role| !guild.roles.contains_key(&RoleId::new(role)))
            && permission_level(self.app.cfg.bot.owner_id, &guild.roles, user, effective)
                < PermLevel::Mod
            && !jmodguard::custom_rules::rule_scope_skips(rule, user, 0, &roles, false, false)
    }

    fn member_admits(
        &self,
        guild: &PartialGuild,
        member: &Member,
        rule: &RuleDef,
        dry_run: bool,
    ) -> bool {
        let (guild_id, user_id) = (self.event.guild, self.event.user.id);
        let joined_at = self.event.joined_at;
        let cached_contradicts = self.cache.guild(guild_id).is_some_and(|cached| {
            let staff = |assigned: &[RoleId]| {
                let roles = assigned
                    .iter()
                    .map(|role| role.get())
                    .chain([guild_id.get()]);
                permission_level(
                    self.app.cfg.bot.owner_id,
                    &cached.roles,
                    user_id.get(),
                    roles,
                ) >= PermLevel::Mod
            };
            cached.owner_id == user_id
                || staff(self.event.roles)
                || staff(&member.roles)
                || cached.members.get(&user_id).is_some_and(|cached_member| {
                    cached_member.joined_at != Some(joined_at)
                        || !self.roles_admit(guild, rule, &cached_member.roles)
                        || staff(&cached_member.roles)
                })
        });
        guild.owner_id != user_id
            && member.user.id == user_id
            && member.joined_at == Some(joined_at)
            && self.roles_admit(guild, rule, self.event.roles)
            && self.roles_admit(guild, rule, &member.roles)
            && !cached_contradicts
            && self.app.activation.may_record()
            && (dry_run || !automod_member_blocked(self.app, guild_id.get(), user_id.get(), "kick"))
    }
}

impl DispatchEvidence {
    fn holds_unmanaged_role(&self, id: RoleId) -> bool {
        self.member.roles.contains(&id)
            && self.guild.roles.get(&id).is_some_and(|role| !role.managed)
    }

    fn can_remove_roles(&self) -> bool {
        self.bot.as_ref().is_some_and(|bot| {
            bot_can_act(
                &self.guild,
                bot,
                &self.member,
                Permissions::MANAGE_ROLES | Permissions::KICK_MEMBERS,
            )
        })
    }
}

fn bot_can_act(
    guild: &PartialGuild,
    bot: &Member,
    member: &Member,
    permissions: Permissions,
) -> bool {
    if bot.user.id == member.user.id
        || !guild.member_permissions(bot).contains(permissions)
        || bot.roles.iter().any(|id| !guild.roles.contains_key(id))
    {
        return false;
    }
    let top = bot.roles.iter().filter_map(|id| guild.roles.get(id)).max();
    top.is_some_and(|top| {
        member
            .roles
            .iter()
            .all(|id| guild.roles.get(id).is_some_and(|role| role < top))
    })
}
