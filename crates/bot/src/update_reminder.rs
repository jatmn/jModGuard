//! Best-effort container-image reminders for completed application updates.

use crate::{db, discord, state::BotState};
use serenity::{builder::CreateMessage, http::Http, model::id::UserId};
use std::collections::BTreeMap;

/// Called by the existing configuration poll with the bot's shared HTTP client.
pub async fn send_pending(app: &BotState, http: &Http) {
    for component in ["bot", "api"] {
        if !app.activation.may_record() {
            return;
        }
        let release = match db::take_container_image_reminder(&app.db, component).await {
            Ok(Some(release)) => release,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(component, %error, "could not read container image reminder");
                continue;
            }
        };
        if app.cfg.bot.owner_id == 0 {
            tracing::warn!(
                component,
                "container image reminder DM skipped; admin account is unset"
            );
            continue;
        }
        let vars = BTreeMap::from([
            ("component".into(), component.into()),
            ("release".into(), release.release_id),
            ("source_sha".into(), release.source_sha),
        ]);
        let text = crate::templates::render_bounded(
            "The {component} in-app self-update to {release} (source {source_sha}) completed. Please cut a matching container image so a container replacement can start that version directly.",
            &vars,
            2000,
        );
        let admin = UserId::new(app.cfg.bot.owner_id);
        let delivery = admin.direct_message(
            http,
            CreateMessage::new().content(text).allowed_mentions(
                serenity::builder::CreateAllowedMentions::new()
                    .empty_users()
                    .empty_roles()
                    .everyone(false),
            ),
        );
        match discord::race_discord_call(
            &app.activation,
            tokio::time::timeout(discord::DISCORD_REST_TIMEOUT, delivery),
        )
        .await
        {
            discord::DiscordCall::Finished(Ok(Ok(_))) => {}
            discord::DiscordCall::Finished(Ok(Err(error))) => {
                tracing::warn!(component, %error, "container image reminder DM failed");
            }
            discord::DiscordCall::Finished(Err(_)) => {
                tracing::warn!(component, "container image reminder DM timed out");
            }
            discord::DiscordCall::Cancelled => {
                tracing::warn!(
                    component,
                    "container image reminder DM cancelled at writer cutoff"
                );
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

    async fn check_delivery(component: &str, update_succeeds: bool, dm_fails: bool) {
        let logs = LogCapture::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let _log_guard = tracing::subscriber::set_default(subscriber);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let server = Server::bind(&([127, 0, 0, 1], 0).into());
        let address = server.local_addr();
        let server = tokio::spawn(server.serve(make_service_fn(move |_| {
            let captured = Arc::clone(&captured);
            async move {
                Ok::<_, Infallible>(service_fn(move |request: Request<Body>| {
                    let captured = Arc::clone(&captured);
                    async move {
                        let path = request.uri().path().to_owned();
                        let body: serde_json::Value = serde_json::from_slice(
                            &hyper::body::to_bytes(request.into_body()).await.unwrap(),
                        ).unwrap();
                        captured.lock().unwrap().push((path.clone(), body));
                        let (status, body) = if path.ends_with("/users/@me/channels") {
                            (200, serde_json::json!({"id":"555", "type":1, "recipients":[{"id":"91","username":"admin","discriminator":0}]}))
                        } else if dm_fails {
                            (403, serde_json::json!({"code":50007,"message":"Cannot send messages to this user"}))
                        } else {
                            (200, serde_json::json!({"id":"1","channel_id":"555","author":{"id":"7","username":"bot","discriminator":0},"content":"sent","timestamp":"2020-01-01T00:00:00+00:00","tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],"attachments":[],"embeds":[],"pinned":false,"type":0}))
                        };
                        Ok::<_, Infallible>(Response::builder().status(status)
                            .header("content-type", "application/json")
                            .body(Body::from(body.to_string())).unwrap())
                    }
                }))
            }
        })));
        let http = serenity::http::HttpBuilder::new("fixture")
            .proxy(format!("http://{address}"))
            .ratelimiter_disabled(true)
            .build();
        let path = std::env::temp_dir().join(format!(
            "update_reminder_{:032x}.db",
            rand::random::<u128>()
        ));
        let database = db::Db::open(&path, 2).unwrap();
        let mut cfg = crate::config::BotProcessConfig::default();
        cfg.bot.owner_id = 91;
        let app = BotState::new(cfg.clone(), database).unwrap();
        app.activation.mark_lock_held(true);
        let release: db::ComponentRelease = serde_json::from_value(serde_json::json!({
            "component":component,"channel":"latest","release_id":"release-fixture",
            "source_sha":"a".repeat(40),"published_at":"2026-09-23T00:00:00Z",
            "image":"fixture","digest":format!("sha256:{}", "a".repeat(64)),
            "min_schema":15,"max_schema":15,"result_schema":15,"min_api":1,"max_api":1,
            "launcher_protocol":1,"base_abi":"debian-bookworm-amd64"
        }))
        .unwrap();
        let observation: db::ComponentObservation = serde_json::from_value(serde_json::json!({
            "source_sha":release.source_sha,"executable_sha256":"e".repeat(64),
            "base_image_sha":"b".repeat(40),"started_at":100,"min_schema":15,"max_schema":15,
            "api_capability":1,"self_update_enabled":true,"base_abi":"debian-bookworm-amd64"
        }))
        .unwrap();
        db::record_update_check(
            &app.db,
            component,
            "latest",
            Some(release.clone()),
            None,
            100,
        )
        .await
        .unwrap();
        db::request_component_install(&app.db, component, &release.digest, 100)
            .await
            .unwrap();
        send_pending(&app, &http).await;
        assert!(
            requests.lock().unwrap().is_empty(),
            "queued is not completed"
        );
        if update_succeeds {
            db::record_component_running(
                &app.db,
                component,
                Some(release.clone()),
                observation.clone(),
                101,
            )
            .await
            .unwrap();
        } else {
            db::record_component_failure(&app.db, component, &release, "startup failed")
                .await
                .unwrap();
            assert!(
                db::record_component_running(
                    &app.db,
                    component,
                    Some(release.clone()),
                    observation.clone(),
                    101
                )
                .await
                .is_err()
            );
        }
        drop(app);
        let app = BotState::new(cfg, db::Db::open(&path, 2).unwrap()).unwrap();
        app.activation.mark_lock_held(true);
        send_pending(&app, &http).await;
        if dm_fails {
            let output = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
            assert!(output.contains("container image reminder DM failed"));
        }
        if update_succeeds {
            let calls = requests.lock().unwrap();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0].1["recipient_id"], "91");
            assert!(calls[1].0.ends_with("/channels/555/messages"));
            let message = calls[1].1["content"].as_str().unwrap();
            assert!(message.contains(component));
            assert!(message.contains("release-fixture"));
            assert!(message.contains(&release.source_sha));
            assert!(message.contains("cut a matching container image"));
            assert_eq!(
                calls[1].1["allowed_mentions"]["parse"],
                serde_json::json!([])
            );
        } else {
            assert!(requests.lock().unwrap().is_empty());
        }
        let state = db::component_update(&app.db, component).await.unwrap();
        if update_succeeds {
            assert_eq!(state.state, "running");
            assert!(state.last_error.is_none());
            assert_eq!(state.installed, Some(release.clone()));
            db::record_component_running(&app.db, component, Some(release), observation, 102)
                .await
                .unwrap();
        } else {
            assert_eq!(state.state, "failed");
            assert!(state.installed.is_none());
        }
        send_pending(&app, &http).await;
        assert_eq!(
            requests.lock().unwrap().len(),
            if update_succeeds { 2 } else { 0 }
        );
        server.abort();
        let _ = server.await;
        drop(app);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn successful_self_update_dms_registered_admin_once() {
        for component in ["bot", "api"] {
            check_delivery(component, true, false).await;
        }
    }

    #[tokio::test]
    async fn failed_self_update_does_not_dm_admin() {
        check_delivery("bot", false, false).await;
    }

    #[tokio::test]
    async fn dm_failure_keeps_self_update_successful() {
        check_delivery("bot", true, true).await;
    }
}
