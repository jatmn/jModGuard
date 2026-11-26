//! Keeps the guild store's role and channel names current for the Web UI
//! pickers (#621) and preset channel selection. A failed write is logged and
//! never affects live moderation. Each refresh stores the whole snapshot again, so a missed
//! event heals on the next role or channel change or Gateway reconnect.

use crate::db::guild_data::{OnboardingTrapSetup, ServerChannelName, ServerNames, ServerRoleName};
use crate::state::BotState;
use serenity::model::{
    channel::{ChannelType, GuildChannel, PermissionOverwrite, PermissionOverwriteType},
    guild::{Guild, Role},
    id::{RoleId, UserId},
    permissions::Permissions,
};
use serenity::prelude::Context;
use std::collections::{HashMap, HashSet, hash_map::Entry};
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
    schedule_refresh(app, ctx, guild_id, Some(user_id));
}

const MAX_PENDING_MEMBERS: usize = 128;

enum PendingRefresh {
    Full,
    Members(HashSet<UserId>),
}

impl PendingRefresh {
    fn needs_snapshot<'a>(
        &self,
        mut overwrites: impl Iterator<Item = &'a PermissionOverwrite>,
    ) -> bool {
        match self {
            Self::Full => true,
            // A grant can belong to a user absent from the member cache.
            Self::Members(users) => overwrites.any(|overwrite| {
                !overwrite.allow.is_empty()
                    && matches!(overwrite.kind, PermissionOverwriteType::Member(user) if users.contains(&user))
            }),
        }
    }
}

/// Busy guilds and changes waiting for their next pass. An empty slot keeps
/// ownership with the worker while it reads the cache and publishes a snapshot.
static REFRESHING: LazyLock<Mutex<HashMap<u64, Option<PendingRefresh>>>> =
    LazyLock::new(Default::default);

/// Store the cached guild's names after a Gateway guild, role or channel event.
/// The cache already reflects the event. One task per guild writes at a time and
/// repeats while newer events arrive, so the last write is always the latest
/// cache and a burst of reorder events costs a few writes, not one each. Runs in
/// the background so activity logs and moderation never wait for it.
pub fn refresh_from_cache(app: &Arc<BotState>, ctx: &Context, guild_id: u64) {
    schedule_refresh(app, ctx, guild_id, None);
}

fn schedule_refresh(app: &Arc<BotState>, ctx: &Context, guild_id: u64, member: Option<UserId>) {
    if !enqueue_refresh(guild_id, member) {
        return;
    }
    let (app, cache) = (Arc::clone(app), Arc::clone(&ctx.cache));
    tokio::spawn(async move {
        refresh_pending(&app, &cache, guild_id).await;
    });
}

async fn refresh_pending(app: &BotState, cache: &serenity::cache::Cache, guild_id: u64) {
    while let Some(pending) = take_refresh(guild_id) {
        // Eligibility and the owned snapshot use the same cache observation.
        let observed_at_ms = jmodguard::control::now_ms();
        let names = cache
            .guild(guild_id)
            .and_then(|guild| snapshot_for_refresh(&guild, cache.current_user().id, &pending));
        if let Some(names) = names {
            record(app, guild_id, names, observed_at_ms).await;
        } else {
            // Keep a sustained stream of negative batches from monopolizing
            // the worker's executor thread.
            tokio::task::yield_now().await;
        }
    }
}

fn snapshot_for_refresh(
    guild: &Guild,
    bot_id: UserId,
    pending: &PendingRefresh,
) -> Option<ServerNames> {
    let overwrites = guild
        .channels
        .values()
        .flat_map(|channel| &channel.permission_overwrites);
    if !pending.needs_snapshot(overwrites) {
        return None;
    }
    let mut names = snapshot(guild.roles.values(), guild.channels.values());
    names.onboarding_trap = Some(onboarding_setup(guild, bot_id));
    Some(names)
}

/// True when this caller should start the refresh task for `guild_id`.
fn enqueue_refresh(guild_id: u64, member: Option<UserId>) -> bool {
    let mut running = REFRESHING
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let (start, pending) = match running.entry(guild_id) {
        Entry::Occupied(entry) => (false, entry.into_mut()),
        Entry::Vacant(entry) => (true, entry.insert(None)),
    };
    if let Some(user) = member {
        match pending.get_or_insert_with(|| PendingRefresh::Members(HashSet::new())) {
            PendingRefresh::Members(users) if users.len() < MAX_PENDING_MEMBERS => {
                users.insert(user);
            }
            PendingRefresh::Members(users) if !users.contains(&user) => {
                // Re-read the full setup rather than discard an unclassified event.
                *pending = Some(PendingRefresh::Full);
            }
            _ => {}
        }
    } else {
        *pending = Some(PendingRefresh::Full);
    }
    start
}

/// Take the next batch, or release the guild when no changes remain.
fn take_refresh(guild_id: u64) -> Option<PendingRefresh> {
    let mut running = REFRESHING
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let next = running.get_mut(&guild_id).and_then(Option::take);
    if next.is_none() {
        running.remove(&guild_id);
    }
    next
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
        use serenity::all::{GuildId, Member};

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
        use serenity::all::Member;

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
    fn role_only_member_burst_scans_the_overwrites_once() {
        let guild = 9_007_199_254_740_001;
        let overwrites = vec![
            PermissionOverwrite {
                allow: Permissions::VIEW_CHANNEL,
                deny: Permissions::empty(),
                kind: PermissionOverwriteType::Role(RoleId::new(17)),
            };
            200 * 10
        ];
        for id in 1..=100 {
            assert_eq!(enqueue_refresh(guild, Some(UserId::new(id))), id == 1);
        }
        let batch = take_refresh(guild).unwrap();
        let mut inspections = 0;
        assert!(!batch.needs_snapshot(overwrites.iter().inspect(|_| inspections += 1)));
        assert_eq!(inspections, 2_000);
        assert!(take_refresh(guild).is_none());
    }

    #[test]
    fn full_refresh_and_pending_member_overflow_keep_full_work() {
        let guild = 9_007_199_254_740_002;
        let member = UserId::new(1);
        for id in 1..=MAX_PENDING_MEMBERS {
            assert_eq!(
                enqueue_refresh(guild, Some(UserId::new(id as u64))),
                id == 1
            );
        }
        assert!(!enqueue_refresh(guild, Some(member)));
        assert!(matches!(
            take_refresh(guild),
            Some(PendingRefresh::Members(users)) if users.len() == MAX_PENDING_MEMBERS
        ));
        for id in 1..=MAX_PENDING_MEMBERS + 1 {
            assert!(!enqueue_refresh(guild, Some(UserId::new(id as u64))));
        }
        assert!(matches!(take_refresh(guild), Some(PendingRefresh::Full)));
        assert!(!enqueue_refresh(guild, Some(member)));
        assert!(!enqueue_refresh(guild, None));
        assert!(!enqueue_refresh(guild, Some(member)));
        assert!(matches!(take_refresh(guild), Some(PendingRefresh::Full)));
        assert!(take_refresh(guild).is_none());
        assert!(enqueue_refresh(guild, Some(member)));
        assert!(matches!(
            take_refresh(guild),
            Some(PendingRefresh::Members(users)) if users == HashSet::from([member])
        ));
        assert!(take_refresh(guild).is_none());
    }

    #[test]
    fn member_batches_require_positive_individual_allows() {
        let member = UserId::new(35);
        let batch = PendingRefresh::Members(HashSet::from([member]));
        let mut overwrite = PermissionOverwrite {
            kind: PermissionOverwriteType::Member(member),
            allow: Permissions::empty(),
            deny: Permissions::empty(),
        };
        assert!(!batch.needs_snapshot([&overwrite].into_iter()));
        overwrite.deny = Permissions::VIEW_CHANNEL;
        assert!(!batch.needs_snapshot([&overwrite].into_iter()));
        overwrite.allow = Permissions::SEND_MESSAGES;
        overwrite.kind = PermissionOverwriteType::Member(UserId::new(36));
        assert!(!batch.needs_snapshot([&overwrite].into_iter()));
        overwrite.kind = PermissionOverwriteType::Member(member);
        assert!(batch.needs_snapshot([&overwrite].into_iter()));
    }

    #[tokio::test]
    async fn setup_tracks_an_absent_grantee_through_a_join_during_refresh_and_rejoin() {
        use serenity::all::{
            Cache, ChannelId, GuildCreateEvent, GuildId, GuildMemberAddEvent,
            GuildMemberRemoveEvent, Member,
        };

        let guild_id = 9_007_199_254_740_003;
        let member_id = UserId::new(8);
        let role_id = RoleId::new(22);
        let directory = std::env::temp_dir().join(format!(
            "server_names_member_grant_{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir(&directory).unwrap();
        let mut cfg = crate::config::BotProcessConfig::default();
        cfg.db.path = directory.join("core.db");
        let db = crate::db::Db::open(&cfg.db.path, 2).unwrap();
        let app = BotState::new(cfg, db).unwrap();
        let enrollment = crate::db::enroll_guild(&app.db, guild_id as i64, 100)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();
        let (store, scope) = app.tenant.live(enrollment).await.unwrap();

        let cache = Cache::new();
        let mut guild = Guild::default();
        guild.id = GuildId::new(guild_id);
        let mut role = Role::default();
        role.id = role_id;
        role.name = "Fixture role".into();
        guild.roles.insert(role_id, role);
        let mut channel = GuildChannel::default();
        channel.id = ChannelId::new(10);
        channel.kind = ChannelType::Text;
        channel.name = "fixture-channel".into();
        channel.permission_overwrites.push(PermissionOverwrite {
            kind: PermissionOverwriteType::Member(member_id),
            allow: Permissions::VIEW_CHANNEL,
            deny: Permissions::empty(),
        });
        guild.channels.insert(channel.id, channel);
        let mut create: GuildCreateEvent =
            serde_json::from_value(serde_json::to_value(guild).unwrap()).unwrap();
        cache.update(&mut create);

        assert!(enqueue_refresh(guild_id, None));
        refresh_pending(&app, &cache, guild_id).await;
        let before = store.server_names(scope.clone()).await.unwrap();
        assert!(
            before
                .onboarding_trap
                .unwrap()
                .roles_with_member_grants
                .is_empty()
        );

        assert!(enqueue_refresh(guild_id, None));
        let first = take_refresh(guild_id).unwrap();
        let captured =
            snapshot_for_refresh(&cache.guild(guild_id).unwrap(), UserId::new(3), &first).unwrap();
        let mut member = Member::default();
        member.guild_id = GuildId::new(guild_id);
        member.user.id = member_id;
        member.roles.push(role_id);
        let mut join: GuildMemberAddEvent =
            serde_json::from_value(serde_json::to_value(&member).unwrap()).unwrap();
        let mut leave: GuildMemberRemoveEvent = serde_json::from_value(serde_json::json!({
            "guild_id": member.guild_id,
            "user": member.user,
        }))
        .unwrap();
        cache.update(&mut join);
        assert!(
            !enqueue_refresh(guild_id, Some(member_id)),
            "join arrived after the active pass read the cache"
        );
        record(&app, guild_id, captured, before.observed_at_ms.unwrap()).await;
        assert!(
            store
                .server_names(scope.clone())
                .await
                .unwrap()
                .onboarding_trap
                .unwrap()
                .roles_with_member_grants
                .is_empty()
        );
        refresh_pending(&app, &cache, guild_id).await;
        let saved = store.server_names(scope.clone()).await.unwrap();
        assert_eq!(
            saved.onboarding_trap.unwrap().roles_with_member_grants,
            [role_id.to_string()]
        );
        for joining in [false, true] {
            if joining {
                cache.update(&mut join);
            } else {
                cache.update(&mut leave);
            }
            assert!(enqueue_refresh(guild_id, Some(member_id)));
            refresh_pending(&app, &cache, guild_id).await;
            let saved = store.server_names(scope.clone()).await.unwrap();
            let expected = if joining {
                vec![role_id.to_string()]
            } else {
                vec![]
            };
            assert_eq!(
                saved.onboarding_trap.unwrap().roles_with_member_grants,
                expected
            );
        }
        drop((store, scope, app));
        std::fs::remove_dir_all(directory).unwrap();
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
