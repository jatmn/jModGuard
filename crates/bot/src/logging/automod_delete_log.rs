use super::*;
use crate::config::BotProcessConfig;
use crate::db::Db;
use crate::state::BotState;
use serenity::model::id::{ChannelId, GuildId, MessageId};
use std::sync::Arc;

struct Fixture {
    app: Arc<BotState>,
    path: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "automod_delete_log_{:032x}",
            rand::random::<u128>()
        ));
        let db = Db::open(&path, 2).unwrap();
        let mut cfg = BotProcessConfig::default();
        cfg.log.discord_concurrency = 4;
        let app = BotState::new(cfg, db).unwrap();
        app.activation.mark_lock_held(true);
        Self { app, path }
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
    messages: std::sync::Mutex<Vec<(ChannelId, serde_json::Value)>>,
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

#[test]
fn automod_delete_template_fits_discord_limit() {
    let template = crate::defaults::activity_log_templates()
        .into_iter()
        .find(|(event, _)| *event == "automod_delete")
        .map(|(_, body)| body)
        .expect("automod_delete template");
    let mut vars = BTreeMap::from([
        ("channel".into(), "<#456>".into()),
        (
            "message_url".into(),
            "https://discord.com/channels/789/456/123".into(),
        ),
        ("mention".into(), "<@42>".into()),
        ("rule".into(), "Bad words".into()),
        ("rule_id".into(), "badword".into()),
        ("content".into(), "x".repeat(900)),
        ("attachments".into(), "a".repeat(400)),
        ("time".into(), chrono::Utc::now().to_rfc3339()),
    ]);
    let rendered = render_log_template("automod_delete", template, &mut vars);
    assert!(rendered.encode_utf16().count() <= 2000);
    assert!(rendered.contains("Bad words"));
}

#[test]
fn automod_delete_is_activity_log_not_store_event() {
    assert!(!crate::types::EVENT_TYPES.contains(&"automod_delete"));
    assert!(crate::types::is_log_event("automod_delete"));
}

#[tokio::test]
async fn core_log_channel_accepts_automod_delete() {
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "automod_delete", 456)
        .await
        .unwrap();
}

#[tokio::test]
async fn confirmed_mark_emits_automod_delete_not_plain_delete() {
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "message_delete", 456)
        .await
        .unwrap();
    f.app
        .automod_delete_marks
        .insert(789, 456, 123, "badword", "Bad words");
    f.app.automod_delete_marks.confirm(789, 456, 123);
    let report = observed_delete_report(
        &f.app,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
        None,
    );
    assert_eq!(report.event, "automod_delete");
    assert_eq!(report.vars["rule"], "Bad words");
    let sink = Sink::default();
    assert_eq!(
        emit_to(&f.app, &sink, report.guild_id, report.event, report.vars).await,
        Delivery::Sent
    );
    let body = sink.messages.lock().unwrap()[0].1.to_string();
    assert!(body.contains("Bad words"));
    let plain = observed_delete_report(
        &f.app,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
        None,
    );
    assert_eq!(plain.event, "message_delete");
}

#[tokio::test]
async fn automod_delete_channel_falls_back_to_message_delete() {
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "message_delete", 456)
        .await
        .unwrap();
    f.app
        .automod_delete_marks
        .insert(789, 456, 123, "invite", "Invites");
    f.app.automod_delete_marks.confirm(789, 456, 123);
    let report = observed_delete_report(
        &f.app,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
        None,
    );
    let sink = Sink::default();
    emit_to(&f.app, &sink, report.guild_id, report.event, report.vars).await;
    assert_eq!(sink.messages.lock().unwrap()[0].0, ChannelId::new(456));
}

#[tokio::test]
async fn unrelated_delete_stays_plain() {
    let f = Fixture::new();
    let report = observed_delete_report(
        &f.app,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(999),
        None,
    );
    assert_eq!(report.event, "message_delete");
}

#[tokio::test]
async fn restart_substitute_has_no_mark() {
    let f = Fixture::new();
    assert!(!f.app.automod_delete_marks.inner.lock().contains(1, 2, 3));
    let report = observed_delete_report(
        &f.app,
        GuildId::new(1),
        ChannelId::new(2),
        MessageId::new(3),
        None,
    );
    assert_eq!(report.event, "message_delete");
}

#[tokio::test]
async fn double_emit_path_consumes_mark_once() {
    let f = Fixture::new();
    f.app
        .automod_delete_marks
        .insert(789, 456, 123, "badword", "Bad words");
    f.app.automod_delete_marks.confirm(789, 456, 123);
    let first = observed_delete_report(
        &f.app,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
        None,
    );
    assert_eq!(first.event, "automod_delete");
    let second = observed_delete_report(
        &f.app,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
        None,
    );
    assert_eq!(second.event, "message_delete");
}

#[tokio::test]
async fn bulk_mix_splits_automod_from_plain_ids() {
    let f = Fixture::new();
    f.app
        .automod_delete_marks
        .insert(789, 456, 10, "badword", "Bad words");
    f.app.automod_delete_marks.confirm(789, 456, 10);
    let automod = observed_delete_report(
        &f.app,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(10),
        None,
    );
    let plain = observed_delete_report(
        &f.app,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(11),
        None,
    );
    assert_eq!(automod.event, "automod_delete");
    assert_eq!(plain.event, "message_delete");
}
