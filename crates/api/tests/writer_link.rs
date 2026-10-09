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
    cfg.bot.control_bind = bound.to_string();
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
                    enrollment_id: enrollment.id,
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
    let text = error.to_string();
    assert!(text.contains("JMODGUARD_CONTROL_SECRET"), "{text}");
    assert!(!text.contains('/'), "{text}");
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
