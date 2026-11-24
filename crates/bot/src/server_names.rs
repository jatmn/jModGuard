//! Keeps the guild store's role and channel names current for the Web UI
//! pickers (#621). Display data only: a failed write is logged and never
//! affects moderation. Each refresh stores the whole snapshot again, so a missed
//! event heals on the next role or channel change or Gateway reconnect.

use crate::db::guild_data::{OnboardingTrapSetup, ServerChannelName, ServerNames, ServerRoleName};
use crate::state::BotState;
use serenity::model::{
    channel::{ChannelType, GuildChannel, PermissionOverwriteType},
    guild::{Guild, Role},
    id::{RoleId, UserId},
    permissions::Permissions,
};
use serenity::prelude::Context;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

fn channel_kind(kind: ChannelType) -> Option<&'static str> {
    Some(match kind {
        ChannelType::Text => "text",
        ChannelType::Voice => "voice",
        ChannelType::Category => "category",
        ChannelType::News => "announcement",
        ChannelType::Stage => "stage",
        ChannelType::Forum => "forum",
        ChannelType::Unknown(16) => "media",
        // Threads, DMs and directories are not server channels a rule can name.
        _ => return None,
    })
}

pub fn snapshot<'a>(
    roles: impl IntoIterator<Item = &'a Role>,
    channels: impl IntoIterator<Item = &'a GuildChannel>,
) -> ServerNames {
    ServerNames {
        roles: roles
            .into_iter()
            .map(|role| ServerRoleName {
                id: role.id.get().to_string(),
                name: role.name.clone(),
                color: role.colour.0,
                position: i64::from(role.position),
                managed: role.managed,
            })
            .collect(),
        channels: channels
            .into_iter()
            .filter_map(|channel| {
                Some(ServerChannelName {
                    id: channel.id.get().to_string(),
                    name: channel.name.clone(),
                    kind: channel_kind(channel.kind)?.to_owned(),
                    parent_id: channel.parent_id.map(|id| id.get().to_string()),
                    position: i64::from(channel.position),
                })
            })
            .collect(),
        observed_at_ms: None,
        onboarding_trap: None,
    }
}

fn onboarding_setup(guild: &Guild, bot_id: UserId) -> OnboardingTrapSetup {
    let bot_permissions = guild.members.get(&bot_id).and_then(|bot| {
        let everyone = guild.roles.get(&RoleId::new(guild.id.get()))?;
        let mut permissions = everyone.permissions;
        let mut top = everyone;
        for id in &bot.roles {
            let role = guild.roles.get(id)?;
            permissions |= role.permissions;
            if top == everyone || role > top {
                top = role;
            }
        }
        Some((permissions, top))
    });
    let roles_at_or_above_bot = bot_permissions.map_or_else(Vec::new, |(_, top)| {
        guild
            .roles
            .values()
            .filter(|role| {
                role.id.get() != guild.id.get() && (top.id.get() == guild.id.get() || *role >= top)
            })
            .map(|role| role.id.to_string())
            .collect()
    });
    let mut grants: std::collections::HashSet<RoleId> = guild
        .roles
        .values()
        .filter(|role| !role.permissions.is_empty())
        .map(|role| role.id)
        .collect();
    let mut member_grants = std::collections::HashSet::new();
    for overwrite in guild
        .channels
        .values()
        .flat_map(|channel| &channel.permission_overwrites)
    {
        if overwrite.allow.is_empty() {
            continue;
        }
        match overwrite.kind {
            PermissionOverwriteType::Role(role) => {
                grants.insert(role);
            }
            PermissionOverwriteType::Member(user) => {
                if let Some(member) = guild.members.get(&user) {
                    member_grants.extend(member.roles.iter().copied());
                }
            }
            _ => {}
        }
    }
    OnboardingTrapSetup {
        bot_can_kick: bot_permissions.map(|(permissions, _)| {
            guild.owner_id == bot_id
                || permissions.intersects(Permissions::ADMINISTRATOR | Permissions::KICK_MEMBERS)
        }),
        roles_at_or_above_bot,
        roles_with_grants: grants.into_iter().map(|role| role.to_string()).collect(),
        roles_with_member_grants: member_grants
            .into_iter()
            .map(|role| role.to_string())
            .collect(),
    }
}

pub(crate) fn refresh_for_member_grants(
    app: &Arc<BotState>,
    ctx: &Context,
    guild_id: u64,
    user_id: UserId,
) {
    let affects_setup = ctx.cache.guild(guild_id).is_some_and(|guild| {
        guild.channels.values().any(|channel| {
            channel.permission_overwrites.iter().any(|overwrite| {
                overwrite.kind == PermissionOverwriteType::Member(user_id)
                    && !overwrite.allow.is_empty()
            })
        })
    });
    if affects_setup {
        refresh_from_cache(app, ctx, guild_id);
    }
}

/// Guilds with a refresh running, and whether another change arrived meanwhile.
/// Entries exist only while a refresh runs, so this stays bounded by busy guilds.
static REFRESHING: LazyLock<Mutex<HashMap<u64, bool>>> = LazyLock::new(Default::default);

/// Store the cached guild's names after a Gateway guild, role or channel event.
/// The cache already reflects the event. One task per guild writes at a time and
/// repeats while newer events arrive, so the last write is always the latest
/// cache and a burst of reorder events costs a few writes, not one each. Runs in
/// the background so activity logs and moderation never wait for it.
pub fn refresh_from_cache(app: &Arc<BotState>, ctx: &Context, guild_id: u64) {
    if !claim(guild_id) {
        return;
    }
    let (app, ctx) = (Arc::clone(app), ctx.clone());
    tokio::spawn(async move {
        loop {
            // Owned data is copied before any await.
            let observed_at_ms = jmodguard::control::now_ms();
            let names = ctx.cache.guild(guild_id).map(|guild| {
                let mut names = snapshot(guild.roles.values(), guild.channels.values());
                names.onboarding_trap = Some(onboarding_setup(&guild, ctx.cache.current_user().id));
                names
            });
            if let Some(names) = names {
                record(&app, guild_id, names, observed_at_ms).await;
            }
            if !finish(guild_id) {
                break;
            }
        }
    });
}

/// True when this caller should start the refresh task for `guild_id`.
fn claim(guild_id: u64) -> bool {
    let mut running = REFRESHING
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    match running.get_mut(&guild_id) {
        Some(again) => {
            *again = true;
            false
        }
        None => {
            running.insert(guild_id, false);
            true
        }
    }
}

/// True when another change arrived during the last write and the task must repeat.
fn finish(guild_id: u64) -> bool {
    let mut running = REFRESHING
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    match running.get_mut(&guild_id) {
        Some(again) if *again => {
            *again = false;
            true
        }
        _ => {
            running.remove(&guild_id);
            false
        }
    }
}

async fn record(app: &BotState, guild_id: u64, names: ServerNames, observed_at_ms: i64) {
    let Some(enrollment) = crate::cases::capture(app, guild_id).await else {
        return;
    };
    let (store, scope) = match app.tenant.live(enrollment).await {
        Ok(live) => live,
        Err(error) => {
            tracing::warn!(guild_id, "server names scope unavailable: {error:#}");
            return;
        }
    };
    if let Err(error) = store
        .replace_server_names(scope, names, observed_at_ms)
        .await
    {
        tracing::warn!(guild_id, "server names not saved: {error:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn onboarding_trap_setup_reports_hierarchy_kick_permission_and_role_grants() {
        use serenity::all::{GuildId, Member, PermissionOverwrite};

        let mut guild = Guild::default();
        guild.id = GuildId::new(10);
        let bot_id = UserId::new(3);
        let everyone = RoleId::new(10);
        let higher_on_tie = RoleId::new(20);
        let bot_role = RoleId::new(21);
        let channel_grant = RoleId::new(22);
        let server_grant = RoleId::new(23);
        for (id, position, permissions) in [
            (everyone, 0, Permissions::empty()),
            (higher_on_tie, 2, Permissions::empty()),
            (bot_role, 2, Permissions::empty()),
            (channel_grant, 1, Permissions::empty()),
            (server_grant, 1, Permissions::VIEW_CHANNEL),
        ] {
            let mut role = Role::default();
            role.id = id;
            role.position = position;
            role.permissions = permissions;
            guild.roles.insert(id, role);
        }
        let mut bot = Member::default();
        bot.roles = vec![bot_role];
        guild.members.insert(bot_id, bot);
        let mut channel = GuildChannel::default();
        channel.permission_overwrites.push(PermissionOverwrite {
            allow: Permissions::VIEW_CHANNEL,
            deny: Permissions::empty(),
            kind: PermissionOverwriteType::Role(channel_grant),
        });
        guild.channels.insert(channel.id, channel);

        let setup = onboarding_setup(&guild, bot_id);
        assert_eq!(setup.bot_can_kick, Some(false));
        assert!(
            setup
                .roles_at_or_above_bot
                .contains(&higher_on_tie.to_string())
        );
        assert!(
            !setup
                .roles_at_or_above_bot
                .contains(&channel_grant.to_string())
        );
        assert!(setup.roles_with_grants.contains(&channel_grant.to_string()));
        assert!(setup.roles_with_grants.contains(&server_grant.to_string()));
        assert!(!setup.roles_with_grants.contains(&higher_on_tie.to_string()));

        guild.roles.get_mut(&bot_role).unwrap().permissions = Permissions::ADMINISTRATOR;
        assert_eq!(onboarding_setup(&guild, bot_id).bot_can_kick, Some(true));
        guild.roles.remove(&bot_role);
        assert_eq!(onboarding_setup(&guild, bot_id).bot_can_kick, None);
    }

    #[test]
    fn onboarding_trap_separates_member_channel_allows_from_role_grants() {
        use serenity::all::{Member, PermissionOverwrite};

        let mut guild = Guild::default();
        let trap = RoleId::new(22);
        let user = UserId::new(8);
        let mut member = Member::default();
        member.roles.push(trap);
        guild.members.insert(user, member);
        let mut channel = GuildChannel::default();
        channel.permission_overwrites.push(PermissionOverwrite {
            kind: PermissionOverwriteType::Member(user),
            allow: Permissions::VIEW_CHANNEL,
            deny: Permissions::empty(),
        });
        guild.channels.insert(channel.id, channel);

        let setup = onboarding_setup(&guild, UserId::new(3));
        assert!(setup.roles_with_grants.is_empty());
        assert_eq!(setup.roles_with_member_grants, [trap.to_string()]);

        guild.members.get_mut(&user).unwrap().roles.clear();
        assert!(
            onboarding_setup(&guild, UserId::new(3))
                .roles_with_member_grants
                .is_empty()
        );
    }

    #[test]
    fn refreshes_coalesce_per_guild_and_repeat_after_a_late_change() {
        let guild = 9_007_199_254_740_001;
        assert!(claim(guild), "the first event starts the task");
        assert!(!claim(guild), "a second event joins the running task");
        assert!(!claim(guild));
        assert!(
            finish(guild),
            "a change during the write runs one more pass"
        );
        assert!(!finish(guild), "no newer change: the task ends");
        assert!(claim(guild), "a later event starts a new task");
        assert!(!finish(guild));
    }

    #[test]
    fn snapshot_keeps_server_channels_and_role_display_facts() {
        let mut role = Role::default();
        role.id = serenity::model::id::RoleId::new(9_007_199_254_740_993);
        role.name = "Moderators".into();
        role.colour = serenity::model::Colour(0x3498db);
        role.position = 4;
        role.managed = true;
        let mut text = GuildChannel::default();
        text.id = serenity::model::id::ChannelId::new(9_007_199_254_740_995);
        text.name = "general".into();
        text.kind = ChannelType::Text;
        text.parent_id = Some(serenity::model::id::ChannelId::new(42));
        text.position = 2;
        let mut thread = text.clone();
        thread.id = serenity::model::id::ChannelId::new(7);
        thread.kind = ChannelType::PublicThread;
        let mut media = text.clone();
        media.id = serenity::model::id::ChannelId::new(8);
        media.kind = ChannelType::Unknown(16);
        media.parent_id = None;

        let names = snapshot([&role], [&text, &thread, &media]);
        assert_eq!(
            names.roles,
            vec![ServerRoleName {
                id: "9007199254740993".into(),
                name: "Moderators".into(),
                color: 0x3498db,
                position: 4,
                managed: true,
            }]
        );
        assert_eq!(
            names.channels,
            vec![
                ServerChannelName {
                    id: "9007199254740995".into(),
                    name: "general".into(),
                    kind: "text".into(),
                    parent_id: Some("42".into()),
                    position: 2,
                },
                ServerChannelName {
                    id: "8".into(),
                    name: "general".into(),
                    kind: "media".into(),
                    parent_id: None,
                    position: 2,
                },
            ]
        );
    }
}
