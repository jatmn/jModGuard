use super::*;
use crate::config::BotProcessConfig;
use crate::db::Db;
use crate::db::SqlExecutor;
use crate::state::BotState;
use std::sync::Mutex;

fn update(fields: serde_json::Value) -> MessageUpdateEvent {
    let mut value = serde_json::json!({"id":"123", "channel_id":"456", "guild_id":"789"});
    value
        .as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    serde_json::from_value(value).unwrap()
}

#[test]
fn basic_updates_use_only_supplied_fields_and_never_invent_history() {
    let report =
        message_update_report(&update(serde_json::json!({"content":"new text"})), None).unwrap();
    assert_eq!(report.vars["after"], "new text");
    assert_eq!(report.vars["before"], "not retained");
    assert_eq!(report.vars["mention"], "unavailable");
    assert_eq!(
        report.vars["message_url"],
        "https://discord.com/channels/789/456/123"
    );
    let attachment_only =
        message_update_report(&update(serde_json::json!({"attachments":[]})), None).unwrap();
    assert_eq!(attachment_only.vars["attachments"], "0");
    assert_eq!(attachment_only.vars["after"], "not supplied in this update");
    let emptied = message_update_report(&update(serde_json::json!({"content":""})), None).unwrap();
    assert_eq!(emptied.vars["after"], "");
    assert!(
        message_update_report(
            &update(serde_json::json!({"embeds":[],"pinned":true})),
            None
        )
        .is_none()
    );
    let mut dm = update(serde_json::json!({"content":"private"}));
    dm.guild_id = None;
    assert!(message_update_report(&dm, None).is_none());
}

#[test]
fn repeated_edit_observations_emit_once_but_changed_content_emits_again() {
    let dedup = EditReportDedup::default();
    let first = update(serde_json::json!({"content":"first"}));
    assert!(dedup.should_report(&first));
    assert!(!dedup.should_report(&first));
    assert!(!dedup.should_report(&first));
    let changed = update(serde_json::json!({"content":"second"}));
    assert!(dedup.should_report(&changed));
    assert!(!dedup.should_report(&changed));
    assert!(
        dedup.should_report(&first),
        "a later distinct observation restores the first fingerprint"
    );
    let mut other_channel = first.clone();
    other_channel.channel_id = ChannelId::new(999);
    assert!(dedup.should_report(&other_channel));
}

#[test]
fn partial_fields_for_one_edit_are_one_observation() {
    let dedup = EditReportDedup::default();
    let content = update(serde_json::json!({"content":"edited"}));
    let attachments = update(serde_json::json!({"attachments":[]}));
    let combined = update(serde_json::json!({"content":"edited","attachments":[]}));
    assert!(dedup.should_report(&content));
    assert!(!dedup.should_report(&attachments));
    assert!(!dedup.should_report(&combined));
    assert!(dedup.should_report(&update(serde_json::json!({"content":"edited again"}))));

    let reverse = EditReportDedup::default();
    assert!(reverse.should_report(&attachments));
    assert!(!reverse.should_report(&content));
    assert!(!reverse.should_report(&combined));
    assert!(reverse.should_report(&update(serde_json::json!({"content":"edited again"}))));
}

#[test]
fn distinct_edit_timestamps_and_attachment_descriptions_are_reported() {
    let dedup = EditReportDedup::default();
    let content = update(serde_json::json!({
        "content":"edited",
        "edited_timestamp":"2026-09-21T00:00:01.000Z"
    }));
    let same_edit = update(serde_json::json!({
        "attachments":[],
        "edited_timestamp":"2026-09-21T00:00:01.000Z"
    }));
    let next_edit = update(serde_json::json!({
        "attachments":[],
        "edited_timestamp":"2026-09-21T00:00:02.000Z"
    }));
    assert!(dedup.should_report(&content));
    assert!(!dedup.should_report(&same_edit));
    assert!(!dedup.should_report(&update(serde_json::json!({
        "content":"edited field supplied again",
        "edited_timestamp":"2026-09-21T00:00:01.000Z"
    }))));
    assert!(dedup.should_report(&next_edit));
    assert!(!dedup.should_report(&next_edit));
    assert!(
        !dedup.should_report(&content),
        "a replay of T1 after T2 is still the same reported edit"
    );
    assert!(dedup.should_report(&update(serde_json::json!({
        "content":"third edit",
        "edited_timestamp":"2026-09-21T00:00:03.000Z"
    }))));

    let two_fields = EditReportDedup::default();
    assert!(two_fields.should_report(&update(serde_json::json!({
        "content":"before", "attachments":[],
        "edited_timestamp":"2026-09-21T00:00:01.000Z"
    }))));
    assert!(two_fields.should_report(&update(serde_json::json!({
        "content":"after",
        "edited_timestamp":"2026-09-21T00:00:02.000Z"
    }))));
    assert!(!two_fields.should_report(&update(serde_json::json!({
        "attachments":[{"id":"555", "filename":"image.png", "description":"new",
            "proxy_url":"https://cdn.example/proxy", "url":"https://cdn.example/image",
            "size":100}],
        "edited_timestamp":"2026-09-21T00:00:02.000Z"
    }))));

    let attachment = |description: &str| {
        update(serde_json::json!({"attachments":[{
            "id":"555", "filename":"image.png", "description":description,
            "proxy_url":"https://cdn.example/proxy", "url":"https://cdn.example/image",
            "size":100
        }]}))
    };
    let metadata = EditReportDedup::default();
    assert!(metadata.should_report(&attachment("before")));
    assert!(!metadata.should_report(&attachment("before")));
    assert!(metadata.should_report(&attachment("after")));
}

#[test]
fn timestamp_arriving_after_a_reported_untimestamped_edit_does_not_report_twice() {
    let dedup = EditReportDedup::default();
    assert!(dedup.should_report(&update(serde_json::json!({
        "content":"before", "edited_timestamp":"2026-09-21T00:00:01.000Z"
    }))));
    assert!(dedup.should_report(&update(serde_json::json!({"content":"after"}))));
    assert!(!dedup.should_report(&update(serde_json::json!({
        "content":"after", "edited_timestamp":"2026-09-21T00:00:02.000Z"
    }))));
    assert!(!dedup.should_report(&update(serde_json::json!({
        "attachments":[], "edited_timestamp":"2026-09-21T00:00:02.000Z"
    }))));
    assert!(dedup.should_report(&update(serde_json::json!({
        "content":"third", "edited_timestamp":"2026-09-21T00:00:03.000Z"
    }))));

    let partial = EditReportDedup::default();
    let attachment = serde_json::json!({
        "id":"555", "filename":"image.png", "description":"new",
        "proxy_url":"https://cdn.example/proxy", "url":"https://cdn.example/image",
        "size":100
    });
    assert!(partial.should_report(&update(serde_json::json!({
        "content":"before", "attachments":[],
        "edited_timestamp":"2026-09-21T00:00:01.000Z"
    }))));
    assert!(partial.should_report(&update(serde_json::json!({"content":"after"}))));
    assert!(!partial.should_report(&update(serde_json::json!({
        "attachments":[attachment],
    }))));
    assert!(!partial.should_report(&update(serde_json::json!({
        "content":"after", "edited_timestamp":"2026-09-21T00:00:02.000Z"
    }))));
}

#[test]
fn supplied_author_id_reaches_message_edit_filter() {
    let mut event = update(serde_json::json!({"content":"edited"}));
    let mut author = serenity::model::user::User::default();
    author.id = serenity::model::id::UserId::new(42);
    author.name = "otherbot".into();
    event.author = Some(author);
    let report = message_update_report(&event, None).unwrap();
    assert_eq!(report.vars["user_id"], "42");
    assert!(excluded(
        &db::LogFilter {
            users: vec!["42".into()],
            channels: vec![]
        },
        &report.vars
    ));
    assert!(!excluded(
        &db::LogFilter {
            users: vec!["otherbot".into()],
            channels: vec![]
        },
        &report.vars
    ));
}

#[test]
fn authorless_edit_uses_an_observed_bot_id_for_exclusion_only() {
    let f = Fixture::new();
    remember_message(&f.app, 789, 456, 123, Some(42), "the other bot's report");
    let known = known_message_author(&f.app, 789, 456, 123);
    assert_eq!(known, Some(42));
    let event = update(serde_json::json!({"content":"edited report"}));
    let report = message_update_report(&event, known).unwrap();
    assert_eq!(report.vars["user_id"], "42");
    assert_eq!(report.vars["user"], "unavailable");
    assert_eq!(report.vars["mention"], "unavailable");
    assert_eq!(report.vars["after"], "edited report");
    assert!(excluded(
        &db::LogFilter {
            users: vec!["42".into()],
            channels: vec![],
        },
        &report.vars,
    ));
    assert_eq!(known_message_author(&f.app, 789, 456, 999), None);
    let cold = message_update_report(&event, None).unwrap();
    assert_eq!(cold.vars["user_id"], "unavailable");
}

#[test]
fn log_filter_matches_known_ids_and_source_channels_only() {
    let filter = db::LogFilter {
        users: vec!["123".into(), "999".into()],
        channels: vec!["456".into()],
    };
    assert!(excluded(
        &filter,
        &BTreeMap::from([("user_id".into(), "123".into())])
    ));
    assert!(excluded(
        &filter,
        &BTreeMap::from([("actor_id".into(), "999".into())])
    ));
    assert!(excluded(
        &filter,
        &BTreeMap::from([("target_id".into(), "123".into())])
    ));
    assert!(excluded(
        &filter,
        &BTreeMap::from([("channel_id".into(), "456".into())])
    ));
    assert!(excluded(
        &filter,
        &BTreeMap::from([
            ("source_channel_id".into(), "456".into()),
            ("channel".into(), "<#789>".into()),
        ])
    ));
    assert!(!excluded(
        &filter,
        &BTreeMap::from([
            ("source_channel_id".into(), "789".into()),
            ("channel".into(), "<#456>".into()),
        ])
    ));
    assert!(!excluded(
        &filter,
        &BTreeMap::from([
            ("user_id".into(), "unavailable".into()),
            ("username".into(), "123".into()),
            ("user".into(), "999".into()),
        ])
    ));
    // Render labels are not user identities, even if another user's ID is ignored.
    assert!(!excluded(
        &db::LogFilter {
            users: vec!["555".into()],
            channels: vec![]
        },
        &BTreeMap::from([
            ("user_id".into(), "999".into()),
            ("actor_id".into(), "999".into()),
            ("user".into(), "messages".into()),
        ])
    ));
}

#[test]
fn basic_delete_reports_ids_and_unavailable_content() {
    let path = std::env::temp_dir().join(format!("log_del_{:032x}.db", rand::random::<u128>()));
    let app = BotState::new(Default::default(), Db::open(&path, 2).unwrap()).unwrap();
    let report = message_delete_report(
        &app,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
    );
    assert_eq!(report.vars["message_id"], "123");
    assert_eq!(report.vars["mention"], "unavailable");
    assert_eq!(report.vars["content"], "not supplied to this process");
    let rendered = templates::render(default_template(report.event), &report.vars);
    assert!(rendered.contains("123"));
    assert!(rendered.contains("not supplied to this process"));
    assert!(!rendered.contains("{mention}"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn observed_delete_reports_can_include_a_recent_snippet() {
    let path = std::env::temp_dir().join(format!("log_snip_{:032x}.db", rand::random::<u128>()));
    let app = BotState::new(Default::default(), Db::open(&path, 2).unwrap()).unwrap();
    remember_message(&app, 789, 456, 123, Some(1), "hello there");
    let report = message_delete_report(
        &app,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
    );
    assert_eq!(report.vars["content"], "hello there");
    let missing = message_delete_report(
        &app,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
    );
    assert_eq!(missing.vars["content"], "not supplied to this process");
    let _ = std::fs::remove_file(path);
}

#[test]
fn partial_update_without_author_preserves_create_snippet_author() {
    let path = std::env::temp_dir().join(format!("log_snip_{:032x}.db", rand::random::<u128>()));
    let app = BotState::new(Default::default(), Db::open(&path, 2).unwrap()).unwrap();
    remember_message(&app, 789, 456, 123, Some(42), "hello there");
    remember_message(&app, 789, 456, 123, None, "edited text");
    let report = message_delete_report(
        &app,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
    );
    assert_eq!(report.vars["content"], "edited text");
    assert_eq!(report.vars["user_id"], "42");
    let _ = std::fs::remove_file(path);
}

#[test]
fn unattributed_update_does_not_create_a_snippet() {
    let path = std::env::temp_dir().join(format!("log_snip_{:032x}.db", rand::random::<u128>()));
    let app = BotState::new(Default::default(), Db::open(&path, 2).unwrap()).unwrap();
    remember_message(&app, 789, 456, 123, None, "orphan edit");
    let report = message_delete_report(
        &app,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
    );
    assert_eq!(report.vars["content"], "not supplied to this process");
    let _ = std::fs::remove_file(path);
}

#[test]
fn delivery_disables_mentions_and_embeds_and_bounds_unicode() {
    let payload = serde_json::to_value(log_message(&format!(
        "@everyone <@123> <@&456> {}",
        "🦀".repeat(3000)
    )))
    .unwrap();
    assert_eq!(payload["allowed_mentions"]["parse"], serde_json::json!([]));
    assert_eq!(payload["allowed_mentions"]["replied_user"], false);
    assert_eq!(
        payload["flags"].as_u64().unwrap() & MessageFlags::SUPPRESS_EMBEDS.bits(),
        MessageFlags::SUPPRESS_EMBEDS.bits()
    );
    let text = payload["content"].as_str().unwrap();
    assert!(text.encode_utf16().count() <= 2000);
    assert!(text.ends_with('…'));
}

struct Fixture {
    app: Arc<BotState>,
    path: std::path::PathBuf,
}
impl Fixture {
    fn new() -> Self {
        Self::with_timeout(1)
    }
    fn with_timeout(timeout_secs: u64) -> Self {
        let path =
            std::env::temp_dir().join(format!("jmodguard_logs_{:032x}.db", rand::random::<u128>()));
        let db = Db::open(&path, 2).unwrap();
        let mut cfg = BotProcessConfig::default();
        cfg.log.discord_concurrency = 1;
        cfg.log.discord_timeout_secs = timeout_secs;
        Self {
            app: BotState::new(cfg, db).unwrap(),
            path,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
        }
        crate::tenant::remove_sidecars(&self.path);
    }
}
#[derive(Default)]
struct Sink {
    messages: Mutex<Vec<(ChannelId, serde_json::Value)>>,
}

#[tokio::test]
async fn persisted_event_filter_blocks_matching_reports_only() {
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "message_edit", 456)
        .await
        .unwrap();
    db::upsert_log_channel(&f.app.db, 789, "message_delete", 456)
        .await
        .unwrap();
    f.app
        .db
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO log_filters(guild_id,event_type,data) VALUES(789,'message_edit',?1)",
                db::params![r#"{"users":["123"],"channels":["999"]}"#],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let sink = Sink::default();
    let guild = GuildId::new(789);
    for vars in [
        BTreeMap::from([("user_id".into(), "123".into())]),
        BTreeMap::from([("channel_id".into(), "999".into())]),
    ] {
        assert_eq!(
            emit_to(&f.app, &sink, guild, "message_edit", vars).await,
            Delivery::Unconfigured
        );
    }
    assert_eq!(
        emit_to(
            &f.app,
            &sink,
            guild,
            "message_edit",
            BTreeMap::from([("user_id".into(), "124".into())])
        )
        .await,
        Delivery::Sent
    );
    assert_eq!(
        emit_to(
            &f.app,
            &sink,
            guild,
            "message_delete",
            BTreeMap::from([("user_id".into(), "123".into())])
        )
        .await,
        Delivery::Sent
    );
    assert_eq!(sink.messages.lock().unwrap().len(), 2);
}
#[serenity::async_trait]
impl LogDelivery for Sink {
    async fn send(&self, channel: ChannelId, message: CreateMessage) -> anyhow::Result<()> {
        self.messages
            .lock()
            .unwrap()
            .push((channel, serde_json::to_value(message)?));
        Ok(())
    }
}

#[tokio::test]
async fn configured_routine_delivery_needs_no_history_tables_and_keeps_guild_routes_separate() {
    let f = Fixture::new();
    for (guild, channel) in [(789, 456), (790, 457)] {
        db::upsert_log_channel(&f.app.db, guild, "message_edit", channel)
            .await
            .unwrap();
    }
    // Removing these tables makes any accidental history access fail immediately.
    f.app
        .db
        .with_conn(|conn| {
            conn.execute_batch(
                "DROP TABLE IF EXISTS messages; DROP TABLE IF EXISTS members; DROP TABLE IF EXISTS users; DROP TABLE events;",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let sink = Sink::default();
    for guild in [789, 790] {
        let mut event = update(serde_json::json!({"content":"hello"}));
        event.guild_id = Some(GuildId::new(guild));
        let report = message_update_report(&event, None).unwrap();
        assert_eq!(
            emit_to(&f.app, &sink, report.guild_id, report.event, report.vars).await,
            Delivery::Sent
        );
    }
    // A warm valid config is sufficient during DB failure; do not query history.
    f.app
        .db
        .with_conn(|conn| {
            conn.execute_batch("DROP TABLE meta;")?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        emit_to(
            &f.app,
            &sink,
            GuildId::new(789),
            "message_edit",
            BTreeMap::new()
        )
        .await,
        Delivery::Sent
    );
    let sent = sink.messages.lock().unwrap();
    assert_eq!(
        sent.iter().map(|(ch, _)| ch.get()).collect::<Vec<_>>(),
        [456, 457, 456]
    );
}

#[tokio::test]
async fn disabled_and_overloaded_delivery_do_not_queue_reports() {
    let f = Fixture::new();
    let sink = Sink::default();
    assert_eq!(
        emit_to(
            &f.app,
            &sink,
            GuildId::new(789),
            "message_delete",
            BTreeMap::new()
        )
        .await,
        Delivery::Unconfigured
    );
    let permit = f.app.log_delivery_slots.acquire().await.unwrap();
    assert_eq!(
        emit_to(
            &f.app,
            &sink,
            GuildId::new(789),
            "message_delete",
            BTreeMap::new()
        )
        .await,
        Delivery::Dropped
    );
    drop(permit);
    assert!(sink.messages.lock().unwrap().is_empty());
    assert_eq!(
        f.app
            .log_delivery_failures
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}

struct StalledSink;
#[serenity::async_trait]
impl LogDelivery for StalledSink {
    async fn send(&self, _: ChannelId, _: CreateMessage) -> anyhow::Result<()> {
        std::future::pending().await
    }
}
#[tokio::test]
async fn slow_delivery_expires_and_releases_capacity_without_retry() {
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "message_delete", 456)
        .await
        .unwrap();
    assert_eq!(
        emit_to(
            &f.app,
            &StalledSink,
            GuildId::new(789),
            "message_delete",
            BTreeMap::new()
        )
        .await,
        Delivery::Failed
    );
    assert_eq!(f.app.log_delivery_slots.available_permits(), 1);
    let sink = Sink::default();
    assert_eq!(
        emit_to(
            &f.app,
            &sink,
            GuildId::new(789),
            "message_delete",
            BTreeMap::new()
        )
        .await,
        Delivery::Sent
    );
    assert_eq!(sink.messages.lock().unwrap().len(), 1);
}

#[test]
fn invalid_delivery_budgets_fail_before_building_runtime_state() {
    let f = Fixture::new();
    for (concurrency, timeout) in [(0, 1), (1, 0), (1, u64::MAX)] {
        let db = Db::open(&f.path, 2).unwrap();
        let mut cfg = BotProcessConfig::default();
        cfg.log.discord_concurrency = concurrency;
        cfg.log.discord_timeout_secs = timeout;
        assert!(BotState::new(cfg, db).is_err());
    }
}

fn audit_fixture(action: Action, target: u64, observed_ms: i64, reason: Option<&str>) -> AuditLogs {
    let id = (((observed_ms as u64 - 1_420_070_400_000) << 22) | action.num() as u64).to_string();
    serde_json::from_value(serde_json::json!({
        "audit_log_entries": [{"id":id,"target_id":target.to_string(),
            "user_id":"777", "action_type":action.num(), "reason":reason}],
        "users":[{"id":"777","username":"moderator","discriminator":"0","avatar":null}],
        "application_commands":[], "auto_moderation_rules":[], "guild_scheduled_events":[],
        "integrations":[], "threads":[], "webhooks":[]
    }))
    .unwrap()
}

#[test]
fn member_audit_requires_unique_recent_target_and_action() {
    let observed = 1_790_086_832_000;
    for event in ["member_ban", "member_unban"] {
        let action = member_audit_action(event).unwrap();
        let mut logs = audit_fixture(action, 123, observed, Some("actual reason"));
        let vars = unclaimed_audit_vars(&logs, action, 123, observed).unwrap();
        assert_eq!(vars["actor"], "moderator (<@777>)");
        assert_eq!(vars["actor_id"], "777");
        assert_eq!(vars["reason"], "actual reason");
        assert!(unclaimed_audit_vars(&logs, action, 999, observed).is_none());
        assert!(
            unclaimed_audit_vars(&logs, Action::Member(MemberAction::Kick), 123, observed)
                .is_none()
        );
        assert!(unclaimed_audit_vars(&logs, action, 123, observed + 10_001).is_none());
        assert!(unclaimed_audit_vars(&logs, action, 123, observed - 3_001).is_none());
        logs.entries.push(logs.entries[0].clone());
        assert!(unclaimed_audit_vars(&logs, action, 123, observed).is_none());
    }
}

#[test]
fn audit_without_reason_or_user_name_keeps_known_identity() {
    let observed = 1_790_086_832_000;
    let action = member_audit_action("member_ban").unwrap();
    let mut logs = audit_fixture(action, 123, observed, None);
    logs.users.clear();
    let vars = unclaimed_audit_vars(&logs, action, 123, observed).unwrap();
    assert_eq!(vars["actor"], "<@777>");
    assert_eq!(vars["reason"], "No reason provided");
}

struct AuditSink {
    sink: Sink,
    calls: std::sync::atomic::AtomicUsize,
    missing_first: bool,
    stalled: bool,
}
#[serenity::async_trait]
impl LogDelivery for AuditSink {
    async fn send(&self, channel: ChannelId, message: CreateMessage) -> anyhow::Result<()> {
        self.sink.send(channel, message).await
    }
    async fn member_audit(&self, guild: GuildId, action: Action) -> anyhow::Result<AuditLogs> {
        assert_eq!(guild.get(), 789);
        let call = self
            .calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if self.stalled {
            std::future::pending::<()>().await;
        }
        let mut logs = audit_fixture(
            action,
            123,
            chrono::Utc::now().timestamp_millis(),
            Some("spam"),
        );
        // Distinct calls in this fixture represent distinct mocked actions.
        // Repeated audit responses are modeled explicitly by RepeatedAuditSink.
        logs.entries[0].id = serenity::model::id::AuditLogEntryId::new(
            logs.entries[0].id.get() + ((call as u64 + 1) << 8),
        );
        if self.missing_first && call == 0 {
            logs.entries.clear();
        }
        Ok(logs)
    }
}

#[tokio::test]
async fn member_reports_render_audit_context_and_apply_actor_filters() {
    let f = Fixture::new();
    for event in ["member_ban", "member_unban"] {
        db::upsert_log_channel(&f.app.db, 789, event, 456)
            .await
            .unwrap();
    }
    let sink = AuditSink {
        sink: Sink::default(),
        calls: Default::default(),
        missing_first: false,
        stalled: false,
    };
    for event in ["member_ban", "member_unban"] {
        let vars = templates::user_vars(123, "username", "Display");
        assert_eq!(
            emit_to(&f.app, &sink, GuildId::new(789), event, vars).await,
            Delivery::Sent
        );
    }
    for (_, message) in sink.sink.messages.lock().unwrap().iter() {
        let text = message["content"].as_str().unwrap();
        assert!(text.contains("moderator (<@777>)"));
        assert!(text.contains("Reason: spam"));
        assert!(!text.contains("{actor}"));
    }
    f.app
        .db
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO log_filters(guild_id,event_type,data) VALUES(789,'member_ban',?1)",
                db::params![r#"{"users":["777"],"channels":[]}"#],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    f.app.invalidate_configs();
    assert_eq!(
        emit_to(
            &f.app,
            &sink,
            GuildId::new(789),
            "member_ban",
            templates::user_vars(123, "username", "Display")
        )
        .await,
        Delivery::Unconfigured
    );
    assert_eq!(sink.sink.messages.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn inaccessible_audit_renders_explicit_fallback_for_both_events() {
    let f = Fixture::new();
    let sink = Sink::default();
    for event in ["member_ban", "member_unban"] {
        db::upsert_log_channel(&f.app.db, 789, event, 456)
            .await
            .unwrap();
        f.app.invalidate_configs();
        assert_eq!(
            emit_to(
                &f.app,
                &sink,
                GuildId::new(789),
                event,
                templates::user_vars(123, "username", "Display")
            )
            .await,
            Delivery::Sent
        );
    }
    for (_, message) in sink.messages.lock().unwrap().iter() {
        let text = message["content"].as_str().unwrap();
        assert!(text.contains("Moderator: unavailable"));
        assert!(text.contains("Reason: unavailable"));
        assert!(!text.contains("{actor}"));
    }
}

#[tokio::test]
async fn stalled_audit_still_delivers_fallback_within_report_budget() {
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "member_ban", 456)
        .await
        .unwrap();
    let sink = AuditSink {
        sink: Sink::default(),
        calls: Default::default(),
        missing_first: false,
        stalled: true,
    };
    assert_eq!(
        emit_to(
            &f.app,
            &sink,
            GuildId::new(789),
            "member_ban",
            templates::user_vars(123, "username", "Display")
        )
        .await,
        Delivery::Sent
    );
    assert_eq!(sink.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(f.app.log_delivery_slots.available_permits(), 1);
}

#[test]
fn leave_uses_supplied_join_time_and_explicit_missing_context() {
    let observed = chrono::DateTime::from_timestamp(1_790_086_832, 0).unwrap();
    let joined =
        serenity::model::Timestamp::from_unix_timestamp(observed.timestamp() - 90_060).unwrap();
    let vars = member_join_vars(Some(joined), observed);
    assert!(vars["duration"].starts_with("1 days, 1 hours, 1 minutes"));
    assert!(vars["joined"].starts_with("<t:"));
    let text = templates::render(default_template("member_leave"), &vars);
    assert!(!text.contains("{duration}"));
    assert_eq!(member_join_vars(None, observed)["duration"], "unavailable");
    assert_eq!(member_join_vars(None, observed)["joined"], "unavailable");
}

#[test]
fn received_message_join_context_is_scoped_consumed_expiring_and_bounded() {
    let cache = MemberJoinContext::default();
    let joined = serenity::model::Timestamp::from_unix_timestamp(1_760_000_000).unwrap();
    cache.remember(1, 2, Some(joined));
    cache.remember(1, 2, None);
    assert_eq!(cache.take(2, 2), None);
    assert_eq!(cache.take(1, 2), Some(joined));
    assert_eq!(cache.take(1, 2), None);
    cache.remember(1, 2, Some(joined));
    cache.inner.lock().get_mut(&(1, 2)).unwrap().1 = Instant::now() - MEMBER_JOINS_KEEP;
    assert_eq!(cache.take(1, 2), None);
    for user in 0..=MEMBER_JOINS_LIMIT {
        cache.remember(1, user as u64, Some(joined));
    }
    assert_eq!(cache.inner.lock().len(), MEMBER_JOINS_LIMIT);
    cache.clear_guild(1);
    assert!(cache.inner.lock().is_empty());
}

#[tokio::test]
async fn late_audit_is_retried_but_disabled_reports_do_not_lookup() {
    let f = Fixture::with_timeout(4);
    let sink = AuditSink {
        sink: Sink::default(),
        calls: Default::default(),
        missing_first: true,
        stalled: false,
    };
    let vars = templates::user_vars(123, "username", "Display");
    assert_eq!(
        emit_to(&f.app, &sink, GuildId::new(789), "member_ban", vars.clone()).await,
        Delivery::Unconfigured
    );
    assert_eq!(sink.calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    db::upsert_log_channel(&f.app.db, 789, "member_ban", 456)
        .await
        .unwrap();
    f.app.invalidate_configs();
    assert_eq!(
        emit_to(&f.app, &sink, GuildId::new(789), "member_ban", vars).await,
        Delivery::Sent
    );
    assert_eq!(sink.calls.load(std::sync::atomic::Ordering::Relaxed), 2);
    assert!(
        sink.sink.messages.lock().unwrap()[0].1["content"]
            .as_str()
            .unwrap()
            .contains("Reason: spam")
    );
}

fn unclaimed_audit_vars(
    logs: &AuditLogs,
    action: Action,
    target: u64,
    observed_ms: i64,
) -> Option<BTreeMap<String, String>> {
    super::member_audit_vars(logs, action, target, observed_ms, |_| false).map(|(_, vars)| vars)
}

struct RepeatedAuditSink {
    sink: Sink,
    responses: Mutex<std::collections::VecDeque<AuditLogs>>,
    fail_first_send: bool,
    sends: std::sync::atomic::AtomicUsize,
}
#[serenity::async_trait]
impl LogDelivery for RepeatedAuditSink {
    async fn send(&self, channel: ChannelId, message: CreateMessage) -> anyhow::Result<()> {
        if self
            .sends
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            == 0
            && self.fail_first_send
        {
            anyhow::bail!("fixture delivery failed");
        }
        self.sink.send(channel, message).await
    }
    async fn member_audit(&self, _: GuildId, _: Action) -> anyhow::Result<AuditLogs> {
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("no response"))
    }
}

#[tokio::test]
async fn repeated_actions_never_reuse_audit_after_sent_filtered_or_failed_report() {
    for event in ["member_ban", "member_unban"] {
        for previous in [Delivery::Sent, Delivery::Unconfigured, Delivery::Failed] {
            let f = Fixture::with_timeout(4);
            db::upsert_log_channel(&f.app.db, 789, event, 456)
                .await
                .unwrap();
            if previous == Delivery::Unconfigured {
                f.app
                    .db
                    .with_conn(move |conn| {
                        conn.execute(
                            "INSERT INTO log_filters(guild_id,event_type,data) VALUES(789,?1,?2)",
                            db::params![event, r#"{"users":["777"],"channels":[]}"#],
                        )?;
                        Ok(())
                    })
                    .await
                    .unwrap();
            }
            let action = member_audit_action(event).unwrap();
            let now = chrono::Utc::now().timestamp_millis();
            let old = audit_fixture(action, 123, now - 4_000, Some("previous action"));
            let old_again = audit_fixture(action, 123, now - 4_000, Some("previous action"));
            let mut current = audit_fixture(action, 123, now, Some("current action"));
            current.entries[0].user_id = serenity::model::id::UserId::new(888);
            current.entries.push(old.entries[0].clone());
            let sink = RepeatedAuditSink {
                sink: Sink::default(),
                responses: Mutex::new([old, old_again, current].into()),
                fail_first_send: previous == Delivery::Failed,
                sends: Default::default(),
            };
            let vars = templates::user_vars(123, "username", "Display");
            assert_eq!(
                emit_to(&f.app, &sink, GuildId::new(789), event, vars.clone()).await,
                previous
            );
            assert_eq!(
                emit_to(&f.app, &sink, GuildId::new(789), event, vars).await,
                Delivery::Sent
            );
            let messages = sink.sink.messages.lock().unwrap();
            let last = messages.last().unwrap().1["content"].as_str().unwrap();
            assert!(last.contains("Moderator: <@888>"), "{last}");
            assert!(last.contains("Reason: current action"), "{last}");
            assert!(!last.contains("previous action"));
            assert!(sink.responses.lock().unwrap().is_empty());
        }
    }
}

#[test]
fn audit_claims_are_scoped_atomic_and_bounded_without_live_eviction() {
    let claims = MemberAuditClaims::default();
    let action = member_audit_action("member_ban").unwrap();
    let observed = chrono::Utc::now().timestamp_millis();
    let logs = audit_fixture(action, 123, observed, None);
    assert!(claims.claim(1, &logs, action, 123, observed).is_some());
    assert!(claims.claim(1, &logs, action, 123, observed).is_none());
    assert!(claims.claim(2, &logs, action, 123, observed).is_some());
    {
        let mut entries = claims.inner.lock();
        for id in 0..MEMBER_AUDIT_LIMIT as u64 - 2 {
            entries.insert((3, id), Instant::now());
        }
        assert_eq!(entries.len(), MEMBER_AUDIT_LIMIT);
    }
    assert!(claims.claim(4, &logs, action, 123, observed).is_none());
    assert!(claims.claim(1, &logs, action, 123, observed).is_none());
    for at in claims.inner.lock().values_mut() {
        *at = Instant::now() - MEMBER_AUDIT_KEEP;
    }
    assert!(claims.claim(4, &logs, action, 123, observed).is_some());
    assert_eq!(claims.inner.lock().len(), 1);
}
