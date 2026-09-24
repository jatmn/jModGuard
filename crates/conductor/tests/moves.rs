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
    setup_with_legacy_cases(false).await
}

async fn setup_with_legacy_cases(legacy_cases: bool) -> Harness {
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
        legacy_cases,
    )
    .await;
    init_guild(&dedicated, DEDICATED, Some(10), &[], false).await;
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
    legacy_cases: bool,
) {
    let db = libsql::Builder::new_local(path).build().await.unwrap();
    let conn = db.connect().unwrap();
    for sql in [
        include_str!("../../../migrations/guild/0001_identity.sql"),
        include_str!("../../../migrations/guild/0002_tenant_log_channels.sql"),
        include_str!("../../../migrations/guild/0003_cases.sql"),
        include_str!("../../../migrations/guild/0004_case_history.sql"),
        include_str!("../../../migrations/guild/0005_case_sources.sql"),
        include_str!("../../../migrations/guild/0006_case_evidence.sql"),
        include_str!("../../../migrations/guild/0007_bunny_evidence.sql"),
        include_str!("../../../migrations/guild/0008_erasure.sql"),
    ] {
        conn.execute_batch(sql).await.unwrap();
    }
    conn.execute(
        "INSERT INTO store_identity(singleton,schema_family,store_id,dedicated_guild_id) VALUES(1,'jmodguard-guild-1',?,?)",
        libsql::params![store_id, dedicated],
    )
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
            libsql::params![
                *guild,
                *enrollment,
                *case_id,
                *source,
                if legacy_cases && *guild == 10 {
                    r#"{"action":"ban","outcome":"succeeded","subject_id":"9007199254740994","occurred_at_ms":1500}"#
                } else {
                    r#"{"keep":true}"#
                }
            ],
        )
        .await
        .unwrap();
        if legacy_cases && *guild == 10 {
            conn.execute(
                "INSERT INTO guild_cases(guild_id,enrollment_id,case_id,source_key,payload,deleted) VALUES(?,?,?,?,?,0)",
                libsql::params![
                    *guild,
                    *enrollment,
                    "88888888888888888888888888888888",
                    "app:99999999999999999999999999999999",
                    r#"{"action":"ban","outcome":"succeeded","subject_id":"9007199254740993","occurred_at_ms":1600}"#
                ],
            )
            .await
            .unwrap();
        }
    }
    conn.execute_batch(include_str!(
        "../../../migrations/guild/0009_member_directory.sql"
    ))
    .await
    .unwrap();
    conn.execute_batch(include_str!(
        "../../../migrations/guild/0010_member_directory_status.sql"
    ))
    .await
    .unwrap();
    conn.execute_batch(include_str!(
        "../../../migrations/guild/0011_member_sync_progress.sql"
    ))
    .await
    .unwrap();
    conn.execute_batch(include_str!(
        "../../../migrations/guild/0012_member_unresolved_audits.sql"
    ))
    .await
    .unwrap();
    conn.execute("PRAGMA user_version = 12", ()).await.unwrap();
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
async fn move_before_case_backfill_keeps_source_profiles_and_trusted_derived_rows() {
    let h = setup_with_legacy_cases(true).await;
    let shared = h.root.join("shared.guild.db");
    let db = libsql::Builder::new_local(&shared).build().await.unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE guild_member_feature SET seed_admitted=1 WHERE guild_id=10",
        (),
    )
    .await
    .unwrap();
    conn.execute(
        "INSERT INTO guild_member_seed(guild_id,enrollment_id,bot_id,initialized_at_ms,initial_completed_at_ms) VALUES(10,?,42,1000,2000)",
        libsql::params![ENROLL_10],
    )
    .await
    .unwrap();
    conn.execute(
        "INSERT INTO guild_member_audit_recovery(guild_id,enrollment_id,bot_id,run_id,started_at_ms,status) VALUES(10,?,42,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaac',1000,'complete')",
        libsql::params![ENROLL_10],
    )
    .await
    .unwrap();
    // The cases predate v9, so bounded backfill has not projected either case.
    // The existing profile must survive the destination trigger unchanged.
    conn.execute(
        "INSERT INTO guild_member_directory(guild_id,enrollment_id,user_id,username,search_text,joined_at_ms,cleanup_after_ms,observed_at_ms,revision) VALUES(10,?,9007199254740993,'Ada','ada',500,3000,1000,7)",
        libsql::params![ENROLL_10],
    )
    .await
    .unwrap();
    assert_eq!(
        scalar(
            &shared,
            "SELECT COUNT(*) FROM guild_member_directory WHERE guild_id=10"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            &shared,
            "SELECT case_backfill_done FROM guild_member_seed WHERE guild_id=10"
        )
        .await,
        0
    );

    let cookie = login(&h).await;
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

    let dedicated = h.root.join("dedicated.guild.db");
    assert_eq!(
        scalar(
            &dedicated,
            "SELECT COUNT(*) FROM guild_member_directory WHERE guild_id=10"
        )
        .await,
        2
    );
    assert_eq!(
        text(&dedicated, "SELECT username FROM guild_member_directory WHERE guild_id=10 AND user_id=9007199254740993").await,
        "Ada"
    );
    for (column, expected) in [
        ("ever_banned", 0),
        ("cleanup_after_ms", 3000),
        ("revision", 7),
        ("joined_at_ms", 500),
    ] {
        assert_eq!(
            scalar(&dedicated, &format!("SELECT {column} FROM guild_member_directory WHERE guild_id=10 AND user_id=9007199254740993")).await,
            expected,
            "source {column} changed during case copy"
        );
    }
    assert_eq!(
        scalar(&dedicated, "SELECT ever_banned FROM guild_member_directory WHERE guild_id=10 AND user_id=9007199254740994").await,
        1
    );
    assert_eq!(
        scalar(
            &dedicated,
            "SELECT initial_completed_at_ms FROM guild_member_seed WHERE guild_id=10"
        )
        .await,
        2000
    );
    assert_eq!(
        scalar(
            &dedicated,
            "SELECT case_backfill_done FROM guild_member_seed WHERE guild_id=10"
        )
        .await,
        0
    );
    assert_eq!(
        scalar(
            &shared,
            "SELECT COUNT(*) FROM guild_member_directory WHERE guild_id=10"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn move_rejects_an_extra_destination_profile_without_a_trusted_case() {
    let h = setup().await;
    let dedicated = h.root.join("dedicated.guild.db");
    let db = libsql::Builder::new_local(&dedicated)
        .build()
        .await
        .unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "INSERT INTO guild_member_directory(guild_id,enrollment_id,user_id,ever_banned,observed_at_ms) VALUES(10,?,9007199254740995,1,1500)",
        libsql::params![ENROLL_10],
    )
    .await
    .unwrap();
    let cookie = login(&h).await;
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
    assert_eq!(move_job.status, "failed");
    assert!(
        move_job
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("without a trusted copied case"),
        "{:?}",
        move_job.last_error
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

#[tokio::test]
async fn shared_to_dedicated_and_back_preserves_the_neighbor() {
    let h = setup().await;
    let shared_before = h.root.join("shared.guild.db");
    let db = libsql::Builder::new_local(&shared_before)
        .build()
        .await
        .unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE guild_member_feature SET seed_admitted=1 WHERE guild_id=10",
        (),
    )
    .await
    .unwrap();
    conn.execute("INSERT INTO guild_member_seed(guild_id,enrollment_id,bot_id,initialized_at_ms,initial_completed_at_ms) VALUES(10,?,42,1000,2000)",libsql::params![ENROLL_10]).await.unwrap();
    conn.execute("INSERT INTO guild_member_sync(guild_id,enrollment_id,bot_id,run_id,run_kind,phase,status,started_at_ms,completed_at_ms) VALUES(10,?,42,'88888888888888888888888888888888','initial','complete','complete',1000,2000)",libsql::params![ENROLL_10]).await.unwrap();
    conn.execute("INSERT INTO guild_member_directory(guild_id,enrollment_id,user_id,username,search_text,is_current,joined_at_ms,current_stay_id,observed_at_ms) VALUES(10,?,9007199254740993,'Ada','ada',1,500,'99999999999999999999999999999999',1000)",libsql::params![ENROLL_10]).await.unwrap();
    conn.execute("INSERT INTO guild_member_stays(guild_id,enrollment_id,stay_id,user_id,joined_at_ms,provenance) VALUES(10,?,'99999999999999999999999999999999',9007199254740993,500,'imported')",libsql::params![ENROLL_10]).await.unwrap();
    conn.execute("INSERT INTO guild_member_state_observations(guild_id,enrollment_id,observation_id,user_id,kind,observed_at_ms) VALUES(10,?,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab',9007199254740993,'ban_absent',1500)",libsql::params![ENROLL_10]).await.unwrap();
    conn.execute("INSERT INTO guild_member_target_checks(guild_id,enrollment_id,user_id,kind,reason,row_revision,requested_at_ms,next_attempt_at_ms) VALUES(10,?,9007199254740993,'member','gap',0,1500,1600)",libsql::params![ENROLL_10]).await.unwrap();
    conn.execute("INSERT INTO guild_member_audit_recovery(guild_id,enrollment_id,bot_id,run_id,started_at_ms,status,processed) VALUES(10,?,42,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaac',1000,'complete',2)",libsql::params![ENROLL_10]).await.unwrap();
    conn.execute("INSERT INTO guild_member_unresolved_audits(guild_id,enrollment_id,entry_id,user_id,action,occurred_at_ms,executor_id,reason) VALUES(10,?,9007199254741993,9007199254740993,'ban',1400,42,'historical own action')",libsql::params![ENROLL_10]).await.unwrap();
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
            "SELECT COUNT(*) FROM guild_cases WHERE guild_id=10"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            &dedicated,
            "SELECT COUNT(*) FROM guild_member_directory WHERE guild_id=10"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            &dedicated,
            "SELECT initial_completed_at_ms FROM guild_member_seed WHERE guild_id=10"
        )
        .await,
        2000
    );
    assert_eq!(
        scalar(
            &dedicated,
            "SELECT reconciliation_needed FROM guild_member_seed WHERE guild_id=10"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            &dedicated,
            "SELECT generation FROM guild_member_sync WHERE guild_id=10"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            &dedicated,
            "SELECT COUNT(*) FROM guild_member_state_observations WHERE guild_id=10"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            &dedicated,
            "SELECT COUNT(*) FROM guild_member_target_checks WHERE guild_id=10"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            &dedicated,
            "SELECT COUNT(*) FROM guild_member_audit_recovery WHERE guild_id=10 AND status='queued'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            &dedicated,
            "SELECT COUNT(*) FROM guild_member_unresolved_audits WHERE guild_id=10 AND entry_id=9007199254741993 AND reason='historical own action'"
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
    assert_eq!(scalar(&shared,"SELECT COUNT(*) FROM guild_member_state_observations WHERE guild_id=10 AND kind='ban_absent'").await,1);
    assert_eq!(scalar(&shared,"SELECT COUNT(*) FROM guild_member_unresolved_audits WHERE guild_id=10 AND entry_id=9007199254741993 AND reason='historical own action'").await,1);
    assert_eq!(
        scalar(
            &shared,
            "SELECT COUNT(*) FROM guild_member_target_checks WHERE guild_id=10"
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
