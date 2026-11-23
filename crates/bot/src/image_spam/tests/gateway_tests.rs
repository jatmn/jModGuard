use super::*;
use serenity::{
    client::EventHandler,
    model::id::{ChannelId, MessageId, UserId},
    model::timestamp::Timestamp,
};
use std::sync::atomic::{AtomicBool, Ordering};

struct DiscordStub {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    requests: Arc<Mutex<Vec<String>>>,
    reports: Arc<Mutex<Vec<serde_json::Value>>>,
    callbacks: Arc<Mutex<Vec<serde_json::Value>>>,
    ctx: Context,
}

impl DiscordStub {
    async fn new() -> Self {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let http = Arc::new(
            serenity::http::HttpBuilder::new("fixture")
                .proxy(format!("http://{addr}"))
                .ratelimiter_disabled(true)
                .build(),
        );
        let ctx = delete_context(http).await;
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let reports = Arc::new(Mutex::new(Vec::new()));
        let server_stop = Arc::clone(&stop);
        let server_requests = Arc::clone(&requests);
        let server_reports = Arc::clone(&reports);
        let callbacks = Arc::new(Mutex::new(Vec::new()));
        let server_callbacks = Arc::clone(&callbacks);
        let thread = std::thread::spawn(move || {
            crate::spam_actions::tests::serve_discord_stub(
                listener,
                server_requests,
                server_stop,
                move |method, path, body| {
                    if method == "POST" && path.ends_with("/channels/999/messages") {
                        server_reports
                            .lock()
                            .push(serde_json::from_str(body).unwrap());
                    }
                    if method == "POST"
                        && path.contains("/interactions/")
                        && path.contains("/callback")
                    {
                        server_callbacks
                            .lock()
                            .push(serde_json::from_str(body).unwrap());
                    }
                    None
                },
            );
        });
        Self {
            stop,
            thread: Some(thread),
            requests,
            reports,
            callbacks,
            ctx,
        }
    }
}

impl Drop for DiscordStub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

struct Fixture {
    handler: crate::handlers::Handler,
    discord: DiscordStub,
    path: std::path::PathBuf,
}

impl Fixture {
    async fn new(cfg: jmodguard::config::BotProcessConfig, rule: crate::types::RuleDef) -> Self {
        let path =
            std::env::temp_dir().join(format!("image_gateway_{:032x}.db", rand::random::<u128>()));
        let app = BotState::new(cfg, crate::db::Db::open(&path, 2).unwrap()).unwrap();
        assert!(
            crate::db::claim_active_bot(
                &app.db,
                app.activation.instance(),
                jmodguard::control::now_ms(),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap()
        );
        app.activation.mark_lock_held(true);
        app.activation.note_discord_connected();
        let enrollment = crate::db::enroll_guild(&app.db, 1, 1)
            .await
            .unwrap()
            .unwrap();
        app.tenant.activate(enrollment).await.unwrap();
        for (id, mut selected) in jmodguard::types::RULE_IDS
            .iter()
            .copied()
            .zip(crate::defaults::default_rules())
        {
            if id == "attachment_flood" {
                selected = rule.clone();
            } else {
                selected.enabled = false;
            }
            crate::db::upsert_rule(&app.db, 1, id, &selected)
                .await
                .unwrap();
        }
        crate::db::upsert_log_channel(&app.db, 1, "automod", 999)
            .await
            .unwrap();
        app.poll_config_version().await.unwrap();
        Self {
            handler: crate::handlers::Handler { app },
            discord: DiscordStub::new().await,
            path,
        }
    }

    async fn post(&self, author: u64, channel: u64, message: u64, age_secs: i64, image_count: u64) {
        let mut msg = Message::default();
        msg.id = MessageId::new(message);
        msg.channel_id = ChannelId::new(channel);
        msg.guild_id = Some(GuildId::new(1));
        msg.author.id = UserId::new(author);
        msg.author.name = "fixture".into();
        msg.timestamp =
            Timestamp::from_millis(chrono::Utc::now().timestamp_millis() - age_secs * 1000)
                .unwrap();
        msg.member = Some(Box::new(
            serde_json::from_value(serde_json::json!({
                "roles": ["7"], "deaf": false, "mute": false, "flags": 0,
            }))
            .unwrap(),
        ));
        for image in 0..image_count {
            let attachment = message + 1000 + 10000 * image;
            let filename = format!("image-{image}.jpg");
            msg.attachments.push(serde_json::from_value(serde_json::json!({
                "id": attachment.to_string(),
                "filename": filename,
                "size": 90536,
                "url": format!("https://cdn.discordapp.com/attachments/{channel}/{attachment}/{filename}"),
                "proxy_url": "",
                "width": 100,
                "height": 100,
                "content_type": "image/jpeg"
            })).unwrap());
        }
        self.handler.message(self.discord.ctx.clone(), msg).await;
    }

    async fn drain(&self) {
        tokio::time::timeout(Duration::from_secs(15), async {
            while self.handler.app.activation.in_flight() != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn burst(&self) {
        self.post(99, 101, 1010, 1, 1).await;
        self.post(99, 102, 1011, 0, 1).await;
        self.drain().await;
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        crate::tenant::remove_sidecars(&self.path);
    }
}

fn live_rule() -> crate::types::RuleDef {
    let mut rule = crate::defaults::default_rules().remove(1);
    rule.dry_run = false;
    rule.params["mode"] = "exact_image".into();
    rule.actions = serde_json::from_value(serde_json::json!([
        {"action": "delete"}, {"action": "ban", "purge_last_hour": true}
    ]))
    .unwrap();
    rule
}

#[tokio::test]
async fn gateway_two_channel_image_burst_bans_and_sweeps_previous_hour() {
    assert_ban_and_sweep(Default::default()).await;
}

async fn assert_ban_and_sweep(cfg: jmodguard::config::BotProcessConfig) {
    let fixture = Fixture::new(cfg, live_rule()).await;
    fixture.post(99, 103, 1001, 3500, 0).await;
    fixture.post(99, 104, 1002, 600, 0).await;
    fixture.post(99, 103, 1003, 3660, 0).await;
    fixture.post(98, 104, 1004, 600, 0).await;
    fixture.burst().await;
    let seen = fixture.discord.requests.lock().clone();
    assert!(
        !seen.iter().any(|line| line.starts_with("PATCH ")),
        "unexpected timeout: {seen:?}"
    );
    let bans: Vec<_> = seen
        .iter()
        .filter(|line| line.starts_with("PUT "))
        .collect();
    assert_eq!(bans.len(), 1, "{seen:?}");
    crate::spam_actions::tests::assert_ban_request(bans[0], 1, 99, 3600);
    let mut deletes: Vec<_> = seen
        .iter()
        .filter(|line| line.starts_with("DELETE "))
        .map(String::as_str)
        .collect();
    deletes.sort_unstable();
    assert_eq!(
        deletes,
        [
            "DELETE /api/v10/channels/101/messages/1010",
            "DELETE /api/v10/channels/102/messages/1011",
            "DELETE /api/v10/channels/103/messages/1001",
            "DELETE /api/v10/channels/104/messages/1002",
        ]
    );
    let reports = fixture.discord.reports.lock();
    assert_eq!(reports.len(), 1);
    let report = &reports[0];
    assert!(
        report["components"].as_array().is_none_or(Vec::is_empty),
        "{report}"
    );
    assert!(
        report["embeds"]
            .as_array()
            .is_some_and(|embeds| !embeds.is_empty()),
        "{report}"
    );
    let text = crate::logging::tests::log_text(report);
    assert!(
        text.contains("ban Succeeded") && text.contains("deleted 4 messages"),
        "{text}"
    );
}

#[tokio::test]
async fn gateway_fast_image_deletes_without_banning_when_moderator_status_is_unknown() {
    let mut rule = live_rule();
    rule.skip_moderators = true;
    let fixture = Fixture::new(Default::default(), rule).await;
    // The member lookup succeeds, but the Gateway cache has no guild roles.
    fixture.burst().await;
    assert_eq!(
        *fixture.discord.requests.lock(),
        [
            "GET /api/v10/guilds/1/members/99",
            "DELETE /api/v10/channels/101/messages/1010",
            "DELETE /api/v10/channels/102/messages/1011",
            "POST /api/v10/channels/999/messages",
        ]
    );
    let reports = fixture.discord.reports.lock();
    assert_eq!(reports.len(), 1);
    let text = crate::logging::tests::log_text(&reports[0]);
    assert!(
        text.contains("Matching image attachment metadata"),
        "{text}"
    );
    assert!(text.contains("moderator status unknown"), "{text}");
}

#[tokio::test]
async fn gateway_fast_image_dry_run_logs_without_mutations_or_components() {
    let mut rule = live_rule();
    rule.dry_run = true;
    rule.params["mode"] = "exact_image_test".into();
    rule.actions = serde_json::from_value(serde_json::json!([
        {"action": "log", "purge_last_hour": true}
    ]))
    .unwrap();
    let fixture = Fixture::new(Default::default(), rule).await;
    fixture.post(99, 103, 1001, 600, 0).await;
    fixture.burst().await;
    assert_eq!(
        *fixture.discord.requests.lock(),
        ["POST /api/v10/channels/999/messages"]
    );
    let reports = fixture.discord.reports.lock();
    assert_eq!(reports.len(), 1);
    let report = &reports[0];
    assert!(
        report["components"].as_array().is_none_or(Vec::is_empty),
        "{report}"
    );
    let text = crate::logging::tests::log_text(report);
    assert!(
        text.contains("Exact image spam test") && text.contains("log only"),
        "{text}"
    );
}

#[tokio::test]
async fn gateway_fast_image_respects_enabled_exceptions_and_channel_scope() {
    use serenity::all::{Guild, GuildCreateEvent, Permissions, Role, RoleId};
    for (label, configure) in [
        (
            "disabled",
            (|rule: &mut crate::types::RuleDef| rule.enabled = false) as fn(&mut _),
        ),
        ("skip role", |rule| rule.whitelist_roles = vec![7]),
        ("skip channel", |rule| rule.whitelist_channels = vec![101]),
        ("skip user", |rule| rule.whitelist_users = vec![99]),
        ("out of scope", |rule| rule.include_channels = vec![102]),
        ("skip moderator", |rule| rule.skip_moderators = true),
    ] {
        let mut rule = live_rule();
        configure(&mut rule);
        let fixture = Fixture::new(Default::default(), rule).await;
        let mut guild = Guild::default();
        guild.id = GuildId::new(1);
        let mut role = Role::default();
        role.id = RoleId::new(7);
        role.permissions = Permissions::MANAGE_MESSAGES;
        guild.roles.insert(role.id, role);
        let mut event: GuildCreateEvent =
            serde_json::from_value(serde_json::to_value(guild).unwrap()).unwrap();
        fixture.discord.ctx.cache.update(&mut event);
        assert_eq!(
            crate::commands::cached_moderator_status(
                &fixture.handler.app,
                &fixture.discord.ctx.cache,
                GuildId::new(1),
                99,
                &[7],
            ),
            crate::commands::ModeratorStatus::Moderator,
        );
        fixture.burst().await;
        let seen = fixture.discord.requests.lock();
        assert!(seen.is_empty(), "{label}: {seen:?}");
    }
}

#[tokio::test]
async fn gateway_fast_match_requires_one_author_two_channels_and_a_recent_pair() {
    let fixture = Fixture::new(Default::default(), live_rule()).await;
    fixture.post(99, 101, 1001, 12, 1).await;
    fixture.post(99, 102, 1002, 0, 1).await; // The only recent post by author 99.
    fixture.post(99, 102, 1003, 0, 1).await; // Repetition in the same channel.
    fixture.post(98, 103, 1004, 0, 1).await; // Another author.
    fixture.drain().await;
    let seen = fixture.discord.requests.lock();
    assert!(seen.is_empty(), "{seen:?}");
}

#[tokio::test]
async fn gateway_fast_match_respects_both_windows_for_two_image_posts() {
    for (window_secs, expired_age_secs) in [(60, 30), (5, 7)] {
        let mut rule = live_rule();
        rule.params["window_secs"] = window_secs.into();
        let fixture = Fixture::new(Default::default(), rule).await;

        // Two attachments bypass the single-image prefilter. Two channels
        // leave the byte matcher below its three-channel minimum.
        fixture.post(99, 101, 1001, expired_age_secs, 2).await;
        fixture.post(99, 102, 1002, 0, 2).await;
        fixture.drain().await;
        {
            let requests = fixture.discord.requests.lock();
            assert!(
                requests.is_empty(),
                "window {window_secs}s, first post {expired_age_secs}s old: {requests:?}",
            );
        }

        fixture.post(98, 101, 1003, 1, 2).await;
        fixture.post(98, 102, 1004, 0, 2).await;
        fixture.drain().await;
        let requests = fixture.discord.requests.lock();
        let bans: Vec<_> = requests
            .iter()
            .filter(|line| line.starts_with("PUT "))
            .collect();
        assert_eq!(bans.len(), 1, "window {window_secs}s: {requests:?}");
        crate::spam_actions::tests::assert_ban_request(bans[0], 1, 98, 3600);
    }
}

#[tokio::test]
async fn gateway_legacy_burst_config_is_ignored() {
    const TEST_NAME: &str =
        "image_spam::tests::gateway_tests::gateway_legacy_burst_config_is_ignored";
    const CHILD: &str = "JMODGUARD_TEST_LEGACY_BURST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let cfg = jmodguard::config::BotProcessConfig::load().unwrap();
        assert_ban_and_sweep(cfg.clone()).await;
        let mut rule = live_rule();
        rule.enabled = false;
        let fixture = Fixture::new(cfg, rule).await;
        fixture.burst().await;
        assert!(fixture.discord.requests.lock().is_empty());
        return;
    }
    let file =
        std::env::temp_dir().join(format!("legacy_burst_{:032x}.toml", rand::random::<u128>()));
    std::fs::write(
        &file,
        "[attachment_burst]\nenabled = false\nmin_channels = 1\ntimeout_secs = 600\n",
    )
    .unwrap();
    let results: Vec<_> = [
        "obsolete and invalid JSON",
        r#"{"enabled":true,"min_channels":2,"window_secs":10,"timeout_secs":600}"#,
    ]
    .into_iter()
    .map(|old_env| {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env_clear()
            .env("TMPDIR", std::env::temp_dir())
            .env(CHILD, "1")
            .env("JMODGUARD_CONFIG", &file)
            .env("JMODGUARD_ATTACHMENT_BURST", old_env)
            .output()
            .unwrap()
    })
    .collect();
    std::fs::remove_file(file).unwrap();
    for result in results {
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr),
        );
    }
}

#[tokio::test]
async fn gateway_legacy_review_buttons_get_ephemeral_retirement_notice() {
    use serenity::all::Interaction;
    let fixture = Fixture::new(Default::default(), live_rule()).await;
    for action in ["r", "b"] {
        let interaction: Interaction = serde_json::from_value(serde_json::json!({
            "id": "1", "application_id": "1", "type": 3,
            "data": {"custom_id": format!("jmgburst:0000000000000001:{action}:2"), "component_type": 2},
            "guild_id": "1", "channel_id": "999",
            "user": {"id": "7", "username": "moderator", "discriminator": "0001", "avatar": null},
            "token": "fixture", "version": 1, "locale": "en-US",
            "message": {
                "id": "501", "channel_id": "999", "type": 0,
                "author": {"id": "1", "username": "bot", "discriminator": "0001", "avatar": null},
                "content": "Old review", "timestamp": "2026-01-01T00:00:00Z",
                "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
                "attachments": [], "embeds": [], "pinned": false
            },
            "app_permissions": "8", "entitlements": [], "authorizing_integration_owners": {},
            "context": 0, "attachment_size_limit": 10000000
        })).unwrap();
        fixture
            .handler
            .interaction_create(fixture.discord.ctx.clone(), interaction)
            .await;
    }
    let requests = fixture.discord.requests.lock();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(
        requests
            .iter()
            .all(|request| request.starts_with("POST ") && request.contains("/callback"))
    );
    let callbacks = fixture.discord.callbacks.lock();
    assert_eq!(callbacks.len(), 2);
    for callback in callbacks.iter() {
        assert_eq!(callback["type"], 4);
        assert_eq!(callback["data"]["flags"], 64);
        assert!(
            callback["data"]["content"]
                .as_str()
                .unwrap()
                .contains("This review has expired.")
        );
    }
}
