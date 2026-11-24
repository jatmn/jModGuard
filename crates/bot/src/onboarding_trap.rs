//! The built-in trap listens to member events and uses the normal automod kick.
#[cfg(test)]
mod tests;

use crate::automod::{action_vars, automod_member_blocked, kick_with_recovery};
use crate::commands::{PermLevel, permission_level};
use crate::discord::{DiscordCall, race_discord_call};
use crate::state::BotState;
use crate::{cases, db, logging};
use jmodguard::onboarding_trap::{RULE_ID, join_window_minutes, role_id};
use jmodguard::types::RuleDef;
use parking_lot::Mutex;
use serenity::all::{
    Cache, GuildId, GuildMemberUpdateEvent, Http, Member, RoleId, Timestamp, User, UserId,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MATCH_LIMIT: usize = 4096;
const MATCH_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

type Subject = (GuildId, UserId);
type MatchKey = (Subject, Timestamp);

#[derive(Default)]
pub(crate) struct RecentMatches(Mutex<[ModeClaims; 2]>);

#[derive(Default)]
struct ModeClaims {
    active: HashMap<Subject, ActiveMatch>,
    completed: HashSet<MatchKey>,
    order: VecDeque<(Instant, MatchKey)>,
}

struct ActiveMatch {
    token: Arc<()>,
    joined_at: Timestamp,
    role: RoleId,
    invalidated: bool,
    latest_roles: Option<Vec<RoleId>>,
}

impl ModeClaims {
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
    token: Arc<()>,
    key: MatchKey,
}

impl MatchClaim<'_> {
    fn complete(&self, dry_run: bool, roles_admit: impl FnOnce(&[RoleId]) -> bool) -> bool {
        let mut modes = self.recent.0.lock();
        let subject = self.key.0;
        if modes[usize::from(self.dry_run)]
            .active
            .get(&subject)
            .is_none_or(|active| {
                !Arc::ptr_eq(&active.token, &self.token)
                    || active.invalidated
                    || !active.latest_roles.as_deref().is_none_or(roles_admit)
            })
        {
            return false;
        }
        // Record the dispatched mode, which can differ from the mode selected
        // by the Gateway matcher if an admin enabled dry-run in the meantime.
        let mode = &mut modes[usize::from(dry_run)];
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
        true
    }
}

impl Drop for MatchClaim<'_> {
    fn drop(&mut self) {
        let subject = self.key.0;
        let mut modes = self.recent.0.lock();
        let mode = &mut modes[usize::from(self.dry_run)];
        if mode
            .active
            .get(&subject)
            .is_some_and(|active| Arc::ptr_eq(&active.token, &self.token))
        {
            mode.active.remove(&subject);
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
    ) -> Option<MatchClaim<'_>> {
        let mut modes = self.0.lock();
        let claims = &mut modes[usize::from(dry_run)];
        let key = (subject, joined_at);
        claims.prune(Instant::now());
        if claims.completed.contains(&key) {
            return None;
        }
        if let Some(active) = claims.active.get(&subject) {
            if active.joined_at > joined_at || active.joined_at == joined_at && !active.invalidated
            {
                return None;
            }
        } else if claims.active.len() >= MATCH_LIMIT {
            return None;
        }
        let token = Arc::new(());
        claims.active.insert(
            subject,
            ActiveMatch {
                token: token.clone(),
                joined_at,
                role,
                invalidated: false,
                latest_roles: None,
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
        for mode in self.0.lock().iter_mut() {
            if let Some(active) = mode.active.get_mut(&(guild, user)) {
                if let Some((joined_at, roles)) = membership
                    && joined_at == active.joined_at
                    && roles.contains(&active.role)
                {
                    active.latest_roles = Some(roles.to_vec());
                } else {
                    active.invalidated = true;
                }
            }
        }
    }

    pub(crate) fn prune(&self) {
        let now = Instant::now();
        for mode in self.0.lock().iter_mut() {
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
    ) else {
        return;
    };
    let Some(enrollment) = cases::capture(app, event.guild.get()).await else {
        return;
    };
    let (mut vars, reason) =
        action_vars(event.user, event.display_name, &rule.name, action, 1, None);

    // Keep rule refresh, claim settlement and kick in the same cutoff race.
    let dispatch = async {
        let (member, dry_run) =
            member_at_dispatch(app, http, cache, &event, &enrollment, rule, &claim).await?;
        if dry_run {
            Some(Dispatch::DryRun)
        } else {
            Some(Dispatch::Kick(
                kick_with_recovery(app, http, event.guild, &member, &reason).await,
            ))
        }
    };
    let DiscordCall::Finished(Some(dispatched)) =
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
            vars.insert(
                "action".into(),
                format!("[DRY RUN] would kick: {}", action.reason),
            );
        }
        Dispatch::Kick(error) => {
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
    Kick(Option<serenity::Error>),
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
) -> Option<(Member, bool)> {
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
    Some((member, dry_run))
}
