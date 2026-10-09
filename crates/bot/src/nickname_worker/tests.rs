use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const JOINED_AT_NS: i64 = 1_000_000_100;
const REJOINED_AT_NS: i64 = 2_000_000_000;

struct Fixture {
    db: db::Db,
    path: std::path::PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_nickname_{:032x}.db",
            rand::random::<u128>()
        ));
        Self {
            db: db::Db::open(&path, 2).unwrap(),
            path,
        }
    }
    async fn job(&self) -> db::NicknameReconcileJob {
        db::enroll_guild(&self.db, 10, JOINED_AT_NS).await.unwrap();
        db::patch_settings(&self.db, 10, None, Some(Some("new nickname".into())), None)
            .await
            .unwrap();
        db::pending_nickname_reconciles(&self.db, 0, 1)
            .await
            .unwrap()
            .remove(0)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
        }
    }
}
enum DuringRead<'a> {
    None,
    Retire(&'a db::Db),
    Replace(&'a db::Db),
}
struct Source<'a> {
    joined: i64,
    fail: bool,
    during: DuringRead<'a>,
    calls: AtomicUsize,
}
impl Default for Source<'_> {
    fn default() -> Self {
        Self {
            joined: JOINED_AT_NS,
            fail: false,
            during: DuringRead::None,
            calls: AtomicUsize::new(0),
        }
    }
}
#[serenity::async_trait]
impl MembershipSource for Source<'_> {
    async fn guilds_after(&self, _: Option<u64>, _: u32) -> Result<Vec<u64>> {
        panic!("nickname work must not list guilds")
    }
    async fn joined_at_ns(&self, guild: u64) -> Result<i64> {
        assert_eq!(guild, 10);
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.during {
            DuringRead::None => {}
            DuringRead::Retire(db) => db::retire_guild_enrollment(db, 10, JOINED_AT_NS).await?,
            DuringRead::Replace(db) => {
                db::patch_settings(
                    db,
                    10,
                    None,
                    Some(Some("superseding nickname".into())),
                    None,
                )
                .await?;
            }
        }
        anyhow::ensure!(!self.fail, "membership unavailable");
        Ok(self.joined)
    }
}

#[tokio::test]
async fn matching_membership_allows_current_request_only() {
    let f = Fixture::new();
    let job = f.job().await;
    let source = Source::default();
    assert!(dispatch_ready(&f.db, &job, &source).await.unwrap());
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    db::acknowledge_nickname_reconcile(&f.db, &job)
        .await
        .unwrap();
    assert!(!dispatch_ready(&f.db, &job, &source).await.unwrap());
    assert_eq!(
        source.calls.load(Ordering::SeqCst),
        1,
        "stale jobs need no HTTP"
    );
}

#[tokio::test]
async fn second_only_rest_timestamp_matches_precise_gateway_membership() {
    let f = Fixture::new();
    let job = f.job().await;
    let source = Source {
        joined: 1_000_000_000,
        ..Source::default()
    };
    assert!(dispatch_ready(&f.db, &job, &source).await.unwrap());
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn later_same_second_timestamp_blocks_old_nickname_work() {
    let f = Fixture::new();
    let job = f.job().await;
    let source = Source {
        joined: JOINED_AT_NS + 100,
        ..Source::default()
    };
    assert!(!dispatch_ready(&f.db, &job, &source).await.unwrap());
    assert_eq!(
        db::nickname_reconcile_status(&f.db, 10)
            .await
            .unwrap()
            .unwrap()
            .state,
        "dormant"
    );
}

#[tokio::test]
async fn move_fence_defers_pending_nickname_without_discord_lookup() {
    let f = Fixture::new();
    let job = f.job().await;
    let destination = db::guild_stores::StoreId::new();
    db::guild_stores::register_store(
        &f.db,
        destination.clone(),
        db::guild_stores::StorePolicy::Dedicated { guild_id: 10 },
    )
    .await
    .unwrap();
    {
        use jmodguard::db::SqlExecutor as _;
        f.db.with_conn(move |conn| {
            conn.execute_bounded(
                "INSERT INTO guild_write_fences(
                    guild_id,generation,phase,destination_store_id,destination_revision,created_at
                 ) VALUES(?1,?2,'freeze',?3,?2,unixepoch())",
                libsql::params![10, format!("{:032x}", 9u128), destination.as_str()],
                64 * 1024 * 1024,
            )?;
            Ok(())
        })
    }
    .await
    .unwrap();
    assert!(
        db::pending_nickname_reconciles(&f.db, i64::MAX, 1)
            .await
            .unwrap()
            .is_empty()
    );
    let source = Source::default();
    assert!(!dispatch_ready(&f.db, &job, &source).await.unwrap());
    assert_eq!(source.calls.load(Ordering::SeqCst), 0);

    {
        use jmodguard::db::SqlExecutor as _;
        f.db.with_conn(|conn| {
            conn.execute_bounded(
                "DELETE FROM guild_write_fences WHERE guild_id=10",
                (),
                64 * 1024 * 1024,
            )?;
            Ok(())
        })
    }
    .await
    .unwrap();
    assert_eq!(
        db::pending_nickname_reconciles(&f.db, i64::MAX, 1)
            .await
            .unwrap(),
        vec![job.clone()]
    );
    assert!(dispatch_ready(&f.db, &job, &source).await.unwrap());
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn changed_membership_blocks_work_without_retiring_or_rebinding() {
    let f = Fixture::new();
    let job = f.job().await;
    let source = Source {
        joined: REJOINED_AT_NS,
        ..Source::default()
    };
    assert!(!dispatch_ready(&f.db, &job, &source).await.unwrap());
    assert_eq!(
        db::guild_enrollment(&f.db, 10).await.unwrap().unwrap().id,
        job.enrollment_id
    );
    assert_eq!(
        db::nickname_reconcile_status(&f.db, 10)
            .await
            .unwrap()
            .unwrap()
            .state,
        "dormant"
    );
    db::enroll_guild(&f.db, 10, REJOINED_AT_NS).await.unwrap();
    assert!(
        db::pending_nickname_reconciles(&f.db, i64::MAX, 1)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn missing_or_invalid_membership_never_dispatches_or_retires() {
    let f = Fixture::new();
    let job = f.job().await;
    for source in [
        Source {
            fail: true,
            ..Source::default()
        },
        Source {
            joined: 0,
            ..Source::default()
        },
    ] {
        assert!(dispatch_ready(&f.db, &job, &source).await.is_err());
        assert_eq!(
            db::guild_enrollment(&f.db, 10).await.unwrap().unwrap().id,
            job.enrollment_id
        );
        assert!(
            db::nickname_reconcile_can_dispatch(&f.db, &job)
                .await
                .unwrap()
        );
    }
}

#[tokio::test]
async fn departure_or_settings_change_during_http_blocks_dispatch() {
    for replace in [false, true] {
        let f = Fixture::new();
        let job = f.job().await;
        let source = Source {
            during: if replace {
                DuringRead::Replace(&f.db)
            } else {
                DuringRead::Retire(&f.db)
            },
            ..Source::default()
        };
        assert!(!dispatch_ready(&f.db, &job, &source).await.unwrap());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_nickname_edit_is_not_done_and_the_next_pass_skips_a_repeat_edit() {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::AtomicBool;
    use std::task::{Context, Poll};
    use std::time::Duration;

    struct Stuck {
        polled: Arc<AtomicBool>,
        started: Arc<tokio::sync::Notify>,
    }

    impl Future for Stuck {
        type Output = serenity::Result<()>;

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.polled.store(true, Ordering::SeqCst);
            self.started.notify_waiters();
            Poll::Pending
        }
    }

    let f = Fixture::new();
    let job = f.job().await;
    let app = crate::state::BotState::new(Default::default(), f.db.clone()).unwrap();
    assert!(
        db::claim_active_bot(
            &f.db,
            app.activation.instance(),
            chrono::Utc::now().timestamp_millis(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await
        .unwrap()
    );
    app.activation.mark_lock_held(true);
    let edits = Arc::new(AtomicUsize::new(0));
    let polled = Arc::new(AtomicBool::new(false));
    let started = Arc::new(tokio::sync::Notify::new());
    let notified = started.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    let task = {
        let app = Arc::clone(&app);
        let job = job.clone();
        let edits = Arc::clone(&edits);
        let polled = Arc::clone(&polled);
        let started = Arc::clone(&started);
        tokio::spawn(async move {
            apply_nickname_edit(
                &app,
                &job,
                || async { panic!("the first edit has no unknown outcome") },
                || {
                    edits.fetch_add(1, Ordering::SeqCst);
                    Stuck { polled, started }
                },
            )
            .await
        })
    };
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(2)) => panic!("edit was not polled"),
        _ = &mut notified => {}
    }
    assert!(polled.load(Ordering::SeqCst));
    db::release_active_bot(&f.db, app.activation.instance())
        .await
        .unwrap();
    app.activation.cut_off();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("cutoff must finish the nickname edit")
        .unwrap();
    assert_eq!(edits.load(Ordering::SeqCst), 1);
    let status = db::nickname_reconcile_status(&f.db, 10)
        .await
        .unwrap()
        .expect("a cancelled edit must not acknowledge the job");
    assert_eq!(status.state, "pending");
    assert_eq!(status.last_error_code.as_deref(), Some("outcome_unknown"));

    let next_job = db::pending_nickname_reconciles(&f.db, i64::MAX, 1)
        .await
        .unwrap()
        .remove(0);
    let next = crate::state::BotState::new(Default::default(), f.db.clone()).unwrap();
    assert!(
        db::claim_active_bot(
            &f.db,
            next.activation.instance(),
            chrono::Utc::now().timestamp_millis(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await
        .unwrap()
    );
    next.activation.mark_lock_held(true);
    let again = Arc::new(AtomicUsize::new(0));
    let again_task = Arc::clone(&again);
    let wanted = next_job.nickname.clone();
    apply_nickname_edit(
        &next,
        &next_job,
        || async move { Ok(Some(wanted)) },
        || {
            again_task.fetch_add(1, Ordering::SeqCst);
            async { Ok(()) }
        },
    )
    .await;
    assert_eq!(
        again.load(Ordering::SeqCst),
        0,
        "an already-applied nickname must not call edit_member"
    );
    assert!(
        db::nickname_reconcile_status(&f.db, 10)
            .await
            .unwrap()
            .is_none()
    );
}
