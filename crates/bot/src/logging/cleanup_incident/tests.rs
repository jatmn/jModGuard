use super::*;
use crate::logging::tests::log_text;

const GUILD: u64 = 8;
const SUBJECT: u64 = 9;
const DESTINATION: u64 = 20;
const BAN_REQUESTED_AT_MS: i64 = 3 * 60 * 60 * 1_000;
const PURGE_WINDOW_SECS: u32 = 60 * 60;

#[derive(Default)]
struct Sink(Mutex<Vec<serde_json::Value>>);

#[serenity::async_trait]
impl LogDelivery for Sink {
    async fn send(&self, channel: ChannelId, message: CreateMessage) -> anyhow::Result<()> {
        assert_eq!(channel.get(), DESTINATION);
        self.0.lock().push(serde_json::to_value(message)?);
        Ok(())
    }
}

struct Fixture {
    app: Arc<BotState>,
    path: std::path::PathBuf,
    sink: Sink,
}

impl Fixture {
    async fn new(enabled: bool) -> Self {
        let path = std::env::temp_dir().join(format!("cleanup_{:032x}.db", rand::random::<u128>()));
        let db = db::Db::open(&path, 2).unwrap();
        db::upsert_log_channel(&db, GUILD as i64, "message_delete", DESTINATION as i64)
            .await
            .unwrap();
        let mut cfg = crate::config::BotProcessConfig::default();
        cfg.log.group_automod_cleanup_deletes = enabled;
        let app = BotState::new(cfg, db).unwrap();
        app.activation.mark_lock_held(true);
        Self {
            app,
            path,
            sink: Sink::default(),
        }
    }

    async fn begin(&self) -> Option<Cleanup<'_>> {
        begin_cleanup(
            &self.app,
            GUILD,
            SUBJECT,
            CleanupRule {
                id: "image_spam",
                name: "Exact image spam",
                reason: "same image",
                action: "ban",
                sample: Some((1, 101, "sample from the first channel")),
                hash: Some(&[0xab; 32]),
            },
        )
        .await
    }

    async fn rest(&self, cleanup: Option<&Cleanup<'_>>, channel: u64, message: u64) {
        let prepared = self.app.automod_delete_marks.prepare_delete(
            GUILD,
            channel,
            message,
            "image_spam",
            "Exact image spam",
            "same image",
        );
        if !cleanup
            .is_some_and(|cleanup| cleanup.deleted(&self.app, channel, message, prepared, false))
        {
            complete_prepared_automod_delete(
                &self.app,
                &self.sink,
                GUILD,
                channel,
                message,
                &prepared,
                "image_spam",
                "Exact image spam",
                "same image",
            )
            .await;
        }
    }

    fn remember(&self, author: u64, channel: u64, message: u64) {
        self.app.recent_messages.remember(
            GUILD,
            author,
            channel,
            message,
            BAN_REQUESTED_AT_MS - 1_000,
        );
    }

    fn single(&self, channel: u64, message: u64) -> Option<MessageReport> {
        observed_delete_report(
            &self.app,
            GuildId::new(GUILD),
            ChannelId::new(channel),
            MessageId::new(message),
            None,
        )
    }

    fn bulk(&self, channel: u64, message: u64) -> BulkDeletePlacement {
        place_bulk_delete(
            &self.app,
            GuildId::new(GUILD),
            ChannelId::new(channel),
            MessageId::new(message),
            None,
            false,
        )
    }

    async fn settle(&self) {
        tokio::time::advance(Duration::from_secs(10)).await;
        flush(&self.app, &self.sink, false).await;
    }

    fn notices(&self) -> Vec<String> {
        self.sink.0.lock().iter().map(log_text).collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        crate::tenant::remove_sidecars(&self.path);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
        }
    }
}

#[tokio::test]
async fn one_notice_for_rest_and_single_and_bulk_purge_echoes_across_three_channels() {
    let f = Fixture::new(true).await;
    let cleanup = f.begin().await.unwrap();
    f.rest(Some(&cleanup), 1, 101).await;

    // Gateway-first REST echo and a repeated bulk echo for the same message.
    let prepared = cleanup.prepare(&f.app, 1, 102);
    assert!(f.single(1, 102).is_none());
    assert!(cleanup.deleted(&f.app, 1, 102, prepared, false));
    assert!(f.bulk(1, 102).report.is_none());
    assert!(f.single(1, 101).is_none());

    f.remember(SUBJECT, 2, 103);
    f.remember(SUBJECT, 3, 104);
    cleanup.begin_purge(PURGE_WINDOW_SECS, BAN_REQUESTED_AT_MS);
    assert!(f.single(2, 103).is_none());
    let purge = f.bulk(3, 104);
    assert!(purge.report.is_none() && !purge.reserved_pending);
    assert!(f.bulk(2, 103).report.is_none());

    cleanup.outcome(CaseOutcome::Succeeded, Some(CaseId::new()));
    drop(cleanup);
    assert!(f.notices().is_empty());
    tokio::time::pause();
    f.settle().await;
    let notices = f.notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    let body = &notices[0];
    assert!(body.contains("Automod cleanup"));
    assert!(body.contains("**Deleted:** 4"), "{body}");
    assert!(body.contains("<#1> 2, <#2> 1, <#3> 1"), "{body}");
    assert!(body.contains("Banned · Case"), "{body}");
    assert!(body.contains("**Time range:**"));
    assert!(body.contains("sample from the first channel"));
    assert!(body.contains(&"ab".repeat(32)));
    assert!(f.single(2, 103).is_none(), "receipt survives after sending");
}

#[tokio::test]
async fn late_strikes_join_and_move_the_settle_time() {
    let f = Fixture::new(true).await;
    let first = f.begin().await.unwrap();
    tokio::time::pause();
    f.rest(Some(&first), 1, 101).await;
    first.outcome(CaseOutcome::Succeeded, None);
    drop(first);
    tokio::time::advance(Duration::from_secs(8)).await;
    let late = f.begin().await.unwrap();
    f.rest(Some(&late), 2, 102).await;
    drop(late);
    tokio::time::advance(Duration::from_secs(2)).await;
    flush(&f.app, &f.sink, false).await;
    assert!(f.notices().is_empty());
    f.settle().await;
    let notices = f.notices();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("**Deleted:** 2"));
    assert!(notices[0].contains("Banned"));
}

#[tokio::test]
async fn partial_failures_are_separate_and_a_purge_can_clear_one() {
    let f = Fixture::new(true).await;
    let cleanup = f.begin().await.unwrap();
    f.rest(Some(&cleanup), 1, 101).await;
    cleanup.failed(2, 102, "missing permission");
    cleanup.failed(2, 103, "missing permission");
    f.remember(SUBJECT, 2, 102);
    cleanup.begin_purge(PURGE_WINDOW_SECS, BAN_REQUESTED_AT_MS);
    assert!(f.single(2, 102).is_none());
    cleanup.outcome(CaseOutcome::Succeeded, None);
    drop(cleanup);
    tokio::time::pause();
    f.settle().await;
    let notices = f.notices();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("**Deleted:** 2"));
    assert!(notices[0].contains("**Delete failures:** 1 · missing permission (<#2> 1)"));
    assert!(notices[0].contains("Banned"));
}

#[tokio::test]
async fn purge_preserves_other_users_and_unknown_or_out_of_window_messages() {
    let f = Fixture::new(true).await;
    let cleanup = f.begin().await.unwrap();
    f.remember(12, 1, 101);
    f.remember(12, 2, 102);
    f.app.recent_messages.remember(
        GUILD,
        SUBJECT,
        3,
        104,
        BAN_REQUESTED_AT_MS - i64::from(PURGE_WINDOW_SECS) * 2 * 1_000,
    );
    cleanup.begin_purge(PURGE_WINDOW_SECS, BAN_REQUESTED_AT_MS);
    for report in [
        f.single(1, 101),
        f.bulk(2, 102).report,
        f.single(3, 103),
        f.single(3, 104),
    ] {
        assert_eq!(report.unwrap().event, "message_delete");
    }
}

#[tokio::test]
async fn planned_restart_flushes_individual_notices_and_replacement_has_no_claims() {
    let f = Fixture::new(true).await;
    let cleanup = f.begin().await.unwrap();
    f.rest(Some(&cleanup), 1, 101).await;
    f.remember(SUBJECT, 2, 102);
    cleanup.begin_purge(PURGE_WINDOW_SECS, BAN_REQUESTED_AT_MS);
    assert!(f.single(2, 102).is_none());
    drop(cleanup);

    f.app.activation.begin_shutdown();
    flush(&f.app, &f.sink, true).await;
    flush(&f.app, &f.sink, true).await;
    let notices = f.notices();
    assert_eq!(notices.len(), 2);
    assert!(
        notices
            .iter()
            .all(|body| body.contains("Automod message deleted"))
    );
    let restarted = Fixture::new(true).await;
    assert_eq!(restarted.single(2, 102).unwrap().event, "message_delete");
}

#[tokio::test]
async fn disabling_grouping_preserves_individual_automod_and_purge_logs() {
    let f = Fixture::new(false).await;
    assert!(f.begin().await.is_none());
    f.rest(None, 1, 101).await;
    let marked = f.single(1, 101).unwrap();
    assert_eq!(marked.event, "automod_delete");
    emit_to(&f.app, &f.sink, marked.guild_id, marked.event, marked.vars).await;
    f.remember(SUBJECT, 2, 102);
    assert_eq!(f.single(2, 102).unwrap().event, "message_delete");
    let notices = f.notices();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("Automod message deleted"));
}

#[tokio::test]
async fn unfinished_cleanup_uses_per_message_fallback_at_the_cap() {
    let f = Fixture::new(true).await;
    let cleanup = f.begin().await.unwrap();
    f.rest(Some(&cleanup), 1, 101).await;
    tokio::time::pause();
    tokio::time::advance(MAX_LIFETIME).await;
    flush(&f.app, &f.sink, false).await;
    assert_eq!(f.notices().len(), 1);
    assert!(f.notices()[0].contains("Automod message deleted"));
    f.rest(Some(&cleanup), 2, 102).await;
    assert_eq!(f.single(2, 102).unwrap().event, "automod_delete");
}

#[tokio::test]
async fn rejected_ban_replays_pending_purge_as_plain_delete() {
    let f = Fixture::new(true).await;
    let cleanup = f.begin().await.unwrap();
    f.rest(Some(&cleanup), 1, 101).await;
    f.remember(SUBJECT, 2, 102);
    cleanup.begin_purge(PURGE_WINDOW_SECS, BAN_REQUESTED_AT_MS);
    assert!(f.single(2, 102).is_none());
    cleanup.outcome(CaseOutcome::Failed, None);
    f.remember(SUBJECT, 3, 103);
    assert_eq!(f.single(3, 103).unwrap().event, "message_delete");
    drop(cleanup);
    tokio::time::pause();
    f.settle().await;
    let notices = f.notices();
    assert_eq!(notices.len(), 2);
    assert!(notices[0].contains("Message deleted"));
    assert!(notices[1].contains("**Deleted:** 1"));
    assert!(notices[1].contains("Ban failed"));
}
