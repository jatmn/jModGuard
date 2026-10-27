use super::*;
use crate::config::BotProcessConfig;
use crate::db::Db;
use crate::state::BotState;
use jmodguard::db::SqlExecutor;
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
    )
    .expect("automod report");
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
    )
    .expect("plain follow-up");
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
    )
    .expect("report");
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
    )
    .expect("plain delete");
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
    )
    .expect("plain delete");
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
    )
    .expect("first");
    assert_eq!(first.event, "automod_delete");
    let second = observed_delete_report(
        &f.app,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
        None,
    )
    .expect("second");
    assert_eq!(second.event, "message_delete");
}

#[tokio::test]
async fn gateway_delete_before_rest_confirm_reconciles_automod_delete() {
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "automod_delete", 456)
        .await
        .unwrap();
    f.app
        .automod_delete_marks
        .insert(789, 456, 123, "badword", "Bad words");
    assert!(
        observed_delete_report(
            &f.app,
            GuildId::new(789),
            ChannelId::new(456),
            MessageId::new(123),
            None,
        )
        .is_none()
    );
    assert!(
        f.app
            .automod_delete_marks
            .inner
            .lock()
            .contains(789, 456, 123)
    );
    let reconcile = f.app.automod_delete_marks.confirm(789, 456, 123);
    assert_eq!(reconcile, Some(("badword".into(), "Bad words".into())));
    let sink = Sink::default();
    let (rule_id, rule_name) = reconcile.unwrap();
    emit_reconciled_automod_delete(&f.app, &sink, 789, 456, 123, &rule_id, &rule_name, None).await;
    assert_eq!(sink.messages.lock().unwrap().len(), 1);
    assert!(
        !f.app
            .automod_delete_marks
            .inner
            .lock()
            .contains(789, 456, 123)
    );
}

#[tokio::test]
async fn gateway_first_spam_delete_reconciles_with_only_automod_channel() {
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "automod_delete", 456)
        .await
        .unwrap();
    f.app
        .automod_delete_marks
        .insert(789, 456, 123, "repeated_text", "Repeated text");
    assert!(
        observed_delete_report(
            &f.app,
            GuildId::new(789),
            ChannelId::new(456),
            MessageId::new(123),
            None,
        )
        .is_none()
    );
    let reconcile = f
        .app
        .automod_delete_marks
        .confirm(789, 456, 123)
        .expect("spam reconcile");
    let sink = Sink::default();
    emit_reconciled_automod_delete(
        &f.app,
        &sink,
        789,
        456,
        123,
        &reconcile.0,
        &reconcile.1,
        None,
    )
    .await;
    assert_eq!(sink.messages.lock().unwrap().len(), 1);
    assert!(
        sink.messages.lock().unwrap()[0]
            .1
            .to_string()
            .contains("Repeated text")
    );
}

async fn set_log_filter(app: &BotState, event: &str, data: Option<&str>) {
    let event = event.to_string();
    let data = data.map(str::to_string);
    app.db
        .with_conn(move |conn| {
            conn.execute_bounded(
                "DELETE FROM log_filters WHERE guild_id=789 AND event_type=?1",
                db::params![event.clone()],
                65_536,
            )?;
            if let Some(data) = data {
                conn.execute_bounded(
                    "INSERT INTO log_filters(guild_id,event_type,data) VALUES(789,?1,?2)",
                    db::params![event, data],
                    65_536,
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
    app.invalidate_configs();
}

#[tokio::test]
async fn automod_delete_fallback_inherits_message_delete_ignore_filter() {
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "message_delete", 456)
        .await
        .unwrap();
    set_log_filter(
        &f.app,
        "message_delete",
        Some(r#"{"users":["42"],"channels":[]}"#),
    )
    .await;
    f.app
        .automod_delete_marks
        .insert(789, 456, 123, "badword", "Bad words");
    f.app.automod_delete_marks.confirm(789, 456, 123);
    let mut report = observed_delete_report(
        &f.app,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
        None,
    )
    .expect("report");
    report.vars.insert("user_id".into(), "42".into());
    let sink = Sink::default();
    let delivery = emit_to(&f.app, &sink, report.guild_id, report.event, report.vars).await;
    assert_eq!(delivery, Delivery::Unconfigured);
    assert!(sink.messages.lock().unwrap().is_empty());
}

#[tokio::test]
async fn gateway_first_user_ignore_filters_plain_peek_and_reconciled_automod() {
    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "message_delete", 456)
        .await
        .unwrap();
    set_log_filter(
        &f.app,
        "message_delete",
        Some(r#"{"users":["42"],"channels":[]}"#),
    )
    .await;
    remember_message(&f.app, 789, 456, 123, Some(42), None, None);
    f.app
        .automod_delete_marks
        .insert(789, 456, 123, "badword", "Bad words");
    assert!(
        observed_delete_report(
            &f.app,
            GuildId::new(789),
            ChannelId::new(456),
            MessageId::new(123),
            None,
        )
        .is_none()
    );
    let peek = message_delete_report_peek(
        &f.app,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
    );
    assert_eq!(peek.vars["user_id"], "42");
    let sink = Sink::default();
    assert_eq!(
        emit_to(&f.app, &sink, peek.guild_id, peek.event, peek.vars).await,
        Delivery::Unconfigured
    );
    let reconcile = f.app.automod_delete_marks.confirm(789, 456, 123).unwrap();
    emit_reconciled_automod_delete(
        &f.app,
        &sink,
        789,
        456,
        123,
        &reconcile.0,
        &reconcile.1,
        None,
    )
    .await;
    assert!(sink.messages.lock().unwrap().is_empty());
}

#[test]
fn bulk_transcript_peel_automod_suppresses_empty_bulk() {
    let mut all_automod = BulkTranscript::new(2);
    all_automod.peel_automod_delete();
    all_automod.peel_automod_delete();
    assert!(!all_automod.worth_sending());

    let mut mixed = BulkTranscript::new(2);
    mixed.peel_automod_delete();
    mixed.push_unknown(MessageId::new(11));
    assert_eq!(mixed.total, 1);
    assert!(mixed.worth_sending());
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
    )
    .expect("automod");
    let plain = observed_delete_report(
        &f.app,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(11),
        None,
    )
    .expect("plain");
    assert_eq!(automod.event, "automod_delete");
    assert_eq!(plain.event, "message_delete");
}

#[tokio::test]
async fn rest_failure_replays_plain_delete_after_gateway_pending() {
    use crate::automod_delete_marks::UnconfirmedMarkRemoval;

    let f = Fixture::new();
    db::upsert_log_channel(&f.app.db, 789, "message_delete", 456)
        .await
        .unwrap();
    f.app.invalidate_configs();
    f.app
        .automod_delete_marks
        .insert(789, 456, 123, "badword", "Bad words");
    assert!(
        observed_delete_report(
            &f.app,
            GuildId::new(789),
            ChannelId::new(456),
            MessageId::new(123),
            None,
        )
        .is_none()
    );
    assert_eq!(
        f.app.automod_delete_marks.remove_unconfirmed(789, 456, 123),
        UnconfirmedMarkRemoval::ReplayPlainDelete
    );
    let sink = Sink::default();
    emit_plain_message_delete(
        &f.app,
        &sink,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
        None,
    )
    .await;
    assert_eq!(sink.messages.lock().unwrap().len(), 1);
}

#[test]
fn automod_delete_report_peeks_snippet_identity() {
    let f = Fixture::new();
    remember_message(&f.app, 789, 456, 123, Some(42), Some("hello"), None);
    let automod = automod_delete_report(
        &f.app,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
        "badword",
        "Bad words",
        None,
    );
    assert_eq!(automod.vars["user_id"], "42");
    let plain = message_delete_report(
        &f.app,
        GuildId::new(789),
        ChannelId::new(456),
        MessageId::new(123),
    );
    assert_eq!(plain.vars["user_id"], "42");
}

#[test]
fn bulk_peel_only_when_automod_individually_routable() {
    let automod = Some(MessageReport {
        guild_id: GuildId::new(789),
        event: "automod_delete",
        vars: BTreeMap::new(),
    });
    assert!(!peel_automod_from_bulk(&automod, false));
    assert!(peel_automod_from_bulk(&automod, true));
    assert!(!peel_automod_from_bulk(&None, false));
    assert!(peel_automod_from_bulk(&None, true));

    let mut bulk_only = BulkTranscript::new(2);
    if peel_automod_from_bulk(&automod, false) {
        bulk_only.peel_automod_delete();
    } else {
        bulk_only.push_unknown(MessageId::new(10));
    }
    assert_eq!(bulk_only.total, 2);
    assert!(bulk_only.worth_sending());
}
