use super::*;
use serenity::model::event::GuildCreateEvent;
use serenity::model::guild::{Guild, Member};
use serenity::model::id::{ApplicationId, GuildId, UserId};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Reads the active ban deadline the moment the moderation log is delivered,
/// so a process that stops during log delivery has already saved it.
#[tokio::test]
async fn timed_ban_saves_its_deadline_before_moderation_log_delivery() {
    // Discord reports no ban (the bot bans), or a ban the mirror lacks.
    for discord_has_ban in [false, true] {
        let path =
            std::env::temp_dir().join(format!("timed_ban_log_{:032x}.db", rand::random::<u128>()));
        let database = db::Db::open(&path, 2).unwrap();
        let seen_at_log = Arc::new(Mutex::new(Vec::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = {
            let (database, seen_at_log, requests) = (
                database.clone(),
                Arc::clone(&seen_at_log),
                Arc::clone(&requests),
            );
            tokio::spawn(async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    let mut request = Vec::new();
                    loop {
                        let mut buf = [0u8; 4096];
                        let n = stream.read(&mut buf).await.unwrap();
                        assert_ne!(n, 0);
                        request.extend_from_slice(&buf[..n]);
                        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&request[..end]);
                            let length = headers
                                .lines()
                                .filter_map(|line| line.split_once(':'))
                                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                                .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                                .unwrap_or(0);
                            if request.len() >= end + 4 + length {
                                break;
                            }
                        }
                    }
                    let request = String::from_utf8(request).unwrap();
                    let line = request.lines().next().unwrap_or_default().to_owned();
                    let (status, body) = if line.starts_with("GET /api/v10/guilds/10/bans/91 ") {
                        if discord_has_ban {
                            (
                                200,
                                r#"{"reason":"spam","user":{"id":"91","username":"target","discriminator":"0","avatar":null}}"#,
                            )
                        } else {
                            (404, r#"{"code":10026,"message":"Unknown Ban"}"#)
                        }
                    } else if line.starts_with("PUT /api/v10/guilds/10/bans/91") {
                        (204, "")
                    } else if line.starts_with("POST /api/v10/channels/555/messages ") {
                        let expires_at = database
                            .with_conn(|conn| {
                                use jmodguard::db::SqlExecutor as _;
                                conn.query_row(
                                    "SELECT (SELECT expires_at FROM bans
                                     WHERE guild_id=10 AND user_id=91 AND active=1)",
                                    (),
                                    |row| Ok(row.get::<Option<i64>>(0)?),
                                )
                            })
                            .await
                            .unwrap();
                        seen_at_log.lock().unwrap().push(expires_at);
                        (403, r#"{"code":50013,"message":"Missing Permissions"}"#)
                    } else {
                        (404, r#"{"code":0,"message":"Not Found"}"#)
                    };
                    requests.lock().unwrap().push(line);
                    let response = format!(
                        "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                }
            })
        };
        let http = Arc::new(
            serenity::http::HttpBuilder::new("fixture")
                .proxy(format!("http://{address}"))
                .application_id(ApplicationId::new(1))
                .ratelimiter_disabled(true)
                .build(),
        );
        let cache = Arc::new(serenity::cache::Cache::new());
        let mut guild = Guild::default();
        guild.id = GuildId::new(10);
        for (id, name) in [(7, "moderator"), (91, "target")] {
            let mut member = Member::default();
            member.guild_id = guild.id;
            member.user.id = UserId::new(id);
            member.user.name = name.into();
            guild.members.insert(member.user.id, member);
        }
        let actor = guild.members[&UserId::new(7)].clone();
        let mut event: GuildCreateEvent =
            serde_json::from_value(serde_json::to_value(guild).unwrap()).unwrap();
        cache.update(&mut event);
        let ctx = mod_reason_dm::fixture_context_for_dm_order_test(http, cache).await;
        let app = Arc::new(crate::state::BotState::new(Default::default(), database).unwrap());
        app.activation.mark_lock_held(true);
        let enrollment = db::enroll_guild(&app.db, 10, 100).await.unwrap().unwrap();
        app.tenant.activate(enrollment).await.unwrap();
        db::upsert_log_channel(&app.db, 10, "mod_action", 555)
            .await
            .unwrap();
        let config = app.configs.get(&app.db, 10).await.unwrap();
        let command = config.commands.get("ban").expect("ban is shipped");
        let env = CmdEnv::new(
            &app,
            &ctx,
            command,
            GuildId::new(10),
            ChannelId::new(1),
            UserId::new(7),
            actor,
            Args::new("91 --minutes 60 spam"),
            false,
            None,
        );
        let before = chrono::Utc::now().timestamp();
        let outcome = run(&env).await;
        let after = chrono::Utc::now().timestamp();
        server.abort();
        let _ = server.await;
        let requests = requests.lock().unwrap().clone();
        outcome.unwrap_or_else(|error| panic!("ban failed: {error:#}; requests {requests:?}"));
        assert_eq!(
            requests
                .iter()
                .filter(|line| line.starts_with("PUT "))
                .count(),
            usize::from(!discord_has_ban),
            "{requests:?}"
        );
        let seen = seen_at_log.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "one moderation log delivery: {requests:?}");
        let deadline = seen[0].expect("deadline saved before log delivery");
        assert!(
            (before + 3600..=after + 3600).contains(&deadline),
            "deadline {deadline} outside [{before}, {after}] + 1h"
        );
        drop(env);
        drop(config);
        drop(app);
        crate::tenant::remove_sidecars(&path);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
}
