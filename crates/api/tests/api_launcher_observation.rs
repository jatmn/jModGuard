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
    let refused_receipt = reader
        .running("api", None, refused, now)
        .await
        .expect_err("a refused writer must not look saved");
    assert_eq!(
        refused_receipt
            .downcast_ref::<db::ReceiptNotSaved>()
            .map(|saved| saved.status.as_str()),
        Some(jmodguard::control::NOT_THE_WRITER)
    );
    let stored = reader.component("api").await.unwrap();
    assert_eq!(stored.observed.unwrap().source_sha, baked.source_sha);
    assert_eq!(probe.immediate(), 0);
    drop(reader);
    drop(bot);
    cleanup(&path);
}

fn api_release(schema: i64, digest_byte: char) -> ComponentRelease {
    ComponentRelease {
        upgrade: Some(db::UpgradeCompatibility {
            control_protocol: db::UPDATE_CONTROL_PROTOCOL,
            min_core: 15,
            min_guild: 1,
            guild_schema: db::guild_data::SCHEMA_VERSION,
        }),
        component: "api".into(),
        channel: "latest".into(),
        release_id: "independent".into(),
        source_sha: digest_byte.to_string().repeat(40),
        published_at: "2026-10-01T00:00:00Z".into(),
        image: "ghcr.io/jatmn/jmodguard-api".into(),
        digest: format!("sha256:{}", digest_byte.to_string().repeat(64)),
        min_schema: schema,
        max_schema: schema,
        result_schema: schema,
        min_api: 1,
        max_api: 1,
        launcher_protocol: 1,
        launcher: Some(db::LauncherCompatibility {
            revision: db::LAUNCHER_REVISION,
            min_revision: db::LAUNCHER_REVISION,
        }),
        base_abi: "debian-bookworm-amd64".into(),
    }
}

struct Linked {
    path: PathBuf,
    bot: Arc<jmodguard_bot::state::BotState>,
    reader: db::UpdateControl,
}

async fn linked(hold_lease: bool) -> Linked {
    let path = temp_db("receipt");
    let bot_db = Db::open(&path, 2).unwrap();
    let mut bot_cfg = BotProcessConfig::default();
    bot_cfg.db.path = path.clone();
    bot_cfg.bot.control_bind = "127.0.0.1:0".into();
    bot_cfg.bot.control_secret = ControlSecret::new("x".repeat(32));
    let bot = jmodguard_bot::state::BotState::new(bot_cfg, bot_db).unwrap();
    if hold_lease {
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
    }
    let bound = jmodguard_bot::control_server::spawn(Arc::clone(&bot))
        .unwrap()
        .expect("control listener");
    let mut opts = jmodguard::config::ApiProcessConfig::default().db_open_options();
    opts.path = path.clone();
    opts.pool_size = 2;
    assert!(opts.read_only);
    let link = jmodguard::control::try_writer_link(&bound.to_string(), &"x".repeat(32))
        .unwrap()
        .expect("writer link");
    let reader = db::UpdateControl::open(&opts)
        .unwrap()
        .unwrap()
        .with_writer_link(link);
    Linked { path, bot, reader }
}

async fn queue(
    bot: &jmodguard_bot::state::BotState,
    release: &ComponentRelease,
) -> ComponentRelease {
    let now = chrono::Utc::now().timestamp();
    db::record_update_check(&bot.db, "api", "latest", Some(release.clone()), None, now)
        .await
        .unwrap();
    db::request_component_install(&bot.db, "api", &release.digest, now)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn read_only_api_failure_and_rollback_clear_desired_so_the_next_poll_has_no_candidate() {
    let linked = linked(true).await;
    let schema = linked.reader.schema_version().await.unwrap();
    let failed = queue(&linked.bot, &api_release(schema, 'a')).await;
    linked
        .reader
        .failed(
            "api",
            &failed,
            "compatibility: candidate cannot open current database schema",
        )
        .await
        .unwrap();
    let after_failure = linked.reader.component("api").await.unwrap();
    assert!(after_failure.desired.is_none());
    assert!(
        after_failure
            .last_error
            .as_deref()
            .unwrap()
            .contains("compatibility")
    );
    let rolled = queue(&linked.bot, &api_release(schema, 'b')).await;
    let now = chrono::Utc::now().timestamp();
    linked
        .reader
        .rolled_back(
            "api",
            &rolled,
            None,
            observation(&"c".repeat(40), schema, now),
            "spawn: replacement child readiness timed out",
            now,
        )
        .await
        .unwrap();
    let after_rollback = linked.reader.component("api").await.unwrap();
    assert!(after_rollback.desired.is_none());
    assert!(
        after_rollback
            .last_error
            .as_deref()
            .unwrap()
            .contains("spawn")
    );
    drop(linked.reader);
    drop(linked.bot);
    cleanup(&linked.path);
}

#[tokio::test(flavor = "multi_thread")]
async fn read_only_api_stores_required_launcher_selection_and_applies_every_receipt() {
    let linked = linked(true).await;
    let schema = linked.reader.schema_version().await.unwrap();
    let release = queue(&linked.bot, &api_release(schema, 'd')).await;
    let refused = linked
        .reader
        .select_required_launcher(&release, Some("not-an-operation"))
        .await
        .expect_err("an operation id is a paired upgrade");
    assert!(
        refused.to_string().contains(db::PAIRED_UPGRADES_DISABLED),
        "{refused:#}"
    );
    assert!(
        linked
            .reader
            .component("api")
            .await
            .unwrap()
            .desired_launcher
            .is_none()
    );
    linked
        .reader
        .select_required_launcher(&release, None)
        .await
        .unwrap();
    let selected = linked.reader.component("api").await.unwrap();
    assert_eq!(selected.desired_launcher.unwrap().digest, release.digest);
    assert_eq!(selected.desired.unwrap().digest, release.digest);
    let now = chrono::Utc::now().timestamp();
    let hash = "ab".repeat(32);
    linked
        .reader
        .launcher_running("api", &release, &hash, now)
        .await
        .unwrap();
    linked
        .reader
        .running("api", None, observation(&"e".repeat(40), schema, now), now)
        .await
        .unwrap();
    linked
        .reader
        .observe_launcher("api", db::LAUNCHER_REVISION)
        .await
        .unwrap();
    let observed = linked.reader.component("api").await.unwrap();
    assert_eq!(
        observed.observed.unwrap().launcher_revision,
        db::LAUNCHER_REVISION
    );
    assert_eq!(observed.installed_launcher.unwrap().digest, release.digest);
    linked
        .reader
        .clear_superseded_launcher("api", &release)
        .await
        .unwrap();
    assert!(
        linked
            .reader
            .component("api")
            .await
            .unwrap()
            .desired_launcher
            .is_none()
    );
    let mut bare = release.clone();
    bare.digest = format!("sha256:{}", "9".repeat(64));
    bare.source_sha = "9".repeat(40);
    bare.launcher = None;
    bare.upgrade = None;
    let bare = queue(&linked.bot, &bare).await;
    linked
        .reader
        .select_required_launcher(&bare, None)
        .await
        .unwrap();
    linked
        .reader
        .clear_uncontracted_launcher("api", &bare)
        .await
        .unwrap();
    assert!(
        linked
            .reader
            .component("api")
            .await
            .unwrap()
            .desired_launcher
            .is_none()
    );
    let release = queue(&linked.bot, &release).await;
    linked
        .reader
        .select_required_launcher(&release, None)
        .await
        .unwrap();
    linked
        .reader
        .launcher_failed("api", &release.digest, "required launcher failed")
        .await
        .unwrap();
    let failed = linked.reader.component("api").await.unwrap();
    assert!(failed.desired_launcher.is_none());
    assert!(failed.last_error.unwrap().contains("required launcher"));
    linked
        .reader
        .baked_launcher("api", &"cd".repeat(32), now)
        .await
        .unwrap();
    let baked = linked.reader.component("api").await.unwrap();
    assert!(baked.installed_launcher.is_none());
    assert_eq!(baked.launcher_sha256.unwrap(), "cd".repeat(32));
    drop(linked.reader);
    drop(linked.bot);
    cleanup(&linked.path);
}

#[tokio::test(flavor = "multi_thread")]
async fn refused_activation_is_retried_until_the_writer_stores_installed() {
    let linked = linked(false).await;
    let schema = linked.reader.schema_version().await.unwrap();
    let release = queue(&linked.bot, &api_release(schema, 'f')).await;
    let now = chrono::Utc::now().timestamp();
    let seen = observation(&"1".repeat(40), schema, now);
    let refused = linked
        .reader
        .running("api", Some(release.clone()), seen.clone(), now)
        .await
        .expect_err("activation before the lease is not durable");
    assert_eq!(
        refused
            .downcast_ref::<db::ReceiptNotSaved>()
            .map(|saved| saved.status.as_str()),
        Some(jmodguard::control::NOT_THE_WRITER)
    );
    assert!(
        linked
            .reader
            .component("api")
            .await
            .unwrap()
            .installed
            .is_none()
    );
    linked.bot.activation.mark_lock_held(true);
    assert!(
        db::claim_active_bot(
            &linked.bot.db,
            linked.bot.activation.instance(),
            jmodguard::control::now_ms(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await
        .unwrap()
    );
    linked
        .reader
        .running("api", Some(release.clone()), seen.clone(), now)
        .await
        .unwrap();
    let stored = linked.reader.component("api").await.unwrap();
    assert_eq!(stored.installed.unwrap().digest, release.digest);
    assert_eq!(stored.observed.unwrap().source_sha, seen.source_sha);
    drop(linked.reader);
    drop(linked.bot);
    cleanup(&linked.path);
}
