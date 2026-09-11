use super::*;
use crate::db::guild_stores::*;
use crate::db::{Db, GuildEnrollment};
use crate::db_compat::{Connection, SqlExecutor};
use hyper::service::service_fn;
use std::collections::HashMap;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Mutex;
use tokio::sync::Notify;

#[derive(Default)]
struct Remote {
    marker: String,
    objects: HashMap<String, Vec<u8>>,
    puts: usize,
    deletes: usize,
    requests: usize,
    put_status: Option<StatusCode>,
    drop_put: bool,
    delay_put: Option<Arc<Notify>>,
    fail_get: bool,
    keep_deleted: bool,
    marker_redirect: bool,
    last_checksum: String,
}
struct Fixture {
    base: PathBuf,
    db: GuildDataStore,
    scope: GuildDataScope,
    case: CaseId,
    backend: BunnyEvidenceStore,
    remote: Arc<Mutex<Remote>>,
    server: tokio::task::JoinHandle<()>,
}
impl Fixture {
    async fn new() -> Self {
        Self::with_database(String::new(), String::new()).await
    }
    async fn with_database(url: String, token: String) -> Self {
        let base = std::env::temp_dir().join(format!("jmodguard-bunny-{}", CaseId::new().as_str()));
        std::fs::create_dir(&base).unwrap();
        let catalog = Db::open(&base.join("catalog.db"), 2).unwrap();
        let reg = register_store(&catalog, StoreId::new(), StorePolicy::Shared)
            .await
            .unwrap();
        let placement = assign_initial_placement(&catalog, 10, reg.id().clone())
            .await
            .unwrap();
        let db = GuildDataStore::initialize(
            GuildStoreOptions {
                path: base.join("guild.db"),
                url,
                token,
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
        let id = CaseId::new();
        let remote = Arc::new(Mutex::new(Remote {
            marker: format!("jmodguard-bunny-evidence-1\n{}\n", id.as_str()),
            ..Default::default()
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = remote.clone();
        let server = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted=listener.accept()=>{
                        let (stream,_)=accepted.unwrap();
                        let state=state.clone();
                        tasks.spawn(async move {
                            let _=hyper::server::conn::Http::new().serve_connection(stream,service_fn(move |request|respond(request,state.clone()))).await;
                        });
                    }
                    _=tasks.join_next(), if !tasks.is_empty()=>{}
                }
            }
        });
        let mut backend = BunnyEvidenceStore::new(
            id,
            "storage.bunnycdn.com",
            "evidence-test",
            "test-secret",
            BunnyTransferLimits {
                buffer_bytes: 2,
                request_timeout: Duration::from_secs(5),
            },
        )
        .unwrap();
        // Private test-only replacement; production construction cannot use HTTP/custom hosts.
        backend.base = format!("http://{address}/evidence-test/");
        Self {
            base,
            db,
            scope,
            case,
            backend,
            remote,
            server,
        }
    }
    fn upload(&self) -> EvidenceUpload {
        EvidenceUpload {
            target: EvidenceTarget {
                scope: self.scope.clone(),
                case_id: self.case.clone(),
                evidence_id: CaseId::new(),
            },
            request: EvidenceRequest {
                storage_id: self.backend.storage_id.clone(),
                bytes: 3,
                origin: EvidenceOrigin {
                    channel_id: 1,
                    message_id: 2,
                    attachment_id: 3,
                },
            },
            limits: EvidenceLimits {
                max_items_per_case: 10,
                max_file_bytes: 100,
                max_case_bytes: 1000,
                max_page_size: 10,
            },
        }
    }
    async fn job(&self, t: &EvidenceTarget) -> EvidenceJob {
        self.db
            .evidence_transfer_job(t.scope.clone(), t.case_id.clone(), t.evidence_id.clone())
            .await
            .unwrap()
    }
    async fn remove(&self) {
        self.db
            .remove_case(self.scope.clone(), self.case.clone())
            .await
            .unwrap();
    }
    fn raw(&self) -> Connection {
        Connection::open(self.base.join("guild.db")).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.base);
    }
}
async fn respond(
    request: Request<Body>,
    remote: Arc<Mutex<Remote>>,
) -> std::result::Result<Response<Body>, std::io::Error> {
    assert_eq!(request.headers()["AccessKey"], "test-secret");
    let path = request.uri().path().to_owned();
    assert!(request.uri().query().is_none());
    let method = request.method().clone();
    remote.lock().unwrap().requests += 1;
    if path == "/evidence-test/jmodguard-evidence-identity" {
        assert_eq!(method, Method::GET);
        let state = remote.lock().unwrap();
        return Ok(if state.marker_redirect {
            Response::builder()
                .status(302)
                .header("Location", "http://127.0.0.1:1/credential-trap")
                .body(Body::empty())
                .unwrap()
        } else {
            Response::new(Body::from(state.marker.clone()))
        });
    }
    assert!(path.starts_with("/evidence-test/evidence/10/"));
    assert!(!path.ends_with('/'));
    if method == Method::PUT {
        assert_eq!(request.headers()[CONTENT_LENGTH], "3");
        assert_eq!(request.headers()[CONTENT_TYPE], "application/octet-stream");
        let checksum = request.headers()["Checksum"].to_str().unwrap().to_string();
        let bytes = hyper::body::to_bytes(request.into_body())
            .await
            .map_err(std::io::Error::other)?
            .to_vec();
        let (delay, status, drop_put) = {
            let mut state = remote.lock().unwrap();
            state.puts += 1;
            state.last_checksum = checksum;
            (state.delay_put.clone(), state.put_status, state.drop_put)
        };
        if let Some(delay) = delay {
            let state = remote.clone();
            // Provider-side work may outlive the HTTP connection after client timeout.
            tokio::spawn(async move {
                delay.notified().await;
                state.lock().unwrap().objects.insert(path, bytes);
            })
            .await
            .unwrap();
        } else {
            remote.lock().unwrap().objects.insert(path, bytes);
        }
        if drop_put {
            return Err(std::io::Error::other("lost PUT response"));
        }
        return Ok(Response::builder()
            .status(status.unwrap_or(StatusCode::CREATED))
            .body(Body::empty())
            .unwrap());
    }
    let mut state = remote.lock().unwrap();
    if method == Method::DELETE {
        state.deletes += 1;
        if !state.keep_deleted {
            state.objects.remove(&path);
        }
        return Ok(Response::new(Body::empty()));
    }
    assert_eq!(method, Method::GET);
    if state.fail_get {
        return Ok(Response::builder().status(503).body(Body::empty()).unwrap());
    }
    Ok(match state.objects.get(&path) {
        Some(data) => Response::new(Body::from(data.clone())),
        None => Response::builder().status(404).body(Body::empty()).unwrap(),
    })
}

#[tokio::test]
async fn real_http_capture_hash_cleanup_and_retry_preserve_other_objects() {
    let f = Fixture::new().await;
    let one = f.upload();
    let a = one.target.clone();
    assert_eq!(
        f.backend
            .capture(f.db.clone(), one, Cursor::new(b"abc".to_vec()))
            .await
            .unwrap(),
        EvidenceState::Available
    );
    assert_eq!(
        f.remote.lock().unwrap().last_checksum,
        "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD"
    );
    let two = f.upload();
    let b = two.target.clone();
    f.backend
        .capture(f.db.clone(), two, Cursor::new(b"xyz".to_vec()))
        .await
        .unwrap();
    assert!(f.backend.cleanup(f.db.clone(), a.clone()).await.is_err());
    f.remove().await;
    let raw = f.raw();
    assert_eq!(
        raw.query_row(
            "SELECT COUNT(*) FROM guild_bunny_evidence WHERE sha256 IS NOT NULL",
            (),
            |r| Ok(r.get::<i64>(0)?)
        )
        .unwrap(),
        0
    );
    raw.execute_batch("CREATE TRIGGER fail_ack BEFORE UPDATE OF state ON guild_case_evidence WHEN NEW.state=3 BEGIN SELECT RAISE(ABORT,'ack failed'); END").unwrap();
    assert!(f.backend.cleanup(f.db.clone(), a.clone()).await.is_err());
    assert_eq!(f.job(&a).await.state, EvidenceState::CleanupPending);
    raw.execute_batch("DROP TRIGGER fail_ack").unwrap();
    assert!(f.backend.cleanup(f.db.clone(), a.clone()).await.unwrap());
    assert!(!f.backend.cleanup(f.db.clone(), a).await.unwrap());
    assert_eq!(f.remote.lock().unwrap().objects.len(), 1);
    f.backend.cleanup(f.db.clone(), b).await.unwrap();
    f.db.close_membership(f.scope.clone()).await.unwrap();
    assert_eq!(
        f.db.purge_closed_cases(f.scope.clone(), 1, CaseLimits::default())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        raw.query_row("SELECT COUNT(*) FROM guild_bunny_evidence", (), |r| Ok(
            r.get::<i64>(0)?
        ))
        .unwrap(),
        0
    );
}

#[tokio::test]
async fn accepted_upload_recovers_verification_and_failed_settlement_without_reupload() {
    let f = Fixture::new().await;
    f.remote.lock().unwrap().fail_get = true;
    let upload = f.upload();
    let t = upload.target.clone();
    assert!(
        f.backend
            .capture(f.db.clone(), upload, Cursor::new(b"abc".to_vec()))
            .await
            .is_err()
    );
    assert_eq!(f.job(&t).await.state, EvidenceState::Capturing);
    f.remote.lock().unwrap().fail_get = false;
    assert!(
        f.db.settle_evidence_capture(
            t.scope.clone(),
            t.case_id.clone(),
            t.evidence_id.clone(),
            EvidenceCapture::Stored {
                bytes: 3,
                sha256: "a".repeat(64)
            }
        )
        .await
        .is_err()
    );
    let raw = f.raw();
    raw.execute_batch("CREATE TRIGGER fail_settle BEFORE UPDATE OF state ON guild_case_evidence WHEN NEW.state=1 BEGIN SELECT RAISE(ABORT,'settle failed'); END").unwrap();
    let mut reopened = f.backend.clone();
    reopened.admission = Arc::new(Semaphore::new(1));
    assert!(reopened.reconcile(f.db.clone(), t.clone()).await.is_err());
    raw.execute_batch("DROP TRIGGER fail_settle").unwrap();
    assert_eq!(
        reopened.reconcile(f.db.clone(), t.clone()).await.unwrap(),
        EvidenceState::Available
    );
    let repeat = EvidenceUpload {
        target: t.clone(),
        ..f.upload()
    };
    assert!(
        reopened
            .capture(f.db.clone(), repeat, Cursor::new(Vec::<u8>::new()))
            .await
            .unwrap_err()
            .to_string()
            .contains("existing reservation")
    );
    assert_eq!(f.remote.lock().unwrap().puts, 1);
}

#[tokio::test]
async fn lost_acceptance_receipt_never_promotes_matching_remote_bytes() {
    let f = Fixture::new().await;
    let raw = f.raw();
    raw.execute_batch("CREATE TRIGGER fail_accept BEFORE UPDATE OF accepted ON guild_bunny_evidence BEGIN SELECT RAISE(ABORT,'accept failed'); END").unwrap();
    let upload = f.upload();
    let t = upload.target.clone();
    assert!(
        f.backend
            .capture(f.db.clone(), upload, Cursor::new(b"abc".to_vec()))
            .await
            .is_err()
    );
    raw.execute_batch("DROP TRIGGER fail_accept").unwrap();
    assert_eq!(f.remote.lock().unwrap().objects.len(), 1);
    assert!(f.backend.reconcile(f.db.clone(), t.clone()).await.is_err());
    f.remove().await;
    assert!(f.backend.cleanup(f.db.clone(), t.clone()).await.is_err());
    assert!(
        f.db.settle_evidence_capture(
            t.scope.clone(),
            t.case_id.clone(),
            t.evidence_id.clone(),
            EvidenceCapture::Discarded
        )
        .await
        .is_err()
    );
    assert_eq!(f.job(&t).await.state, EvidenceState::Capturing);
    assert_eq!(f.remote.lock().unwrap().deletes, 0);
}

#[tokio::test]
async fn timed_out_put_cannot_be_erased_before_a_late_write() {
    let mut f = Fixture::new().await;
    f.backend.limits.request_timeout = Duration::from_millis(100);
    let release = Arc::new(Notify::new());
    f.remote.lock().unwrap().delay_put = Some(release.clone());
    let upload = f.upload();
    let t = upload.target.clone();
    assert!(
        f.backend
            .capture(f.db.clone(), upload, Cursor::new(b"abc".to_vec()))
            .await
            .is_err()
    );
    assert_eq!(f.remote.lock().unwrap().puts, 1);
    assert!(f.remote.lock().unwrap().objects.is_empty()); // An early 404 would be misleading.
    f.remove().await;
    assert!(f.backend.reconcile(f.db.clone(), t.clone()).await.is_err());
    assert!(f.backend.cleanup(f.db.clone(), t.clone()).await.is_err());
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        while f.remote.lock().unwrap().objects.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(f.job(&t).await.state, EvidenceState::Capturing);
    assert!(f.backend.cleanup(f.db.clone(), t).await.is_err());
    assert_eq!(f.remote.lock().unwrap().deletes, 0);
}

#[tokio::test]
async fn cancellation_keeps_started_transfer_alive_and_independent_cleanup_blocked() {
    let f = Fixture::new().await;
    let release = Arc::new(Notify::new());
    f.remote.lock().unwrap().delay_put = Some(release.clone());
    let upload = f.upload();
    let t = upload.target.clone();
    let backend = f.backend.clone();
    let db = f.db.clone();
    let task = tokio::spawn(async move {
        backend
            .capture(db, upload, Cursor::new(b"abc".to_vec()))
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while f.remote.lock().unwrap().puts == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    f.remove().await;
    let mut independent = f.backend.clone();
    independent.admission = Arc::new(Semaphore::new(1));
    assert!(independent.cleanup(f.db.clone(), t.clone()).await.is_err());
    assert!(f.backend.cleanup(f.db.clone(), t.clone()).await.is_err());
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if f.job(&t).await.state == EvidenceState::CleanupPending
                && f.backend.cleanup(f.db.clone(), t.clone()).await.is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(f.job(&t).await.state, EvidenceState::Erased);
}

#[tokio::test]
async fn invalid_source_marker_or_binding_never_sends_an_unauthorized_put() {
    let f = Fixture::new().await;
    for data in [b"ab".as_slice(), b"abcd".as_slice()] {
        let upload = f.upload();
        let t = upload.target.clone();
        assert!(
            f.backend
                .capture(f.db.clone(), upload, Cursor::new(data.to_vec()))
                .await
                .is_err()
        );
        assert!(
            f.db.find_evidence_transfer_job(t.scope, t.case_id, t.evidence_id)
                .await
                .unwrap()
                .is_none()
        );
    }
    f.remote.lock().unwrap().marker = "foreign".into();
    assert!(
        f.backend
            .capture(f.db.clone(), f.upload(), Cursor::new(b"abc".to_vec()))
            .await
            .is_err()
    );
    f.remote.lock().unwrap().marker_redirect = true;
    assert!(
        f.backend
            .capture(f.db.clone(), f.upload(), Cursor::new(b"abc".to_vec()))
            .await
            .unwrap_err()
            .to_string()
            .contains("marker unavailable")
    );
    assert_eq!(f.remote.lock().unwrap().puts, 0);
    for host in [
        "localhost",
        "storage.bunnycdn.com.evil.test",
        "https://storage.bunnycdn.com",
        "storage.bunnycdn.com:443",
    ] {
        assert!(
            BunnyEvidenceStore::new(CaseId::new(), host, "zone", "secret", f.backend.limits)
                .is_err()
        );
    }
    for zone in ["../backup", "zone/backup", "zone?x=1", "zone%2fbackup", ""] {
        assert!(
            BunnyEvidenceStore::new(
                CaseId::new(),
                "storage.bunnycdn.com",
                zone,
                "secret",
                f.backend.limits
            )
            .is_err()
        );
    }
}

#[tokio::test]
async fn corrupt_object_failed_absence_and_changed_binding_stay_pending() {
    let f = Fixture::new().await;
    f.remote.lock().unwrap().fail_get = true;
    let upload = f.upload();
    let t = upload.target.clone();
    assert!(
        f.backend
            .capture(f.db.clone(), upload, Cursor::new(b"abc".to_vec()))
            .await
            .is_err()
    );
    {
        let mut state = f.remote.lock().unwrap();
        state.fail_get = false;
        *state.objects.values_mut().next().unwrap() = b"xyz".to_vec();
    }
    assert!(f.backend.reconcile(f.db.clone(), t.clone()).await.is_err());
    let mut wrong = f.backend.clone();
    wrong.base.push_str("elsewhere/");
    assert!(
        wrong
            .reconcile(f.db.clone(), t.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("binding changed")
    );
    f.remove().await;
    f.remote.lock().unwrap().keep_deleted = true;
    assert!(f.backend.cleanup(f.db.clone(), t.clone()).await.is_err());
    assert_eq!(f.job(&t).await.state, EvidenceState::CleanupPending);
    f.remote.lock().unwrap().keep_deleted = false;
    assert!(f.backend.cleanup(f.db.clone(), t).await.unwrap());
}

#[tokio::test]
async fn failed_or_redirected_put_is_not_retried_or_settled() {
    for status in [
        StatusCode::TEMPORARY_REDIRECT,
        StatusCode::BAD_REQUEST,
        StatusCode::UNAUTHORIZED,
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::INTERNAL_SERVER_ERROR,
    ] {
        let f = Fixture::new().await;
        f.remote.lock().unwrap().put_status = Some(status);
        let upload = f.upload();
        let t = upload.target.clone();
        let error = f
            .backend
            .capture(f.db.clone(), upload, Cursor::new(b"abc".to_vec()))
            .await
            .unwrap_err();
        assert!(!format!("{error:?}").contains("test-secret"));
        assert!(f.backend.reconcile(f.db.clone(), t.clone()).await.is_err());
        assert_eq!(f.job(&t).await.state, EvidenceState::Capturing);
        assert_eq!(f.remote.lock().unwrap().puts, 1);
    }
    let f = Fixture::new().await;
    f.remote.lock().unwrap().drop_put = true;
    let upload = f.upload();
    let t = upload.target.clone();
    assert!(
        f.backend
            .capture(f.db.clone(), upload, Cursor::new(b"abc".to_vec()))
            .await
            .is_err()
    );
    assert!(f.backend.reconcile(f.db.clone(), t).await.is_err());
    assert_eq!(f.remote.lock().unwrap().puts, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a distinct empty disposable loopback libSQL server"]
async fn remote_bunny_journal_recovers_commits() {
    use crate::db::test_drop_next_commit_response;
    let url = std::env::var("JMODGUARD_REMOTE_TEST_URL").unwrap();
    let parsed = url::Url::parse(&url).unwrap();
    assert_eq!(parsed.scheme(), "http");
    assert_eq!(parsed.host_str(), Some("127.0.0.1"));
    let token = std::env::var("JMODGUARD_REMOTE_TEST_TOKEN").unwrap();
    assert_eq!(token, "local-ci-token");
    let f = Fixture::with_database(url, token).await;
    let upload = f.upload();
    let t = upload.target.clone();
    test_drop_next_commit_response();
    let reserved =
        f.db.reserve_case_evidence(
            t.scope.clone(),
            t.case_id.clone(),
            t.evidence_id.clone(),
            upload.request,
            upload.limits,
        )
        .await
        .unwrap();
    assert!(reserved.created);
    let hash = f
        .backend
        .source_hash(&mut Cursor::new(b"abc"), 3, None)
        .await
        .unwrap();
    test_drop_next_commit_response();
    f.db.start_bunny_transfer(
        t.scope.clone(),
        t.case_id.clone(),
        t.evidence_id.clone(),
        f.backend.base.clone(),
        hash.clone(),
    )
    .await
    .unwrap();
    assert!(
        !f.db
            .bunny_transfer(t.scope.clone(), t.case_id.clone(), t.evidence_id.clone())
            .await
            .unwrap()
            .accepted
    );
    let mut request = f
        .backend
        .request(Method::PUT, &reserved.job.object_key, Body::from("abc"))
        .unwrap();
    request
        .headers_mut()
        .insert(CONTENT_LENGTH, HeaderValue::from_static("3"));
    request.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    request.headers_mut().insert(
        "Checksum",
        HeaderValue::from_str(&hash.to_ascii_uppercase()).unwrap(),
    );
    assert_eq!(
        f.backend.send(request).await.unwrap().status(),
        StatusCode::CREATED
    );
    test_drop_next_commit_response();
    f.db.accept_bunny_transfer(t.scope.clone(), t.case_id.clone(), t.evidence_id.clone())
        .await
        .unwrap();
    test_drop_next_commit_response();
    assert_eq!(
        f.backend.reconcile(f.db.clone(), t.clone()).await.unwrap(),
        EvidenceState::Available
    );
    test_drop_next_commit_response();
    f.remove().await;
    assert!(
        f.db.bunny_transfer(t.scope.clone(), t.case_id.clone(), t.evidence_id.clone())
            .await
            .unwrap()
            .sha256
            .is_none()
    );
    test_drop_next_commit_response();
    assert!(f.backend.cleanup(f.db.clone(), t.clone()).await.unwrap());
    assert!(!f.backend.cleanup(f.db.clone(), t.clone()).await.unwrap());
    f.db.close_membership(t.scope.clone()).await.unwrap();
    test_drop_next_commit_response();
    assert_eq!(
        f.db.purge_closed_cases(t.scope.clone(), 1, CaseLimits::default())
            .await
            .unwrap(),
        1
    );
    assert!(
        f.db.bunny_transfer(t.scope, t.case_id, t.evidence_id)
            .await
            .is_err()
    );
    assert!(f.remote.lock().unwrap().objects.is_empty());
}

struct MeasuredSource {
    inner: Cursor<Vec<u8>>,
    maximum: Arc<std::sync::atomic::AtomicUsize>,
    change_on_rewind: bool,
    seeks: usize,
}
impl AsyncRead for MeasuredSource {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.maximum
            .fetch_max(buf.remaining(), std::sync::atomic::Ordering::SeqCst);
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}
impl AsyncSeek for MeasuredSource {
    fn start_seek(mut self: std::pin::Pin<&mut Self>, position: SeekFrom) -> std::io::Result<()> {
        self.seeks += 1;
        if self.change_on_rewind && self.seeks == 2 {
            self.inner = Cursor::new(b"xyz".to_vec());
        }
        std::pin::Pin::new(&mut self.inner).start_seek(position)
    }
    fn poll_complete(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<u64>> {
        std::pin::Pin::new(&mut self.inner).poll_complete(cx)
    }
}
#[tokio::test]
async fn transfer_reads_are_bounded_and_changed_sources_cannot_become_available() {
    for change_on_rewind in [false, true] {
        let f = Fixture::new().await;
        let maximum = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let source = MeasuredSource {
            inner: Cursor::new(b"abc".to_vec()),
            maximum: maximum.clone(),
            change_on_rewind,
            seeks: 0,
        };
        let upload = f.upload();
        let t = upload.target.clone();
        let result = f.backend.capture(f.db.clone(), upload, source).await;
        assert_eq!(result.is_err(), change_on_rewind);
        assert_eq!(maximum.load(std::sync::atomic::Ordering::SeqCst), 2);
        if change_on_rewind {
            assert_eq!(f.job(&t).await.state, EvidenceState::Capturing);
        }
    }
}

#[tokio::test]
async fn independent_captures_of_one_identity_send_only_one_put() {
    let f = Fixture::new().await;
    let one = f.upload();
    let t = one.target.clone();
    let two = EvidenceUpload {
        target: t.clone(),
        request: one.request.clone(),
        limits: one.limits,
    };
    let mut independent = f.backend.clone();
    independent.admission = Arc::new(Semaphore::new(1));
    let (a, b) = tokio::join!(
        f.backend
            .capture(f.db.clone(), one, Cursor::new(b"abc".to_vec())),
        independent.capture(f.db.clone(), two, Cursor::new(b"abc".to_vec()))
    );
    assert_ne!(a.is_ok(), b.is_ok());
    assert_eq!(f.remote.lock().unwrap().puts, 1);
    assert_eq!(f.job(&t).await.state, EvidenceState::Available);
}
