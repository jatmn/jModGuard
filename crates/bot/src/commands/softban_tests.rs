use super::*;
use serenity::model::event::GuildCreateEvent;
use serenity::model::guild::{Guild, Member};
use serenity::model::id::{ApplicationId, GuildId, UserId};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn softban_deletes_recent_messages_then_unbans_and_reports_failures() {
    for (ban_status, unban_status) in [(204, 204), (403, 204), (204, 403)] {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
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
                let status = if request.starts_with("PUT ") {
                    ban_status
                } else {
                    unban_status
                };
                captured.lock().unwrap().push(request);
                let body = if status == 204 {
                    ""
                } else {
                    r#"{"code":50013,"message":"Missing Permissions"}"#
                };
                let response = format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
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
        let path = std::env::temp_dir().join(format!("softban_{:032x}.db", rand::random::<u128>()));
        let app = Arc::new(
            crate::state::BotState::new(Default::default(), db::Db::open(&path, 2).unwrap())
                .unwrap(),
        );
        app.activation.mark_lock_held(true);
        let enrollment = db::enroll_guild(&app.db, 10, 100).await.unwrap().unwrap();
        app.tenant.activate(enrollment).await.unwrap();
        // Load through the same overlay used by prefix and slash commands.
        let config = app.configs.get(&app.db, 10).await.unwrap();
        let command = config.commands.get("softban").expect("softban is shipped");
        assert_eq!(command.permission, "mod");
        let spec = slash_spec(&command.behavior);
        assert_eq!(spec.len(), 2);
        assert_eq!(spec[0].name, "user");
        assert_eq!(spec[0].kind, CommandOptionType::User);
        assert!(spec[0].required);
        assert_eq!(spec[1].name, "reason");
        assert!(!spec[1].required);
        let args = if unban_status == 403 { "91" } else { "91 spam" };
        let env = CmdEnv::new(
            &app,
            &ctx,
            command,
            GuildId::new(10),
            ChannelId::new(1),
            UserId::new(7),
            actor,
            Args::new(args),
            false,
            None,
        );
        let outcome = run(&env).await;
        server.abort();
        let _ = server.await;
        let requests = requests.lock().unwrap().clone();
        assert_eq!(requests.len(), if ban_status == 403 { 1 } else { 2 });
        assert!(
            requests[0].starts_with("PUT /api/v10/guilds/10/bans/91?")
                && requests[0].contains("delete_message_seconds=86400 "),
            "unexpected ban request {:?}",
            requests[0].lines().next()
        );
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("x-audit-log-reason:")
        );
        if ban_status == 204 {
            assert!(requests[1].starts_with("DELETE /api/v10/guilds/10/bans/91 "));
        }
        if ban_status == 204 && unban_status == 204 {
            let outcome = outcome.unwrap();
            assert_eq!(outcome.vars["target"], "target");
            assert_eq!(outcome.vars["reason"], "spam");
            assert_eq!(
                crate::templates::render(&command.response, &outcome.vars),
                "Softbanned target | spam"
            );
        } else {
            assert!(outcome.is_err(), "failed step must fail the command");
        }
        drop(config);
        drop(app);
        crate::tenant::remove_sidecars(&path);
        std::fs::remove_file(path).unwrap();
    }
}
