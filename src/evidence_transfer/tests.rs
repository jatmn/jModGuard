use super::*;
use crate::db::guild_stores::*;
use crate::db::{Db, GuildEnrollment};
use crate::db_compat::Connection;
use std::io::Cursor;
use std::os::unix::fs::symlink;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc,
};

struct Fixture {
    base: PathBuf,
    db: GuildDataStore,
    local: LocalEvidenceStore,
    scope: GuildDataScope,
    case: CaseId,
}
impl Fixture {
    async fn new() -> Self {
        let base =
            std::env::temp_dir().join(format!("jmodguard-transfer-{}", CaseId::new().as_str()));
        fs::DirBuilder::new().mode(0o700).create(&base).unwrap();
        let catalog = Db::open(&base.join("catalog.db"), 2).unwrap();
        let reg = register_store(&catalog, StoreId::new(), StorePolicy::Shared)
            .await
            .unwrap();
        let placement = assign_initial_placement(&catalog, 10, reg.id().clone())
            .await
            .unwrap();
        let db = GuildDataStore::initialize(
            GuildStoreOptions {
                read_only: false,
                path: base.join("guild.db"),
                url: String::new(),
                token: String::new(),
                pool_size: 2,
            },
            reg,
        )
        .unwrap();
        db.bind_initial_placement(placement.clone()).await.unwrap();
        let scope = GuildDataScope::new(
            placement,
            GuildEnrollment {
                guild_id: 10,
                id: CaseId::new().as_str().into(),
                joined_at_ns: 100,
            },
        )
        .unwrap();
        db.bind_initial_membership(scope.clone()).await.unwrap();
        let case = CaseId::new();
        db.record_case(
            scope.clone(),
            case.clone(),
            CaseData {
                source: CaseSource::DiscordAudit { entry_id: 1 },
                action: CaseAction::Ban,
                outcome: CaseOutcome::Uncertain,
                subject_id: 20,
                actor_id: None,
                occurred_at_ms: 100,
                reason: None,
                evidence: CaseEvidence::Unavailable,
            },
            CaseLimits::default(),
        )
        .await
        .unwrap();
        let local = LocalEvidenceStore::initialize(base.join("objects"), CaseId::new(), 2).unwrap();
        Self {
            base,
            db,
            local,
            scope,
            case,
        }
    }
    fn limits() -> EvidenceLimits {
        EvidenceLimits {
            max_items_per_case: 10,
            max_file_bytes: 100,
            max_case_bytes: 1000,
            max_page_size: 10,
        }
    }
    fn upload(&self, bytes: u64) -> EvidenceUpload {
        EvidenceUpload {
            target: EvidenceTarget {
                scope: self.scope.clone(),
                case_id: self.case.clone(),
                evidence_id: CaseId::new(),
            },
            request: EvidenceRequest {
                storage_id: self.local.storage_id.clone(),
                bytes,
                origin: EvidenceOrigin {
                    channel_id: 1,
                    message_id: 2,
                    attachment_id: 3,
                },
            },
            limits: Self::limits(),
        }
    }
    async fn job(&self, t: &EvidenceTarget) -> EvidenceJob {
        self.db
            .evidence_transfer_job(t.scope.clone(), t.case_id.clone(), t.evidence_id.clone())
            .await
            .unwrap()
    }
    async fn reserve(&self, upload: EvidenceUpload) -> EvidenceTarget {
        let t = upload.target;
        self.db
            .reserve_case_evidence(
                t.scope.clone(),
                t.case_id.clone(),
                t.evidence_id.clone(),
                upload.request,
                upload.limits,
            )
            .await
            .unwrap();
        t
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

struct Measured {
    inner: Cursor<Vec<u8>>,
    largest: Arc<AtomicUsize>,
}
impl Read for Measured {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        self.largest.fetch_max(b.len(), Ordering::SeqCst);
        self.inner.read(b)
    }
}

#[tokio::test]
async fn streams_verifies_and_removes_real_objects_with_bounded_reads() {
    let f = Fixture::new().await;
    let upload = f.upload(3);
    let t = upload.target.clone();
    let largest = Arc::new(AtomicUsize::new(0));
    assert_eq!(
        f.local
            .capture(
                f.db.clone(),
                upload,
                Measured {
                    inner: Cursor::new(b"abc".to_vec()),
                    largest: largest.clone()
                }
            )
            .await
            .unwrap(),
        EvidenceState::Available
    );
    assert_eq!(largest.load(Ordering::SeqCst), 2);
    let visible =
        f.db.available_case_evidence(t.scope.clone(), t.case_id.clone(), t.evidence_id.clone())
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        visible.sha256,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let paths = f.local.paths(&visible.job, false).unwrap();
    assert_eq!(fs::read(&paths[0]).unwrap(), b"abc");
    assert!(paths[1].exists());
    assert!(f.local.cleanup(f.db.clone(), t.clone()).await.is_err());
    f.db.remove_case(t.scope.clone(), t.case_id.clone())
        .await
        .unwrap();
    assert!(f.local.cleanup(f.db.clone(), t.clone()).await.unwrap());
    assert!(!f.local.cleanup(f.db.clone(), t.clone()).await.unwrap());
    assert!(paths.iter().all(|p| !p.exists()));
    assert!(!f.local.root.join("evidence").exists());
    assert_eq!(f.job(&t).await.state, EvidenceState::Erased);
}

#[tokio::test]
async fn oversize_short_and_failed_streams_never_become_available() {
    let f = Fixture::new().await;
    for bytes in [b"abcd".as_slice(), b"ab".as_slice()] {
        let upload = f.upload(3);
        let t = upload.target.clone();
        assert!(
            f.local
                .capture(f.db.clone(), upload, Cursor::new(bytes.to_vec()))
                .await
                .is_err()
        );
        assert_eq!(f.job(&t).await.state, EvidenceState::CleanupPending);
        assert!(f.local.cleanup(f.db.clone(), t).await.unwrap());
    }
    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("interrupted source"))
        }
    }
    let upload = f.upload(3);
    let t = upload.target.clone();
    assert!(f.local.capture(f.db.clone(), upload, Broken).await.is_err());
    assert_eq!(f.job(&t).await.state, EvidenceState::CleanupPending);
    f.local.cleanup(f.db.clone(), t).await.unwrap();
}

#[tokio::test]
async fn completed_file_survives_failed_sql_settlement_and_reopens_without_upload() {
    let f = Fixture::new().await;
    let upload = f.upload(3);
    let t = upload.target.clone();
    let raw = Connection::open(f.base.join("guild.db")).unwrap();
    raw.execute_batch_bounded("CREATE TRIGGER fail_settle BEFORE UPDATE OF state ON guild_case_evidence WHEN NEW.state=1 BEGIN SELECT RAISE(ABORT,'injected settlement failure'); END", 64 * 1024 * 1024).unwrap();
    assert!(
        f.local
            .capture(f.db.clone(), upload, Cursor::new(b"abc".to_vec()))
            .await
            .is_err()
    );
    assert_eq!(f.job(&t).await.state, EvidenceState::Capturing);
    raw.execute_batch_bounded("DROP TRIGGER fail_settle", 64 * 1024 * 1024)
        .unwrap();
    let reopened =
        LocalEvidenceStore::open(f.local.root.clone(), f.local.storage_id.clone(), 1).unwrap();
    assert_eq!(
        reopened.reconcile(f.db.clone(), t.clone()).await.unwrap(),
        EvidenceState::Available
    );
    let repeat = EvidenceUpload {
        target: t.clone(),
        ..f.upload(3)
    };
    assert!(
        reopened
            .capture(f.db.clone(), repeat, Cursor::new(b"xxx".to_vec()))
            .await
            .is_err()
    );
    assert_eq!(
        fs::read(&reopened.paths(&f.job(&t).await, false).unwrap()[0]).unwrap(),
        b"abc"
    );
}

#[tokio::test]
async fn incomplete_and_corrupted_receipts_recover_to_cleanup_without_reupload() {
    let f = Fixture::new().await;
    let incomplete = f.reserve(f.upload(3)).await;
    let job = f.job(&incomplete).await;
    let paths = f.local.paths(&job, true).unwrap();
    new_file(&paths[0]).unwrap().write_all(b"ab").unwrap();
    assert_eq!(
        f.local
            .reconcile(f.db.clone(), incomplete.clone())
            .await
            .unwrap(),
        EvidenceState::CleanupPending
    );
    f.local.cleanup(f.db.clone(), incomplete).await.unwrap();
    let complete = f.reserve(f.upload(3)).await;
    let job = f.job(&complete).await;
    f.local
        .write_object(&job, &mut Cursor::new(b"abc"))
        .unwrap();
    let paths = f.local.paths(&job, false).unwrap();
    OpenOptions::new()
        .write(true)
        .open(&paths[0])
        .unwrap()
        .write_all(b"xyz")
        .unwrap();
    assert!(
        f.local
            .reconcile(f.db.clone(), complete.clone())
            .await
            .is_err()
    );
    assert_eq!(f.job(&complete).await.state, EvidenceState::CleanupPending);
    f.local.cleanup(f.db.clone(), complete).await.unwrap();
    let absent = f.reserve(f.upload(3)).await;
    assert_eq!(
        f.local
            .reconcile(f.db.clone(), absent.clone())
            .await
            .unwrap(),
        EvidenceState::CleanupPending
    );
    f.local.cleanup(f.db.clone(), absent).await.unwrap();
}

struct BlockingSource {
    started: Option<mpsc::Sender<()>>,
    resume: mpsc::Receiver<()>,
    data: Cursor<Vec<u8>>,
}
impl Read for BlockingSource {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        if let Some(started) = self.started.take() {
            started.send(()).unwrap();
            self.resume.recv().unwrap();
        }
        self.data.read(b)
    }
}
fn child_probe(local: &LocalEvidenceStore, busy: bool) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "evidence_transfer::tests::child_resource_lock_probe",
            "--nocapture",
        ])
        .env("JMODGUARD_LOCAL_TEST_ROOT", &local.root)
        .env("JMODGUARD_LOCAL_TEST_ID", local.storage_id.as_str())
        .env("JMODGUARD_LOCAL_TEST_BUSY", if busy { "1" } else { "0" })
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
#[test]
#[ignore = "subprocess lock probe invoked by the cancellation regression"]
fn child_resource_lock_probe() {
    let root = std::env::var_os("JMODGUARD_LOCAL_TEST_ROOT").unwrap();
    let id = CaseId::try_from(std::env::var("JMODGUARD_LOCAL_TEST_ID").unwrap()).unwrap();
    let busy = std::env::var("JMODGUARD_LOCAL_TEST_BUSY").unwrap() == "1";
    assert_eq!(LocalEvidenceStore::open(root.into(), id, 2).is_err(), busy);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn caller_cancellation_and_removal_cannot_release_a_live_writer_lock() {
    let f = Fixture::new().await;
    let independent =
        LocalEvidenceStore::open(f.local.root.clone(), f.local.storage_id.clone(), 2).unwrap();
    let upload = f.upload(3);
    let t = upload.target.clone();
    let (started_tx, started_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let local = f.local.clone();
    let db = f.db.clone();
    let task = tokio::spawn(async move {
        local
            .capture(
                db,
                upload,
                BlockingSource {
                    started: Some(started_tx),
                    resume: resume_rx,
                    data: Cursor::new(b"abc".to_vec()),
                },
            )
            .await
    });
    // Yield so the async caller can launch its blocking operation.
    tokio::task::yield_now().await;
    started_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .unwrap();
    f.db.remove_case(t.scope.clone(), t.case_id.clone())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(f.local.cleanup(f.db.clone(), t.clone()).await.is_err());
    assert!(independent.cleanup(f.db.clone(), t.clone()).await.is_err());
    child_probe(&f.local, true);
    resume_tx.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if f.job(&t).await.state == EvidenceState::CleanupPending
                && independent.cleanup(f.db.clone(), t.clone()).await.is_ok()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    child_probe(&f.local, false);
    assert_eq!(f.job(&t).await.state, EvidenceState::Erased);
}

#[tokio::test]
async fn resource_and_path_guards_refuse_foreign_or_symlinked_storage() {
    let f = Fixture::new().await;
    assert!(LocalEvidenceStore::open(f.local.root.clone(), CaseId::new(), 2).is_err());
    let foreign = f.base.join("backups");
    fs::DirBuilder::new().mode(0o700).create(&foreign).unwrap();
    new_file(&foreign.join("backup"))
        .unwrap()
        .write_all(b"retain")
        .unwrap();
    assert!(LocalEvidenceStore::initialize(foreign.clone(), CaseId::new(), 2).is_err());
    assert!(!foreign.join("lock").exists());
    symlink(&foreign, f.base.join("alias")).unwrap();
    assert!(LocalEvidenceStore::initialize(f.base.join("alias"), CaseId::new(), 2).is_err());
    symlink(&foreign, f.local.root.join("evidence")).unwrap();
    let upload = f.upload(3);
    let t = upload.target.clone();
    assert!(
        f.local
            .capture(f.db.clone(), upload, Cursor::new(b"abc".to_vec()))
            .await
            .is_err()
    );
    assert!(f.local.cleanup(f.db.clone(), t.clone()).await.is_err());
    assert_eq!(fs::read(foreign.join("backup")).unwrap(), b"retain");
    fs::remove_file(f.local.root.join("evidence")).unwrap();
    f.local.cleanup(f.db.clone(), t).await.unwrap();
    let mut upload = f.upload(3);
    upload.request.storage_id = CaseId::new();
    let t = upload.target.clone();
    assert!(
        f.local
            .capture(f.db.clone(), upload, Cursor::new(b"abc".to_vec()))
            .await
            .is_err()
    );
    assert!(
        f.db.evidence_transfer_job(t.scope, t.case_id, t.evidence_id)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn cleanup_failure_keeps_journal_pending_and_preserves_other_objects() {
    let f = Fixture::new().await;
    let one = f.upload(3);
    let a = one.target.clone();
    let two = f.upload(3);
    let b = two.target.clone();
    f.local
        .capture(f.db.clone(), one, Cursor::new(b"abc".to_vec()))
        .await
        .unwrap();
    f.local
        .capture(f.db.clone(), two, Cursor::new(b"xyz".to_vec()))
        .await
        .unwrap();
    let a_paths = f.local.paths(&f.job(&a).await, false).unwrap();
    let b_paths = f.local.paths(&f.job(&b).await, false).unwrap();
    // Wrong case identity never permits deleting a matching evidence ID.
    assert!(
        f.local
            .cleanup(
                f.db.clone(),
                EvidenceTarget {
                    case_id: CaseId::new(),
                    ..a.clone()
                }
            )
            .await
            .is_err()
    );
    f.db.remove_case(a.scope.clone(), a.case_id.clone())
        .await
        .unwrap();
    let hardlink = f.base.join("linked-data");
    fs::hard_link(&a_paths[0], &hardlink).unwrap();
    assert!(f.local.cleanup(f.db.clone(), a.clone()).await.is_err());
    assert_eq!(f.job(&a).await.state, EvidenceState::CleanupPending);
    fs::remove_file(hardlink).unwrap();
    let raw = Connection::open(f.base.join("guild.db")).unwrap();
    raw.execute_batch_bounded("CREATE TRIGGER fail_erase BEFORE UPDATE OF state ON guild_case_evidence WHEN NEW.state=3 BEGIN SELECT RAISE(ABORT,'injected deletion acknowledgment failure'); END", 64 * 1024 * 1024).unwrap();
    assert!(f.local.cleanup(f.db.clone(), a.clone()).await.is_err());
    assert!(a_paths.iter().all(|path| !path.exists()));
    assert_eq!(f.job(&a).await.state, EvidenceState::CleanupPending);
    raw.execute_batch_bounded("DROP TRIGGER fail_erase", 64 * 1024 * 1024)
        .unwrap();
    assert!(f.local.cleanup(f.db.clone(), a).await.unwrap());
    assert_eq!(fs::read(&b_paths[0]).unwrap(), b"xyz");
    assert!(b_paths[1].exists());
    f.local.cleanup(f.db.clone(), b).await.unwrap();
}

#[tokio::test]
async fn closed_membership_cancels_recovery_and_allows_final_owned_purge() {
    let f = Fixture::new().await;
    let t = f.reserve(f.upload(3)).await;
    let job = f.job(&t).await;
    f.local
        .write_object(&job, &mut Cursor::new(b"abc"))
        .unwrap();
    f.db.close_membership(t.scope.clone()).await.unwrap();
    assert!(
        f.db.purge_closed_cases(t.scope.clone(), 1, CaseLimits::default())
            .await
            .is_err()
    );
    assert_eq!(
        f.local.reconcile(f.db.clone(), t.clone()).await.unwrap(),
        EvidenceState::CleanupPending
    );
    assert!(f.local.cleanup(f.db.clone(), t.clone()).await.unwrap());
    assert_eq!(
        f.db.purge_closed_cases(t.scope, 1, CaseLimits::default())
            .await
            .unwrap(),
        1
    );
    assert!(!f.local.root.join("evidence").exists());
}

#[tokio::test]
async fn available_download_rechecks_resource_identity() {
    let f = Fixture::new().await;
    let upload = f.upload(3);
    let target = upload.target.clone();
    f.local
        .capture(f.db.clone(), upload, Cursor::new(b"abc".to_vec()))
        .await
        .unwrap();
    let available =
        f.db.available_case_evidence(f.scope.clone(), f.case.clone(), target.evidence_id)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        f.local
            .read_available(&available.job, &available.sha256)
            .unwrap(),
        b"abc"
    );
    let marker = f.local.root.join("identity");
    let original = fs::read(&marker).unwrap();
    fs::write(&marker, b"wrong resource\n").unwrap();
    assert!(
        f.local
            .read_available(&available.job, &available.sha256)
            .is_err()
    );
    fs::write(marker, original).unwrap();
}
