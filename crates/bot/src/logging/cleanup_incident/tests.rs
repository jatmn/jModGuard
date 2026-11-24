use super::*;
use crate::logging::tests::{
    ScriptedHttpReply, log_text, read_proxied_request, replay_snapshot, request_log_payload,
    scripted_discord_proxy,
};

const GUILD: u64 = 8;
const SUBJECT: u64 = 9;
const DESTINATION: u64 = 20;
const BAN_REQUESTED_AT_MS: i64 = 3 * 60 * 60 * 1_000;
const PURGE_WINDOW_SECS: u32 = 60 * 60;

#[derive(Default)]
struct Sink(Mutex<Vec<(ChannelId, serde_json::Value)>>);

#[serenity::async_trait]
impl LogDelivery for Sink {
    async fn send(&self, channel: ChannelId, message: CreateMessage) -> anyhow::Result<()> {
        self.0
            .lock()
            .push((channel, serde_json::to_value(message)?));
        Ok(())
    }
}

struct Fixture {
    app: Arc<BotState>,
    path: Arc<FixturePath>,
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
            path: Arc::new(FixturePath(path)),
            sink: Sink::default(),
        }
    }

    async fn begin(&self) -> Option<Cleanup<'_>> {
        self.begin_for(SUBJECT).await
    }

    async fn begin_for(&self, user: u64) -> Option<Cleanup<'_>> {
        self.begin_in(GUILD, user).await
    }

    async fn begin_in(&self, guild: u64, user: u64) -> Option<Cleanup<'_>> {
        begin_cleanup(
            &self.app,
            guild,
            user,
            CleanupRule {
                id: "image_spam",
                name: "Exact image spam",
                reason: "same image",
                action: "ban",
                join_only: false,
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
        let grouped = match cleanup {
            Some(cleanup) => {
                cleanup
                    .deleted(&self.app, channel, message, prepared, false)
                    .await
            }
            None => false,
        };
        if !grouped {
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

    async fn enroll(&self) -> db::GuildEnrollment {
        let enrollment = db::enroll_guild(&self.app.db, GUILD as i64, 100)
            .await
            .unwrap()
            .unwrap();
        self.app.tenant.activate(enrollment.clone()).await.unwrap();
        enrollment
    }

    async fn set_filter(&self, event: &str, filter: db::LogFilter) {
        let event = event.to_owned();
        let data = serde_json::to_string(&filter).unwrap();
        self.app
            .db
            .with_conn(move |conn| {
                use db::SqlExecutor;
                conn.execute_bounded(
                    "INSERT OR REPLACE INTO log_filters(guild_id,event_type,data) VALUES(?1,?2,?3)",
                    db::params![GUILD, event, data],
                    1,
                )?;
                Ok(())
            })
            .await
            .unwrap();
        self.app.configs.invalidate();
    }

    fn notices(&self) -> Vec<String> {
        self.sink
            .0
            .lock()
            .iter()
            .map(|(_, notice)| log_text(notice))
            .collect()
    }
}

struct FixturePath(std::path::PathBuf);

impl Drop for FixturePath {
    fn drop(&mut self) {
        crate::tenant::remove_sidecars(&self.0);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
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
    assert!(cleanup.deleted(&f.app, 1, 102, prepared, false).await);
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
async fn abrupt_restart_recovers_confirmed_deletes_once_as_individual_notices() {
    let f = Fixture::new(true).await;
    f.enroll().await;
    let cleanup = f.begin().await.unwrap();
    f.rest(Some(&cleanup), 1, 101).await;
    f.remember(SUBJECT, 2, 102);
    cleanup.begin_purge(PURGE_WINDOW_SECS, BAN_REQUESTED_AT_MS);
    claim_cleanup_gateway(
        &f.app,
        GuildId::new(GUILD),
        ChannelId::new(2),
        MessageId::new(102),
        None,
        GatewayDeleteOrigin::Bulk,
    );
    persist_cleanup_observations(&f.app).await;
    cleanup.failed(3, 103, "missing permission");
    let path = Arc::clone(&f.path);
    drop(cleanup);
    assert!(f.notices().is_empty());
    drop(f); // close the original state and database without a shutdown flush

    // Recovery is still needed if an admin disables grouping before restarting.
    let mut cfg = crate::config::BotProcessConfig::default();
    cfg.log.group_automod_cleanup_deletes = false;
    let restarted = BotState::new(cfg, db::Db::open(&path.0, 2).unwrap()).unwrap();
    restarted.activation.mark_lock_held(true);
    let sink = Sink::default();
    let (last, retry) = recover_cleanup_page(&restarted, &sink, 0).await.unwrap();
    assert!(last.is_some() && !retry);
    assert_eq!(
        recover_cleanup_page(&restarted, &sink, 0).await.unwrap(),
        (None, false)
    );
    let notices: Vec<_> = sink
        .0
        .lock()
        .iter()
        .map(|(_, payload)| log_text(payload))
        .collect();
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert!(
        notices
            .iter()
            .any(|text| text.contains("Automod message deleted") && text.contains("<#1>")),
        "{notices:?}"
    );
    assert!(
        notices
            .iter()
            .any(|text| text.contains("Message deleted") && text.contains("<#2>")),
        "{notices:?}"
    );
    assert!(
        notices
            .iter()
            .all(|text| !text.contains("**Deleted:**") && !text.contains("<#3>")),
        "{notices:?}"
    );
}

#[tokio::test]
async fn a_summary_dispatched_before_an_abrupt_restart_is_not_replayed() {
    let f = Fixture::new(true).await;
    f.enroll().await;
    let cleanup = f.begin().await.unwrap();
    f.rest(Some(&cleanup), 1, 101).await;
    let path = Arc::clone(&f.path);
    drop(cleanup);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(10)).await;
    tokio::time::resume();
    let sink = Arc::new(FailsOrBlocks {
        response: SummaryResponse::Blocked,
        ..Default::default()
    });
    let worker = tokio::spawn({
        let app = Arc::clone(&f.app);
        let sink = Arc::clone(&sink);
        async move { flush(&app, sink.as_ref(), false).await }
    });
    tokio::time::timeout(Duration::from_secs(5), sink.entered.notified())
        .await
        .unwrap();
    assert_eq!(sink.rejected.lock().len(), 1);
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    drop(f);

    let restarted = BotState::new(Default::default(), db::Db::open(&path.0, 2).unwrap()).unwrap();
    restarted.activation.mark_lock_held(true);
    let recovery = Sink::default();
    assert_eq!(
        recover_cleanup_page(&restarted, &recovery, 0)
            .await
            .unwrap(),
        (None, false)
    );
    assert!(recovery.0.lock().is_empty());
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

#[derive(Default)]
enum SummaryResponse {
    #[default]
    Lost,
    Blocked,
    Rejected(serenity::http::ErrorResponse),
}

#[derive(Default)]
struct FailsOrBlocks {
    rejected: Mutex<Vec<String>>,
    response: SummaryResponse,
    entered: tokio::sync::Notify,
}

impl FailsOrBlocks {
    async fn reject(status: u16, body: &'static str) -> Self {
        let stub = StubDiscord::start(move |_, _| (status, body));
        let err = stub
            .http
            .get_channel(ChannelId::new(DESTINATION))
            .await
            .unwrap_err();
        let serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response)) = err
        else {
            panic!("expected a definite fixture rejection: {err}")
        };
        Self {
            response: SummaryResponse::Rejected(response),
            ..Default::default()
        }
    }
}

#[serenity::async_trait]
impl LogDelivery for FailsOrBlocks {
    async fn send(&self, _: ChannelId, message: CreateMessage) -> anyhow::Result<()> {
        self.rejected
            .lock()
            .push(log_text(&serde_json::to_value(message)?));
        self.entered.notify_one();
        match &self.response {
            SummaryResponse::Lost => {
                anyhow::bail!("fixture lost the response after accepting the message")
            }
            SummaryResponse::Blocked => std::future::pending().await,
            SummaryResponse::Rejected(error) => Err(serenity::Error::Http(
                serenity::http::HttpError::UnsuccessfulRequest(error.clone()),
            )
            .into()),
        }
    }
}

#[tokio::test]
async fn failures_only_survive_rejected_summary_and_fallback() {
    let f = Fixture::new(true).await;
    let cleanup = f.begin().await.unwrap();
    cleanup.failed(2, 102, "missing permission");
    drop(cleanup);
    let reject =
        FailsOrBlocks::reject(400, r#"{"code":50035,"message":"Invalid form body"}"#).await;

    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(10)).await;
    flush(&f.app, &reject, false).await;
    {
        let rejected = reject.rejected.lock();
        assert_eq!(rejected.len(), 2, "{rejected:?}");
        assert!(rejected[0].contains("**Deleted:** 0"));
        assert!(rejected[0].contains("**Time range:**"));
        assert!(rejected[1].contains("Automod cleanup delete failures"));
    }

    f.settle().await;
    let notices = f.notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("**Delete failures:** 1 · missing permission (<#2> 1)"));
    f.settle().await;
    assert_eq!(f.notices().len(), 1, "success settles the pending failure");
}

#[tokio::test]
async fn retry_keeps_absorbed_delete_after_both_sends_fail() {
    let f = Fixture::new(true).await;
    let cleanup = f.begin().await.unwrap();
    f.rest(Some(&cleanup), 1, 101).await;
    drop(cleanup);
    let reject =
        FailsOrBlocks::reject(400, r#"{"code":50035,"message":"Invalid form body"}"#).await;

    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(10)).await;
    flush(&f.app, &reject, false).await;
    {
        let rejected = reject.rejected.lock();
        assert_eq!(rejected.len(), 2, "{rejected:?}");
        assert!(rejected[0].contains("Automod cleanup"));
        assert!(rejected[1].contains("Automod message deleted"));
    }
    assert!(
        f.single(1, 101).is_none(),
        "queued fallback still owns the echo"
    );
    f.settle().await;
    assert_eq!(f.notices().len(), 1);
    assert!(f.notices()[0].contains("Automod message deleted"));
    f.settle().await;
    assert_eq!(f.notices().len(), 1);
}

#[tokio::test]
async fn excluded_user_is_intentionally_settled_without_leaking_into_bulk() {
    let f = Fixture::new(true).await;
    db::upsert_log_channel(&f.app.db, GUILD as i64, "message_bulk_delete", 30)
        .await
        .unwrap();
    f.set_filter(
        "automod_delete",
        db::LogFilter {
            users: vec![SUBJECT.to_string()],
            channels: vec![],
        },
    )
    .await;
    let cleanup = f.begin().await.unwrap();
    f.rest(Some(&cleanup), 1, 101).await;
    cleanup.failed(2, 102, "missing permission");
    drop(cleanup);
    assert!(f.single(1, 101).is_none());
    tokio::time::pause();
    f.settle().await;
    assert!(f.notices().is_empty());
    let state = f.app.cleanup_incidents.inner.lock();
    assert!(state.active.is_empty() && state.pending.is_empty());
    assert!(
        f.single(1, 101).is_none(),
        "the exclusion also settles later echoes"
    );
}

#[tokio::test]
async fn shutdown_replays_only_the_due_incident_that_has_not_started_sending() {
    let f = Fixture::new(true).await;
    for (user, channel, message) in [(SUBJECT, 1, 101), (SUBJECT + 1, 2, 102)] {
        let cleanup = f.begin_for(user).await.unwrap();
        f.rest(Some(&cleanup), channel, message).await;
    }
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(10)).await;
    let sink = Arc::new(FailsOrBlocks {
        response: SummaryResponse::Blocked,
        ..Default::default()
    });
    let worker = tokio::spawn({
        let app = Arc::clone(&f.app);
        let sink = Arc::clone(&sink);
        async move { flush(&app, sink.as_ref(), false).await }
    });
    sink.entered.notified().await;
    assert_eq!(sink.rejected.lock().len(), 1);
    f.app.activation.begin_shutdown();
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());

    flush(&f.app, &f.sink, true).await;
    let notices = f.notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("Automod message deleted"));
    assert!(notices[0].contains("<#2>"));
    assert!(
        f.single(1, 101).is_none(),
        "the possible summary retains echo ownership"
    );
}

#[tokio::test]
async fn cap_fallback_reports_failed_and_successful_deletes() {
    let f = Fixture::new(true).await;
    let cleanup = f.begin().await.unwrap();
    f.rest(Some(&cleanup), 1, 101).await;
    cleanup.failed(2, 102, "missing permission");
    tokio::time::pause();
    tokio::time::advance(MAX_LIFETIME).await;
    flush(&f.app, &f.sink, false).await;
    let notices = f.notices();
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert!(notices[0].contains("Automod message deleted"));
    assert!(notices[1].contains("**Delete failures:** 1 · missing permission (<#2> 1)"));
}

#[tokio::test]
async fn a_new_incident_can_open_after_quiet_before_the_worker_flushes() {
    let f = Fixture::new(true).await;
    let first = f.begin().await.unwrap();
    f.rest(Some(&first), 1, 101).await;
    drop(first);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(10)).await;
    let second = f.begin().await.unwrap();
    f.rest(Some(&second), 2, 102).await;
    drop(second);

    flush(&f.app, &f.sink, false).await;
    let notices = f.notices();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("**Channels:** <#1> 1"));
    f.settle().await;
    let notices = f.notices();
    assert_eq!(notices.len(), 2);
    assert!(notices[1].contains("**Channels:** <#2> 1"));
}

#[tokio::test]
async fn rejected_purge_replays_bulk_to_its_destination_with_bulk_exclusions() {
    let f = Fixture::new(true).await;
    let bulk_destination = 30;
    db::upsert_log_channel(
        &f.app.db,
        GUILD as i64,
        "message_bulk_delete",
        bulk_destination,
    )
    .await
    .unwrap();
    f.set_filter(
        "message_bulk_delete",
        db::LogFilter {
            users: vec![SUBJECT.to_string()],
            channels: vec!["3".into()],
        },
    )
    .await;
    let cleanup = f.begin().await.unwrap();
    cleanup.begin_purge(PURGE_WINDOW_SECS, BAN_REQUESTED_AT_MS);
    for (channel, message) in [(1, 101), (2, 102), (3, 103), (2, 104)] {
        f.remember(SUBJECT, channel, message);
    }
    assert!(f.single(1, 101).is_none());
    assert!(f.bulk(2, 102).report.is_none());
    assert!(f.bulk(3, 103).report.is_none());
    let prepared = cleanup.prepare(&f.app, 2, 104);
    claim_cleanup_gateway(
        &f.app,
        GuildId::new(GUILD),
        ChannelId::new(2),
        MessageId::new(104),
        None,
        GatewayDeleteOrigin::Bulk,
    );
    // The raw event can arrive before the snapshot handler classifies the bulk member.
    assert!(cleanup.deleted(&f.app, 2, 104, prepared, true).await);
    assert!(f.bulk(2, 104).report.is_none());
    cleanup.outcome(CaseOutcome::Failed, None);
    drop(cleanup);
    tokio::time::pause();
    f.settle().await;

    let notices = f.sink.0.lock();
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert_eq!(notices[0].0.get(), DESTINATION);
    assert!(log_text(&notices[0].1).contains("Message deleted"));
    assert_eq!(notices[1].0.get(), bulk_destination as u64);
    let bulk = log_text(&notices[1].1);
    assert!(bulk.contains("2 excluded by user filter"), "{bulk}");
    assert!(!bulk.contains("<#3>"), "{bulk}");
}

#[tokio::test]
async fn saved_legacy_and_grouped_templates_control_sample_visibility() {
    for (template, expected) in [
        (
            "My delete notice for {rule}",
            "My delete notice for Exact image spam",
        ),
        ("My cleanup counted {cleanup_total}", "My cleanup counted 1"),
        (
            "My cleanup counted {cleanup_total} · Link {message_url} · Text {content}",
            "My cleanup counted 1 · Link unavailable for grouped cleanup · Text unavailable for grouped cleanup",
        ),
    ] {
        let f = Fixture::new(true).await;
        db::upsert_template(&f.app.db, GUILD as i64, "automod_delete", template)
            .await
            .unwrap();
        let cleanup = f.begin().await.unwrap();
        f.rest(Some(&cleanup), 1, 101).await;
        drop(cleanup);
        tokio::time::pause();
        f.settle().await;
        let notices = f.notices();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains(expected), "{notices:?}");
        assert!(!notices[0].contains("sample from the first channel"));
        tokio::time::resume();
    }
}

#[tokio::test]
async fn a_cleanup_sample_alone_opts_into_a_custom_grouped_notice() {
    let f = Fixture::new(true).await;
    db::upsert_template(
        &f.app.db,
        GUILD as i64,
        "automod_delete",
        "My cleanup sampled {cleanup_sample}",
    )
    .await
    .unwrap();
    let cleanup = f.begin().await.unwrap();
    f.rest(Some(&cleanup), 1, 101).await;
    drop(cleanup);
    tokio::time::pause();
    f.settle().await;
    let notices = f.notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("My cleanup sampled sample from the first channel"),
        "{notices:?}"
    );
}

#[tokio::test]
async fn sweep_10008_keeps_identity_for_a_later_gateway_delete_without_snapshot() {
    let f = Fixture::new(true).await;
    let enrollment = f.enroll().await;
    let http = StubDiscord::start(|_, _| (404, r#"{"code":10008,"message":"Unknown Message"}"#));
    let cleanup = f.begin().await.unwrap();
    f.remember(SUBJECT, 2, 102);
    cleanup.begin_purge(PURGE_WINDOW_SECS, BAN_REQUESTED_AT_MS);
    cleanup.outcome(CaseOutcome::Succeeded, None);
    for _ in 0..2 {
        assert_eq!(
            crate::recent_messages::sweep_after_ban(
                &f.app,
                &http.http,
                &enrollment,
                SUBJECT,
                PURGE_WINDOW_SECS,
                BAN_REQUESTED_AT_MS,
                Some(&cleanup),
                || true,
            )
            .await,
            0
        );
    }
    assert_eq!(
        http.requests.lock().as_slice(),
        &[(
            hyper::Method::DELETE,
            "/api/v10/channels/2/messages/102".into()
        )]
    );
    assert!(f.bulk(2, 102).report.is_none());
    drop(cleanup);
    tokio::time::pause();
    f.settle().await;
    let notices = f.notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("**Deleted:** 1"));
    assert!(notices[0].contains("**Channels:** <#2> 1"));
}

struct StubDiscord {
    http: serenity::http::Http,
    requests: Arc<Mutex<Vec<(hyper::Method, String)>>>,
    server: tokio::task::JoinHandle<Result<(), hyper::Error>>,
}

impl StubDiscord {
    fn start(
        reply: impl Fn(&hyper::Method, &str) -> (u16, &'static str) + Send + Sync + 'static,
    ) -> Self {
        use hyper::service::{make_service_fn, service_fn};
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let server = hyper::Server::from_tcp(listener)
            .unwrap()
            .serve(make_service_fn({
                let requests = Arc::clone(&requests);
                let reply = Arc::new(reply);
                move |_| {
                    let requests = Arc::clone(&requests);
                    let reply = Arc::clone(&reply);
                    async move {
                        Ok::<_, std::convert::Infallible>(service_fn(
                            move |request: hyper::Request<hyper::Body>| {
                                let (status, body) = reply(request.method(), request.uri().path());
                                requests.lock().push((
                                    request.method().clone(),
                                    request.uri().path().to_owned(),
                                ));
                                async move {
                                    Ok::<_, std::convert::Infallible>(
                                        hyper::Response::builder()
                                            .status(status)
                                            .header("content-type", "application/json")
                                            .body(hyper::Body::from(body))
                                            .unwrap(),
                                    )
                                }
                            },
                        ))
                    }
                }
            }));
        let http = serenity::http::HttpBuilder::new("token")
            .proxy(format!("http://{addr}"))
            .ratelimiter_disabled(true)
            .build();
        Self {
            http,
            requests,
            server: tokio::spawn(server),
        }
    }
}

impl Drop for StubDiscord {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[tokio::test]
async fn live_velocity_kick_groups_its_existing_late_strike_deletes() {
    use crate::automod::{AutomodEngine, MsgCtx, execute_pending_with};
    use serenity::all::{Cache, Member, User, UserId};

    let f = Fixture::new(true).await;
    f.enroll().await;
    let mut rule = crate::defaults::default_rules()
        .into_iter()
        .find(|rule| rule.name == "Message velocity")
        .unwrap();
    rule.enabled = true;
    rule.dry_run = false;
    rule.skip_moderators = false;
    rule.log_automod = false;
    rule.open_case = true;
    rule.params["max_messages"] = 0.into();
    let mut kick = rule
        .actions
        .into_iter()
        .find(|step| step.action == "kick")
        .unwrap();
    kick.at = 1;
    rule.actions = vec![kick];
    db::upsert_rule(&f.app.db, GUILD as i64, "velocity", &rule)
        .await
        .unwrap();
    let cfg = f.app.configs.get(&f.app.db, GUILD).await.unwrap();
    let stub = StubDiscord::start(|_, _| (204, ""));
    let cache = Cache::new();
    let mut user = User::default();
    user.id = UserId::new(SUBJECT);
    let mut member = Member::default();
    member.guild_id = GuildId::new(GUILD);
    member.user = user.clone();
    let engine = AutomodEngine::new();
    tokio::time::pause();
    for (channel, message, action) in [(1, 101, "kick"), (2, 102, "delete"), (3, 103, "delete")] {
        let ctx = MsgCtx {
            guild_id: GUILD,
            message_id: message,
            channel_id: channel,
            author_id: SUBJECT,
            content: "message from the flood",
            attachment_count: 0,
            everyone_mentions: 0,
            here_mentions: 0,
            role_mentions: 0,
            joined_secs_ago: None,
            account_age_days: Some(30),
            member_roles: &[],
            is_moderator: false,
            can_mention_everyone: true,
            mention_permission_unknown: false,
            received: None,
        };
        let pending = engine.evaluate(&cfg, &ctx).unwrap();
        assert_eq!(pending.action.action, action);
        execute_pending_with(
            &f.app,
            &stub.http,
            &cache,
            GuildId::new(GUILD),
            &user,
            Some(member.clone()),
            "member",
            Some(message),
            Some(channel),
            &pending,
        )
        .await;
        if action == "delete" {
            assert!(f.single(channel, message).is_none());
        }
        tokio::time::advance(Duration::from_secs(1)).await;
    }
    f.settle().await;
    let notices = f.notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("Kicked · Case"), "{notices:?}");
    assert!(notices[0].contains("**Deleted:** 2"));
    assert!(notices[0].contains("**Channels:** <#2> 1, <#3> 1"));
    assert!(notices[0].contains("**Sample:** message from the flood"));
    assert_eq!(stub.requests.lock().len(), 3, "one kick and two deletes");
}

#[tokio::test]
async fn live_ban_cleanup_records_blocked_and_cancelled_outcomes() {
    use crate::automod::{PendingAction, execute_pending_with};
    use serenity::all::{Cache, User, UserId};

    for (blocked, expected) in [(true, CaseOutcome::Failed), (false, CaseOutcome::Uncertain)] {
        let f = Fixture::new(true).await;
        f.enroll().await;
        let mut rule = crate::defaults::default_rules().remove(0);
        let rule_id = "mention_spam";
        rule.enabled = true;
        rule.dry_run = false;
        rule.skip_moderators = false;
        rule.log_automod = false;
        rule.open_case = true;
        assert_eq!(rule.actions[0].action, "ban");
        db::upsert_rule(&f.app.db, GUILD as i64, rule_id, &rule)
            .await
            .unwrap();

        let pending = PendingAction {
            rule_id: rule_id.into(),
            rule_name: rule.name.clone(),
            action: rule.actions[0].clone(),
            strikes: 1,
            evidence: serde_json::json!({"content":"sample"}),
            mention_permission_unknown: false,
            matched_rule: Some(rule),
            dry_run: false,
        };
        let app = Arc::clone(&f.app);
        let stub = StubDiscord::start(move |method, _| match *method {
            hyper::Method::GET => (404, r#"{"code":10026,"message":"Unknown Ban"}"#),
            hyper::Method::PUT => {
                if !blocked {
                    app.activation.cut_off();
                }
                (403, r#"{"code":50013,"message":"Missing Permissions"}"#)
            }
            _ => panic!("unexpected request {method}"),
        });
        let mut user = User::default();
        user.id = UserId::new(SUBJECT);
        let cache = Cache::new();
        for _ in 0..if blocked { 2 } else { 1 } {
            execute_pending_with(
                &f.app,
                &stub.http,
                &cache,
                GuildId::new(GUILD),
                &user,
                None,
                "member",
                Some(101),
                Some(1),
                &pending,
            )
            .await;
            if blocked {
                // A second enforcement gets a fresh incident and finds the remembered rejection.
                if f.app.cleanup_incidents.inner.lock().active.len() == 1 {
                    tokio::time::pause();
                    tokio::time::advance(Duration::from_secs(11)).await;
                    tokio::time::resume();
                }
            }
        }
        let state = f.app.cleanup_incidents.inner.lock();
        assert_eq!(state.active.len(), if blocked { 2 } else { 1 });
        let (_, incident) = state.active.values().next_back().unwrap();
        assert_eq!(incident.outcome, Some(expected));
        if blocked {
            assert!(incident.purge_receipt_until.is_none());
        }
        assert_eq!(
            stub.requests
                .lock()
                .iter()
                .filter(|(method, _)| *method == hyper::Method::PUT)
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn ambiguous_summary_never_retries_or_expands_to_individual_logs() {
    for response in [SummaryResponse::Lost, SummaryResponse::Blocked] {
        let f = Fixture::new(true).await;
        let cleanup = f.begin().await.unwrap();
        for (channel, message) in [(1, 101), (2, 102), (3, 103)] {
            f.rest(Some(&cleanup), channel, message).await;
        }
        cleanup.failed(4, 104, "missing permission");
        drop(cleanup);

        let sink = FailsOrBlocks {
            response,
            ..Default::default()
        };
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(10)).await;
        flush(&f.app, &sink, false).await;
        {
            let captured = sink.rejected.lock();
            assert_eq!(captured.len(), 1, "{captured:?}");
            assert!(captured[0].contains("**Deleted:** 3"));
            assert!(captured[0].contains("**Delete failures:** 1"));
        }
        f.settle().await;
        f.app.activation.begin_shutdown();
        flush(&f.app, &f.sink, true).await;
        assert!(f.notices().is_empty());
        assert!(f.app.cleanup_incidents.inner.lock().pending.is_empty());
        assert!(f.single(1, 101).is_none());
        tokio::time::resume();
    }
}

struct GuildRoutingSink {
    bad: FailsOrBlocks,
    good: Sink,
}

#[serenity::async_trait]
impl LogDelivery for GuildRoutingSink {
    async fn send(&self, channel: ChannelId, message: CreateMessage) -> anyhow::Result<()> {
        if channel.get() == DESTINATION {
            self.bad.send(channel, message).await
        } else {
            self.good.send(channel, message).await
        }
    }
}

#[tokio::test]
async fn persistently_rejected_guild_cannot_fill_other_guilds_cleanup_capacity() {
    const HEALTHY_GUILD: u64 = 18;
    const HEALTHY_DESTINATION: u64 = 21;
    let f = Fixture::new(true).await;
    db::upsert_log_channel(
        &f.app.db,
        HEALTHY_GUILD as i64,
        "message_delete",
        HEALTHY_DESTINATION as i64,
    )
    .await
    .unwrap();
    let sink = GuildRoutingSink {
        bad: FailsOrBlocks::reject(403, r#"{"code":50013,"message":"Missing Permissions"}"#).await,
        good: Sink::default(),
    };
    tokio::time::pause();
    for i in 0..=MAX_INCIDENTS as u64 {
        let cleanup = f.begin_for(SUBJECT + i).await.unwrap();
        f.rest(Some(&cleanup), 1, 101 + i).await;
        drop(cleanup);
        tokio::time::advance(Duration::from_secs(10)).await;
        flush(&f.app, &sink, false).await;
    }

    {
        let state = f.app.cleanup_incidents.inner.lock();
        assert_eq!(state.pending.len(), MAX_RETRIES_PER_GUILD);
        assert_eq!(
            state.pending.first_key_value().unwrap().0,
            &((MAX_INCIDENTS + 1 - MAX_RETRIES_PER_GUILD) as u64)
        );
        assert!(
            state
                .pending
                .values()
                .flatten()
                .any(|pending| pending.retry_delay > DELIVERY_RETRY)
        );
        assert!(
            state
                .pending
                .values()
                .flatten()
                .all(|pending| pending.subject.0 == GUILD)
        );
    }
    assert!(!sink.bad.rejected.lock().is_empty());
    assert!(
        f.single(1, 101).is_none(),
        "eviction settles the original echo receipt"
    );

    let cleanup = f
        .begin_in(HEALTHY_GUILD, SUBJECT)
        .await
        .expect("healthy guild can still group");
    for (channel, message) in [(2, 201), (3, 202)] {
        let prepared = cleanup.prepare(&f.app, channel, message);
        assert!(
            cleanup
                .deleted(&f.app, channel, message, prepared, false)
                .await
        );
    }
    drop(cleanup);
    tokio::time::advance(Duration::from_secs(10)).await;
    flush(&f.app, &sink, false).await;
    let sent = sink.good.0.lock();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].0.get(), HEALTHY_DESTINATION);
    assert!(log_text(&sent[0].1).contains("**Deleted:** 2"));
}

async fn queue_summary(f: &Fixture) {
    let cleanup = f.begin().await.unwrap();
    f.rest(Some(&cleanup), 1, 101).await;
    f.rest(Some(&cleanup), 2, 102).await;
    drop(cleanup);
}

fn assert_waiting_summary(f: &Fixture) {
    let state = f.app.cleanup_incidents.inner.lock();
    assert_eq!(state.pending.len(), 1);
    let pending = state.pending.values().next().unwrap().as_ref().unwrap();
    assert!(!pending.fallback);
    assert!(matches!(
        pending.summary_dispatch,
        LogDispatch::Pending | LogDispatch::RateLimited(_)
    ));
    assert_eq!(pending.retry_delay, DELIVERY_RETRY);
    assert_eq!(pending.incident.messages.len(), 2);
}

fn successful_summary_reply() -> ScriptedHttpReply {
    ScriptedHttpReply {
        status: 200,
        headers: "",
        body: "{}",
    }
}

#[tokio::test]
async fn summary_timeout_waiting_for_serenity_bucket_keeps_the_grouped_retry() {
    use std::sync::atomic::Ordering;
    let f = Fixture::new(true).await;
    let (http, calls, server) = scripted_discord_proxy(vec![successful_summary_reply()]).await;
    let route = serenity::http::Route::ChannelMessages {
        channel_id: DESTINATION.into(),
    };
    let routes = http.ratelimiter.as_ref().unwrap().routes();
    let bucket = Arc::clone(
        routes
            .write()
            .await
            .entry(route.ratelimiting_bucket())
            .or_default(),
    );
    let held = bucket.lock().await;
    queue_summary(&f).await;

    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(10)).await;
    flush(&f.app, &http, false).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "no POST before the bucket releases"
    );
    assert_waiting_summary(&f);
    assert!(f.single(1, 101).is_none());
    drop(held);

    flush(&f.app, &http, false).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the summary waits for its backoff"
    );
    tokio::time::advance(DELIVERY_RETRY).await;
    tokio::time::resume();
    flush(&f.app, &http, false).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "one grouped POST on retry");
    assert!(f.app.cleanup_incidents.inner.lock().pending.is_empty());
    server.abort();
}

#[tokio::test]
async fn summary_timeout_during_429_backoff_keeps_the_grouped_retry() {
    use std::sync::atomic::Ordering;
    for headers in [
        "retry-after: 60\r\n",
        "x-ratelimit-global: true\r\nretry-after: 60\r\n",
    ] {
        let f = Fixture::new(true).await;
        let (http, calls, server) = scripted_discord_proxy(vec![
            ScriptedHttpReply {
                status: 429,
                headers,
                body: "{}",
            },
            successful_summary_reply(),
        ])
        .await;
        let http = Arc::new(http);
        let waiting = Arc::new(tokio::sync::Notify::new());
        queue_summary(&f).await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::time::resume();

        let mut worker = tokio::spawn({
            let app = Arc::clone(&f.app);
            let http = Arc::clone(&http);
            let waiting = Arc::clone(&waiting);
            async move {
                crate::discord::with_rate_limit_wait_hook(
                    Arc::new(move || waiting.notify_one()),
                    flush(&app, http.as_ref(), false),
                )
                .await;
            }
        });
        tokio::select! {
            _ = waiting.notified() => {}
            result = &mut worker => panic!("summary completed before rate-limit wait: {result:?}"),
        }
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(f.app.cfg.log.discord_timeout_secs)).await;
        worker.await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "only the rejected POST was sent"
        );
        assert_waiting_summary(&f);

        tokio::time::advance(DELIVERY_RETRY).await;
        flush(&f.app, http.as_ref(), false).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the provider backoff still applies"
        );
        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::time::resume();
        flush(&f.app, http.as_ref(), false).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "the retry sends one summary"
        );
        assert!(f.app.cleanup_incidents.inner.lock().pending.is_empty());
        server.abort();
    }
}

#[tokio::test]
async fn lost_summary_ack_from_http_keeps_receipts_without_replaying() {
    let f = Fixture::new(true).await;
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_proxied_request(&mut stream).await.unwrap();
        // The complete POST is received. Drop the connection without an acknowledgement.
        String::from_utf8(request).unwrap()
    });
    let http = serenity::http::HttpBuilder::new("fixture")
        .proxy(format!("http://{addr}"))
        .build();
    queue_summary(&f).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(10)).await;
    tokio::time::resume();
    flush(&f.app, &http, false).await;
    let request = server.await.unwrap();
    assert!(request.starts_with("POST /api/v10/channels/"));
    assert!(log_text(&request_log_payload(&request)).contains("**Deleted:** 2"));
    assert!(f.app.cleanup_incidents.inner.lock().pending.is_empty());
    assert!(f.single(1, 101).is_none());
    tokio::time::pause();
    f.settle().await;
    flush(&f.app, &f.sink, true).await;
    assert!(f.notices().is_empty());
}

#[tokio::test]
async fn grouped_template_uses_a_private_single_message_notice_on_fallback_and_when_disabled() {
    for enabled in [true, false] {
        let f = Fixture::new(enabled).await;
        db::upsert_template(
            &f.app.db,
            GUILD as i64,
            "automod_delete",
            "My cleanup counted {cleanup_total}, channels {cleanup_channels}",
        )
        .await
        .unwrap();
        let cleanup = f.begin().await;
        let guild = GuildId::new(GUILD);
        let channel = ChannelId::new(1);
        let id = MessageId::new(101);
        let row = MessageSnapshot {
            guild_id: GUILD as i64,
            channel_id: channel.get() as i64,
            message_id: id.get() as i64,
            ..replay_snapshot("private retained message")
        };
        let observe = || observed_delete_report(&f.app, guild, channel, id, Some(&row));
        if let Some(cleanup) = cleanup {
            let prepared = cleanup.prepare(&f.app, channel.get(), id.get());
            assert!(observe().is_none());
            assert!(
                cleanup
                    .deleted(&f.app, channel.get(), id.get(), prepared, false)
                    .await
            );
            drop(cleanup);
            f.app.activation.begin_shutdown();
            flush(&f.app, &f.sink, true).await;
        } else {
            f.rest(None, channel.get(), id.get()).await;
            let report = observe().unwrap();
            assert_eq!(report.event, "automod_delete");
            emit_to(&f.app, &f.sink, guild, report.event, report.vars).await;
        }
        let notices = f.notices();
        assert_eq!(notices.len(), 1, "{notices:?}");
        let notice = &notices[0];
        assert!(notice.contains("Automod message deleted"));
        assert!(notice.contains("/channels/8/1/101"), "{notice}");
        assert!(!notice.contains("{cleanup_"));
        assert!(!notice.contains("private retained message"));
    }
}
