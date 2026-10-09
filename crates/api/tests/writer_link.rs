//! The production API open is read-only. A settings save reaches a separate writer.
use jmodguard::config::{ApiProcessConfig, BotProcessConfig};
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
    let bound = jmodguard_bot::control_server::spawn(Arc::clone(&bot)).unwrap();

    let mut cfg = ApiProcessConfig::default();
    cfg.db.path = path.clone();
    cfg.db.pool_size = 2;
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
    let reply = app
        .commit(db::ControlOp::PatchSettings {
            scope: db::ScopeWire {
                global: false,
                guild_id: 55_555,
                enrollment_id: enrollment.id,
                joined_at_ns: 1,
            },
            prefix: db::FieldPatch::Set("!saved".into()),
            bot_name: db::FieldPatch::Absent,
            reconcile: false,
        })
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
