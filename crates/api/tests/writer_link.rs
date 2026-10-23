//! The production API open is read-only. A settings save reaches a separate writer.
use jmodguard::config::{ApiProcessConfig, BotProcessConfig, ControlSecret};
use jmodguard::db::{self, Db, SqlExecutor};
use jmodguard::tenant::TenantRuntime;
use jmodguard_api::web::state::ApiState;
use jmodguard_api::web::updates::{
    RELEASE_LOOKUP_TIMEOUT, ReleaseCheckTick, ScheduledInstallPolicy, poll_scheduled_installs,
    record_release_checks, run_release_check_loop,
};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn temp_db(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "api_writer_{label}_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

struct InstallSnapshot {
    desired: Option<db::ComponentRelease>,
    desired_launcher: Option<db::ComponentRelease>,
    installed: Option<db::ComponentRelease>,
    state: String,
    auto_last_digest: Option<String>,
    auto_last_window: Option<String>,
}

async fn install_snapshot(db: &Db, component: &str) -> InstallSnapshot {
    let row = db::component_update(db, component).await.unwrap();
    let component_id = component.to_owned();
    let (auto_last_digest, auto_last_window) = db
        .with_conn(move |conn| {
            conn.query_row(
                "SELECT auto_last_digest, auto_last_window FROM component_updates WHERE component=?1",
                [component_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
        })
        .await
        .unwrap();
    InstallSnapshot {
        desired: row.desired,
        desired_launcher: row.desired_launcher,
        installed: row.installed,
        state: row.state,
        auto_last_digest,
        auto_last_window,
    }
}

async fn assert_install_unchanged(db: &Db, component: &str, before: &InstallSnapshot) {
    let after = install_snapshot(db, component).await;
    assert_eq!(after.desired, before.desired, "{component} desired");
    assert_eq!(
        after.desired_launcher, before.desired_launcher,
        "{component} desired_launcher"
    );
    assert_eq!(after.installed, before.installed, "{component} installed");
    assert_eq!(after.state, before.state, "{component} state");
    assert_ne!(after.state, "queued", "{component}");
    assert_eq!(
        after.auto_last_digest, before.auto_last_digest,
        "{component} auto_last_digest"
    );
    assert_eq!(
        after.auto_last_window, before.auto_last_window,
        "{component} auto_last_window"
    );
}

fn discover_release(
    component: &str,
    channel: &str,
    digest_nibble: char,
) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<db::ComponentRelease>> + Send>> {
    let release = test_release(component, channel, digest_nibble);
    Box::pin(async move { Ok(release) })
}

fn test_release(component: &str, channel: &str, digest_nibble: char) -> db::ComponentRelease {
    let digest = format!("sha256:{}", digest_nibble.to_string().repeat(64));
    db::ComponentRelease {
        upgrade: None,
        component: component.into(),
        channel: channel.into(),
        release_id: "test".into(),
        source_sha: "a".repeat(40),
        published_at: "2026-10-01T00:00:00Z".into(),
        image: format!("ghcr.io/jatmn/jmodguard-{component}"),
        digest,
        min_schema: db::CORE_SCHEMA_VERSION,
        max_schema: db::CORE_SCHEMA_VERSION,
        result_schema: db::CORE_SCHEMA_VERSION,
        min_api: 1,
        max_api: 1,
        launcher_protocol: 1,
        launcher: None,
        base_abi: if component == "webui" {
            "static-webui-v1".into()
        } else {
            "debian-bookworm-amd64".into()
        },
    }
}

fn pending_shutdown() -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(std::future::pending())
}

fn watch_shutdown(
    rx: &tokio::sync::watch::Receiver<bool>,
) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    let mut rx = rx.clone();
    Box::pin(async move {
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    })
}

fn cleanup(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        let _ = std::fs::remove_file(format!("{}.guild.db{suffix}", path.display()));
    }
    jmodguard::tenant::remove_sidecars(path);
    if let Some(name) = path.file_name() {
        let mut key = name.to_os_string();
        key.push(".writer-link");
        let _ = std::fs::remove_file(path.with_file_name(key));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn api_paths_open_no_write_connection_and_save_through_the_bot() {
    let path = temp_db("control");
    let bot_db = Db::open(&path, 2).unwrap();
    let mut bot_cfg = BotProcessConfig::default();
    bot_cfg.db.path = path.clone();
    bot_cfg.bot.control_bind = "127.0.0.1:0".into();
    bot_cfg.bot.control_secret = ControlSecret::new("x".repeat(32));
    let _tenant = TenantRuntime::open(
        &bot_db,
        &bot_cfg.db,
        &bot_cfg.guild_stores,
        &bot_cfg.evidence,
    )
    .unwrap();
    let enrollment = db::enroll_guild(&bot_db, 55_555, 1).await.unwrap().unwrap();
    let bot = jmodguard_bot::state::BotState::new(bot_cfg, bot_db).unwrap();
    bot.activation.mark_lock_held(true);
    assert!(
        db::claim_active_bot(
            &bot.db,
            bot.activation.instance(),
            jmodguard::control::now_ms(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await
        .unwrap()
    );
    bot.install_control_actor_lookup(|actor, _op| {
        (actor == 7).then_some(db::CachedGuildActor {
            guild_owner_id: 0,
            permissions: 1 << 5,
        })
    });
    let bound = jmodguard_bot::control_server::spawn(Arc::clone(&bot))
        .unwrap()
        .expect("control listener");

    let mut cfg = ApiProcessConfig::default();
    cfg.db.path = path.clone();
    cfg.db.pool_size = 2;
    cfg.bot.control_secret = ControlSecret::new("x".repeat(32));
    let opts = cfg.db_open_options();
    assert!(opts.read_only, "production API options stay read-only");
    assert!(!opts.migrate);
    let probe = Arc::new(db::StartupProbe::default());
    let _guard = db::enter_startup_probe(Some(Arc::clone(&probe)));
    let db = Db::open_with(opts).unwrap();
    assert!(db.is_read_only());
    let refused = db
        .with_conn(|conn| conn.transaction_immediate().map(|_| ()))
        .await;
    assert!(
        refused.is_err(),
        "a read-only API connection cannot begin a write"
    );
    assert_eq!(probe.immediate(), 0);
    cfg.bot.control_target = bound.to_string();
    let app = ApiState::new(cfg, db).unwrap();
    let rejected = app
        .commit_actor(
            9,
            db::ControlOp::PatchSettings {
                scope: db::ScopeWire {
                    global: false,
                    guild_id: 55_555,
                    enrollment_id: enrollment.id.clone(),
                    joined_at_ns: 1,
                },
                prefix: db::FieldPatch::Set("!revoked".into()),
                bot_name: db::FieldPatch::Absent,
                reconcile: false,
            },
        )
        .await
        .unwrap();
    assert!(matches!(rejected, db::ControlReply::Failed { .. }));
    assert_ne!(
        db::get_settings(&bot.db, 55_555).await.unwrap().prefix,
        "!revoked"
    );
    let reply = app
        .commit_actor(
            7,
            db::ControlOp::PatchSettings {
                scope: db::ScopeWire {
                    global: false,
                    guild_id: 55_555,
                    enrollment_id: enrollment.id.clone(),
                    joined_at_ns: 1,
                },
                prefix: db::FieldPatch::Set("!saved".into()),
                bot_name: db::FieldPatch::Absent,
                reconcile: false,
            },
        )
        .await
        .unwrap();
    assert!(matches!(reply, db::ControlReply::Saved { .. }));
    assert_eq!(
        probe.immediate(),
        0,
        "the settings save must not open a write transaction on the API"
    );
    assert_eq!(
        db::get_settings(&bot.db, 55_555).await.unwrap().prefix,
        "!saved"
    );
    // Only the production bot writer can persist the managed list.
    let scope = db::ConfigurationScope::Guild(enrollment.clone());
    let hash = "ab".repeat(32);
    let add = db::ControlOp::AddKnownBadImage {
        scope: scope.to_wire(),
        hash: hash.clone(),
        note: Some("confirmed spam".into()),
    };
    assert!(matches!(
        app.commit_actor(9, add.clone()).await.unwrap(),
        db::ControlReply::Failed { .. }
    ));
    assert!(matches!(
        app.commit_actor(7, add.clone()).await.unwrap(),
        db::ControlReply::Saved { .. }
    ));
    assert!(matches!(
        app.commit_actor(7, add).await.unwrap(),
        db::ControlReply::Unchanged { .. }
    ));
    let rows = db::load_enrolled_web_config_snapshot(&app.db, scope.clone())
        .await
        .unwrap()
        .known_bad_images;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].hash, hash);
    assert_eq!(rows[0].actor, "unknown");
    assert_eq!(probe.immediate(), 0, "hash saves open no API writer");
    let remove = db::ControlOp::RemoveKnownBadImage {
        scope: scope.to_wire(),
        hash,
    };
    assert!(matches!(
        app.commit_actor(7, remove).await.unwrap(),
        db::ControlReply::Saved { .. }
    ));
    assert!(
        db::load_enrolled_web_config_snapshot(&app.db, scope)
            .await
            .unwrap()
            .known_bad_images
            .is_empty()
    );
    drop(app);
    drop(bot);
    cleanup(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_control_secret_fails_api_writes_closed() {
    let path = temp_db("secret");
    let _writer = Db::open(&path, 2).unwrap();
    let mut cfg = ApiProcessConfig::default();
    cfg.db.path = path.clone();
    cfg.db.pool_size = 2;
    let db = Db::open_with(cfg.db_open_options()).unwrap();
    assert!(db.is_read_only());
    let app = ApiState::new(cfg, db).unwrap();
    let error = app
        .commit(db::ControlOp::AbortUpgrade { id: "1".into() })
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "control link unavailable");
    drop(app);
    cleanup(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_control_secret_does_not_start_the_listener() {
    let path = temp_db("bot-secret");
    let db = Db::open(&path, 2).unwrap();
    let mut cfg = BotProcessConfig::default();
    cfg.db.path = path.clone();
    cfg.bot.control_bind = "127.0.0.1:0".into();
    let bot = jmodguard_bot::state::BotState::new(cfg, db).unwrap();
    let started = jmodguard_bot::control_server::spawn(Arc::clone(&bot)).unwrap();
    assert!(started.is_none());
    drop(bot);
    cleanup(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn normal_api_routes_and_startup_do_not_open_a_writer() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let path = temp_db("router");
    let schema = Db::open(&path, 2).unwrap();
    drop(schema);
    let mut cfg = ApiProcessConfig::default();
    cfg.db.path = path.clone();
    cfg.db.pool_size = 2;
    let opts = cfg.db_open_options();
    assert!(opts.read_only);
    assert!(!opts.migrate);
    let flag = Arc::new(AtomicBool::new(false));
    db::arm_writer_open_tripwire(Arc::clone(&flag));
    let db = jmodguard_api::startup::open_database(&cfg).await.unwrap();
    assert!(db.is_read_only());
    assert!(
        !flag.load(Ordering::SeqCst),
        "startup open must stay read-only"
    );
    let app = ApiState::new(cfg, db).unwrap();
    assert!(app.db.is_read_only());
    let _router = jmodguard_api::web::router(Arc::clone(&app));
    assert!(
        !flag.load(Ordering::SeqCst),
        "building the normal router must not open a writer"
    );
    cleanup(&path);
}

async fn read_serves_and_write_is_unavailable(cfg: ApiProcessConfig, path: PathBuf) {
    let db = Db::open_with(cfg.db_open_options()).unwrap();
    assert!(db.is_read_only());
    let app = ApiState::new(cfg, db).unwrap();
    assert!(app.poll_config_version().await.is_ok());
    let error = app
        .commit(db::ControlOp::AbortUpgrade { id: "1".into() })
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "control link unavailable");
    drop(app);
    cleanup(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn unset_control_target_serves_a_read_and_a_write_is_unavailable() {
    let path = temp_db("unset-bind");
    let _writer = Db::open(&path, 2).unwrap();
    let mut cfg = ApiProcessConfig::default();
    cfg.db.path = path.clone();
    cfg.db.pool_size = 2;
    cfg.bot.control_secret = ControlSecret::new("x".repeat(32));
    assert!(cfg.bot.control_target.is_empty());
    read_serves_and_write_is_unavailable(cfg, path).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_control_target_serves_a_read_and_a_write_is_unavailable() {
    let path = temp_db("empty-bind");
    let _writer = Db::open(&path, 2).unwrap();
    let mut cfg = ApiProcessConfig::default();
    cfg.db.path = path.clone();
    cfg.db.pool_size = 2;
    cfg.bot.control_target = String::new();
    cfg.bot.control_secret = ControlSecret::new("x".repeat(32));
    read_serves_and_write_is_unavailable(cfg, path).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn owner_check_through_the_control_link_stores_a_fresh_candidate() {
    let path = temp_db("check");
    let bot_db = Db::open(&path, 2).unwrap();
    let mut bot_cfg = BotProcessConfig::default();
    bot_cfg.db.path = path.clone();
    bot_cfg.bot.control_bind = "127.0.0.1:0".into();
    bot_cfg.bot.control_secret = ControlSecret::new("x".repeat(32));
    let bot = jmodguard_bot::state::BotState::new(bot_cfg, bot_db).unwrap();
    bot.activation.mark_lock_held(true);
    assert!(
        db::claim_active_bot(
            &bot.db,
            bot.activation.instance(),
            jmodguard::control::now_ms(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await
        .unwrap()
    );
    let bound = jmodguard_bot::control_server::spawn(Arc::clone(&bot))
        .unwrap()
        .expect("control listener");
    let mut cfg = ApiProcessConfig::default();
    cfg.db.path = path.clone();
    cfg.db.pool_size = 2;
    cfg.bot.control_secret = ControlSecret::new("x".repeat(32));
    cfg.bot.control_target = bound.to_string();
    let probe = Arc::new(db::StartupProbe::default());
    let _guard = db::enter_startup_probe(Some(Arc::clone(&probe)));
    let db = Db::open_with(cfg.db_open_options()).unwrap();
    let app = Arc::new(ApiState::new(cfg, db).unwrap());
    let state = db::component_update(&app.db, "bot").await.unwrap();
    let now = chrono::Utc::now().timestamp();
    let candidate = db::ComponentRelease {
        component: "bot".into(),
        channel: state.channel.clone(),
        release_id: "test".into(),
        source_sha: "a".repeat(40),
        published_at: "2026-10-01T00:00:00Z".into(),
        image: "ghcr.io/jatmn/jmodguard-bot".into(),
        digest: format!("sha256:{}", "c".repeat(64)),
        min_schema: db::CORE_SCHEMA_VERSION,
        max_schema: db::CORE_SCHEMA_VERSION,
        result_schema: db::CORE_SCHEMA_VERSION,
        min_api: 1,
        max_api: 1,
        launcher_protocol: 1,
        launcher: None,
        base_abi: "debian-bookworm-amd64".into(),
        upgrade: None,
    };
    let reply = app
        .commit(db::ControlOp::RecordUpdateCheck {
            component: "bot".into(),
            channel: state.channel.clone(),
            candidate: Some(candidate),
            error: None,
            now,
        })
        .await
        .unwrap();
    assert!(matches!(reply, db::ControlReply::Saved { .. }), "{reply:?}");
    let stored = db::component_update(&bot.db, "bot").await.unwrap();
    assert_eq!(
        stored.candidate.unwrap().digest,
        format!("sha256:{}", "c".repeat(64))
    );
    assert!(
        !jmodguard_api::web::updates::release_check_is_stale(stored.checked_at, now),
        "an install immediately after Check must not be stale"
    );
    let before_probe = probe.immediate();
    let before_bot = install_snapshot(&bot.db, "bot").await;
    let before_api = install_snapshot(&bot.db, "api").await;
    let before_webui = install_snapshot(&bot.db, "webui").await;
    assert!(
        record_release_checks(
            &app,
            |component, channel| discover_release(component, channel, 'd'),
            RELEASE_LOOKUP_TIMEOUT,
            pending_shutdown(),
        )
        .await
        .finished
    );
    assert_eq!(
        probe.immediate(),
        before_probe,
        "the hourly API timer must not open a local write transaction"
    );
    let expected_digest = format!("sha256:{}", "d".repeat(64));
    for component in ["bot", "api", "webui"] {
        let stored = db::component_update(&bot.db, component).await.unwrap();
        assert_eq!(stored.candidate.unwrap().digest, expected_digest);
        assert!(stored.checked_at.is_some());
        assert!(stored.check_error.is_none());
    }
    assert_install_unchanged(&bot.db, "bot", &before_bot).await;
    assert_install_unchanged(&bot.db, "api", &before_api).await;
    assert_install_unchanged(&bot.db, "webui", &before_webui).await;

    let policy = app
        .commit(db::ControlOp::SetUpdatePolicy {
            component: "api".into(),
            channel: "stable".into(),
            cadence: "daily".into(),
            unattended_allowed: true,
        })
        .await
        .unwrap();
    assert!(matches!(policy, db::ControlReply::Saved { .. }));
    let before_api = install_snapshot(&bot.db, "api").await;
    assert!(
        record_release_checks(
            &app,
            |component, channel| discover_release(component, channel, 'e'),
            RELEASE_LOOKUP_TIMEOUT,
            pending_shutdown(),
        )
        .await
        .finished
    );
    let api_row = db::component_update(&bot.db, "api").await.unwrap();
    assert!(api_row.desired.is_none());
    assert_ne!(api_row.state, "queued");
    assert_install_unchanged(&bot.db, "api", &before_api).await;
    drop(app);
    drop(bot);
    cleanup(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn background_check_records_the_channel_digest_and_does_not_install() {
    let (bot, app, path, _enrollment) = linked_writer("bg-digest").await;
    let probe = Arc::new(db::StartupProbe::default());
    let _guard = db::enter_startup_probe(Some(Arc::clone(&probe)));
    let before_probe = probe.immediate();
    let snapshots = [
        ("bot", install_snapshot(&bot.db, "bot").await),
        ("api", install_snapshot(&bot.db, "api").await),
        ("webui", install_snapshot(&bot.db, "webui").await),
    ];
    app.commit(db::ControlOp::RecordUpdateCheck {
        component: "bot".into(),
        channel: db::component_update(&app.db, "bot").await.unwrap().channel,
        candidate: None,
        error: Some("seed".into()),
        now: chrono::Utc::now().timestamp(),
    })
    .await
    .unwrap();
    assert!(
        record_release_checks(
            &app,
            |component, channel| discover_release(component, channel, 'f'),
            RELEASE_LOOKUP_TIMEOUT,
            pending_shutdown(),
        )
        .await
        .finished
    );
    assert_eq!(probe.immediate(), before_probe);
    let expected_digest = format!("sha256:{}", "f".repeat(64));
    for component in ["bot", "api", "webui"] {
        let stored = db::component_update(&bot.db, component).await.unwrap();
        assert_eq!(stored.candidate.unwrap().digest, expected_digest);
        assert!(stored.checked_at.is_some());
        assert!(stored.check_error.is_none());
    }
    for (component, before) in snapshots {
        assert_install_unchanged(&bot.db, component, &before).await;
    }
    drop(app);
    drop(bot);
    cleanup(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn background_check_records_a_lookup_error_and_does_not_install() {
    let (bot, app, path, _enrollment) = linked_writer("bg-err").await;
    let seed = test_release("bot", "latest", '1');
    app.commit(db::ControlOp::RecordUpdateCheck {
        component: "bot".into(),
        channel: "latest".into(),
        candidate: Some(seed.clone()),
        error: None,
        now: 1,
    })
    .await
    .unwrap();
    let before = [
        ("bot", install_snapshot(&bot.db, "bot").await),
        ("api", install_snapshot(&bot.db, "api").await),
        ("webui", install_snapshot(&bot.db, "webui").await),
    ];
    let calls = AtomicUsize::new(0);
    assert!(
        record_release_checks(
            &app,
            |_component, _channel| {
                calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { anyhow::bail!("registry lookup failed") })
            },
            RELEASE_LOOKUP_TIMEOUT,
            pending_shutdown(),
        )
        .await
        .finished
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    for component in ["bot", "api", "webui"] {
        let stored = db::component_update(&bot.db, component).await.unwrap();
        assert!(
            stored
                .check_error
                .as_deref()
                .is_some_and(|err| err.contains("registry lookup failed"))
        );
        assert!(stored.checked_at.is_some());
    }
    let bot_row = db::component_update(&bot.db, "bot").await.unwrap();
    assert_eq!(bot_row.candidate.unwrap().digest, seed.digest);
    for (component, snap) in before {
        assert_install_unchanged(&bot.db, component, &snap).await;
    }
    drop(app);
    drop(bot);
    cleanup(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn background_check_records_a_lookup_timeout_and_does_not_install() {
    let (bot, app, path, _enrollment) = linked_writer("bg-timeout").await;
    let before = [
        ("bot", install_snapshot(&bot.db, "bot").await),
        ("api", install_snapshot(&bot.db, "api").await),
        ("webui", install_snapshot(&bot.db, "webui").await),
    ];
    let started = std::time::Instant::now();
    assert!(
        record_release_checks(
            &app,
            |_component, _channel| {
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    Ok(test_release("bot", "latest", '2'))
                })
            },
            Duration::from_millis(50),
            pending_shutdown(),
        )
        .await
        .finished
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    for component in ["bot", "api", "webui"] {
        let stored = db::component_update(&bot.db, component).await.unwrap();
        assert!(
            stored
                .check_error
                .as_deref()
                .is_some_and(|err| err.contains("timed out"))
        );
    }
    for (component, snap) in before {
        assert_install_unchanged(&bot.db, component, &snap).await;
    }
    drop(app);
    drop(bot);
    cleanup(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn background_check_records_http_429_and_does_not_retry() {
    let (bot, app, path, _enrollment) = linked_writer("bg-429").await;
    let seed = test_release("bot", "latest", '3');
    app.commit(db::ControlOp::RecordUpdateCheck {
        component: "bot".into(),
        channel: "latest".into(),
        candidate: Some(seed.clone()),
        error: None,
        now: 1,
    })
    .await
    .unwrap();
    let before = install_snapshot(&bot.db, "bot").await;
    let calls = AtomicUsize::new(0);
    assert!(
        record_release_checks(
            &app,
            |_component, _channel| {
                calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { anyhow::bail!("registry returned HTTP 429") })
            },
            RELEASE_LOOKUP_TIMEOUT,
            pending_shutdown(),
        )
        .await
        .finished
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let bot_row = db::component_update(&bot.db, "bot").await.unwrap();
    assert!(
        bot_row
            .check_error
            .as_deref()
            .is_some_and(|err| err.contains("429"))
    );
    assert_eq!(bot_row.candidate.unwrap().digest, seed.digest);
    assert_install_unchanged(&bot.db, "bot", &before).await;
    drop(app);
    drop(bot);
    cleanup(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn background_check_skips_record_when_update_lease_held() {
    let (bot, app, path, _enrollment) = linked_writer("bg-lease").await;
    let owner = test_release("bot", "latest", '3');
    let now = chrono::Utc::now().timestamp();
    app.commit(db::ControlOp::RecordUpdateCheck {
        component: "bot".into(),
        channel: "latest".into(),
        candidate: Some(owner.clone()),
        error: None,
        now,
    })
    .await
    .unwrap();
    assert_eq!(
        db::component_update(&bot.db, "bot")
            .await
            .unwrap()
            .candidate
            .unwrap()
            .digest,
        owner.digest
    );
    let held = "ab".repeat(16);
    let claim = app
        .commit(db::ControlOp::ClaimUpdateLease {
            component: "bot".into(),
            owner: held.clone(),
            now,
            ttl: 240,
        })
        .await
        .unwrap();
    assert!(matches!(claim, db::ControlReply::Saved { .. }), "{claim:?}");
    assert!(
        record_release_checks(
            &app,
            |component, channel| discover_release(component, channel, 'd'),
            RELEASE_LOOKUP_TIMEOUT,
            pending_shutdown(),
        )
        .await
        .finished
    );
    let bot_row = db::component_update(&bot.db, "bot").await.unwrap();
    assert_eq!(bot_row.candidate.unwrap().digest, owner.digest);
    let api_row = db::component_update(&bot.db, "api").await.unwrap();
    assert_eq!(
        api_row.candidate.unwrap().digest,
        format!("sha256:{}", "d".repeat(64))
    );
    app.commit(db::ControlOp::ReleaseUpdateLease {
        component: "bot".into(),
        owner: held,
    })
    .await
    .unwrap();
    drop(app);
    drop(bot);
    cleanup(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn background_check_skips_scheduled_install_when_update_lease_held() {
    let (bot, app, path, _enrollment) = linked_writer("bg-lease-install").await;
    let now = chrono::Utc::now().timestamp();
    for component in ["bot", "api"] {
        app.commit(db::ControlOp::RecordBakedComponentRunning {
            component: component.into(),
            observation: running_observation(now),
            now,
        })
        .await
        .unwrap();
    }
    app.commit(db::ControlOp::SetUpdatePolicy {
        component: "bot".into(),
        channel: "nightly".into(),
        cadence: "weekly".into(),
        unattended_allowed: true,
    })
    .await
    .unwrap();
    app.commit(db::ControlOp::SetUpdatePolicy {
        component: "api".into(),
        channel: "stable".into(),
        cadence: "daily".into(),
        unattended_allowed: true,
    })
    .await
    .unwrap();
    let bot_seed = promoted_release("bot", "nightly", '0');
    app.commit(db::ControlOp::RecordUpdateCheck {
        component: "bot".into(),
        channel: "nightly".into(),
        candidate: Some(bot_seed),
        error: None,
        now,
    })
    .await
    .unwrap();
    let held = "cd".repeat(16);
    let claim = app
        .commit(db::ControlOp::ClaimUpdateLease {
            component: "bot".into(),
            owner: held.clone(),
            now,
            ttl: 240,
        })
        .await
        .unwrap();
    assert!(matches!(claim, db::ControlReply::Saved { .. }), "{claim:?}");
    let before_bot = install_snapshot(&bot.db, "bot").await;
    let apply = ScheduledInstallPolicy::enabled(true, true);
    let batch = record_release_checks(
        &app,
        |component, channel| {
            let nibble = if component == "api" { 'a' } else { 'c' };
            let release = promoted_release(component, channel, nibble);
            Box::pin(async move { Ok(release) })
        },
        RELEASE_LOOKUP_TIMEOUT,
        pending_shutdown(),
    )
    .await;
    assert!(batch.finished);
    assert!(batch.lease_held.contains("bot"));
    poll_scheduled_installs(&app, &batch.lease_held, apply).await;
    assert_install_unchanged(&bot.db, "bot", &before_bot).await;
    let api_row = db::component_update(&bot.db, "api").await.unwrap();
    assert_eq!(api_row.state, "queued");
    assert_eq!(
        api_row.desired.unwrap().digest,
        format!("sha256:{}", "a".repeat(64))
    );
    app.commit(db::ControlOp::ReleaseUpdateLease {
        component: "bot".into(),
        owner: held,
    })
    .await
    .unwrap();
    drop(app);
    drop(bot);
    cleanup(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn background_check_skips_overlapping_ticks() {
    let (bot, app, path, _enrollment) = linked_writer("bg-overlap").await;
    let gate = Arc::new(tokio::sync::Notify::new());
    let tick_notify = Arc::new(tokio::sync::Notify::new());
    let in_flight = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let (lookup_started_tx, mut lookup_started_rx) = tokio::sync::mpsc::channel(8);
    let (parked_tx, mut parked_rx) = tokio::sync::watch::channel(0u32);
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let app_task = Arc::clone(&app);
    let gate_worker = Arc::clone(&gate);
    let in_flight_worker = Arc::clone(&in_flight);
    let calls_worker = Arc::clone(&calls);
    let tick = ReleaseCheckTick::coalescing(Arc::clone(&tick_notify), parked_tx);
    tick.fire();
    let worker = tokio::spawn(async move {
        run_release_check_loop(
            app_task,
            tick,
            RELEASE_LOOKUP_TIMEOUT,
            move |component, channel| {
                let (component, channel) = (component.to_owned(), channel.to_owned());
                let gate = Arc::clone(&gate_worker);
                let in_flight = Arc::clone(&in_flight_worker);
                let calls = Arc::clone(&calls_worker);
                let lookup_started_tx = lookup_started_tx.clone();
                Box::pin(async move {
                    let ordinal = calls.fetch_add(1, Ordering::SeqCst) + 1;
                    let active = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    assert!(active <= 1, "overlapping release checks");
                    lookup_started_tx
                        .send(ordinal)
                        .await
                        .expect("test must receive lookup start signals");
                    if ordinal == 1 {
                        gate.notified().await;
                    }
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    Ok(test_release(&component, &channel, '4'))
                })
            },
            || watch_shutdown(&stop_rx),
            ScheduledInstallPolicy::enabled(false, false),
        )
        .await;
    });
    let wait = Duration::from_secs(5);
    assert_eq!(
        tokio::time::timeout(wait, lookup_started_rx.recv())
            .await
            .expect("first lookup did not start in time"),
        Some(1)
    );
    tick_notify.notify_one();
    tick_notify.notify_one();
    tick_notify.notify_one();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    gate.notify_one();
    for expected in [2, 3] {
        assert_eq!(
            tokio::time::timeout(wait, lookup_started_rx.recv())
                .await
                .expect("lookup did not finish the first tick in time"),
            Some(expected)
        );
    }
    for expected in [4, 5, 6] {
        assert_eq!(
            tokio::time::timeout(wait, lookup_started_rx.recv())
                .await
                .expect("second tick did not run in time"),
            Some(expected)
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 6);
    const SECOND_TICK_PARKED: u32 = 2;
    let parked = tokio::time::timeout(wait, async {
        while *parked_rx.borrow() < SECOND_TICK_PARKED {
            if parked_rx.changed().await.is_err() {
                break;
            }
        }
    })
    .await;
    assert!(
        parked.is_ok() && *parked_rx.borrow() >= SECOND_TICK_PARKED,
        "release check loop did not park after the second tick"
    );
    let _ = stop_tx.send(true);
    worker.await.unwrap();
    drop(app);
    drop(bot);
    cleanup(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn background_check_stops_on_shutdown_without_installing() {
    let (bot, app, path, _enrollment) = linked_writer("bg-shutdown").await;
    let api_gate = Arc::new(tokio::sync::Notify::new());
    let webui_called = Arc::new(AtomicUsize::new(0));
    let (api_entered_tx, mut api_entered_rx) = tokio::sync::mpsc::channel(1);
    let tick_notify = Arc::new(tokio::sync::Notify::new());
    let (parked_tx, _parked_rx) = tokio::sync::watch::channel(0u32);
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let app_task = Arc::clone(&app);
    let api_gate_worker = Arc::clone(&api_gate);
    let webui_called_worker = Arc::clone(&webui_called);
    let tick = ReleaseCheckTick::coalescing(Arc::clone(&tick_notify), parked_tx);
    tick.fire();
    let worker = tokio::spawn(async move {
        run_release_check_loop(
            app_task,
            tick,
            RELEASE_LOOKUP_TIMEOUT,
            move |component, channel| {
                let (component, channel) = (component.to_owned(), channel.to_owned());
                let api_gate = Arc::clone(&api_gate_worker);
                let webui_called = Arc::clone(&webui_called_worker);
                let api_entered_tx = api_entered_tx.clone();
                Box::pin(async move {
                    if component == "bot" {
                        return Ok(test_release(&component, &channel, '5'));
                    }
                    if component == "api" {
                        let _ = api_entered_tx.try_send(());
                        api_gate.notified().await;
                        Ok(test_release(&component, &channel, '6'))
                    } else {
                        webui_called.fetch_add(1, Ordering::SeqCst);
                        Ok(test_release(&component, &channel, '7'))
                    }
                })
            },
            || watch_shutdown(&stop_rx),
            ScheduledInstallPolicy::enabled(false, false),
        )
        .await;
    });
    let wait = Duration::from_secs(5);
    tokio::time::timeout(wait, api_entered_rx.recv())
        .await
        .expect("api discover did not start in time")
        .expect("api discover exited before signaling");
    let _ = stop_tx.send(true);
    api_gate.notify_one();
    tokio::time::timeout(wait, worker)
        .await
        .expect("release check loop did not stop in time")
        .unwrap();
    let bot_row = db::component_update(&bot.db, "bot").await.unwrap();
    assert_eq!(
        bot_row.candidate.unwrap().digest,
        format!("sha256:{}", "5".repeat(64))
    );
    let api_row = db::component_update(&bot.db, "api").await.unwrap();
    assert!(api_row.check_error.is_none());
    assert_eq!(webui_called.load(Ordering::SeqCst), 0);
    for component in ["bot", "api", "webui"] {
        let row = db::component_update(&bot.db, component).await.unwrap();
        assert_ne!(row.state, "queued");
    }
    let now = chrono::Utc::now().timestamp();
    for component in ["bot", "api", "webui"] {
        let owner = format!("{:032x}", rand::random::<u128>());
        let claim = app
            .commit(db::ControlOp::ClaimUpdateLease {
                component: component.into(),
                owner,
                now,
                ttl: 240,
            })
            .await
            .unwrap();
        assert!(matches!(claim, db::ControlReply::Saved { .. }), "{claim:?}");
    }
    drop(app);
    drop(bot);
    cleanup(&path);
}

fn running_observation(now: i64) -> db::ComponentObservation {
    db::ComponentObservation {
        source_sha: "b".repeat(40),
        executable_sha256: "c".repeat(64),
        admin_sha256: None,
        base_image_sha: "d".repeat(40),
        started_at: now,
        min_schema: db::CORE_SCHEMA_VERSION,
        max_schema: db::CORE_SCHEMA_VERSION,
        api_capability: 1,
        self_update_enabled: true,
        base_abi: "debian-bookworm-amd64".into(),
        guild_schema: Some(1),
        update_protocol: db::UPDATE_CONTROL_PROTOCOL,
        launcher_revision: db::LAUNCHER_REVISION,
    }
}

fn promoted_release(component: &str, channel: &str, nibble: char) -> db::ComponentRelease {
    let mut release = test_release(component, channel, nibble);
    release.upgrade = Some(db::UpgradeCompatibility {
        control_protocol: db::UPDATE_CONTROL_PROTOCOL,
        min_core: 15,
        min_guild: 1,
        guild_schema: 1,
    });
    release.launcher = Some(db::LauncherCompatibility {
        revision: db::LAUNCHER_REVISION,
        min_revision: db::REQUIRED_LAUNCHER_REVISION,
    });
    release
}

#[tokio::test(flavor = "multi_thread")]
async fn background_check_still_schedules_promoted_cadences() {
    let (bot, app, path, _enrollment) = linked_writer("bg-cadence").await;
    let now = chrono::Utc::now().timestamp();
    for component in ["bot", "api"] {
        app.commit(db::ControlOp::RecordBakedComponentRunning {
            component: component.into(),
            observation: running_observation(now),
            now,
        })
        .await
        .unwrap();
    }
    app.commit(db::ControlOp::SetUpdatePolicy {
        component: "bot".into(),
        channel: "nightly".into(),
        cadence: "weekly".into(),
        unattended_allowed: true,
    })
    .await
    .unwrap();
    app.commit(db::ControlOp::SetUpdatePolicy {
        component: "api".into(),
        channel: "stable".into(),
        cadence: "daily".into(),
        unattended_allowed: true,
    })
    .await
    .unwrap();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let app_task = Arc::clone(&app);
    let worker = tokio::spawn(async move {
        let tick_notify = Arc::new(tokio::sync::Notify::new());
        let (parked_tx, _parked_rx) = tokio::sync::watch::channel(0u32);
        let tick = ReleaseCheckTick::coalescing(tick_notify.clone(), parked_tx);
        tick.fire();
        run_release_check_loop(
            app_task,
            tick,
            RELEASE_LOOKUP_TIMEOUT,
            |component, channel| {
                let nibble = match component {
                    "bot" => 'b',
                    "api" => 'a',
                    _ => 'c',
                };
                let release = promoted_release(component, channel, nibble);
                Box::pin(async move { Ok(release) })
            },
            || watch_shutdown(&stop_rx),
            ScheduledInstallPolicy::enabled(true, true),
        )
        .await;
    });
    let started = std::time::Instant::now();
    loop {
        let bot_row = db::component_update(&bot.db, "bot").await.unwrap();
        let api_row = db::component_update(&bot.db, "api").await.unwrap();
        if bot_row.desired.is_some() && api_row.desired.is_some() {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "promoted cadence was not queued: bot={bot_row:?} api={api_row:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _ = stop_tx.send(true);
    worker.await.unwrap();
    let bot_row = db::component_update(&bot.db, "bot").await.unwrap();
    let api_row = db::component_update(&bot.db, "api").await.unwrap();
    let webui_row = db::component_update(&bot.db, "webui").await.unwrap();
    assert_eq!(bot_row.state, "queued");
    assert_eq!(api_row.state, "queued");
    assert_eq!(
        bot_row.desired.unwrap().digest,
        format!("sha256:{}", "b".to_string().repeat(64))
    );
    assert_eq!(
        api_row.desired.unwrap().digest,
        format!("sha256:{}", "a".to_string().repeat(64))
    );
    assert!(webui_row.desired.is_none());
    assert_ne!(webui_row.state, "queued");
    assert!(webui_row.candidate.is_some());
    drop(app);
    drop(bot);
    cleanup(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn scheduled_install_respects_active_lease_without_skip_set() {
    let (bot, app, path, _enrollment) = linked_writer("bg-active-lease").await;
    let now = chrono::Utc::now().timestamp();
    app.commit(db::ControlOp::RecordBakedComponentRunning {
        component: "bot".into(),
        observation: running_observation(now),
        now,
    })
    .await
    .unwrap();
    app.commit(db::ControlOp::SetUpdatePolicy {
        component: "bot".into(),
        channel: "stable".into(),
        cadence: "daily".into(),
        unattended_allowed: true,
    })
    .await
    .unwrap();
    let seed = promoted_release("bot", "stable", '0');
    app.commit(db::ControlOp::RecordUpdateCheck {
        component: "bot".into(),
        channel: "stable".into(),
        candidate: Some(seed),
        error: None,
        now,
    })
    .await
    .unwrap();
    let held = "ef".repeat(16);
    app.commit(db::ControlOp::ClaimUpdateLease {
        component: "bot".into(),
        owner: held.clone(),
        now,
        ttl: 240,
    })
    .await
    .unwrap();
    let before = install_snapshot(&bot.db, "bot").await;
    poll_scheduled_installs(
        &app,
        &std::collections::HashSet::new(),
        ScheduledInstallPolicy::enabled(true, true),
    )
    .await;
    assert_install_unchanged(&bot.db, "bot", &before).await;
    app.commit(db::ControlOp::ReleaseUpdateLease {
        component: "bot".into(),
        owner: held,
    })
    .await
    .unwrap();
    drop(app);
    drop(bot);
    cleanup(&path);
}

fn writer_db(bot: &Arc<std::sync::Mutex<Option<Arc<jmodguard_bot::state::BotState>>>>) -> Db {
    bot.lock().unwrap().as_ref().expect("bot writer").db.clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn background_check_skips_install_after_failed_renew() {
    let (bot, app, path, _enrollment) = linked_writer("bg-renew-fail").await;
    let bot_slot = Arc::new(std::sync::Mutex::new(Some(bot)));
    let db = writer_db(&bot_slot);
    let now = chrono::Utc::now().timestamp();
    app.commit(db::ControlOp::RecordBakedComponentRunning {
        component: "bot".into(),
        observation: running_observation(now),
        now,
    })
    .await
    .unwrap();
    app.commit(db::ControlOp::SetUpdatePolicy {
        component: "bot".into(),
        channel: "stable".into(),
        cadence: "daily".into(),
        unattended_allowed: true,
    })
    .await
    .unwrap();
    let seed = promoted_release("bot", "stable", '4');
    app.commit(db::ControlOp::RecordUpdateCheck {
        component: "bot".into(),
        channel: "stable".into(),
        candidate: Some(seed),
        error: None,
        now,
    })
    .await
    .unwrap();
    let gate = Arc::new(tokio::sync::Notify::new());
    let gate_release = Arc::clone(&gate);
    let (bot_entered_tx, mut bot_entered_rx) = tokio::sync::mpsc::channel(1);
    let app_task = Arc::clone(&app);
    let worker = tokio::spawn(async move {
        record_release_checks(
            &app_task,
            move |component, channel| {
                let (component, channel) = (component.to_owned(), channel.to_owned());
                let gate = Arc::clone(&gate);
                let bot_entered_tx = bot_entered_tx.clone();
                Box::pin(async move {
                    if component == "bot" {
                        let _ = bot_entered_tx.try_send(());
                        gate.notified().await;
                        Ok(promoted_release(&component, &channel, '5'))
                    } else {
                        Ok(promoted_release(&component, &channel, '6'))
                    }
                })
            },
            RELEASE_LOOKUP_TIMEOUT,
            pending_shutdown(),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), bot_entered_rx.recv())
        .await
        .expect("bot check did not start in time");
    let owner: String = db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT lease_owner FROM component_updates WHERE component='bot'",
                (),
                |row| row.get::<String>(0).map_err(anyhow::Error::from),
            )
        })
        .await
        .unwrap();
    app.commit(db::ControlOp::ReleaseUpdateLease {
        component: "bot".into(),
        owner,
    })
    .await
    .unwrap();
    gate_release.notify_one();
    let batch = tokio::time::timeout(Duration::from_secs(5), worker)
        .await
        .expect("release checks did not finish in time")
        .unwrap();
    assert!(batch.lease_held.contains("bot"));
    let before = install_snapshot(&db, "bot").await;
    poll_scheduled_installs(
        &app,
        &batch.lease_held,
        ScheduledInstallPolicy::enabled(true, true),
    )
    .await;
    assert_install_unchanged(&db, "bot", &before).await;
    drop(app);
    drop(bot_slot);
    cleanup(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn check_without_a_control_target_is_control_link_unavailable() {
    let path = temp_db("check-down");
    let _writer = Db::open(&path, 2).unwrap();
    let mut cfg = ApiProcessConfig::default();
    cfg.db.path = path.clone();
    cfg.db.pool_size = 2;
    cfg.bot.control_secret = ControlSecret::new("x".repeat(32));
    let db = Db::open_with(cfg.db_open_options()).unwrap();
    let app = Arc::new(ApiState::new(cfg, db).unwrap());
    let error = jmodguard_api::web::updates::check_one(&app, "bot")
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "control link unavailable");
    drop(app);
    cleanup(&path);
}

fn add_badword(enrollment: &db::GuildEnrollment, pattern: &str) -> db::ControlOp {
    let scope = db::ScopeWire {
        global: false,
        guild_id: enrollment.guild_id,
        enrollment_id: enrollment.id.clone(),
        joined_at_ns: enrollment.joined_at_ns,
    };
    db::ControlOp::AddBadword {
        request: scope.clone(),
        target: scope,
        pattern: pattern.into(),
        note: None,
    }
}

async fn linked_writer(
    label: &str,
) -> (
    Arc<jmodguard_bot::state::BotState>,
    Arc<ApiState>,
    PathBuf,
    db::GuildEnrollment,
) {
    let path = temp_db(label);
    let bot_db = Db::open(&path, 2).unwrap();
    let mut bot_cfg = BotProcessConfig::default();
    bot_cfg.db.path = path.clone();
    bot_cfg.bot.control_bind = "127.0.0.1:0".into();
    bot_cfg.bot.control_secret = ControlSecret::new("x".repeat(32));
    let _tenant = TenantRuntime::open(
        &bot_db,
        &bot_cfg.db,
        &bot_cfg.guild_stores,
        &bot_cfg.evidence,
    )
    .unwrap();
    let enrollment = db::enroll_guild(&bot_db, 55_555, 1).await.unwrap().unwrap();
    let bot = jmodguard_bot::state::BotState::new(bot_cfg, bot_db).unwrap();
    bot.activation.mark_lock_held(true);
    assert!(
        db::claim_active_bot(
            &bot.db,
            bot.activation.instance(),
            jmodguard::control::now_ms(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await
        .unwrap()
    );
    bot.install_control_actor_lookup(|actor, _| {
        (actor == 7).then_some(db::CachedGuildActor {
            guild_owner_id: 0,
            permissions: 1 << 5,
        })
    });
    let bound = jmodguard_bot::control_server::spawn(Arc::clone(&bot))
        .unwrap()
        .expect("control listener");
    let mut cfg = ApiProcessConfig::default();
    cfg.db.path = path.clone();
    cfg.db.pool_size = 2;
    cfg.bot.control_secret = ControlSecret::new("x".repeat(32));
    cfg.bot.control_target = bound.to_string();
    let db = Db::open_with(cfg.db_open_options()).unwrap();
    let app = ApiState::new(cfg, db).unwrap();
    (bot, app, path, enrollment)
}

#[tokio::test(flavor = "multi_thread")]
async fn held_moderation_saves_one_badword_when_released_before_expiry() {
    let (bot, app, path, enrollment) = linked_writer("hold-save").await;
    let hold = bot.activation.hold_rule();
    let app_task = Arc::clone(&app);
    let op = add_badword(&enrollment, "once");
    let pending = tokio::spawn(async move { app_task.commit_json_actor(7, op).await });
    let admitted = std::time::Instant::now();
    while bot.activation.in_flight() < 2 {
        assert!(
            admitted.elapsed() < std::time::Duration::from_secs(3),
            "the control job was not admitted"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    drop(hold);
    let saved = pending.await.unwrap().unwrap();
    assert!(!saved.0);
    assert_eq!(
        db::list_badwords(&bot.db, enrollment.guild_id)
            .await
            .unwrap()
            .len(),
        1
    );
    drop(app);
    drop(bot);
    cleanup(&path);
}

#[tokio::test(flavor = "multi_thread")]
async fn held_moderation_past_expiry_returns_request_expired_and_a_fresh_submit_applies_once() {
    let (bot, app, path, enrollment) = linked_writer("hold-expire").await;
    let hold = bot.activation.hold_rule();
    let app_task = Arc::clone(&app);
    let op = add_badword(&enrollment, "late");
    let pending = tokio::spawn(async move { app_task.commit_json_actor(7, op).await });
    let admitted = std::time::Instant::now();
    while bot.activation.in_flight() < 2 {
        assert!(
            admitted.elapsed() < std::time::Duration::from_secs(3),
            "the control job was not admitted"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    tokio::time::sleep(std::time::Duration::from_secs(16)).await;
    drop(hold);
    let error = pending.await.unwrap().unwrap_err();
    assert_eq!(error.to_string(), "request expired");
    assert!(
        db::list_badwords(&bot.db, enrollment.guild_id)
            .await
            .unwrap()
            .is_empty()
    );
    let saved = app
        .commit_json_actor(7, add_badword(&enrollment, "late"))
        .await
        .unwrap();
    assert!(!saved.0);
    assert_eq!(
        db::list_badwords(&bot.db, enrollment.guild_id)
            .await
            .unwrap()
            .len(),
        1
    );
    drop(app);
    drop(bot);
    cleanup(&path);
}
