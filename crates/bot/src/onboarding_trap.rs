//! The built-in trap listens to member events and uses the normal automod kick.
#[cfg(test)]
mod tests;

use crate::automod::{PendingAction, execute_pending_with};
use crate::commands::{PermLevel, permission_level};
use crate::state::BotState;
use jmodguard::onboarding_trap::{RULE_ID, join_window_minutes, role_id};
use parking_lot::Mutex;
use serenity::all::{
    Cache, GuildId, GuildMemberUpdateEvent, Http, Member, RoleId, Timestamp, User, UserId,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const MATCH_LIMIT: usize = 4096;
const MATCH_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

type MatchKey = (GuildId, UserId, Timestamp, bool);

#[derive(Default)]
pub(crate) struct RecentMatches(Mutex<HashMap<MatchKey, Instant>>);

impl RecentMatches {
    /// A join, role update, and concurrent replay can all describe the same stay.
    /// Dry-run claims are independent so a dry-run cannot consume a live match.
    pub(crate) fn claim(&self, member: &Member, dry_run: bool) -> bool {
        let Some(joined_at) = member.joined_at else {
            return false;
        };
        let key = (member.guild_id, member.user.id, joined_at, dry_run);
        let mut matches = self.0.lock();
        let now = Instant::now();
        if matches
            .get(&key)
            .is_some_and(|at| now.duration_since(*at) < MATCH_RETENTION)
        {
            return false;
        }
        if matches.len() >= MATCH_LIMIT && !matches.contains_key(&key) {
            return false;
        }
        matches.insert(key, now);
        true
    }

    pub(crate) fn prune(&self) {
        let now = Instant::now();
        self.0
            .lock()
            .retain(|_, at| now.saturating_duration_since(*at) < MATCH_RETENTION);
    }
}

/// A later Gateway observation can reveal a staff promotion during the REST
/// preflight. Deny on that positive evidence before the Discord mutation.
pub(crate) fn cached_staff(
    app: &BotState,
    cache: &Cache,
    guild_id: GuildId,
    user_id: UserId,
) -> bool {
    cache.guild(guild_id).is_some_and(|guild| {
        guild.owner_id == user_id
            || guild.members.get(&user_id).is_some_and(|member| {
                let roles = member
                    .roles
                    .iter()
                    .map(|role| role.get())
                    .chain([guild_id.get()]);
                permission_level(app.cfg.bot.owner_id, &guild.roles, user_id.get(), roles)
                    >= PermLevel::Mod
            })
    })
}

struct RoleEvent<'a> {
    guild: GuildId,
    user: &'a User,
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
    // A delayed update must not act on another stay, an already-kicked
    // member, or staff whose permissions changed since the Gateway event.
    let Ok(member) = http.get_member(event.guild, event.user.id).await else {
        return;
    };
    if member.joined_at != Some(event.joined_at) || !member.roles.contains(&role) {
        return;
    }
    let Ok(guild) = http.get_guild(event.guild).await else {
        return;
    };
    // Resolve @everyone as well as all assigned roles. Refuse incomplete
    // permission information, including during guild or role changes.
    let roles = member
        .roles
        .iter()
        .map(|role| role.get())
        .chain([event.guild.get()]);
    if guild.owner_id == member.user.id
        || roles
            .clone()
            .any(|role| !guild.roles.contains_key(&RoleId::new(role)))
        || permission_level(
            app.cfg.bot.owner_id,
            &guild.roles,
            member.user.id.get(),
            roles,
        ) >= PermLevel::Mod
    {
        return;
    }

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
    let display_name = member.display_name().to_owned();
    execute_pending_with(
        app,
        http,
        cache,
        event.guild,
        event.user,
        Some(member),
        &display_name,
        None,
        None,
        &pending,
    )
    .await;
}
