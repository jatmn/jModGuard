//! The built-in trap listens to member events and uses the normal automod kick.
#[cfg(test)]
mod tests;

use crate::automod::{PendingAction, execute_pending_with};
use crate::commands::{PermLevel, permission_level};
use crate::state::BotState;
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
pub(crate) struct RecentMatches([Mutex<ModeClaims>; 2]);

#[derive(Default)]
struct ModeClaims {
    active: HashMap<Subject, ActiveMatch>,
    completed: HashSet<MatchKey>,
    order: VecDeque<(Instant, MatchKey)>,
}

struct ActiveMatch {
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
    mode: &'a Mutex<ModeClaims>,
    key: MatchKey,
}

impl Drop for MatchClaim<'_> {
    fn drop(&mut self) {
        let (subject, joined_at) = self.key;
        let mut mode = self.mode.lock();
        if mode
            .active
            .get(&subject)
            .is_some_and(|active| active.joined_at == joined_at)
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
        let mode = &self.0[usize::from(dry_run)];
        let mut claims = mode.lock();
        let key = (subject, joined_at);
        claims.prune(Instant::now());
        if claims.completed.contains(&key) {
            return None;
        }
        if let Some(active) = claims.active.get(&subject) {
            if active.joined_at >= joined_at {
                return None;
            }
        } else if claims.active.len() >= MATCH_LIMIT {
            return None;
        }
        claims.active.insert(
            subject,
            ActiveMatch {
                joined_at,
                role,
                invalidated: false,
                latest_roles: None,
            },
        );
        Some(MatchClaim { mode, key })
    }

    pub(crate) fn observe(
        &self,
        guild: GuildId,
        user: UserId,
        membership: Option<(Timestamp, &[RoleId])>,
    ) {
        for mode in &self.0 {
            let mut mode = mode.lock();
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

    fn complete_if_current(
        &self,
        subject: Subject,
        joined_at: Timestamp,
        dry_run: bool,
        roles_admit: impl FnOnce(&[RoleId]) -> bool,
    ) -> bool {
        let mut mode = self.0[usize::from(dry_run)].lock();
        let key = (subject, joined_at);
        if mode.completed.contains(&key)
            || mode.active.get(&subject).is_none_or(|active| {
                active.joined_at != joined_at
                    || active.invalidated
                    || !active.latest_roles.as_deref().is_none_or(roles_admit)
            })
        {
            return false;
        }
        if mode.completed.len() >= MATCH_LIMIT {
            mode.evict_oldest();
        }
        mode.completed.insert(key);
        mode.order.push_back((Instant::now(), key));
        true
    }

    pub(crate) fn prune(&self) {
        let now = Instant::now();
        for mode in &self.0 {
            mode.lock().prune(now);
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
    let Some(_claim) = app.automod.onboarding_matches.claim(
        (event.guild, event.user.id),
        event.joined_at,
        role,
        rule.dry_run,
    ) else {
        return;
    };
    let mut member = Member::default();
    member.guild_id = event.guild;
    member.user = event.user.clone();
    member.joined_at = Some(event.joined_at);
    member.roles = event.roles.to_vec();

    let pending = PendingAction {
        rule_id: RULE_ID.into(),
        rule_name: rule.name.clone(),
        action: action.clone(),
        strikes: 1,
        evidence: serde_json::json!({}),
        mention_permission_unknown: false,
        matched_rule: Some(rule.clone()),
        dry_run: rule.dry_run,
    };
    execute_pending_with(
        app,
        http,
        cache,
        event.guild,
        event.user,
        Some(member),
        event.display_name,
        None,
        None,
        &pending,
    )
    .await;
}

// Called after configuration and fence reads. Refresh the member last and
// check any Gateway evidence that arrived while the HTTP calls were pending.
pub(crate) async fn member_at_dispatch(
    app: &BotState,
    http: &Http,
    cache: &Cache,
    matched: &Member,
    rule: &RuleDef,
    claimed_dry_run: bool,
) -> Option<Member> {
    use crate::discord::{DiscordCall, race_discord_call};
    let (guild_id, user_id) = (matched.guild_id, matched.user.id);
    let joined_at = matched.joined_at?;
    let role = RoleId::new(role_id(rule)?);
    let DiscordCall::Finished(Ok(guild)) =
        race_discord_call(&app.activation, http.get_guild(guild_id)).await
    else {
        return None;
    };
    if guild.owner_id == user_id {
        return None;
    }
    let DiscordCall::Finished(Ok(member)) =
        race_discord_call(&app.activation, http.get_member(guild_id, user_id)).await
    else {
        return None;
    };
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
                rule,
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
            || staff(&member.roles)
            || cached.members.get(&user_id).is_some_and(|cached_member| {
                cached_member.joined_at != Some(joined_at)
                    || !roles_admit(&cached_member.roles)
                    || staff(&cached_member.roles)
            })
    });
    if member.user.id != user_id
        || member.joined_at != Some(joined_at)
        || !roles_admit(&member.roles)
        || cached_contradicts
        || !app.activation.may_record()
        || !app.automod.onboarding_matches.complete_if_current(
            (guild_id, user_id),
            joined_at,
            claimed_dry_run,
            roles_admit,
        )
    {
        return None;
    }
    Some(member)
}
