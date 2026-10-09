//! A read-only API launcher records its own observation through the bot.
use jmodguard::config::{BotProcessConfig, ControlSecret};
use jmodguard::db::{self, ComponentObservation, ComponentRelease, Db};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn temp_db(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "api_observe_{label}_{}_{}.db",
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
    }
    jmodguard::tenant::remove_sidecars(path);
}

fn observation(source_sha: &str, schema: i64, now: i64) -> ComponentObservation {
    ComponentObservation {
        source_sha: source_sha.into(),
        executable_sha256: "b".repeat(64),
        admin_sha256: None,
        base_image_sha: "c".repeat(64),
        started_at: now,
        min_schema: schema,
        max_schema: schema,
        api_capability: 1,
        self_update_enabled: true,
        base_abi: "debian-bookworm-amd64".into(),
        guild_schema: Some(db::guild_data::SCHEMA_VERSION),
        update_protocol: db::UPDATE_CONTROL_PROTOCOL,
        launcher_revision: db::LAUNCHER_REVISION,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn read_only_api_launcher_forwards_observations_and_the_counterpart_gate_passes() {
    let path = temp_db("forward");
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
    let mut opts = jmodguard::config::ApiProcessConfig::default().db_open_options();
    opts.path = path.clone();
    opts.pool_size = 2;
    assert!(opts.read_only);
    let probe = Arc::new(db::StartupProbe::default());
    let _guard = db::enter_startup_probe(Some(Arc::clone(&probe)));
    let link = jmodguard::control::try_writer_link(&bound.to_string(), &"x".repeat(32))
        .unwrap()
        .expect("writer link");
    let reader = db::UpdateControl::open(&opts)
        .unwrap()
        .unwrap()
        .with_writer_link(link);
    assert!(!reader.writes_locally());
    let schema = reader.schema_version().await.unwrap();
    let now = chrono::Utc::now().timestamp();
    let running = observation(&"a".repeat(40), schema, now);
    reader
        .running("api", None, running.clone(), now)
        .await
        .unwrap();
    let stored = reader.component("api").await.unwrap();
    assert_eq!(
        stored.observed.as_ref().unwrap().source_sha,
        running.source_sha
    );
    assert_eq!(stored.observed_at, Some(now));
    let baked = observation(&"d".repeat(40), schema, now);
    reader
        .baked_running("api", baked.clone(), now)
        .await
        .unwrap();
    let stored = reader.component("api").await.unwrap();
    assert_eq!(stored.observed.unwrap().source_sha, baked.source_sha);
    assert_eq!(stored.observed_at, Some(now));
    let webui = ComponentRelease {
        upgrade: None,
        component: "webui".into(),
        channel: "latest".into(),
        release_id: "v1".into(),
        source_sha: "e".repeat(40),
        published_at: "2026-09-23T00:00:00Z".into(),
        image: "ghcr.io/jatmn/jmodguard-webui".into(),
        digest: format!("sha256:{}", "f".repeat(64)),
        min_schema: schema,
        max_schema: schema,
        result_schema: schema,
        min_api: 1,
        max_api: 1,
        launcher_protocol: 1,
        launcher: None,
        base_abi: "static-webui-v1".into(),
    };
    jmodguard_updater::compatible(&bot.db, &webui, now)
        .await
        .unwrap();
    bot.activation.note_lease_lost();
    let refused = observation(&"1".repeat(40), schema, now);
    reader.running("api", None, refused, now).await.unwrap();
    let stored = reader.component("api").await.unwrap();
    assert_eq!(stored.observed.unwrap().source_sha, baked.source_sha);
    assert_eq!(probe.immediate(), 0);
    drop(reader);
    drop(bot);
    cleanup(&path);
}
