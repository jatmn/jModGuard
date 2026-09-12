use super::*;
use crate::config::BotProcessConfig;
use crate::db::Db;
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
    let report = message_update_report(&update(serde_json::json!({"content":"new text"}))).unwrap();
    assert_eq!(report.vars["after"], "new text");
    assert_eq!(report.vars["before"], "not retained");
    assert_eq!(report.vars["mention"], "unavailable");
    assert_eq!(
        report.vars["message_url"],
        "https://discord.com/channels/789/456/123"
    );
    let attachment_only =
        message_update_report(&update(serde_json::json!({"attachments":[]}))).unwrap();
    assert_eq!(attachment_only.vars["attachments"], "0");
    assert_eq!(attachment_only.vars["after"], "not supplied in this update");
    let emptied = message_update_report(&update(serde_json::json!({"content":""}))).unwrap();
    assert_eq!(emptied.vars["after"], "");
    assert!(
        message_update_report(&update(serde_json::json!({"embeds":[],"pinned":true}))).is_none()
    );
    let mut dm = update(serde_json::json!({"content":"private"}));
    dm.guild_id = None;
    assert!(message_update_report(&dm).is_none());
}

#[test]
fn basic_delete_reports_ids_and_unavailable_content() {
    let report = message_delete_report(GuildId::new(789), ChannelId::new(456), MessageId::new(123));
    assert_eq!(report.vars["message_id"], "123");
    assert_eq!(report.vars["mention"], "unavailable");
    assert_eq!(report.vars["content"], "not retained");
    let rendered = templates::render(default_template(report.event), &report.vars);
    assert!(rendered.contains("123"));
    assert!(rendered.contains("not retained"));
    assert!(!rendered.contains("{mention}"));
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
        let path =
            std::env::temp_dir().join(format!("jmodguard_logs_{:032x}.db", rand::random::<u128>()));
        let db = Db::open(&path, 2).unwrap();
        let mut cfg = BotProcessConfig::default();
        cfg.log.discord_concurrency = 1;
        cfg.log.discord_timeout_secs = 1;
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
        let report = message_update_report(&event).unwrap();
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
