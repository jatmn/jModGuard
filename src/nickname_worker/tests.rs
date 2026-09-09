use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

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
        db::enroll_guild(&self.db, 10, 100).await.unwrap();
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
            joined: 100,
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
            DuringRead::Retire(db) => db::retire_guild_enrollment(db, 10, 100).await?,
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
async fn changed_membership_blocks_work_without_retiring_or_rebinding() {
    let f = Fixture::new();
    let job = f.job().await;
    let source = Source {
        joined: 101,
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
    db::enroll_guild(&f.db, 10, 101).await.unwrap();
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
