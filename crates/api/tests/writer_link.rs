//! The production API open is read-only. A settings save reaches a separate writer.
use jmodguard::config::{ApiProcessConfig, BotProcessConfig, ControlSecret};
use jmodguard::db::{self, Db};
use jmodguard::tenant::TenantRuntime;
use jmodguard_api::web::state::ApiState;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    let before = probe.immediate();
    jmodguard_api::web::updates::poll_releases(&app, async |_component, _channel| {
        Ok::<_, anyhow::Error>("sha256:dddd".to_owned())
    })
    .await;
    assert_eq!(
        probe.immediate(),
        before,
        "the hourly API timer writes nothing"
    );
    drop(app);
    drop(bot);
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
