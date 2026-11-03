//! Owner alerts for boot repair version mismatches recorded by the launchers.

use crate::{db, discord, state::BotState};
use serenity::{
    builder::{CreateAllowedMentions, CreateMessage},
    http::Http,
    model::id::ChannelId,
};
use std::collections::BTreeMap;

/// Called by the existing configuration poll with the bot's shared HTTP client.
/// Each mismatch state is consumed before its single best-effort post.
pub async fn send_pending(app: &BotState, http: &Http) {
    if !app.activation.may_record() {
        return;
    }
    let (channel, due) = match db::take_boot_mismatch_alerts(&app.db).await {
        Ok(pending) => pending,
        Err(error) => {
            tracing::warn!(%error, "could not read boot mismatch alerts");
            return;
        }
    };
    for record in due {
        let running = if record.running.is_empty() {
            "nothing (application stopped)".to_owned()
        } else {
            format!("built-in build {}", record.running)
        };
        let Some(channel) = channel else {
            tracing::warn!(
                component = %record.component,
                running = %running,
                expected = %record.expected.release_id,
                reason = %record.reason,
                "boot repair version mismatch; no alert channel is set, see the Administration page"
            );
            continue;
        };
        let vars = BTreeMap::from([
            ("component".into(), record.component.clone()),
            ("running".into(), running),
            ("release".into(), record.expected.release_id),
            ("source_sha".into(), record.expected.source_sha),
            ("reason".into(), record.reason),
        ]);
        let text = crate::templates::render_bounded(
            "Boot repair alert: the {component} is running {running}, but the last recorded release is {release} (source {source_sha}). Repair failed: {reason}. Restore that release by hand.",
            &vars,
            2000,
        );
        let delivery = ChannelId::new(channel).send_message(
            http,
            CreateMessage::new().content(text).allowed_mentions(
                CreateAllowedMentions::new()
                    .empty_users()
                    .empty_roles()
                    .everyone(false),
            ),
        );
        let component = record.component.as_str();
        match discord::race_discord_call(
            &app.activation,
            tokio::time::timeout(discord::DISCORD_REST_TIMEOUT, delivery),
        )
        .await
        {
            discord::DiscordCall::Finished(Ok(Ok(_))) => {}
            discord::DiscordCall::Finished(Ok(Err(error))) => {
                tracing::warn!(component, %error, "boot mismatch alert post failed");
            }
            discord::DiscordCall::Finished(Err(_)) => {
                tracing::warn!(component, "boot mismatch alert post timed out");
            }
            discord::DiscordCall::Cancelled => {
                tracing::warn!(component, "boot mismatch alert cancelled at writer cutoff");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::{
        Body, Request, Response, Server,
        service::{make_service_fn, service_fn},
    };
    use std::{
        convert::Infallible,
        sync::{Arc, Mutex},
    };

    #[derive(Clone, Default)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogCapture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    type Calls = Arc<Mutex<Vec<(String, serde_json::Value)>>>;

    fn discord_fixture() -> (Http, Calls, tokio::task::JoinHandle<()>) {
        let requests = Calls::default();
        let captured = Arc::clone(&requests);
        let server = Server::bind(&([127, 0, 0, 1], 0).into());
        let address = server.local_addr();
        let server = tokio::spawn(async move {
            let _ = server.serve(make_service_fn(move |_| {
                let captured = Arc::clone(&captured);
                async move {
                    Ok::<_, Infallible>(service_fn(move |request: Request<Body>| {
                        let captured = Arc::clone(&captured);
                        async move {
                            let path = request.uri().path().to_owned();
                            let body: serde_json::Value = serde_json::from_slice(
                                &hyper::body::to_bytes(request.into_body()).await.unwrap(),
                            ).unwrap();
                            captured.lock().unwrap().push((path, body));
                            let body = serde_json::json!({"id":"1","channel_id":"777","author":{"id":"7","username":"bot","discriminator":0},"content":"sent","timestamp":"2020-01-01T00:00:00+00:00","tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],"attachments":[],"embeds":[],"pinned":false,"type":0});
                            Ok::<_, Infallible>(Response::builder().status(200)
                                .header("content-type", "application/json")
                                .body(Body::from(body.to_string())).unwrap())
                        }
                    }))
                }
            })).await;
        });
        let http = serenity::http::HttpBuilder::new("fixture")
            .proxy(format!("http://{address}"))
            .ratelimiter_disabled(true)
            .build();
        (http, requests, server)
    }

    fn record(component: &str, running: &str, reason: &str) -> db::BootMismatchRecord {
        db::BootMismatchRecord {
            component: component.into(),
            running: running.into(),
            expected: db::BootMismatchExpected {
                release_id: format!("{component}-release"),
                source_sha: "c".repeat(40),
            },
            reason: reason.into(),
        }
    }

    async fn open(path: &std::path::Path) -> std::sync::Arc<BotState> {
        let app = BotState::new(
            crate::config::BotProcessConfig::default(),
            db::Db::open(path, 2).unwrap(),
        )
        .unwrap();
        app.activation.mark_lock_held(true);
        app
    }

    fn cleanup(path: &std::path::Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn alerts_each_mismatch_once_and_again_after_versions_match() {
        let (http, calls, server) = discord_fixture();
        let path = std::env::temp_dir().join(format!(
            "boot_mismatch_alert_{:032x}.db",
            rand::random::<u128>()
        ));
        let app = open(&path).await;
        db::set_update_alert_channel(&app.db, Some(777))
            .await
            .unwrap();
        send_pending(&app, &http).await;
        assert!(calls.lock().unwrap().is_empty(), "no mismatch, no alert");

        let bot = record("bot", &"b".repeat(40), "registry unreachable");
        let api = record("api", "", "bad image signature");
        db::record_boot_mismatch(&app.db, bot.clone())
            .await
            .unwrap();
        db::record_boot_mismatch(&app.db, api.clone())
            .await
            .unwrap();
        send_pending(&app, &http).await;
        {
            let calls = calls.lock().unwrap();
            assert_eq!(calls.len(), 2);
            for (path, body) in calls.iter() {
                assert!(path.ends_with("/channels/777/messages"), "{path}");
                assert_eq!(body["allowed_mentions"]["parse"], serde_json::json!([]));
            }
            let api_text = calls[0].1["content"].as_str().unwrap();
            assert!(api_text.contains("the api is running nothing (application stopped)"));
            assert!(api_text.contains("api-release"));
            assert!(api_text.contains(&"c".repeat(40)));
            assert!(api_text.contains("Repair failed: bad image signature"));
            let bot_text = calls[1].1["content"].as_str().unwrap();
            assert!(bot_text.contains(&format!(
                "the bot is running built-in build {}",
                "b".repeat(40)
            )));
            assert!(bot_text.contains("bot-release"));
            assert!(bot_text.contains("Repair failed: registry unreachable"));
        }

        // Launcher retries rewrite the same state; a restarted bot does not repeat it.
        db::record_boot_mismatch(&app.db, bot.clone())
            .await
            .unwrap();
        send_pending(&app, &http).await;
        drop(app);
        let app = open(&path).await;
        send_pending(&app, &http).await;
        assert_eq!(calls.lock().unwrap().len(), 2);

        // A different failure reason is a new mismatch state.
        let schema = record(
            "api",
            "",
            "registry unreachable; built-in application schema does not match the database",
        );
        db::record_boot_mismatch(&app.db, schema).await.unwrap();
        send_pending(&app, &http).await;
        {
            let calls = calls.lock().unwrap();
            assert_eq!(calls.len(), 3);
            assert!(calls[2].1["content"].as_str().unwrap().contains(
                "Repair failed: registry unreachable; built-in application schema does not match the database"
            ));
        }

        // Versions match again: the state clears, and a later recurrence alerts.
        db::clear_boot_mismatch(&app.db, "bot").await.unwrap();
        send_pending(&app, &http).await;
        assert_eq!(calls.lock().unwrap().len(), 3);
        db::record_boot_mismatch(&app.db, bot).await.unwrap();
        send_pending(&app, &http).await;
        assert_eq!(calls.lock().unwrap().len(), 4);

        server.abort();
        drop(app);
        cleanup(&path);
    }

    #[tokio::test]
    async fn without_a_channel_logs_once_then_posts_when_one_is_chosen() {
        let logs = LogCapture::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let _log_guard = tracing::subscriber::set_default(subscriber);
        let (http, calls, server) = discord_fixture();
        let path = std::env::temp_dir().join(format!(
            "boot_mismatch_unset_{:032x}.db",
            rand::random::<u128>()
        ));
        let app = open(&path).await;
        db::record_boot_mismatch(&app.db, record("api", "", "registry download failed"))
            .await
            .unwrap();
        send_pending(&app, &http).await;
        send_pending(&app, &http).await;
        let output = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert_eq!(
            output.matches("no alert channel is set").count(),
            1,
            "{output}"
        );
        assert!(output.contains("registry download failed"), "{output}");
        assert!(calls.lock().unwrap().is_empty());
        // The Administration page still reads the mismatch.
        assert!(
            db::read_boot_mismatch_map(&app.db)
                .await
                .unwrap()
                .contains_key("api")
        );

        db::set_update_alert_channel(&app.db, Some(777))
            .await
            .unwrap();
        send_pending(&app, &http).await;
        send_pending(&app, &http).await;
        assert_eq!(calls.lock().unwrap().len(), 1);

        server.abort();
        drop(app);
        cleanup(&path);
    }
}
