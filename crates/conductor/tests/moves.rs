//! Local shared↔dedicated guild-store moves. No live Bunny or Discord.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use jmodguard_conductor::config::{CaptureStore, Config, GuildStorePath};
use jmodguard_conductor::{Conductor, router_for};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

const SHARED: &str = "11111111111111111111111111111111";
const DEDICATED: &str = "22222222222222222222222222222222";
const ENROLL_10: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const ENROLL_20: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const REV_10: &str = "cccccccccccccccccccccccccccccccc";
const REV_20: &str = "dddddddddddddddddddddddddddddddd";
const CASE_10: &str = "33333333333333333333333333333333";
const CASE_20: &str = "44444444444444444444444444444444";
const SRC_10: &str = "app:55555555555555555555555555555555";
const SRC_20: &str = "app:66666666666666666666666666666666";
const CFG_REV: &str = "77777777777777777777777777777777";

struct Harness {
    conductor: Arc<Conductor>,
    router: axum::Router,
    root: PathBuf,
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

fn unique_root() -> PathBuf {
    std::env::temp_dir().join(format!(
        "jmodguard-moves-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ))
}

async fn setup() -> Harness {
    setup_with_destination_version(9).await
}

async fn setup_with_destination_version(destination_version: i64) -> Harness {
    let (discord, task) = discord_stub(42).await;
    let root = unique_root();
    std::fs::create_dir_all(&root).unwrap();
    let core = root.join("core.db");
    let shared = root.join("shared.guild.db");
    let dedicated = root.join("dedicated.guild.db");
    write_core(&core).await;
    init_guild(
        &shared,
        SHARED,
        None,
        &[
            (10, ENROLL_10, REV_10, CASE_10, SRC_10),
            (20, ENROLL_20, REV_20, CASE_20, SRC_20),
        ],
        9,
    )
    .await;
    init_guild(&dedicated, DEDICATED, Some(10), &[], destination_version).await;
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
        backup_root: None,
        backup_key: None,
        capture_stores: vec![CaptureStore {
            role: "core".into(),
            path: core,
        }],
        evidence_root: None,
        restore_target: None,
        guild_stores: vec![
            GuildStorePath {
                id: SHARED.into(),
                role: "shared".into(),
                path: shared,
            },
            GuildStorePath {
                id: DEDICATED.into(),
                role: "dedicated".into(),
                path: dedicated,
            },
        ],
        release_feed: None,
        release_keys: Default::default(),
        installed_release: None,
        update_root: None,
        artifact_root: None,
        update_provider: None,
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

async fn write_core(path: &Path) {
    let db = libsql::Builder::new_local(path).build().await.unwrap();
    let conn = db.connect().unwrap();
    conn.execute_batch(include_str!(
        "../../../migrations/0010_guild_store_catalog.sql"
    ))
    .await
    .unwrap();
    conn.execute_batch(include_str!("../../../migrations/0011_tenant_runtime.sql"))
        .await
        .unwrap();
    conn.execute_batch(include_str!("../../../migrations/0012_guild_moves.sql"))
        .await
        .unwrap();
    conn.execute_batch(
        "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
         INSERT INTO meta(key,value) VALUES('schema_family','jmodguard-core-1');
         INSERT INTO guild_store_catalog(store_id, dedicated_guild_id) VALUES
           ('11111111111111111111111111111111', NULL),
           ('22222222222222222222222222222222', 10);
         INSERT INTO guild_store_placements(guild_id, store_id, revision) VALUES
           (10, '11111111111111111111111111111111', 'cccccccccccccccccccccccccccccccc'),
           (20, '11111111111111111111111111111111', 'dddddddddddddddddddddddddddddddd');",
    )
    .await
    .unwrap();
}

async fn init_guild(
    path: &Path,
    store_id: &str,
    dedicated: Option<i64>,
    guilds: &[(i64, &str, &str, &str, &str)],
    version: i64,
) {
    let db = libsql::Builder::new_local(path).build().await.unwrap();
    let conn = db.connect().unwrap();
    for (migration, sql) in [
        (
            1,
            include_str!("../../../migrations/guild/0001_identity.sql"),
        ),
        (
            2,
            include_str!("../../../migrations/guild/0002_tenant_log_channels.sql"),
        ),
        (3, include_str!("../../../migrations/guild/0003_cases.sql")),
        (
            4,
            include_str!("../../../migrations/guild/0004_case_history.sql"),
        ),
        (
            5,
            include_str!("../../../migrations/guild/0005_case_sources.sql"),
        ),
        (
            6,
            include_str!("../../../migrations/guild/0006_case_evidence.sql"),
        ),
        (
            7,
            include_str!("../../../migrations/guild/0007_bunny_evidence.sql"),
        ),
        (
            8,
            include_str!("../../../migrations/guild/0008_erasure.sql"),
        ),
        (
            9,
            include_str!("../../../migrations/guild/0009_message_snapshots.sql"),
        ),
    ] {
        if migration <= version {
            conn.execute_batch(sql).await.unwrap();
        }
    }
    conn.execute(
        "INSERT INTO store_identity(singleton,schema_family,store_id,dedicated_guild_id) VALUES(1,'jmodguard-guild-1',?,?)",
        libsql::params![store_id, dedicated],
    )
    .await
    .unwrap();
    conn.execute(&format!("PRAGMA user_version = {version}"), ())
        .await
        .unwrap();
    for (guild, enrollment, revision, case_id, source) in guilds {
        conn.execute(
            "INSERT INTO guild_bindings(guild_id,revision) VALUES(?,?)",
            libsql::params![*guild, *revision],
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO guild_memberships(guild_id,enrollment_id,joined_at_ns,active,config_revision) VALUES(?,?,1,1,?)",
            libsql::params![*guild, *enrollment, CFG_REV],
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO guild_log_channels(guild_id,enrollment_id,event_type,channel_id) VALUES(?,?,'automod',99)",
            libsql::params![*guild, *enrollment],
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO guild_cases(guild_id,enrollment_id,case_id,source_key,payload,deleted) VALUES(?,?,?,?,?,0)",
            libsql::params![*guild, *enrollment, *case_id, *source, "{\"keep\":true}"],
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
    body: Option<&str>,
) -> (StatusCode, String, Option<String>) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let request = builder
        .body(Body::from(body.unwrap_or("").to_string()))
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
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    (
        status,
        String::from_utf8_lossy(&bytes).into_owned(),
        set_cookie,
    )
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
    let (status, _, session) = call(
        &h.router,
        "GET",
        &format!("/oauth/callback?code=ok&state={state}"),
        Some(&format!("jmodguard_conductor_oauth_state={state}")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "login failed");
    session.unwrap().split(';').next().unwrap().to_string()
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
    panic!("move job did not finish");
}

async fn scalar(path: &Path, sql: &str) -> i64 {
    let db = libsql::Builder::new_local(path).build().await.unwrap();
    let conn = db.connect().unwrap();
    let mut rows = conn.query(sql, ()).await.unwrap();
    rows.next().await.unwrap().unwrap().get(0).unwrap()
}

async fn text(path: &Path, sql: &str) -> String {
    let db = libsql::Builder::new_local(path).build().await.unwrap();
    let conn = db.connect().unwrap();
    let mut rows = conn.query(sql, ()).await.unwrap();
    rows.next().await.unwrap().unwrap().get(0).unwrap()
}

async fn upgrade_guild(path: &Path, from: i64, to: i64) {
    let db = libsql::Builder::new_local(path).build().await.unwrap();
    let conn = db.connect().unwrap();
    for version in from + 1..=to {
        let sql = match version {
            10 => include_str!("../../../migrations/guild/0010_member_directory.sql"),
            11 => include_str!("../../../migrations/guild/0011_member_directory_status.sql"),
            12 => include_str!("../../../migrations/guild/0012_member_sync_progress.sql"),
            13 => include_str!("../../../migrations/guild/0013_member_unresolved_audits.sql"),
            14 => include_str!("../../../migrations/guild/0014_member_case_retain_revision.sql"),
            _ => panic!("unsupported fixture migration {version}"),
        };
        conn.execute_batch(sql).await.unwrap();
    }
    conn.execute(&format!("PRAGMA user_version = {to}"), ())
        .await
        .unwrap();
}

async fn start_dedicated_move(h: &Harness) -> (String, Option<String>) {
    let cookie = login(h).await;
    let (status, body, _) = call(
        &h.router,
        "POST",
        "/api/moves",
        Some(&cookie),
        Some(r#"{"guild_id":"10","destination_store_id":"22222222222222222222222222222222","preop":"skip","confirm_shared_impact":true}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    tick_until_idle(h).await;
    let jobs = h.conductor.store().unwrap().list_jobs(8).await.unwrap();
    let job = jobs.iter().find(|job| job.kind == "guild_move").unwrap();
    (job.status.clone(), job.last_error.clone())
}

const DEST_REV: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

#[tokio::test]
async fn clear_stale_fences_keeps_fence_while_placement_still_on_source() {
    let h = setup().await;
    let core = h.root.join("core.db");
    let db = libsql::Builder::new_local(&core).build().await.unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "INSERT INTO guild_write_fences(guild_id,generation,phase,destination_store_id,destination_revision,created_at)
         VALUES(10, ?, 'freeze', ?, ?, unixepoch())",
        libsql::params![DEST_REV, DEDICATED, DEST_REV],
    )
    .await
    .unwrap();
    jmodguard_conductor::guild_move::clear_stale_fences(
        &h.conductor.cfg,
        h.conductor.store().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        scalar(&core, "SELECT COUNT(*) FROM guild_write_fences").await,
        1
    );
}

#[tokio::test]
async fn clear_stale_fences_drops_orphan_fence_after_cutover() {
    let h = setup().await;
    let core = h.root.join("core.db");
    let db = libsql::Builder::new_local(&core).build().await.unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "INSERT INTO guild_write_fences(guild_id,generation,phase,destination_store_id,destination_revision,created_at)
         VALUES(10, ?, 'switch', ?, ?, unixepoch())",
        libsql::params![DEST_REV, DEDICATED, DEST_REV],
    )
    .await
    .unwrap();
    conn.execute(
        "UPDATE guild_store_placements SET store_id=?, revision=? WHERE guild_id=10",
        libsql::params![DEDICATED, DEST_REV],
    )
    .await
    .unwrap();
    jmodguard_conductor::guild_move::clear_stale_fences(
        &h.conductor.cfg,
        h.conductor.store().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        scalar(&core, "SELECT COUNT(*) FROM guild_write_fences").await,
        0
    );
}

#[tokio::test]
async fn start_move_rejected_while_move_fence_requires_job_retry() {
    let h = setup().await;
    let cookie = login(&h).await;
    let core = h.root.join("core.db");
    let db = libsql::Builder::new_local(&core).build().await.unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "INSERT INTO guild_write_fences(guild_id,generation,phase,destination_store_id,destination_revision,created_at)
         VALUES(10, ?, 'freeze', ?, ?, unixepoch())",
        libsql::params![DEST_REV, DEDICATED, DEST_REV],
    )
    .await
    .unwrap();
    let store = h.conductor.store().unwrap();
    let job_id = store
        .insert_job(
            "guild_move",
            &serde_json::json!({
                "guild_id": 10,
                "destination_store_id": DEDICATED,
                "destination_revision": DEST_REV,
                "preop": "skip",
            }),
            1,
        )
        .await
        .unwrap();
    let claimed = store.claim_job().await.unwrap().unwrap();
    assert_eq!(claimed.id, job_id);
    store
        .finish_job(&job_id, false, Some("simulated failure"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(2)).await;
    for index in 0..51 {
        store
            .insert_job("notify_test", &serde_json::json!({ "index": index }), 1)
            .await
            .unwrap();
    }

    let (status, body, _) = call(
        &h.router,
        "POST",
        "/api/moves",
        Some(&cookie),
        Some(r#"{"guild_id":"10","destination_store_id":"22222222222222222222222222222222","preop":"skip","confirm_shared_impact":true}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains(&job_id), "{body}");
    assert!(body.contains("retry job"), "{body}");
}

#[tokio::test]
async fn failed_move_jobs_surface_retry_on_moves_api() {
    let h = setup().await;
    let cookie = login(&h).await;
    let store = h.conductor.store().unwrap();
    let job_id = store
        .insert_job(
            "guild_move",
            &serde_json::json!({
                "guild_id": 10,
                "destination_store_id": DEDICATED,
                "destination_revision": DEST_REV,
                "preop": "skip",
            }),
            1,
        )
        .await
        .unwrap();
    let claimed = store.claim_job().await.unwrap().unwrap();
    assert_eq!(claimed.id, job_id);
    store
        .finish_job(&job_id, false, Some("simulated failure"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(2)).await;
    for index in 0..51 {
        store
            .insert_job("notify_test", &serde_json::json!({ "index": index }), 1)
            .await
            .unwrap();
    }

    let (status, body, _) = call(&h.router, "GET", "/api/moves", Some(&cookie), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("\"failed_jobs\""), "{body}");
    assert!(body.contains(&job_id), "{body}");
    assert!(body.contains("\"status\":\"failed\""), "{body}");
}

#[tokio::test]
async fn denied_users_cannot_plan_or_start_moves() {
    let h = setup().await;
    let (status, _, _) = call(&h.router, "GET", "/api/moves", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = call(
        &h.router,
        "POST",
        "/api/moves",
        None,
        Some(r#"{"guild_id":"10","destination_store_id":"22222222222222222222222222222222","confirm_shared_impact":true,"preop":"skip"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn shared_to_dedicated_and_back_preserves_the_neighbor() {
    let h = setup().await;
    let shared = h.root.join("shared.guild.db");
    let db = libsql::Builder::new_local(&shared).build().await.unwrap();
    let conn = db.connect().unwrap();
    for (guild, enrollment) in [(10, ENROLL_10), (20, ENROLL_20)] {
        conn.execute("INSERT INTO guild_message_snapshots(guild_id,enrollment_id,channel_id,message_id,content,created_at_ms,first_observed_at_ms,last_observed_at_ms,expires_at_ms,coverage_epoch,encoded_bytes) VALUES(?,?,30,40,'transient',1000,1001,1001,86401000,1,9)", libsql::params![guild,enrollment]).await.unwrap();
    }
    for message_id in 41..141 {
        conn.execute("INSERT INTO guild_message_snapshots(guild_id,enrollment_id,channel_id,message_id,content,created_at_ms,first_observed_at_ms,last_observed_at_ms,expires_at_ms,coverage_epoch,encoded_bytes) VALUES(?,?,30,?,'transient',1000,1001,1001,86401000,1,9)", libsql::params![10_i64, ENROLL_10, message_id]).await.unwrap();
    }
    drop(conn);
    drop(db);
    let cookie = login(&h).await;
    let (status, body, _) = call(&h.router, "GET", "/version", Some(&cookie), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("\"moves_available\":true"), "{body}");

    let (status, body, _) = call(
        &h.router,
        "POST",
        "/api/moves",
        Some(&cookie),
        Some(r#"{"guild_id":"10","destination_store_id":"22222222222222222222222222222222","preop":"skip"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("confirm"), "{body}");
    assert_eq!(
        scalar(
            &h.root.join("shared.guild.db"),
            "SELECT COUNT(*) FROM guild_message_snapshots WHERE guild_id=10"
        )
        .await,
        101
    );

    let (status, plan, _) = call(
        &h.router,
        "GET",
        "/api/moves/plan?guild_id=10&destination_store_id=22222222222222222222222222222222",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{plan}");
    assert!(plan.contains("neighbor_guilds_on_source"), "{plan}");
    assert!(plan.contains("\"20\""), "{plan}");

    let (status, body, _) = call(
        &h.router,
        "POST",
        "/api/moves",
        Some(&cookie),
        Some(r#"{"guild_id":"10","destination_store_id":"22222222222222222222222222222222","preop":"skip","confirm_shared_impact":true}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    tick_until_idle(&h).await;
    let jobs = h.conductor.store().unwrap().list_jobs(8).await.unwrap();
    let move_job = jobs.iter().find(|job| job.kind == "guild_move").unwrap();
    assert_eq!(move_job.status, "succeeded", "{:?}", move_job.last_error);

    let core = h.root.join("core.db");
    assert_eq!(
        text(
            &core,
            "SELECT store_id FROM guild_store_placements WHERE guild_id=10"
        )
        .await,
        DEDICATED
    );
    assert_eq!(
        text(
            &core,
            "SELECT store_id FROM guild_store_placements WHERE guild_id=20"
        )
        .await,
        SHARED
    );
    let dedicated = h.root.join("dedicated.guild.db");
    let shared = h.root.join("shared.guild.db");
    assert_eq!(
        scalar(
            &dedicated,
            "SELECT COUNT(*) FROM guild_message_snapshots WHERE guild_id=10"
        )
        .await,
        0
    );
    assert_eq!(
        scalar(
            &shared,
            "SELECT COUNT(*) FROM guild_message_snapshots WHERE guild_id=10"
        )
        .await,
        0
    );
    assert_eq!(
        scalar(
            &shared,
            "SELECT COUNT(*) FROM guild_message_snapshots WHERE guild_id=20"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            &dedicated,
            "SELECT COUNT(*) FROM guild_cases WHERE guild_id=10"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            &shared,
            "SELECT COUNT(*) FROM guild_cases WHERE guild_id=10"
        )
        .await,
        0
    );
    assert_eq!(
        scalar(
            &shared,
            "SELECT COUNT(*) FROM guild_cases WHERE guild_id=20"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            &shared,
            "SELECT COUNT(*) FROM guild_memberships WHERE guild_id=20"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(&core, "SELECT COUNT(*) FROM guild_write_fences").await,
        0
    );

    let (status, body, _) = call(
        &h.router,
        "POST",
        "/api/moves",
        Some(&cookie),
        Some(r#"{"guild_id":"10","destination_store_id":"11111111111111111111111111111111","preop":"skip","confirm_shared_impact":true}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    tick_until_idle(&h).await;
    let jobs = h.conductor.store().unwrap().list_jobs(8).await.unwrap();
    assert!(
        jobs.iter()
            .any(|job| job.kind == "guild_move" && job.status == "succeeded"),
        "{jobs:?}"
    );
    assert_eq!(
        text(
            &core,
            "SELECT store_id FROM guild_store_placements WHERE guild_id=10"
        )
        .await,
        SHARED
    );
    assert_eq!(
        scalar(
            &shared,
            "SELECT COUNT(*) FROM guild_cases WHERE guild_id=10"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            &shared,
            "SELECT COUNT(*) FROM guild_cases WHERE guild_id=20"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            &dedicated,
            "SELECT COUNT(*) FROM guild_cases WHERE guild_id=10"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn move_budget_ignores_source_free_pages_and_unrelated_rows() {
    let h = setup().await;
    // These allocations simulate an established shared store and a destination
    // with limited headroom. They are fixture writes, not application admission.
    for (name, bytes, free) in [
        ("shared.guild.db", 300_000_000_i64, true),
        ("dedicated.guild.db", 880_000_000_i64, false),
    ] {
        let db = libsql::Builder::new_local(h.root.join(name))
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        conn.execute(
            "CREATE TABLE fixture_unrelated(guild_id INTEGER, payload BLOB)",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO fixture_unrelated VALUES(20,zeroblob(?))",
            libsql::params![bytes],
        )
        .await
        .unwrap();
        if free {
            conn.execute("DELETE FROM fixture_unrelated", ())
                .await
                .unwrap();
        }
    }
    let cookie = login(&h).await;
    let (status, body, _) = call(
        &h.router, "POST", "/api/moves", Some(&cookie),
        Some(r#"{"guild_id":"10","destination_store_id":"22222222222222222222222222222222","preop":"skip","confirm_shared_impact":true}"#),
    ).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    tick_until_idle(&h).await;
    let jobs = h.conductor.store().unwrap().list_jobs(8).await.unwrap();
    let job = jobs.iter().find(|job| job.kind == "guild_move").unwrap();
    assert_eq!(job.status, "succeeded", "{:?}", job.last_error);
    assert_eq!(
        scalar(
            &h.root.join("dedicated.guild.db"),
            "SELECT COUNT(*) FROM guild_cases WHERE guild_id=10"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            &h.root.join("shared.guild.db"),
            "SELECT COUNT(*) FROM guild_cases WHERE guild_id=20"
        )
        .await,
        1
    );
}

#[tokio::test]
async fn move_upgrades_old_destination_before_member_copy() {
    for destination_version in [8, 10] {
        let h = setup_with_destination_version(destination_version).await;
        let shared = h.root.join("shared.guild.db");
        let dedicated = h.root.join("dedicated.guild.db");
        upgrade_guild(&shared, 9, 14).await;
        if destination_version > 9 {
            upgrade_guild(&dedicated, 9, destination_version).await;
        }
        let db = libsql::Builder::new_local(&shared).build().await.unwrap();
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE guild_member_feature SET seed_admitted=1 WHERE guild_id=10",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO guild_member_seed(guild_id,enrollment_id,bot_id,initialized_at_ms,reconciliation_needed,gap_generation) VALUES(10,?,42,1000,1,2)",
            libsql::params![ENROLL_10],
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO guild_member_sync(guild_id,enrollment_id,bot_id,run_id,run_kind,phase,status,started_at_ms,reconcile_gap_generation) VALUES(10,?,42,'run','reconcile','members','waiting',1000,2)",
            libsql::params![ENROLL_10],
        )
        .await
        .unwrap();
        drop(conn);
        drop(db);

        let (status, error) = start_dedicated_move(&h).await;
        assert_eq!(status, "succeeded", "v{destination_version}: {error:?}");
        assert_eq!(scalar(&dedicated, "PRAGMA user_version").await, 14);
        assert_eq!(
            scalar(
                &dedicated,
                "SELECT COUNT(*) FROM sqlite_schema WHERE name='guild_message_snapshots'"
            )
            .await,
            1
        );
        assert_eq!(
            scalar(
                &dedicated,
                "SELECT seed_admitted FROM guild_member_feature WHERE guild_id=10"
            )
            .await,
            1
        );
        assert_eq!(
            scalar(
                &dedicated,
                "SELECT reconcile_gap_generation FROM guild_member_sync WHERE guild_id=10"
            )
            .await,
            2
        );
        assert_eq!(
            scalar(
                &shared,
                "SELECT COUNT(*) FROM guild_bindings WHERE guild_id=10"
            )
            .await,
            0
        );
    }
}

#[tokio::test]
async fn move_upgrades_destination_for_unresolved_audit_rows() {
    let h = setup().await;
    let shared = h.root.join("shared.guild.db");
    let dedicated = h.root.join("dedicated.guild.db");
    upgrade_guild(&shared, 9, 14).await;
    upgrade_guild(&dedicated, 9, 12).await;
    let db = libsql::Builder::new_local(&shared).build().await.unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "INSERT INTO guild_member_unresolved_audits(guild_id,enrollment_id,entry_id,user_id,action,occurred_at_ms) VALUES(10,?,123,7,'ban',1000)",
        libsql::params![ENROLL_10],
    )
    .await
    .unwrap();
    drop(conn);
    drop(db);

    let (status, error) = start_dedicated_move(&h).await;
    assert_eq!(status, "succeeded", "{error:?}");
    assert_eq!(scalar(&dedicated, "PRAGMA user_version").await, 14);
    assert_eq!(
        scalar(
            &dedicated,
            "SELECT COUNT(*) FROM guild_member_unresolved_audits WHERE guild_id=10 AND entry_id=123"
        )
        .await,
        1
    );
}

#[tokio::test]
async fn member_heavy_move_refuses_insufficient_pre_copy_capacity() {
    let h = setup().await;
    let shared = h.root.join("shared.guild.db");
    let dedicated = h.root.join("dedicated.guild.db");
    upgrade_guild(&shared, 9, 14).await;
    upgrade_guild(&dedicated, 9, 14).await;
    let db = libsql::Builder::new_local(&shared).build().await.unwrap();
    let conn = db.connect().unwrap();
    for user_id in 1..=150_i64 {
        conn.execute(
            "INSERT INTO guild_member_directory(guild_id,enrollment_id,user_id,observed_at_ms) VALUES(10,?,?,1000)",
            libsql::params![ENROLL_10, user_id],
        )
        .await
        .unwrap();
    }
    drop(conn);
    drop(db);
    let db = libsql::Builder::new_local(&dedicated)
        .build()
        .await
        .unwrap();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE fixture_unrelated(payload BLOB)", ())
        .await
        .unwrap();
    conn.execute(
        "INSERT INTO fixture_unrelated VALUES(zeroblob(892000000))",
        (),
    )
    .await
    .unwrap();
    drop(conn);
    drop(db);
    assert!(
        scalar(&dedicated, "PRAGMA page_count").await
            * scalar(&dedicated, "PRAGMA page_size").await
            < 896_000_000
    );

    let (status, error) = start_dedicated_move(&h).await;
    assert_eq!(status, "failed");
    assert!(
        error
            .unwrap_or_default()
            .contains("application database capacity budget exhausted")
    );
    assert_eq!(
        scalar(
            &dedicated,
            "SELECT COUNT(*) FROM guild_bindings WHERE guild_id=10"
        )
        .await,
        0
    );
    assert_eq!(
        text(
            &h.root.join("core.db"),
            "SELECT store_id FROM guild_store_placements WHERE guild_id=10"
        )
        .await,
        SHARED
    );
}
