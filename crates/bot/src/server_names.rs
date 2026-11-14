//! Keeps the guild store's role and channel names current for the Web UI
//! pickers (#621). Display data only: a failed write is logged and never
//! affects moderation. Each change stores the whole snapshot again, so a missed
//! event heals on the next role or channel change or Gateway reconnect.

use crate::db::guild_data::{ServerChannelName, ServerNames, ServerRoleName};
use crate::state::BotState;
use serenity::model::{
    channel::{ChannelType, GuildChannel},
    guild::{Guild, Role},
};
use serenity::prelude::Context;

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
    }
}

/// Store a full Gateway guild's names after its membership observation.
pub async fn record_guild(app: &BotState, guild: &Guild) {
    let names = snapshot(guild.roles.values(), guild.channels.values());
    record(app, guild.id.get(), names).await;
}

/// Store the cached guild's names after a role or channel change. The cache
/// already reflects the event; owned data is copied before any await.
pub async fn refresh_from_cache(app: &BotState, ctx: &Context, guild_id: u64) {
    let names = {
        let Some(guild) = ctx.cache.guild(guild_id) else {
            return;
        };
        snapshot(guild.roles.values(), guild.channels.values())
    };
    record(app, guild_id, names).await;
}

async fn record(app: &BotState, guild_id: u64, names: ServerNames) {
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
        .replace_server_names(scope, names, jmodguard::control::now_ms())
        .await
    {
        tracing::warn!(guild_id, "server names not saved: {error:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
