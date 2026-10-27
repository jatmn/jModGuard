//! Cached voice-channel lookup for the `!voice` command.

use serenity::model::guild::Guild;
use serenity::model::id::{ChannelId, GuildId, UserId};
use serenity::model::voice::VoiceState;

/// Shipped success line when the member is in a cached voice channel.
pub const IN_CHANNEL_TEMPLATE: &str = "**{target}** is in **{channel}** (`{channel_id}`)";
/// Rendered when the member has no cached voice state or left a channel.
pub const NOT_IN_CHANNEL_TEMPLATE: &str = "**{target}** is not in a voice channel.";
/// Rendered when the guild or voice-state cache is empty (for example right after a restart).
pub const NOT_CACHED_TEMPLATE: &str = "**{target}** voice state is not cached.";

const CHANNEL_NAME_CLIP: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceLookupSnapshot {
    InChannel {
        channel_id: u64,
        channel_name: String,
    },
    NotInChannel,
    NotCached,
}

/// Reads the gateway cache and returns an owned snapshot. Callers must drop the
/// cache reference before awaiting.
pub fn snapshot_cached_voice(
    cache: &serenity::cache::Cache,
    guild_id: GuildId,
    target: UserId,
) -> VoiceLookupSnapshot {
    let Some(guild) = cache.guild(guild_id) else {
        return VoiceLookupSnapshot::NotCached;
    };
    if guild.voice_states.is_empty() {
        return VoiceLookupSnapshot::NotCached;
    }
    match guild.voice_states.get(&target) {
        None => VoiceLookupSnapshot::NotInChannel,
        Some(state) => snapshot_voice_state(cache, guild_id, &guild, state),
    }
}

fn snapshot_voice_state(
    cache: &serenity::cache::Cache,
    guild_id: GuildId,
    guild: &Guild,
    state: &VoiceState,
) -> VoiceLookupSnapshot {
    let Some(channel_id) = state.channel_id else {
        return VoiceLookupSnapshot::NotInChannel;
    };
    if !channel_belongs_to_guild(cache, guild_id, guild, channel_id) {
        return VoiceLookupSnapshot::NotCached;
    }
    let channel_name = guild
        .channels
        .get(&channel_id)
        .map(|channel| clip_channel_name(&channel.name))
        .unwrap_or_else(|| channel_id.to_string());
    VoiceLookupSnapshot::InChannel {
        channel_id: channel_id.get(),
        channel_name,
    }
}

fn channel_belongs_to_guild(
    cache: &serenity::cache::Cache,
    guild_id: GuildId,
    guild: &Guild,
    channel_id: ChannelId,
) -> bool {
    if guild.channels.contains_key(&channel_id) {
        return true;
    }
    for cached_guild_id in cache.guilds() {
        if cache
            .guild(cached_guild_id)
            .is_some_and(|cached| cached.channels.contains_key(&channel_id))
        {
            return cached_guild_id == guild_id;
        }
    }
    false
}

fn clip_channel_name(name: &str) -> String {
    if name.chars().count() <= CHANNEL_NAME_CLIP {
        return name.to_string();
    }
    let clipped: String = name.chars().take(CHANNEL_NAME_CLIP).collect();
    format!("{clipped}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serenity::model::channel::ChannelType;
    use serenity::model::channel::GuildChannel;
    use serenity::model::event::GuildCreateEvent;
    use serenity::model::guild::Guild;
    use serenity::model::id::{ChannelId, GuildId, UserId};
    use serenity::model::voice::VoiceState;
    use std::collections::HashMap;
    use std::sync::Arc;

    const GUILD: u64 = 10;
    const TARGET: u64 = 42;
    const VOICE_CHANNEL: u64 = 100;
    const OTHER_GUILD: u64 = 20;

    fn voice_state(user: u64, channel: Option<u64>) -> VoiceState {
        let value = serde_json::json!({
            "channel_id": channel.map(|id| id.to_string()),
            "deaf": false,
            "guild_id": GUILD.to_string(),
            "mute": false,
            "self_deaf": false,
            "self_mute": false,
            "self_stream": null,
            "self_video": false,
            "session_id": "fixture",
            "suppress": false,
            "user_id": user.to_string(),
        });
        serde_json::from_value(value).unwrap()
    }

    fn voice_channel(name: &str) -> GuildChannel {
        let mut channel = GuildChannel::default();
        channel.id = ChannelId::new(VOICE_CHANNEL);
        channel.guild_id = GuildId::new(GUILD);
        channel.kind = ChannelType::Voice;
        channel.name = name.into();
        channel
    }

    fn cache_with_guild(
        voice_states: HashMap<UserId, VoiceState>,
        channels: Vec<GuildChannel>,
    ) -> Arc<serenity::cache::Cache> {
        let cache = Arc::new(serenity::cache::Cache::new());
        let mut guild = Guild::default();
        guild.id = GuildId::new(GUILD);
        guild.name = "fixture".into();
        guild.voice_states = voice_states;
        for channel in channels {
            guild.channels.insert(channel.id, channel);
        }
        let mut event: GuildCreateEvent =
            serde_json::from_value(serde_json::to_value(guild).unwrap()).unwrap();
        cache.update(&mut event);
        cache
    }

    #[test]
    fn cached_voice_state_returns_channel_name_and_id() {
        let mut states = HashMap::new();
        states.insert(
            UserId::new(TARGET),
            voice_state(TARGET, Some(VOICE_CHANNEL)),
        );
        let cache = cache_with_guild(states, vec![voice_channel("Lounge")]);
        let snapshot = snapshot_cached_voice(&cache, GuildId::new(GUILD), UserId::new(TARGET));
        assert_eq!(
            snapshot,
            VoiceLookupSnapshot::InChannel {
                channel_id: VOICE_CHANNEL,
                channel_name: "Lounge".into(),
            }
        );
    }

    #[test]
    fn null_channel_is_not_in_voice() {
        let mut states = HashMap::new();
        states.insert(UserId::new(TARGET), voice_state(TARGET, None));
        let cache = cache_with_guild(states, vec![]);
        assert_eq!(
            snapshot_cached_voice(&cache, GuildId::new(GUILD), UserId::new(TARGET)),
            VoiceLookupSnapshot::NotInChannel
        );
    }

    #[test]
    fn missing_user_voice_state_is_not_in_voice() {
        let mut states = HashMap::new();
        states.insert(UserId::new(99), voice_state(99, Some(VOICE_CHANNEL)));
        let cache = cache_with_guild(states, vec![voice_channel("Lounge")]);
        assert_eq!(
            snapshot_cached_voice(&cache, GuildId::new(GUILD), UserId::new(TARGET)),
            VoiceLookupSnapshot::NotInChannel
        );
    }

    #[test]
    fn empty_guild_cache_is_not_cached() {
        let cache = Arc::new(serenity::cache::Cache::new());
        assert_eq!(
            snapshot_cached_voice(&cache, GuildId::new(GUILD), UserId::new(TARGET)),
            VoiceLookupSnapshot::NotCached
        );
    }

    #[test]
    fn empty_voice_states_is_not_cached_after_restart() {
        let cache = cache_with_guild(HashMap::new(), vec![]);
        assert_eq!(
            snapshot_cached_voice(&cache, GuildId::new(GUILD), UserId::new(TARGET)),
            VoiceLookupSnapshot::NotCached
        );
    }

    #[test]
    fn foreign_guild_channel_is_not_cached() {
        let mut states = HashMap::new();
        states.insert(
            UserId::new(TARGET),
            voice_state(TARGET, Some(VOICE_CHANNEL)),
        );
        let cache = cache_with_guild(states, vec![]);
        let mut foreign_guild = Guild::default();
        foreign_guild.id = GuildId::new(OTHER_GUILD);
        foreign_guild.name = "other".into();
        let mut foreign = GuildChannel::default();
        foreign.id = ChannelId::new(VOICE_CHANNEL);
        foreign.guild_id = GuildId::new(OTHER_GUILD);
        foreign.kind = ChannelType::Voice;
        foreign.name = "Elsewhere".into();
        foreign_guild.channels.insert(foreign.id, foreign);
        let mut event: GuildCreateEvent =
            serde_json::from_value(serde_json::to_value(foreign_guild).unwrap()).unwrap();
        cache.update(&mut event);
        assert_eq!(
            snapshot_cached_voice(&cache, GuildId::new(GUILD), UserId::new(TARGET)),
            VoiceLookupSnapshot::NotCached
        );
    }

    #[test]
    fn snapshot_is_owned_before_await_point() {
        fn assert_owned(snapshot: VoiceLookupSnapshot) -> VoiceLookupSnapshot {
            match snapshot {
                VoiceLookupSnapshot::InChannel {
                    channel_id,
                    channel_name,
                } => VoiceLookupSnapshot::InChannel {
                    channel_id,
                    channel_name,
                },
                other => other,
            }
        }
        let mut states = HashMap::new();
        states.insert(
            UserId::new(TARGET),
            voice_state(TARGET, Some(VOICE_CHANNEL)),
        );
        let cache = cache_with_guild(states, vec![voice_channel("Lounge")]);
        let snapshot = snapshot_cached_voice(&cache, GuildId::new(GUILD), UserId::new(TARGET));
        drop(cache);
        assert_eq!(
            assert_owned(snapshot),
            VoiceLookupSnapshot::InChannel {
                channel_id: VOICE_CHANNEL,
                channel_name: "Lounge".into(),
            }
        );
    }

    #[test]
    fn long_channel_names_are_clipped() {
        let long = "x".repeat(CHANNEL_NAME_CLIP + 10);
        let mut states = HashMap::new();
        states.insert(
            UserId::new(TARGET),
            voice_state(TARGET, Some(VOICE_CHANNEL)),
        );
        let cache = cache_with_guild(states, vec![voice_channel(&long)]);
        let snapshot = snapshot_cached_voice(&cache, GuildId::new(GUILD), UserId::new(TARGET));
        let VoiceLookupSnapshot::InChannel { channel_name, .. } = snapshot else {
            panic!("expected in-channel snapshot");
        };
        assert!(channel_name.ends_with('…'));
        assert_eq!(channel_name.chars().count(), CHANNEL_NAME_CLIP + 1);
    }

    #[test]
    fn voice_command_requires_mod_permission() {
        let voice = crate::defaults::default_commands()
            .into_iter()
            .find(|cmd| cmd.name == "voice")
            .expect("voice command is seeded");
        assert_eq!(voice.permission, "mod");
        assert!(matches!(voice.behavior, crate::types::Behavior::Voice));
    }

    async fn fixture_context(
        http: Arc<serenity::http::Http>,
        cache: Arc<serenity::cache::Cache>,
    ) -> serenity::client::Context {
        use serenity::client::Context;
        use serenity::gateway::{
            Shard, ShardManager, ShardManagerOptions, ShardMessenger, ShardRunner,
            ShardRunnerOptions,
        };
        use serenity::model::gateway::{GatewayIntents, ShardInfo};
        use serenity::model::id::ShardId;
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let url = Arc::new(tokio::sync::Mutex::new(format!(
            "ws://{}",
            listener.local_addr().unwrap()
        )));
        let accept = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            tokio_tungstenite::accept_async(stream).await.unwrap()
        });
        let intents = GatewayIntents::empty();
        let shard = Shard::new(
            Arc::clone(&url),
            "fixture",
            ShardInfo {
                id: ShardId(0),
                total: 1,
            },
            intents,
            None,
        )
        .await
        .unwrap();
        let socket = accept.await.unwrap();
        let data = Arc::default();
        let (manager, _) = ShardManager::new(ShardManagerOptions {
            data: Arc::clone(&data),
            event_handlers: vec![],
            raw_event_handlers: vec![],
            shard_index: 0,
            shard_init: 0,
            shard_total: 1,
            ws_url: url,
            cache: Arc::clone(&cache),
            http: Arc::clone(&http),
            intents,
            presence: None,
        });
        let runner = ShardRunner::new(ShardRunnerOptions {
            data: Arc::clone(&data),
            event_handlers: vec![],
            raw_event_handlers: vec![],
            manager: Arc::clone(&manager),
            shard,
            cache: Arc::clone(&cache),
            http: Arc::clone(&http),
        });
        let ctx = Context {
            data,
            shard: ShardMessenger::new(&runner),
            shard_id: ShardId(0),
            http,
            cache,
        };
        manager.shutdown_all().await;
        drop(runner);
        drop(socket);
        ctx
    }

    #[tokio::test]
    async fn empty_cache_voice_command_makes_no_http_requests() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let server = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut request = Vec::new();
                loop {
                    let mut buf = [0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                    if request.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                counter.fetch_add(1, Ordering::SeqCst);
                let response =
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });

        let http = Arc::new(
            serenity::http::HttpBuilder::new("fixture")
                .proxy(format!("http://{address}"))
                .application_id(serenity::model::id::ApplicationId::new(1))
                .ratelimiter_disabled(true)
                .build(),
        );
        let cache = Arc::new(serenity::cache::Cache::new());
        let ctx = fixture_context(http, cache).await;

        let path = std::env::temp_dir().join(format!(
            "voice_lookup_http_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = crate::db::Db::open(&path, 2).unwrap();
        let app = crate::state::BotState::new(Default::default(), db).unwrap();
        app.activation.mark_lock_held(true);

        let cmd = crate::defaults::default_commands()
            .into_iter()
            .find(|c| c.name == "voice")
            .unwrap();
        let mut actor = serenity::model::guild::Member::default();
        actor.user.id = UserId::new(1);
        actor.user.name = "mod".into();
        actor.guild_id = GuildId::new(GUILD);

        let env = crate::commands::CmdEnv {
            app: &app,
            ctx: &ctx,
            cmd: &cmd,
            guild_id: GuildId::new(GUILD),
            channel_id: ChannelId::new(1),
            actor: actor.user.id,
            actor_member: actor,
            args: crate::commands::Args::new(&TARGET.to_string()),
            source_message: None,
        };
        let outcome = crate::commands::behaviors::run(&env).await.unwrap();
        assert!(
            outcome
                .response
                .as_deref()
                .is_some_and(|text| text.contains("not cached")),
            "{:?}",
            outcome.response
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "voice must not call Discord"
        );

        server.abort();
        drop(app);
        let _ = std::fs::remove_file(path);
    }
}
