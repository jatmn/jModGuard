//! Local encrypted backup/restore. No live Bunny, Discord or provider account.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use jmodguard_conductor::config::{CaptureStore, Config};
use jmodguard_conductor::{Conductor, router_for};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

struct Harness {
    conductor: Arc<Conductor>,
    router: axum::Router,
    root: std::path::PathBuf,
    _discord: tokio::task::JoinHandle<()>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self._discord.abort();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn discord_stub(owner: u64) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route(
            "/token",
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({
                    "access_token": "conductor-token",
                    "token_type": "Bearer"
                }))
            }),
        )
        .route(
            "/me",
            axum::routing::get(move || async move {
                axum::Json(serde_json::json!({
                    "id": owner.to_string(),
                    "username": "operator",
                    "global_name": "Operator",
                }))
            }),
        );
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), task)
}

fn unique_root() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "jmodguard-backup-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ))
}

async fn setup(restore_exists: bool) -> Harness {
    let (discord, task) = discord_stub(42).await;
    let root = unique_root();
    std::fs::create_dir_all(&root).unwrap();
    let core = root.join("core.db");
    write_core(&core, true).await;
    let evidence = root.join("evidence");
    std::fs::create_dir_all(&evidence).unwrap();
    std::fs::write(evidence.join("note.bin"), b"case-note").unwrap();
    let backup_root = root.join("backups");
    let restore = root.join("restore-target");
    if restore_exists {
        std::fs::create_dir_all(&restore).unwrap();
    }
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        log_filter: "warn".into(),
        shutdown_grace: Duration::from_secs(1),
        db_path: Some(root.join("control.db")),
        public_url: "http://127.0.0.1:8081".into(),
        owner_id: 42,
        oauth_client_id: "cid".into(),
        oauth_client_secret: "csecret".into(),
        bot_health_url: String::new(),
        api_health_url: String::new(),
        oauth_token_url: format!("{discord}/token"),
        oauth_me_url: format!("{discord}/me"),
        backup_root: Some(backup_root),
        backup_key: Some([7u8; 32]),
        capture_stores: vec![CaptureStore {
            role: "core".into(),
            path: core,
        }],
        evidence_root: Some(evidence),
        restore_target: Some(restore),
    };
    let conductor = Conductor::start(cfg, false).await.unwrap();
    let router = router_for(Arc::clone(&conductor));
    Harness {
        conductor,
        router,
        root,
        _discord: task,
    }
}

async fn write_core(path: &std::path::Path, with_actions: bool) {
    let _ = std::fs::remove_file(path);
    let db = libsql::Builder::new_local(path).build().await.unwrap();
    let conn = db.connect().unwrap();
    conn.execute_batch(
        "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
         INSERT INTO meta(key,value) VALUES('schema_family','jmodguard-core-1');
         CREATE TABLE guild_settings(guild_id INTEGER, prefix TEXT);
         INSERT INTO guild_settings(guild_id, prefix) VALUES(10, '!');
         CREATE TABLE guild_enrollments(
           guild_id INTEGER PRIMARY KEY,
           id TEXT NOT NULL,
           joined_at_ns INTEGER NOT NULL,
           active INTEGER NOT NULL
         );
         INSERT INTO guild_enrollments VALUES
           (10, 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 1, 1);
         CREATE TABLE guild_cases(
           guild_id INTEGER NOT NULL,
           enrollment_id TEXT NOT NULL,
           case_id TEXT NOT NULL,
           payload TEXT,
           PRIMARY KEY(guild_id, enrollment_id, case_id)
         );
         INSERT INTO guild_cases VALUES
           (10, 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', '11111111111111111111111111111111', 'remove'),
           (10, 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', '22222222222222222222222222222222', 'keep');
         CREATE TRIGGER protect_case_delete BEFORE DELETE ON guild_cases
         BEGIN SELECT RAISE(ABORT, 'application lifecycle guard'); END;
         CREATE TABLE bot_actions(id INTEGER PRIMARY KEY, payload TEXT);
         CREATE TABLE backup_cleanup_intents(
           intent_id TEXT PRIMARY KEY,
           kind TEXT,
           guild_id INTEGER,
           enrollment_id TEXT,
           case_id TEXT,
           evidence_id TEXT,
           object_key TEXT,
           created_at INTEGER
         );",
    )
    .await
    .unwrap();
    if with_actions {
        conn.execute(
            "INSERT INTO bot_actions(id, payload) VALUES (1, 'pending-ban')",
            (),
        )
        .await
        .unwrap();
    }
}

async fn call(
    router: &axum::Router,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: Option<&[u8]>,
    content_type: Option<&str>,
) -> (StatusCode, Vec<u8>, Option<String>) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    let request = builder
        .body(Body::from(body.unwrap_or(b"").to_vec()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let set_cookie = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("jmodguard_conductor_session="))
        .map(|v| v.to_string());
    let bytes = axum::body::to_bytes(response.into_body(), 8_000_000)
        .await
        .unwrap();
    (status, bytes.to_vec(), set_cookie)
}

async fn tick_until_idle(h: &Harness) {
    for _ in 0..16 {
        h.conductor.tick_job().await.unwrap();
        if !h
            .conductor
            .store()
            .unwrap()
            .has_active_backup_job()
            .await
            .unwrap()
        {
            return;
        }
    }
}

async fn complete_archive_id(h: &Harness, cookie: &str) -> String {
    let (status, listed, _) =
        call(&h.router, "GET", "/api/backups", Some(cookie), None, None).await;
    assert_eq!(status, StatusCode::OK);
    let listed: serde_json::Value = serde_json::from_slice(&listed).unwrap();
    listed["archives"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["status"] == "complete")
        .expect("complete backup")["id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn login(h: &Harness) -> String {
    let login = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let oauth = login
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.contains("jmodguard_conductor_oauth_state="))
        .unwrap();
    let state = oauth
        .split(';')
        .next()
        .unwrap()
        .trim_start_matches("jmodguard_conductor_oauth_state=");
    let (_, _, session) = call(
        &h.router,
        "GET",
        &format!("/oauth/callback?code=ok&state={state}"),
        Some(&format!("jmodguard_conductor_oauth_state={state}")),
        None,
        None,
    )
    .await;
    session.unwrap().split(';').next().unwrap().to_string()
}

#[tokio::test]
async fn backups_require_owner_session_and_stay_off_unconfigured_version() {
    let router = jmodguard_conductor::router();
    let (status, body, _) = call(&router, "GET", "/version", None, None, None).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("\"backup_available\":false"));
    assert!(text.contains("\"capture_available\":false"));
    let h = setup(false).await;
    let (status, _, _) = call(&h.router, "GET", "/api/backups", None, None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = call(
        &h.router,
        "GET",
        "/api/backups",
        Some("jmodguard_session=guild"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let cookie = login(&h).await;
    let (status, body, _) = call(&h.router, "GET", "/version", Some(&cookie), None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("\"backup_available\":true"));
    assert!(String::from_utf8_lossy(&body).contains("\"capture_available\":true"));
}

#[tokio::test]
async fn manual_backup_excludes_pending_actions_and_restores_without_replay() {
    let h = setup(false).await;
    let cookie = login(&h).await;
    let (status, queued, _) = call(
        &h.router,
        "POST",
        "/api/backups",
        Some(&cookie),
        Some(br#"{"kind":"manual"}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&queued)
    );
    tick_until_idle(&h).await;
    let id = complete_archive_id(&h, &cookie).await;
    let (status, _, _) = call(
        &h.router,
        "POST",
        &format!("/api/backups/{id}/restore"),
        Some(&cookie),
        Some(br#"{"acknowledge_historical":false}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tick_until_idle(&h).await;
    let restored = h.root.join("restore-target").join("core.sqlite");
    assert!(restored.exists(), "restored store missing");
    let db = libsql::Builder::new_local(&restored).build().await.unwrap();
    let conn = db.connect().unwrap();
    let settings: i64 = conn
        .query("SELECT COUNT(*) FROM guild_settings WHERE guild_id=10", ())
        .await
        .unwrap()
        .next()
        .await
        .unwrap()
        .unwrap()
        .get(0)
        .unwrap();
    let actions = conn
        .query(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='bot_actions'",
            (),
        )
        .await
        .unwrap()
        .next()
        .await
        .unwrap()
        .unwrap()
        .get::<i64>(0)
        .unwrap();
    assert_eq!(settings, 1);
    assert_eq!(actions, 0);
    assert!(h.root.join("restore-target").join("COMPLETE").exists());
    assert!(
        h.root
            .join("restore-target")
            .join("evidence")
            .join("note.bin")
            .exists()
    );
}

#[tokio::test]
async fn nested_evidence_is_restored_at_its_original_relative_path() {
    let h = setup(false).await;
    let cookie = login(&h).await;
    let relative = "evidence/10/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/22222222222222222222222222222222/33333333333333333333333333333333";
    let base = h.root.join("evidence").join(relative);
    std::fs::create_dir_all(base.parent().unwrap()).unwrap();
    std::fs::write(base.with_extension("data"), b"nested evidence").unwrap();
    std::fs::write(base.with_extension("complete"), b"15\nchecksum\n").unwrap();

    let (status, _, _) = call(
        &h.router,
        "POST",
        "/api/backups",
        Some(&cookie),
        Some(br#"{"kind":"manual"}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tick_until_idle(&h).await;
    let id = complete_archive_id(&h, &cookie).await;
    let (status, _, _) = call(
        &h.router,
        "POST",
        &format!("/api/backups/{id}/restore"),
        Some(&cookie),
        Some(br#"{}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tick_until_idle(&h).await;

    let restored = h
        .root
        .join("restore-target")
        .join("evidence")
        .join(relative);
    assert_eq!(
        std::fs::read(restored.with_extension("data")).unwrap(),
        b"nested evidence"
    );
    assert!(restored.with_extension("complete").exists());
}

#[tokio::test]
async fn case_cleanup_intent_removes_nested_evidence_from_sanitized_archives() {
    let h = setup(false).await;
    let enrollment = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let removed_case = "11111111111111111111111111111111";
    let kept_case = "22222222222222222222222222222222";
    for (case, contents) in [
        (removed_case, b"remove".as_slice()),
        (kept_case, b"keep".as_slice()),
    ] {
        let base = h.root.join("evidence").join(format!(
            "evidence/10/{enrollment}/{case}/44444444444444444444444444444444"
        ));
        std::fs::create_dir_all(base.parent().unwrap()).unwrap();
        std::fs::write(base.with_extension("data"), contents).unwrap();
        std::fs::write(base.with_extension("complete"), b"receipt").unwrap();
    }
    let settings = h.conductor.settings().await.unwrap();
    jmodguard_conductor::backup::capture(
        &h.conductor.cfg,
        h.conductor.store().unwrap(),
        h.conductor.backup_zone().unwrap(),
        "manual",
        &settings,
    )
    .await
    .unwrap();
    let core = h.root.join("core.db");
    let db = libsql::Builder::new_local(&core).build().await.unwrap();
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO backup_cleanup_intents(intent_id, kind, guild_id, enrollment_id, case_id, created_at)
             VALUES('case-evidence-cleanup','case',10,?1,?2,1)",
            libsql::params![enrollment, removed_case],
        )
        .await
        .unwrap();
    let intents = jmodguard_conductor::backup::load_cleanup_intents(&h.conductor.cfg)
        .await
        .unwrap();
    let request = jmodguard_conductor::backup::RestoreRequest {
        acknowledge_historical: true,
        current_state_available: true,
        removed_guilds: intents.guilds,
        removed_cases: intents.cases,
        removed_evidence: intents.evidence,
        removed_object_keys: intents.object_keys,
    };
    jmodguard_conductor::backup::sanitize_complete_archives(
        &h.conductor.cfg,
        h.conductor.store().unwrap(),
        h.conductor.backup_zone().unwrap(),
        &settings,
        &request,
    )
    .await
    .unwrap();
    let archive = h
        .conductor
        .store()
        .unwrap()
        .list_complete_archives()
        .await
        .unwrap()
        .remove(0);
    jmodguard_conductor::backup::restore_archive(
        &h.conductor.cfg,
        h.conductor.store().unwrap(),
        h.conductor.backup_zone().unwrap(),
        &archive.id,
        None,
        jmodguard_conductor::backup::RestoreRequest {
            acknowledge_historical: true,
            current_state_available: true,
            ..Default::default()
        },
        &settings,
    )
    .await
    .unwrap();

    let evidence = h.root.join("restore-target").join("evidence");
    assert!(
        !evidence
            .join(format!(
                "evidence/10/{enrollment}/{removed_case}/44444444444444444444444444444444.data"
            ))
            .exists()
    );
    assert!(
        evidence
            .join(format!(
                "evidence/10/{enrollment}/{kept_case}/44444444444444444444444444444444.data"
            ))
            .exists()
    );
}

#[tokio::test]
async fn capture_omits_nested_evidence_covered_by_case_intents() {
    let h = setup(false).await;
    let enrollment = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let removed_case = "11111111111111111111111111111111";
    let relative =
        format!("evidence/10/{enrollment}/{removed_case}/55555555555555555555555555555555");
    let base = h.root.join("evidence").join(&relative);
    std::fs::create_dir_all(base.parent().unwrap()).unwrap();
    std::fs::write(base.with_extension("data"), b"removed evidence").unwrap();
    std::fs::write(base.with_extension("complete"), b"receipt").unwrap();
    let core = h.root.join("core.db");
    let db = libsql::Builder::new_local(&core).build().await.unwrap();
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO backup_cleanup_intents(intent_id, kind, guild_id, enrollment_id, case_id, created_at)
             VALUES('capture-case-cleanup','case',10,?1,?2,1)",
            libsql::params![enrollment, removed_case],
        )
        .await
        .unwrap();
    let settings = h.conductor.settings().await.unwrap();
    let archive = jmodguard_conductor::backup::capture(
        &h.conductor.cfg,
        h.conductor.store().unwrap(),
        h.conductor.backup_zone().unwrap(),
        "manual",
        &settings,
    )
    .await
    .unwrap();
    jmodguard_conductor::backup::restore_archive(
        &h.conductor.cfg,
        h.conductor.store().unwrap(),
        h.conductor.backup_zone().unwrap(),
        &archive.id,
        None,
        jmodguard_conductor::backup::RestoreRequest {
            acknowledge_historical: true,
            current_state_available: true,
            ..Default::default()
        },
        &settings,
    )
    .await
    .unwrap();
    assert!(
        !h.root
            .join("restore-target")
            .join("evidence")
            .join(relative)
            .with_extension("data")
            .exists()
    );
}

#[tokio::test]
async fn departed_guild_intent_preserves_a_newer_enrollment() {
    let h = setup(false).await;
    let old = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let current = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let evidence_id = "66666666666666666666666666666666";
    for (enrollment, contents) in [(old, b"old".as_slice()), (current, b"current".as_slice())] {
        let base = h.root.join("evidence").join(format!(
            "evidence/10/{enrollment}/22222222222222222222222222222222/{evidence_id}"
        ));
        std::fs::create_dir_all(base.parent().unwrap()).unwrap();
        std::fs::write(base.with_extension("data"), contents).unwrap();
        std::fs::write(base.with_extension("complete"), b"receipt").unwrap();
    }
    let core = h.root.join("core.db");
    let db = libsql::Builder::new_local(&core).build().await.unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE guild_enrollments SET id = ?1 WHERE guild_id = 10",
        libsql::params![current],
    )
    .await
    .unwrap();
    conn.execute(
        "INSERT INTO backup_cleanup_intents(intent_id, kind, guild_id, enrollment_id, created_at)
         VALUES('old-guild-cleanup','guild',10,?1,1)",
        libsql::params![old],
    )
    .await
    .unwrap();
    let settings = h.conductor.settings().await.unwrap();
    jmodguard_conductor::backup::capture(
        &h.conductor.cfg,
        h.conductor.store().unwrap(),
        h.conductor.backup_zone().unwrap(),
        "manual",
        &settings,
    )
    .await
    .unwrap();
    let intents = jmodguard_conductor::backup::load_cleanup_intents(&h.conductor.cfg)
        .await
        .unwrap();
    let request = jmodguard_conductor::backup::RestoreRequest {
        acknowledge_historical: true,
        current_state_available: true,
        removed_guilds: intents.guilds,
        removed_cases: intents.cases,
        removed_evidence: intents.evidence,
        removed_object_keys: intents.object_keys,
    };
    jmodguard_conductor::backup::sanitize_complete_archives(
        &h.conductor.cfg,
        h.conductor.store().unwrap(),
        h.conductor.backup_zone().unwrap(),
        &settings,
        &request,
    )
    .await
    .unwrap();
    let archive = h
        .conductor
        .store()
        .unwrap()
        .list_complete_archives()
        .await
        .unwrap()
        .remove(0);
    jmodguard_conductor::backup::restore_archive(
        &h.conductor.cfg,
        h.conductor.store().unwrap(),
        h.conductor.backup_zone().unwrap(),
        &archive.id,
        None,
        jmodguard_conductor::backup::RestoreRequest {
            acknowledge_historical: true,
            current_state_available: true,
            ..Default::default()
        },
        &settings,
    )
    .await
    .unwrap();

    let restored_core = h.root.join("restore-target").join("core.sqlite");
    let db = libsql::Builder::new_local(&restored_core)
        .build()
        .await
        .unwrap();
    let conn = db.connect().unwrap();
    let mut rows = conn
        .query("SELECT id FROM guild_enrollments WHERE guild_id = 10", ())
        .await
        .unwrap();
    assert_eq!(
        rows.next()
            .await
            .unwrap()
            .unwrap()
            .get::<String>(0)
            .unwrap(),
        current
    );
    let mut rows = conn
        .query(
            "SELECT COUNT(*) FROM guild_settings WHERE guild_id = 10",
            (),
        )
        .await
        .unwrap();
    assert_eq!(
        rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
        1
    );
    let evidence = h.root.join("restore-target").join("evidence");
    assert!(
        !evidence
            .join(format!(
                "evidence/10/{old}/22222222222222222222222222222222/{evidence_id}.data"
            ))
            .exists()
    );
    assert!(
        evidence
            .join(format!(
                "evidence/10/{current}/22222222222222222222222222222222/{evidence_id}.data"
            ))
            .exists()
    );
}

#[tokio::test]
async fn interrupted_restore_staging_is_recovered_before_retry() {
    let h = setup(false).await;
    let settings = h.conductor.settings().await.unwrap();
    let archive = jmodguard_conductor::backup::capture(
        &h.conductor.cfg,
        h.conductor.store().unwrap(),
        h.conductor.backup_zone().unwrap(),
        "manual",
        &settings,
    )
    .await
    .unwrap();
    let stage = h.root.join(".restore-target.jmodguard-restore-staging");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::write(
        stage.join(".jmodguard-restore-staging"),
        b"jmodguard-restore-staging-1\n",
    )
    .unwrap();
    std::fs::write(stage.join("partial"), b"partial restore").unwrap();

    jmodguard_conductor::backup::restore_archive(
        &h.conductor.cfg,
        h.conductor.store().unwrap(),
        h.conductor.backup_zone().unwrap(),
        &archive.id,
        None,
        jmodguard_conductor::backup::RestoreRequest {
            acknowledge_historical: true,
            current_state_available: true,
            ..Default::default()
        },
        &settings,
    )
    .await
    .unwrap();
    assert!(h.root.join("restore-target").join("COMPLETE").exists());
    assert!(!stage.exists());
}

#[tokio::test]
async fn restore_rejects_archive_ids_that_can_escape_the_backup_zone() {
    let h = setup(false).await;
    let settings = h.conductor.settings().await.unwrap();
    let error = jmodguard_conductor::backup::restore_archive(
        &h.conductor.cfg,
        h.conductor.store().unwrap(),
        h.conductor.backup_zone().unwrap(),
        "../outside",
        None,
        jmodguard_conductor::backup::RestoreRequest {
            acknowledge_historical: true,
            current_state_available: true,
            ..Default::default()
        },
        &settings,
    )
    .await
    .expect_err("path-like archive ids must be rejected");
    assert!(
        error.to_string().contains("invalid archive id"),
        "{error:#}"
    );
    assert!(!h.root.join("restore-target").exists());
}

#[tokio::test]
async fn historical_restore_requires_acknowledgment_and_rejects_corrupt_uploads() {
    let h = setup(false).await;
    let cookie = login(&h).await;
    let (status, _, _) = call(
        &h.router,
        "POST",
        "/api/backups",
        Some(&cookie),
        Some(br#"{"kind":"manual"}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tick_until_idle(&h).await;
    let id = complete_archive_id(&h, &cookie).await;
    std::fs::remove_file(h.root.join("core.db")).unwrap();
    std::fs::File::create(h.root.join("core.db")).unwrap();
    let (status, _, _) = call(
        &h.router,
        "POST",
        &format!("/api/backups/{id}/restore"),
        Some(&cookie),
        Some(br#"{"acknowledge_historical":false}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tick_until_idle(&h).await;
    let jobs = h.conductor.store().unwrap().list_jobs(20).await.unwrap();
    let restore = jobs
        .iter()
        .find(|job| job.kind == "backup_restore")
        .expect("restore job");
    assert_eq!(restore.status, "failed");
    assert!(
        restore
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("historical restore acknowledgment required"),
        "{restore:?}"
    );
    assert!(!h.root.join("restore-target").exists());
    let (status, _, _) = call(
        &h.router,
        "POST",
        &format!("/api/backups/{id}/restore"),
        Some(&cookie),
        Some(br#"{"acknowledge_historical":true}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tick_until_idle(&h).await;
    assert!(h.root.join("restore-target").join("COMPLETE").exists());
    let (status, body, _) = call(
        &h.router,
        "POST",
        "/api/backups/upload",
        Some(&cookie),
        Some(b"not-an-archive"),
        Some("application/octet-stream"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("unsupported archive format"));
}

#[tokio::test]
async fn restore_refuses_an_existing_target_and_restores_quarantined_uploads() {
    let existing = setup(true).await;
    let cookie = login(&existing).await;
    let (status, _, _) = call(
        &existing.router,
        "POST",
        "/api/backups",
        Some(&cookie),
        Some(br#"{"kind":"manual"}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tick_until_idle(&existing).await;
    let id = complete_archive_id(&existing, &cookie).await;
    let (status, _, _) = call(
        &existing.router,
        "POST",
        &format!("/api/backups/{id}/restore"),
        Some(&cookie),
        Some(br#"{}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tick_until_idle(&existing).await;
    let jobs = existing
        .conductor
        .store()
        .unwrap()
        .list_jobs(20)
        .await
        .unwrap();
    assert!(
        jobs.iter()
            .any(|job| job.kind == "backup_restore" && job.status == "failed"),
        "{jobs:?}"
    );

    let h = setup(false).await;
    let cookie = login(&h).await;
    let (status, _, _) = call(
        &h.router,
        "POST",
        "/api/backups",
        Some(&cookie),
        Some(br#"{"kind":"manual"}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tick_until_idle(&h).await;
    let id = complete_archive_id(&h, &cookie).await;
    let (status, bytes, _) = call(
        &h.router,
        "GET",
        &format!("/api/backups/{id}/download"),
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(bytes.starts_with(jmodguard_conductor::backup::MAGIC));
    let (status, uploaded, _) = call(
        &h.router,
        "POST",
        "/api/backups/upload",
        Some(&cookie),
        Some(&bytes),
        Some("application/octet-stream"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&uploaded)
    );
    let uploaded: serde_json::Value = serde_json::from_slice(&uploaded).unwrap();
    let upload_id = uploaded["id"].as_str().unwrap();
    let (status, _, _) = call(
        &h.router,
        "POST",
        &format!("/api/backups/{upload_id}/restore"),
        Some(&cookie),
        Some(br#"{}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tick_until_idle(&h).await;
    assert!(h.root.join("restore-target").join("COMPLETE").exists());
}

#[tokio::test]
async fn known_removal_is_applied_and_cleanup_intents_are_acked() {
    let h = setup(false).await;
    let cookie = login(&h).await;
    let (status, _, _) = call(
        &h.router,
        "POST",
        "/api/backups",
        Some(&cookie),
        Some(br#"{"kind":"manual"}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tick_until_idle(&h).await;
    let core = h.root.join("core.db");
    let db = libsql::Builder::new_local(&core).build().await.unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "INSERT INTO backup_cleanup_intents(intent_id, kind, guild_id, enrollment_id, object_key, created_at)
         VALUES('intent-1','guild',10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','note.bin',1)",
        (),
    )
        .await
        .unwrap();
    tick_until_idle(&h).await;
    let listed = h
        .conductor
        .store()
        .unwrap()
        .list_archives(20)
        .await
        .unwrap();
    assert!(
        listed
            .iter()
            .any(|row| row.status == "replaced" || row.scope_json.contains("sanitized")),
        "{listed:?}"
    );
}

#[tokio::test]
async fn portable_restore_password_is_ephemeral_and_plaintext_scratch_is_removed() {
    let h = setup(false).await;
    let cookie = login(&h).await;
    let (status, _, _) = call(
        &h.router,
        "POST",
        "/api/backups",
        Some(&cookie),
        Some(br#"{"kind":"manual"}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tick_until_idle(&h).await;
    let id = complete_archive_id(&h, &cookie).await;
    let password = "ephemeral-restore-secret";
    let (status, portable, _) = call(
        &h.router,
        "POST",
        &format!("/api/backups/{id}/portable"),
        Some(&cookie),
        Some(format!(r#"{{"password":"{password}"}}"#).as_bytes()),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !h.root
            .join("backups")
            .join("scratch")
            .join(format!("portable-{id}"))
            .exists(),
        "portable plaintext scratch survived response creation"
    );

    let (status, uploaded, _) = call(
        &h.router,
        "POST",
        "/api/backups/upload",
        Some(&cookie),
        Some(&portable),
        Some("application/octet-stream"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let uploaded: serde_json::Value = serde_json::from_slice(&uploaded).unwrap();
    let upload_id = uploaded["id"].as_str().unwrap();
    let (status, _, _) = call(
        &h.router,
        "POST",
        &format!("/api/backups/{upload_id}/restore"),
        Some(&cookie),
        Some(br#"{"password":"wrong-password"}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let queued = h
        .conductor
        .store()
        .unwrap()
        .list_jobs(20)
        .await
        .unwrap()
        .into_iter()
        .find(|job| job.kind == "backup_restore" && job.status == "queued")
        .expect("queued restore");
    assert!(
        !queued.payload_json.contains(password) && !queued.payload_json.contains("wrong-password"),
        "{queued:?}"
    );
    assert!(queued.payload_json.contains("\"password_required\":true"));

    tick_until_idle(&h).await;
    assert!(
        h.conductor
            .store()
            .unwrap()
            .replay_job(&queued.id)
            .await
            .unwrap()
    );
    tick_until_idle(&h).await;
    let replayed = h
        .conductor
        .store()
        .unwrap()
        .list_jobs(20)
        .await
        .unwrap()
        .into_iter()
        .find(|job| job.id == queued.id)
        .unwrap();
    assert!(
        replayed
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("password is no longer available")),
        "{replayed:?}"
    );
    let (status, _, _) = call(
        &h.router,
        "POST",
        &format!("/api/backups/{upload_id}/restore"),
        Some(&cookie),
        Some(format!(r#"{{"password":"{password}"}}"#).as_bytes()),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tick_until_idle(&h).await;
    assert!(h.root.join("restore-target").join("COMPLETE").exists());
}

#[tokio::test]
async fn upload_stream_enforces_the_configured_archive_budget() {
    let h = setup(false).await;
    let cookie = login(&h).await;
    let (status, patched, _) = call(
        &h.router,
        "PATCH",
        "/api/settings",
        Some(&cookie),
        Some(br#"{"revision":1,"backup_max_archive_bytes":1048576}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&patched)
    );
    let mut oversized = jmodguard_conductor::backup::MAGIC.to_vec();
    oversized.resize(1_048_577, 0);
    let (status, _, _) = call(
        &h.router,
        "POST",
        "/api/backups/upload",
        Some(&cookie),
        Some(&oversized),
        Some("application/octet-stream"),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        std::fs::read_dir(h.root.join("backups").join("quarantine"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn capture_refuses_to_report_success_without_a_core_store() {
    let h = setup(false).await;
    let mut cfg = h.conductor.cfg.clone();
    cfg.capture_stores.clear();
    let error = jmodguard_conductor::backup::capture(
        &cfg,
        h.conductor.store().unwrap(),
        h.conductor.backup_zone().unwrap(),
        "manual",
        &h.conductor.settings().await.unwrap(),
    )
    .await
    .expect_err("capture without core store must fail");
    assert!(
        error
            .to_string()
            .contains("capture requires a configured core store"),
        "{error:#}"
    );
    assert!(
        h.conductor
            .store()
            .unwrap()
            .list_archives(10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn case_cleanup_is_scoped_and_preserves_database_triggers() {
    let h = setup(false).await;
    let cookie = login(&h).await;
    let (status, _, _) = call(
        &h.router,
        "POST",
        "/api/backups",
        Some(&cookie),
        Some(br#"{"kind":"manual"}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tick_until_idle(&h).await;

    let core = h.root.join("core.db");
    let db = libsql::Builder::new_local(&core).build().await.unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "INSERT INTO backup_cleanup_intents(
           intent_id, kind, guild_id, enrollment_id, case_id, created_at
         ) VALUES(
           '33333333333333333333333333333333', 'case', 10,
           'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
           '11111111111111111111111111111111', 1
         )",
        (),
    )
    .await
    .unwrap();
    tick_until_idle(&h).await;
    let sanitized_id = complete_archive_id(&h, &cookie).await;
    let (status, _, _) = call(
        &h.router,
        "POST",
        &format!("/api/backups/{sanitized_id}/restore"),
        Some(&cookie),
        Some(br#"{}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tick_until_idle(&h).await;

    let restored = h.root.join("restore-target").join("core.sqlite");
    let db = libsql::Builder::new_local(&restored).build().await.unwrap();
    let conn = db.connect().unwrap();
    let removed: i64 = conn
        .query(
            "SELECT COUNT(*) FROM guild_cases
             WHERE case_id = '11111111111111111111111111111111'",
            (),
        )
        .await
        .unwrap()
        .next()
        .await
        .unwrap()
        .unwrap()
        .get(0)
        .unwrap();
    let kept: i64 = conn
        .query(
            "SELECT COUNT(*) FROM guild_cases
             WHERE case_id = '22222222222222222222222222222222'",
            (),
        )
        .await
        .unwrap()
        .next()
        .await
        .unwrap()
        .unwrap()
        .get(0)
        .unwrap();
    let trigger: i64 = conn
        .query(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'trigger' AND name = 'protect_case_delete'",
            (),
        )
        .await
        .unwrap()
        .next()
        .await
        .unwrap()
        .unwrap()
        .get(0)
        .unwrap();
    assert_eq!((removed, kept, trigger), (0, 1, 1));
}

#[tokio::test]
async fn backup_catalog_surfaces_failed_jobs_for_retry() {
    let h = setup(false).await;
    let cookie = login(&h).await;
    let store = h.conductor.store().unwrap();
    let id = store
        .insert_job("backup_capture", &serde_json::json!({"kind":"manual"}), 1)
        .await
        .unwrap();
    assert_eq!(store.claim_job().await.unwrap().unwrap().id, id);
    store
        .finish_job(&id, false, Some("capture failed"))
        .await
        .unwrap();
    let (status, body, _) = call(&h.router, "GET", "/api/backups", Some(&cookie), None, None).await;
    assert_eq!(status, StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["failed_jobs"][0]["id"], id);
    assert_eq!(body["failed_jobs"][0]["status"], "failed");
}

#[tokio::test]
async fn backup_job_admission_is_atomic() {
    let h = setup(false).await;
    let store = h.conductor.store().unwrap();
    let first_payload = serde_json::json!({"kind":"manual"});
    let second_payload = serde_json::json!({"kind":"manual"});
    let first = store.insert_backup_job_if_idle("backup_capture", &first_payload, 3);
    let second = store.insert_backup_job_if_idle("backup_capture", &second_payload, 3);
    let (first, second) = tokio::join!(first, second);
    assert_eq!(
        usize::from(first.unwrap().is_some()) + usize::from(second.unwrap().is_some()),
        1
    );
}

#[tokio::test]
async fn failed_capture_does_not_block_pending_backup_sanitization() {
    let h = setup(false).await;
    let cookie = login(&h).await;
    let (status, _, _) = call(
        &h.router,
        "POST",
        "/api/backups",
        Some(&cookie),
        Some(br#"{"kind":"manual"}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tick_until_idle(&h).await;

    let store = h.conductor.store().unwrap();
    let failed = store
        .insert_job("backup_capture", &serde_json::json!({"kind":"manual"}), 1)
        .await
        .unwrap();
    assert_eq!(store.claim_job().await.unwrap().unwrap().id, failed);
    store
        .finish_job(&failed, false, Some("old capture failure"))
        .await
        .unwrap();
    let db = libsql::Builder::new_local(h.root.join("core.db"))
        .build()
        .await
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO backup_cleanup_intents(intent_id, kind, guild_id, enrollment_id, created_at)
             VALUES('44444444444444444444444444444444','guild',10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',1)",
            (),
        )
        .await
        .unwrap();
    tick_until_idle(&h).await;

    let db = libsql::Builder::new_local(h.root.join("core.db"))
        .build()
        .await
        .unwrap();
    let mut rows = db
        .connect()
        .unwrap()
        .query("SELECT COUNT(*) FROM backup_cleanup_intents", ())
        .await
        .unwrap();
    assert_eq!(
        rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
        0
    );
    assert!(
        store
            .list_jobs(50)
            .await
            .unwrap()
            .iter()
            .any(|job| job.id == failed && job.status == "failed")
    );
}

#[tokio::test]
async fn failed_restore_does_not_block_scheduled_capture() {
    let h = setup(false).await;
    let store = h.conductor.store().unwrap();
    let (mut settings, revision, _) = store.load_settings().await.unwrap();
    settings.backup_enabled = true;
    store.save_settings(&settings, revision).await.unwrap();
    let failed = store
        .insert_job(
            "backup_restore",
            &serde_json::json!({"id":"missing","password_required":false}),
            1,
        )
        .await
        .unwrap();
    assert_eq!(store.claim_job().await.unwrap().unwrap().id, failed);
    store
        .finish_job(&failed, false, Some("old restore failure"))
        .await
        .unwrap();

    h.conductor.tick_job().await.unwrap();
    assert_eq!(store.list_complete_archives().await.unwrap().len(), 1);
    assert!(
        store
            .list_jobs(20)
            .await
            .unwrap()
            .iter()
            .any(|job| job.id == failed && job.status == "failed")
    );
}

#[tokio::test]
async fn capture_preflights_sqlite_size_before_snapshotting() {
    let h = setup(false).await;
    let oversized = h.root.join("oversized-core.db");
    let db = libsql::Builder::new_local(&oversized)
        .build()
        .await
        .unwrap();
    let conn = db.connect().unwrap();
    conn.execute_batch("CREATE TABLE payload(value BLOB);")
        .await
        .unwrap();
    conn.execute("INSERT INTO payload VALUES(zeroblob(2097152))", ())
        .await
        .unwrap();
    drop(conn);
    drop(db);

    let mut cfg = h.conductor.cfg.clone();
    cfg.capture_stores[0].path = oversized;
    let mut settings = h.conductor.settings().await.unwrap();
    settings.backup_max_archive_bytes = 1_048_576;
    let error = jmodguard_conductor::backup::capture(
        &cfg,
        h.conductor.store().unwrap(),
        h.conductor.backup_zone().unwrap(),
        "manual",
        &settings,
    )
    .await
    .expect_err("oversized logical database must be refused");
    assert!(
        error
            .to_string()
            .contains("backup payload exceeds configured byte budget"),
        "{error:#}"
    );
}

#[tokio::test]
async fn disabled_backup_notifications_do_not_create_failed_notify_jobs() {
    let h = setup(false).await;
    std::fs::remove_file(h.root.join("core.db")).unwrap();
    h.conductor
        .store()
        .unwrap()
        .insert_job("backup_capture", &serde_json::json!({"kind":"manual"}), 1)
        .await
        .unwrap();
    h.conductor.tick_job().await.unwrap();
    let jobs = h.conductor.store().unwrap().list_jobs(20).await.unwrap();
    assert!(
        jobs.iter().all(|job| job.kind != "notify_event"),
        "{jobs:?}"
    );
}

#[tokio::test]
async fn startup_recovers_archive_catalog_and_untracked_complete_files() {
    let h = setup(false).await;
    let store = h.conductor.store().unwrap();
    store
        .insert_archive(&jmodguard_conductor::store::ArchiveRow {
            id: "stale-staging".into(),
            kind: "manual".into(),
            status: "staging".into(),
            created_ms: 1,
            captured_ms: None,
            scope_json: "{}".into(),
            object_key: None,
            sha256: None,
            bytes: None,
            family: "jmodguard-core-1".into(),
            last_error: None,
        })
        .await
        .unwrap();
    let orphan = h.root.join("backups").join("complete").join("orphan.jmb");
    std::fs::write(&orphan, b"orphan").unwrap();

    let restarted = Conductor::start(h.conductor.cfg.clone(), false)
        .await
        .unwrap();
    let stale = restarted
        .store()
        .unwrap()
        .get_archive("stale-staging")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stale.status, "failed");
    assert!(!orphan.exists());
}

#[tokio::test]
async fn complete_archive_enumeration_is_not_limited_by_catalog_history() {
    let h = setup(false).await;
    let store = h.conductor.store().unwrap();
    for index in 0..205 {
        store
            .insert_archive(&jmodguard_conductor::store::ArchiveRow {
                id: format!("complete-{index:03}"),
                kind: "manual".into(),
                status: "complete".into(),
                created_ms: index,
                captured_ms: Some(index),
                scope_json: "{}".into(),
                object_key: None,
                sha256: None,
                bytes: Some(1),
                family: "jmodguard-core-1".into(),
                last_error: None,
            })
            .await
            .unwrap();
    }
    assert_eq!(store.list_complete_archives().await.unwrap().len(), 205);
}

#[tokio::test]
async fn catalog_overdue_matches_scheduled_backoff_after_failed_capture() {
    let h = setup(false).await;
    let cookie = login(&h).await;
    let store = h.conductor.store().unwrap();
    let (mut settings, revision, _) = store.load_settings().await.unwrap();
    settings.backup_enabled = true;
    store.save_settings(&settings, revision).await.unwrap();
    store
        .insert_job(
            "backup_capture",
            &serde_json::json!({"kind":"scheduled"}),
            1,
        )
        .await
        .unwrap();
    let claimed = store.claim_job().await.unwrap().unwrap();
    store
        .finish_job(&claimed.id, false, Some("scheduled capture failed"))
        .await
        .unwrap();

    h.conductor.tick_job().await.unwrap();
    let (status, body, _) = call(&h.router, "GET", "/api/backups", Some(&cookie), None, None).await;
    assert_eq!(status, StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["overdue"], false);
    assert!(
        !store
            .list_jobs(50)
            .await
            .unwrap()
            .iter()
            .any(|job| job.kind == "backup_capture" && job.status == "queued"),
        "scheduler must not enqueue another scheduled capture during backoff"
    );
}

#[tokio::test]
async fn capture_requires_a_reachable_core_store() {
    let (discord, task) = discord_stub(42).await;
    let root = unique_root();
    std::fs::create_dir_all(&root).unwrap();
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        log_filter: "warn".into(),
        shutdown_grace: Duration::from_secs(1),
        db_path: Some(root.join("control.db")),
        public_url: "http://127.0.0.1:8081".into(),
        owner_id: 42,
        oauth_client_id: "cid".into(),
        oauth_client_secret: "csecret".into(),
        bot_health_url: String::new(),
        api_health_url: String::new(),
        oauth_token_url: format!("{discord}/token"),
        oauth_me_url: format!("{discord}/me"),
        backup_root: Some(root.join("backups")),
        backup_key: Some([7u8; 32]),
        capture_stores: Vec::new(),
        evidence_root: None,
        restore_target: Some(root.join("restore-target")),
    };
    let conductor = Conductor::start(cfg, false).await.unwrap();
    let router = router_for(Arc::clone(&conductor));
    let h = Harness {
        conductor,
        router,
        root,
        _discord: task,
    };
    let cookie = login(&h).await;
    let (status, body, _) = call(&h.router, "GET", "/version", Some(&cookie), None, None).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("\"backup_available\":true"));
    assert!(text.contains("\"capture_available\":false"));
    let (status, listed, _) =
        call(&h.router, "GET", "/api/backups", Some(&cookie), None, None).await;
    assert_eq!(status, StatusCode::OK);
    let listed: serde_json::Value = serde_json::from_slice(&listed).unwrap();
    assert_eq!(listed["capture_available"], false);
    let (status, create, _) = call(
        &h.router,
        "POST",
        "/api/backups",
        Some(&cookie),
        Some(br#"{"kind":"manual"}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        String::from_utf8_lossy(&create).contains("capture requires a configured core store"),
        "{}",
        String::from_utf8_lossy(&create)
    );
}

#[tokio::test]
async fn backup_job_replay_honors_the_idle_mutex() {
    let h = setup(false).await;
    let cookie = login(&h).await;
    let store = h.conductor.store().unwrap();
    let restore = store
        .insert_job(
            "backup_restore",
            &serde_json::json!({"id":"missing","password_required":false}),
            1,
        )
        .await
        .unwrap();
    assert_eq!(store.claim_job().await.unwrap().unwrap().id, restore);
    let failed = store
        .insert_job("backup_capture", &serde_json::json!({"kind":"manual"}), 1)
        .await
        .unwrap();
    assert_eq!(store.claim_job().await.unwrap().unwrap().id, failed);
    store
        .finish_job(&failed, false, Some("capture failed"))
        .await
        .unwrap();
    let (status, body, _) = call(
        &h.router,
        "POST",
        &format!("/api/jobs/{failed}/retry"),
        Some(&cookie),
        Some(br#"{}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        String::from_utf8_lossy(&body).contains("already running"),
        "{}",
        String::from_utf8_lossy(&body)
    );
    store
        .finish_job(&restore, false, Some("restore failed"))
        .await
        .unwrap();
    let (status, _, _) = call(
        &h.router,
        "POST",
        &format!("/api/jobs/{failed}/retry"),
        Some(&cookie),
        Some(br#"{}"#),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}
