//! Real supervisor, OCI extraction, SQLite operation state and reaped OS children.
//! Tiny ELF fixtures stand in for Discord/application work; their readiness and
//! exit behavior are real, while the test explicitly simulates a future migration.
use super::*;
use axum::{Router, body::Body, extract::State, http::Uri, response::Response, routing::get};
use db::{Db, DbOpenOptions, SqlExecutor, params};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

async fn object(State(objects): State<Arc<HashMap<String, Vec<u8>>>>, uri: Uri) -> Response {
    match objects.get(uri.path().rsplit('/').next().unwrap_or("")) {
        Some(bytes) => Response::new(Body::from(bytes.clone())),
        None => Response::builder().status(404).body(Body::empty()).unwrap(),
    }
}

fn image(objects: &mut HashMap<String, Vec<u8>>, role: &str, binary: &[u8]) -> ComponentRelease {
    let config = serde_json::to_vec(&json!({"architecture":"amd64","os":"linux","config":{"Labels":{
        "org.opencontainers.image.revision":"c".repeat(40), "io.jmodguard.release-id":"paired-fixture"
    }}})).unwrap();
    let mut layer = Vec::new();
    {
        let mut tar = tar::Builder::new(&mut layer);
        let name = format!("usr/local/bin/jmodguard-{role}");
        let inventory = format!(
            "{hash}  bin/jmodguard-{role}\n{hash}  bin/jmodguard-launcher\n",
            hash = sha256_hex(binary)
        );
        for (path, bytes) in [
            (name.as_str(), binary),
            ("usr/local/bin/jmodguard-launcher", binary),
            (
                "usr/local/share/jmodguard/files.sha256",
                inventory.as_bytes(),
            ),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, path, bytes).unwrap();
        }
        tar.finish().unwrap();
    }
    let digest = |bytes: &[u8]| format!("sha256:{}", sha256_hex(bytes));
    let config_hash = digest(&config);
    let layer_hash = digest(&layer);
    let manifest = serde_json::to_vec(&json!({"schemaVersion":2,
        "config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":config_hash,"size":config.len()},
        "layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":layer_hash,"size":layer.len()}]
    })).unwrap();
    let manifest_hash = digest(&manifest);
    objects.insert(config_hash, config);
    objects.insert(layer_hash, layer);
    objects.insert(manifest_hash.clone(), manifest);
    ComponentRelease {
        component: role.into(),
        channel: "latest".into(),
        release_id: "paired-fixture".into(),
        source_sha: "c".repeat(40),
        published_at: "2026-10-01T00:00:00Z".into(),
        image: format!("ghcr.io/jatmn/jmodguard-{role}"),
        digest: manifest_hash,
        min_schema: db::CORE_SCHEMA_VERSION + 1,
        max_schema: db::CORE_SCHEMA_VERSION + 1,
        result_schema: db::CORE_SCHEMA_VERSION + 1,
        min_api: 1,
        max_api: 1,
        launcher_protocol: 1,
        launcher: Some(db::LauncherCompatibility {
            revision: db::LAUNCHER_REVISION,
            min_revision: db::REQUIRED_LAUNCHER_REVISION,
        }),
        base_abi: "debian-bookworm-amd64".into(),
        upgrade: Some(db::UpgradeCompatibility {
            control_protocol: db::UPDATE_CONTROL_PROTOCOL,
            min_core: 15,
            min_guild: 1,
            guild_schema: db::guild_data::SCHEMA_VERSION + 1,
        }),
    }
}

async fn executable(root: &Path, name: &str) -> PathBuf {
    let path = root.join(name);
    let code = format!(
        r#"
use std::io::Write;
use std::sync::atomic::{{AtomicBool, Ordering}};
static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: i32) {{ STOP.store(true, Ordering::SeqCst); }}
unsafe extern "C" {{ fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize; }}
fn main() {{
    unsafe {{ signal(15, stop); }}
    let root = std::path::Path::new({root:?});
    let maintenance = std::env::var("JMODGUARD_UPDATE_MAINTENANCE").as_deref() == Ok("1");
    let event = |suffix: &str| {{
        let mut log = std::fs::OpenOptions::new().create(true).append(true).open(root.join("events")).unwrap();
        writeln!(log, "{name}.{{suffix}}").unwrap();
    }};
    if !maintenance {{
        event("start");
        if root.join("fail-{name}").exists() {{
            std::fs::write(std::env::var("JMODGUARD_READY_FILE").unwrap(), "stale marker").unwrap();
            std::process::exit(7);
        }}
        while root.join("hold-{name}-ready").exists() && !STOP.load(Ordering::SeqCst) {{ std::thread::sleep(std::time::Duration::from_millis(5)); }}
        if STOP.load(Ordering::SeqCst) {{ event("stop"); return; }}
        std::fs::write(std::env::var("JMODGUARD_READY_FILE").unwrap(), "ready").unwrap();
        event("ready");
    }}
    while !STOP.load(Ordering::SeqCst) {{ std::thread::sleep(std::time::Duration::from_millis(5)); }}
    if !maintenance {{
        event("stopping");
        while root.join("hold-{name}-stop").exists() {{ std::thread::sleep(std::time::Duration::from_millis(5)); }}
        event("stop");
    }}
}}
"#,
        root = root.to_str().unwrap(),
        name = name
    );
    let source = root.join(format!("{}.rs", name.replace('-', "_")));
    tokio::fs::write(&source, code).await.unwrap();
    let output = Command::new("rustc")
        .args(["--edition=2024", "-C", "debuginfo=0", "-o"])
        .arg(&path)
        .arg(&source)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    path
}

fn controller(
    role: &str,
    baked: PathBuf,
    registry: Registry,
    control: UpdateControl,
    root: &Path,
) -> Supervisor {
    Supervisor {
        role: role.into(),
        base_sha: "b".repeat(40),
        base_abi: "debian-bookworm-amd64".into(),
        enabled: true,
        launcher_revision: db::LAUNCHER_REVISION,
        baked,
        baked_compatibility: db::BakedCompatibility::compiled(&"b".repeat(40)),
        registry,
        control,
        instance: format!("{:032x}", rand::random::<u128>()),
        running: None,
        restart: RestartPolicy::default(),
        maintenance: None,
        health: None,
        health_bind: None,
        staging: root.join(format!("staged-{role}")),
        prepared: None,
        standby: None,
        rollback: None,
        pending_failure: None,
        pending_startup: Vec::new(),
        receipt_attempt: 0,
        receipt_not_before: None,
        reported: false,
        last_observed: None,
    }
}

async fn restart(service: &mut Supervisor) {
    service.stop_application().await.unwrap();
    service.end_idle().await.unwrap();
    let _ = tokio::fs::remove_dir_all(&service.staging).await;
    service.prepared = None;
    service.instance = format!("{:032x}", rand::random::<u128>());
    service.standby = None;
    service.rollback = None;
    service.pending_failure = None;
    service.pending_startup.clear();
    service.receipt_attempt = 0;
    service.receipt_not_before = None;
}

#[tokio::test]
async fn paired_processes_restore_across_every_phase_and_recover_forward_after_failure() {
    let root =
        std::env::temp_dir().join(format!("paired-processes-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let old_bot = executable(&root, "old-bot").await;
    let old_api = executable(&root, "old-api").await;
    let new_bot = executable(&root, "new-bot").await;
    let new_api = executable(&root, "new-api").await;
    let mut objects = HashMap::new();
    let bot_release = image(
        &mut objects,
        "bot",
        &tokio::fs::read(new_bot).await.unwrap(),
    );
    let api_release = image(
        &mut objects,
        "api",
        &tokio::fs::read(new_api).await.unwrap(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let registry = Registry::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .fallback(get(object))
                .with_state(Arc::new(objects)),
        )
        .await
        .unwrap();
    });
    let path = root.join("core.db");
    let db = Db::open(&path, 2).unwrap();
    let opts = DbOpenOptions {
        read_only: false,
        path,
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: false,
        default_prefix: "!".into(),
        retention_days: 1,
    };
    let control = UpdateControl::open(&opts).unwrap().unwrap();
    // Reproduce the reported pre-baseline lockout: persisted managed image
    // labels say schema15, although the bot already migrated the real DB to16.
    // Reloading the fixed base images must adopt their baked baseline once.
    for target in [&bot_release, &api_release] {
        let mut legacy = target.clone();
        legacy.upgrade = None;
        legacy.min_schema = 15;
        legacy.max_schema = 15;
        legacy.result_schema = 15;
        control
            .checked(
                &legacy.component,
                "latest",
                Some(legacy.clone()),
                None,
                now(),
            )
            .await
            .unwrap();
        db::request_component_install(&db, &legacy.component, &legacy.digest, now())
            .await
            .unwrap();
        let seen = ComponentObservation {
            source_sha: "a".repeat(40),
            executable_sha256: "b".repeat(64),
            admin_sha256: None,
            base_image_sha: "a".repeat(40),
            started_at: now(),
            min_schema: 15,
            max_schema: 15,
            api_capability: 1,
            self_update_enabled: true,
            base_abi: "debian-bookworm-amd64".into(),
            guild_schema: None,
            update_protocol: 0,
            launcher_revision: 0,
        };
        control
            .running(&legacy.component.clone(), Some(legacy), seen, now())
            .await
            .unwrap();
    }
    let mut bot = controller("bot", old_bot, registry.clone(), control.clone(), &root);
    let mut api = controller("api", old_api, registry, control.clone(), &root);
    let delayed_baked = root.join("hold-old-bot-ready");
    tokio::fs::write(&delayed_baked, "wait").await.unwrap();
    bot.tick().await.unwrap();
    api.tick().await.unwrap();
    bot.running.as_mut().unwrap().started_at = now() - READY_TIMEOUT.as_secs() as i64 - 1;
    assert!(
        !bot.report_ready().await.unwrap(),
        "baked startup has no replacement deadline"
    );
    assert!(
        bot.running
            .as_mut()
            .unwrap()
            .child
            .try_wait()
            .unwrap()
            .is_none()
    );
    tokio::fs::remove_file(delayed_baked).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !bot.reported || !api.reported {
        assert!(Instant::now() < deadline);
        bot.tick().await.unwrap();
        api.tick().await.unwrap();
        tokio::time::sleep(Duration::from_millis(15)).await;
    }

    for role in ["bot", "api"] {
        let restored = control.component(role).await.unwrap();
        assert!(restored.installed.is_none());
        assert_eq!(
            restored.observed.unwrap().max_schema,
            db::CORE_SCHEMA_VERSION
        );
    }
    for release in [&bot_release, &api_release] {
        control
            .checked(
                &release.component,
                "latest",
                Some(release.clone()),
                None,
                now(),
            )
            .await
            .unwrap();
    }
    assert!(
        compatible_control(&control, &bot_release, now())
            .await
            .is_err()
    );
    let operation = control
        .queue_paired_upgrade_for_tests(&bot_release.digest, &api_release.digest, false, now())
        .await
        .unwrap();
    // Simulate an older capable controller executing the real prerequisite path.
    // OCI extraction, stop/reap and PID1 handoff file are real; re-execution at
    // the newer capability is represented by rebuilding the local supervisor.
    bot.launcher_revision = 0;
    assert!(
        bot.tick().await.unwrap(),
        "required launcher must hand off before application preparation"
    );
    assert!(bot.running.is_none());
    let waiting = control.upgrade().await.unwrap().unwrap();
    assert_eq!(waiting.phase, Phase::Preparing);
    assert!(!waiting.bot.prepared && !waiting.irreversible);
    let handoff = tokio::fs::read_to_string(root.join("jmodguard-launcher-next"))
        .await
        .unwrap();
    assert!(handoff.ends_with(&format!("{}\n", bot_release.digest)));
    assert_eq!(
        control
            .component("bot")
            .await
            .unwrap()
            .desired_launcher
            .unwrap(),
        bot_release
    );
    assert!(
        !tokio::fs::read_to_string(root.join("events"))
            .await
            .unwrap()
            .contains("new-bot.start")
    );
    bot.launcher_revision = db::LAUNCHER_REVISION;
    activate_launcher(
        &control,
        "bot",
        &bot_release,
        handoff.lines().nth(1).unwrap(),
    )
    .await
    .unwrap();
    restart(&mut bot).await;
    // Restart the bot while the API is inside a real graceful stop. Its
    // subsequent stop acknowledgement is stale progress, not an upgrade error.
    while control.upgrade().await.unwrap().unwrap().phase != Phase::StoppingApi {
        bot.tick().await.unwrap();
        api.tick().await.unwrap();
    }
    let hold = root.join("hold-old-api-stop");
    tokio::fs::write(&hold, "hold").await.unwrap();
    let stopping = tokio::spawn(async move {
        let result = api.tick().await;
        (api, result)
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if tokio::fs::read_to_string(root.join("events"))
            .await
            .unwrap()
            .contains("old-api.stopping")
        {
            break;
        }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    restart(&mut bot).await;
    control
        .join_upgrade(&operation.id, "bot", &bot.instance)
        .await
        .unwrap();
    tokio::fs::remove_file(hold).await.unwrap();
    let (mut api, stopped) = stopping.await.unwrap();
    stopped.unwrap();
    assert!(control.upgrade().await.unwrap().unwrap().error.is_none());
    let mut restarted = HashSet::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let op = control.upgrade().await.unwrap().unwrap();
        assert!(Instant::now() < deadline, "stalled operation: {op:?}");
        if op.phase == Phase::Complete {
            break;
        }
        let phase = format!("{:?}", op.phase);
        if restarted.insert(phase) {
            restart(&mut bot).await;
            restart(&mut api).await;
            bot.control = UpdateControl::open(&opts).unwrap().unwrap();
            api.control = UpdateControl::open(&opts).unwrap().unwrap();
        }
        bot.tick().await.unwrap();
        if control.schema_version().await.unwrap() == db::CORE_SCHEMA_VERSION
            && bot
                .running
                .as_ref()
                .is_some_and(|child| child.release.is_some())
        {
            // Future application migration is deliberately simulated, using a
            // real primary SQLite write after actual API stop/reap and bot spawn.
            assert!(api.running.is_none());
            db.with_conn(|conn| {
                conn.pragma_update(None, "user_version", db::CORE_SCHEMA_VERSION + 1)?;
                Ok(())
            })
            .await
            .unwrap();
        }
        api.tick().await.unwrap();
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    assert_eq!(restarted.len(), 5);
    assert!(Db::open_with(opts.clone()).is_err());
    let events = tokio::fs::read_to_string(root.join("events"))
        .await
        .unwrap();
    assert!(events.find("old-api.stop").unwrap() < events.find("new-bot.start").unwrap());
    assert!(events.find("new-bot.ready").unwrap() < events.find("new-api.start").unwrap());
    let old_starts =
        events.matches("old-api.start").count() + events.matches("old-bot.start").count();
    // A failed replacement / old baked fallback cannot use installed_launcher
    // as proof of its own capability and must never start the dependent app.
    restart(&mut bot).await;
    bot.launcher_revision = 0;
    assert!(
        bot.tick().await.unwrap(),
        "cold restore must restore its prerequisite first"
    );
    assert!(bot.running.is_none());
    bot.launcher_revision = db::LAUNCHER_REVISION;
    // Cold starts have no local artifacts and an old baked executable. They
    // must fetch the installed selection without launching either old app.
    restart(&mut bot).await;
    restart(&mut api).await;
    bot.tick().await.unwrap();
    api.tick().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    bot.tick().await.unwrap();
    api.tick().await.unwrap();
    assert_eq!(
        control.upgrade().await.unwrap().unwrap().phase,
        Phase::Complete
    );
    let events = tokio::fs::read_to_string(root.join("events"))
        .await
        .unwrap();
    assert_eq!(
        events.matches("old-api.start").count() + events.matches("old-bot.start").count(),
        old_starts
    );
    // A permanent new API startup failure after migration must leave recovery
    // alive and require both real children stopped before owner retry.
    for failure in ["fail-new-api", "hold-new-api-ready"] {
        tokio::fs::write(root.join(failure), "fail").await.unwrap();
        restart(&mut api).await;
        api.tick().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        if failure == "hold-new-api-ready" {
            api.running.as_mut().unwrap().started_at = now() - READY_TIMEOUT.as_secs() as i64 - 1;
        }
        api.tick().await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let failed = loop {
            bot.tick().await.unwrap();
            api.tick().await.unwrap();
            let failed = control.upgrade().await.unwrap().unwrap();
            assert!(
                Instant::now() < deadline,
                "failed operation did not quiesce: {failed:?}"
            );
            if failed.error.is_some()
                && failed.irreversible
                && failed.bot.stopped
                && failed.api.stopped
            {
                break failed;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        };
        assert!(api.maintenance.is_some());
        assert!(control.abort_upgrade(&failed.id).await.is_err());
        tokio::fs::remove_file(root.join(failure)).await.unwrap();
        control
            .queue_paired_upgrade_for_tests(&bot_release.digest, &api_release.digest, true, now())
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while control.upgrade().await.unwrap().unwrap().phase != Phase::Complete {
            assert!(Instant::now() < deadline);
            bot.tick().await.unwrap();
            api.tick().await.unwrap();
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }
    assert_ne!(control.upgrade().await.unwrap().unwrap().id, operation.id);
    restart(&mut bot).await;
    restart(&mut api).await;
    server.abort();
    let _ = server.await;
    drop((bot, api, control, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn independent_update_requires_launcher_before_spawn_and_retains_app_on_download_failure() {
    let root = std::env::temp_dir().join(format!(
        "launcher-prerequisite-{:032x}",
        rand::random::<u128>()
    ));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let baked = executable(&root, "old-bot").await;
    let next = executable(&root, "new-bot").await;
    let mut objects = HashMap::new();
    let mut release = image(&mut objects, "bot", &tokio::fs::read(next).await.unwrap());
    release.min_schema = db::CORE_SCHEMA_VERSION;
    release.max_schema = db::CORE_SCHEMA_VERSION;
    release.result_schema = db::CORE_SCHEMA_VERSION;
    release.upgrade.as_mut().unwrap().guild_schema = db::guild_data::SCHEMA_VERSION;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let registry = Registry::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .fallback(get(object))
                .with_state(Arc::new(objects)),
        )
        .await
        .unwrap();
    });
    let db = Db::open(&root.join("core.db"), 2).unwrap();
    let control = UpdateControl::from_application(&db);
    let mut bot = controller("bot", baked, registry.clone(), control.clone(), &root);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !bot.reported {
        assert!(Instant::now() < deadline);
        bot.tick().await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let seen = control.component("bot").await.unwrap().observed.unwrap();
    control.running("api", None, seen, now()).await.unwrap();
    control
        .checked("bot", "latest", Some(release.clone()), None, now())
        .await
        .unwrap();
    let mut row = control.component("bot").await.unwrap();
    row.observed.as_mut().unwrap().self_update_enabled = true;
    let mut optional = release.clone();
    optional.launcher.as_mut().unwrap().revision += 1;
    row.candidate = Some(optional.clone());
    let optional_advice = jmodguard_updater::launcher_status(&row, now());
    assert_eq!(optional_advice["state"], "optional");
    assert_eq!(optional_advice["installable"], true);
    optional.launcher.as_mut().unwrap().min_revision = db::LAUNCHER_REVISION + 1;
    row.candidate = Some(optional.clone());
    row.installed_launcher = Some(optional); // historical metadata is not active capability
    let required_advice = jmodguard_updater::launcher_status(&row, now());
    assert_eq!(required_advice["state"], "required_first");
    assert_eq!(
        required_advice["installable"], false,
        "a required launcher is installed by normal Update, not the optional control"
    );
    row.observed_at = Some(now() - 7201);
    assert_eq!(
        jmodguard_updater::launcher_status(&row, now())["state"],
        "unknown"
    );
    control
        .observe_launcher("bot", db::LAUNCHER_REVISION + 1)
        .await
        .unwrap();
    let active = control.component("bot").await.unwrap();
    assert_eq!(
        active.observed.unwrap().launcher_revision,
        db::LAUNCHER_REVISION + 1
    );
    control
        .observe_launcher("bot", db::LAUNCHER_REVISION)
        .await
        .unwrap();
    db::request_component_install(&db, "bot", &release.digest, now())
        .await
        .unwrap();
    let old_pid = bot.running.as_ref().unwrap().child.id();
    bot.launcher_revision = 0; // simulated older compiled controller, same production paths
    bot.registry = Registry::new("http://127.0.0.1:1").unwrap();
    assert!(!bot.tick().await.unwrap());
    assert_eq!(bot.running.as_ref().unwrap().child.id(), old_pid);
    let failed = control.component("bot").await.unwrap();
    assert!(failed.desired.is_none());
    assert!(
        failed
            .last_error
            .unwrap()
            .contains("required launcher failed")
    );
    bot.registry = registry;
    db::request_component_install(&db, "bot", &release.digest, now())
        .await
        .unwrap();
    assert!(bot.tick().await.unwrap());
    assert!(bot.running.is_none());
    let handoff = tokio::fs::read_to_string(root.join("jmodguard-launcher-next"))
        .await
        .unwrap();
    assert!(handoff.ends_with(&format!("{}\n", release.digest)));
    let mut mislabeled = release.clone();
    mislabeled.launcher.as_mut().unwrap().revision += 1;
    let published = control
        .component("bot")
        .await
        .unwrap()
        .observed
        .unwrap()
        .launcher_revision;
    assert!(
        establish_launcher(
            &control,
            "bot",
            handoff.lines().nth(1).unwrap(),
            Some("sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        )
        .await
        .is_err()
    );
    assert_eq!(
        control
            .component("bot")
            .await
            .unwrap()
            .observed
            .unwrap()
            .launcher_revision,
        published,
        "rejected handoff must not publish launcher capability"
    );
    assert!(
        activate_launcher(
            &control,
            "bot",
            &mislabeled,
            handoff.lines().nth(1).unwrap()
        )
        .await
        .is_err()
    );
    assert!(
        control
            .component("bot")
            .await
            .unwrap()
            .installed_launcher
            .is_none()
    );
    activate_launcher(&control, "bot", &release, handoff.lines().nth(1).unwrap())
        .await
        .unwrap();
    bot.launcher_revision = db::LAUNCHER_REVISION; // re-exec simulated; ELF children are real
    restart(&mut bot).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while control.component("bot").await.unwrap().installed.is_none() {
        assert!(Instant::now() < deadline);
        assert!(!bot.tick().await.unwrap());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // A newer baked controller can already satisfy an earlier queued launcher
    // request. Settle that request without claiming the requested bytes ran.
    let state = control.component("bot").await.unwrap();
    assert!(matches!(
        bot.launcher(&state).await.unwrap(),
        ControlStep::Proceed
    ));
    assert!(
        control
            .component("bot")
            .await
            .unwrap()
            .desired_launcher
            .is_none()
    );
    // A different build with a newer optional launcher must not cause a handoff.
    let mut optional = release.clone();
    optional.launcher.as_mut().unwrap().revision += 1;
    assert!(matches!(
        bot.required_launcher(&optional, None).await.unwrap(),
        ControlStep::Proceed
    ));
    let advice =
        jmodguard_updater::launcher_status(&control.component("bot").await.unwrap(), now());
    assert_eq!(advice["state"], "satisfied");
    // Direct start/rollback cannot bypass a dependency even with valid app bytes.
    bot.launcher_revision = 0;
    let path = bot.running.as_ref().unwrap().path.clone();
    assert!(
        bot.start(&path, &release.source_sha, Some(release.clone()))
            .await
            .is_err()
    );
    restart(&mut bot).await;
    bot.enabled = false;
    assert!(bot.tick().await.is_err());
    assert!(bot.running.is_none());
    bot.enabled = true;
    assert!(bot.tick().await.unwrap());
    assert!(bot.running.is_none());
    restart(&mut bot).await;
    server.abort();
    let _ = server.await;
    drop((bot, control, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn independent_spawn_failure_settles_once() {
    let root =
        std::env::temp_dir().join(format!("independent-spawn-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let old_bot = executable(&root, "old-bot").await;
    let old_api = executable(&root, "old-api").await;
    let mut objects = HashMap::new();
    let mut candidate = image(&mut objects, "api", b"\x7fELFnot-an-executable");
    candidate.min_schema = db::CORE_SCHEMA_VERSION;
    candidate.max_schema = db::CORE_SCHEMA_VERSION;
    candidate.result_schema = db::CORE_SCHEMA_VERSION;
    candidate.upgrade.as_mut().unwrap().guild_schema = db::guild_data::SCHEMA_VERSION;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let registry = Registry::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .fallback(get(object))
                .with_state(Arc::new(objects)),
        )
        .await
        .unwrap();
    });
    let db = Db::open(&root.join("core.db"), 2).unwrap();
    let control = UpdateControl::from_application(&db);
    let mut bot = controller("bot", old_bot, registry.clone(), control.clone(), &root);
    let mut api = controller("api", old_api, registry, control.clone(), &root);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !bot.reported || !api.reported {
        assert!(Instant::now() < deadline);
        bot.tick().await.unwrap();
        api.tick().await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    api.running.as_mut().unwrap().started_at = now() - 100;
    control
        .running(
            "api",
            None,
            deployment_observation(
                api.running.as_ref().unwrap(),
                &api.base_sha,
                &api.base_abi,
                api.enabled,
                &api.baked_compatibility,
            ),
            now(),
        )
        .await
        .unwrap();
    control
        .checked("api", "latest", Some(candidate.clone()), None, now())
        .await
        .unwrap();
    db::request_component_install(&db, "api", &candidate.digest, now())
        .await
        .unwrap();
    // Spawn errors are tolerated so the durable-settlement oracle can run.
    let _ = api.tick().await;
    while !api.reported || control.component("api").await.unwrap().desired.is_some() {
        assert!(
            Instant::now() < deadline,
            "failed request kept retrying instead of settling"
        );
        let _ = api.tick().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let installed = control.component("api").await.unwrap();
    assert_eq!(installed.state, "failed");
    assert!(installed.installed.is_none());
    assert!(
        installed
            .last_error
            .as_deref()
            .unwrap_or("")
            .contains("spawn:")
    );
    let starts = tokio::fs::read_to_string(root.join("events"))
        .await
        .unwrap()
        .matches("old-api.start")
        .count();
    for _ in 0..3 {
        api.tick().await.unwrap();
    }
    assert_eq!(
        tokio::fs::read_to_string(root.join("events"))
            .await
            .unwrap()
            .matches("old-api.start")
            .count(),
        starts
    );
    api.stop_application().await.unwrap();
    api.end_idle().await.unwrap();
    bot.stop_application().await.unwrap();
    bot.end_idle().await.unwrap();
    server.abort();
    let _ = server.await;
    drop((api, bot, control, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn newer_launcher_cancellation_restores_baked_schema_and_observation() {
    let root = std::env::temp_dir().join(format!("baked-contract-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let baked = executable(&root, "old-bot").await;
    let db = Db::open(&root.join("core.db"), 2).unwrap();
    let control = UpdateControl::from_application(&db);
    let base_sha = "b".repeat(40);
    assert!(
        control
            .baked_compatibility("bot", &base_sha, false)
            .await
            .is_err()
    );
    control
        .baked_compatibility("bot", &base_sha, true)
        .await
        .unwrap();
    // Represent the persistent record written by an older baked controller.
    // The executing test launcher is compiled for core16; baked app/DB are15.
    let old = db::BakedCompatibility {
        source_sha: base_sha.clone(),
        core: 15,
        guild: db::guild_data::SCHEMA_VERSION,
    };
    let raw = serde_json::to_string(&old).unwrap();
    db.with_conn(move |conn| {
        conn.pragma_update(None, "user_version", 15)?;
        let tx = conn.transaction_immediate()?;
        tx.execute_bounded(
            "UPDATE meta SET value=?1 WHERE key='component_baked_compatibility_bot_v1'",
            crate::db::params![raw],
            64 * 1024,
        )?;
        tx.commit()
    })
    .await
    .unwrap();
    let contract = control
        .baked_compatibility("bot", &base_sha, false)
        .await
        .unwrap();
    assert_eq!(contract.core, 15);
    assert!(
        control
            .baked_compatibility("bot", &"d".repeat(40), false)
            .await
            .is_err()
    );
    let mut bot = controller(
        "bot",
        baked,
        Registry::ghcr().unwrap(),
        control.clone(),
        &root,
    );
    bot.baked_compatibility = contract;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !bot.reported {
        assert!(Instant::now() < deadline);
        bot.tick().await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let seen = control.component("bot").await.unwrap().observed.unwrap();
    assert_eq!(seen.max_schema, 15);
    control.running("api", None, seen, now()).await.unwrap();
    let mut objects = HashMap::new();
    let mut target = image(&mut objects, "bot", b"\x7fELFfixture");
    target.min_schema = 16;
    target.max_schema = 16;
    target.result_schema = 16;
    target.upgrade.as_mut().unwrap().guild_schema = db::guild_data::SCHEMA_VERSION;
    let mut api = target.clone();
    api.component = "api".into();
    api.image = "ghcr.io/jatmn/jmodguard-api".into();
    for release in [&target, &api] {
        control
            .checked(
                &release.component,
                "latest",
                Some(release.clone()),
                None,
                now(),
            )
            .await
            .unwrap();
    }
    let op = control
        .queue_paired_upgrade_for_tests(&target.digest, &api.digest, false, now())
        .await
        .unwrap();
    restart(&mut bot).await; // application stopped for the launcher handoff
    control.abort_upgrade(&op.id).await.unwrap();
    assert!(!bot.tick().await.unwrap());
    assert!(
        bot.running.is_some(),
        "cancel must resume baked15, not apply launcher's compiled16 gate"
    );
    assert_eq!(control.schema_version().await.unwrap(), 15);
    restart(&mut bot).await;
    let refreshed = control
        .baked_compatibility("bot", &"d".repeat(40), true)
        .await
        .unwrap();
    assert_eq!(refreshed.core, db::CORE_SCHEMA_VERSION);
    // Protocol2 installed JSON has no launcher field. Preserve canonical bytes
    // so the existing desired/installed CAS can restore and report it again.
    let mut legacy = target.clone();
    legacy.launcher = None;
    legacy.upgrade.as_mut().unwrap().control_protocol = 2;
    let legacy_json = serde_json::to_string(&legacy).unwrap();
    assert!(!legacy_json.contains("\"launcher\":"));
    let parsed: ComponentRelease = serde_json::from_str(&legacy_json).unwrap();
    assert_eq!(serde_json::to_string(&parsed).unwrap(), legacy_json);
    parsed.supports_launcher(db::LAUNCHER_REVISION).unwrap();
    control
        .checked("bot", "latest", Some(legacy.clone()), None, now())
        .await
        .unwrap();
    db::request_component_install(&db, "bot", &legacy.digest, now())
        .await
        .unwrap();
    let seen = control.component("bot").await.unwrap().observed.unwrap();
    control
        .running("bot", Some(parsed), seen, now())
        .await
        .unwrap();
    drop((bot, control, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

fn release_at(role: &str, digest_byte: char, schema: i64) -> ComponentRelease {
    ComponentRelease {
        component: role.into(),
        channel: "latest".into(),
        release_id: "paired-fixture".into(),
        source_sha: "c".repeat(40),
        published_at: "2026-10-01T00:00:00Z".into(),
        image: format!("ghcr.io/jatmn/jmodguard-{role}"),
        digest: format!("sha256:{}", digest_byte.to_string().repeat(64)),
        min_schema: schema,
        max_schema: schema,
        result_schema: schema,
        min_api: 1,
        max_api: 1,
        launcher_protocol: 1,
        launcher: Some(db::LauncherCompatibility {
            revision: db::LAUNCHER_REVISION,
            min_revision: db::REQUIRED_LAUNCHER_REVISION,
        }),
        base_abi: "debian-bookworm-amd64".into(),
        upgrade: Some(db::UpgradeCompatibility {
            control_protocol: db::UPDATE_CONTROL_PROTOCOL,
            min_core: 15,
            min_guild: 1,
            guild_schema: db::guild_data::SCHEMA_VERSION,
        }),
    }
}

fn fresh_observation(stamp: i64) -> ComponentObservation {
    ComponentObservation {
        source_sha: "d".repeat(40),
        executable_sha256: "e".repeat(64),
        admin_sha256: None,
        base_image_sha: "d".repeat(40),
        started_at: stamp,
        min_schema: db::CORE_SCHEMA_VERSION,
        max_schema: db::CORE_SCHEMA_VERSION,
        api_capability: 1,
        self_update_enabled: true,
        base_abi: "debian-bookworm-amd64".into(),
        guild_schema: Some(db::guild_data::SCHEMA_VERSION),
        update_protocol: db::UPDATE_CONTROL_PROTOCOL,
        launcher_revision: db::LAUNCHER_REVISION,
    }
}

fn assert_complete_unchanged(before: &db::ServiceUpgrade, after: &db::ServiceUpgrade) {
    assert_eq!(after.id, before.id);
    assert_eq!(after.phase, Phase::Complete);
    assert!(
        after.error.is_none(),
        "completed upgrade was rewritten: {after:?}"
    );
}

/// A finished coordinated upgrade stays in meta. Later independent installs are
/// allowed because `Complete` is not an active phase.
async fn completed_coordinated_upgrade(root: &Path) -> (Db, UpdateControl, ComponentRelease) {
    let stamp = now();
    let db = Db::open(&root.join("core.db"), 2).unwrap();
    let control = UpdateControl::from_application(&db);
    let bot = release_at("bot", 'a', db::CORE_SCHEMA_VERSION);
    let api = release_at("api", 'b', db::CORE_SCHEMA_VERSION);
    for item in [&bot, &api] {
        control
            .checked(&item.component, "latest", Some(item.clone()), None, stamp)
            .await
            .unwrap();
        control
            .running(&item.component, None, fresh_observation(stamp), stamp)
            .await
            .unwrap();
    }
    let op = control
        .queue_paired_upgrade_for_tests(&bot.digest, &api.digest, false, stamp)
        .await
        .unwrap();
    let seen = control.component("bot").await.unwrap().observed.unwrap();
    control
        .running("bot", Some(op.bot.release.clone()), seen, stamp)
        .await
        .unwrap();
    for role in ["bot", "api"] {
        control.join_upgrade(&op.id, role, role).await.unwrap();
        control
            .acknowledge(&op.id, role, role, Ack::Prepared)
            .await
            .unwrap();
    }
    control
        .acknowledge(&op.id, "api", "api", Ack::Stopped)
        .await
        .unwrap();
    control
        .acknowledge(&op.id, "bot", "bot", Ack::Stopped)
        .await
        .unwrap();
    control
        .acknowledge(&op.id, "bot", "bot", Ack::Starting)
        .await
        .unwrap();
    control
        .acknowledge(&op.id, "bot", "bot", Ack::Ready)
        .await
        .unwrap();
    control
        .acknowledge(&op.id, "api", "api", Ack::Starting)
        .await
        .unwrap();
    let done = control
        .acknowledge(&op.id, "api", "api", Ack::Ready)
        .await
        .unwrap();
    assert_eq!(done.phase, Phase::Complete);
    assert!(done.error.is_none());
    assert_eq!(
        control
            .component("bot")
            .await
            .unwrap()
            .installed
            .unwrap()
            .digest,
        op.bot.release.digest
    );
    (db, control, op.bot.release)
}

fn offline_controller(role: &str, control: UpdateControl, root: &Path) -> Supervisor {
    controller(
        role,
        root.join(format!("baked-{role}")),
        Registry::new("http://127.0.0.1:9").unwrap(),
        control,
        root,
    )
}

#[tokio::test]
async fn stale_complete_upgrade_ignores_unrelated_restore_failure() {
    let root = std::env::temp_dir().join(format!(
        "stale-complete-restore-{:032x}",
        rand::random::<u128>()
    ));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let (db, control, coordinated) = completed_coordinated_upgrade(&root).await;
    let stamp = now();
    let mut independent = release_at("bot", 'c', db::CORE_SCHEMA_VERSION + 1);
    independent.source_sha = "d".repeat(40);
    control
        .checked("bot", "latest", Some(independent.clone()), None, stamp)
        .await
        .unwrap();
    db::request_component_install(&db, "bot", &independent.digest, stamp)
        .await
        .unwrap();
    let seen = control.component("bot").await.unwrap().observed.unwrap();
    control
        .running("bot", Some(independent.clone()), seen, stamp)
        .await
        .unwrap();
    let installed = control.component("bot").await.unwrap().installed.unwrap();
    assert_ne!(installed.digest, coordinated.digest);
    let before = control.upgrade().await.unwrap().unwrap();
    assert_eq!(before.phase, Phase::Complete);
    let mut bot = offline_controller("bot", control.clone(), &root);
    let error = bot.tick().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("installed application cannot open database"),
        "{error:#}"
    );
    assert_complete_unchanged(&before, &control.upgrade().await.unwrap().unwrap());
    drop((bot, control, db));
    tokio::fs::remove_dir_all(&root).await.unwrap();
}

#[tokio::test]
async fn complete_upgrade_restore_during_rollback_is_not_rewritten() {
    let root = std::env::temp_dir().join(format!(
        "complete-rollback-restore-{:032x}",
        rand::random::<u128>()
    ));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let (db, control, coordinated) = completed_coordinated_upgrade(&root).await;
    let before = control.upgrade().await.unwrap().unwrap();
    let mut bot = offline_controller("bot", control.clone(), &root);
    bot.rollback = Some((coordinated, "previous start failed".into()));
    let error = bot.tick().await.unwrap_err();
    assert!(
        error.to_string().contains("rollback artifact missing"),
        "{error:#}"
    );
    assert_complete_unchanged(&before, &control.upgrade().await.unwrap().unwrap());
    drop((bot, control, db));
    tokio::fs::remove_dir_all(&root).await.unwrap();
}

#[tokio::test]
async fn ready_failure_of_another_release_leaves_complete_upgrade() {
    let root = std::env::temp_dir().join(format!(
        "complete-ready-mismatch-{:032x}",
        rand::random::<u128>()
    ));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let (db, control, coordinated) = completed_coordinated_upgrade(&root).await;
    let before = control.upgrade().await.unwrap().unwrap();
    assert_eq!(
        control
            .component("bot")
            .await
            .unwrap()
            .installed
            .unwrap()
            .digest,
        coordinated.digest
    );
    let mut other = release_at("bot", 'c', db::CORE_SCHEMA_VERSION);
    other.source_sha = "d".repeat(40);
    let mut child = launch(
        Path::new("/bin/true"),
        &other.source_sha,
        Some(other.clone()),
    )
    .await
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if child.child.try_wait().unwrap().is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "fixture child did not exit");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut bot = offline_controller("bot", control.clone(), &root);
    bot.running = Some(child);
    let error = bot.tick().await.unwrap_err();
    assert!(
        error.to_string().contains("application exited"),
        "{error:#}"
    );
    assert_complete_unchanged(&before, &control.upgrade().await.unwrap().unwrap());
    drop((bot, control, db));
    tokio::fs::remove_dir_all(&root).await.unwrap();
}

struct ContractWarnings(Arc<std::sync::Mutex<Vec<String>>>);

impl tracing::Subscriber for ContractWarnings {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut message = String::new();
        event.record(&mut MessageVisit(&mut message));
        if message.contains("no capability contract")
            || (event.metadata().level() == &tracing::Level::ERROR
                && message == "discord token rejected; bot held, fix token and restart container")
            || (event.metadata().level() == &tracing::Level::WARN
                && message == "retrying bot after discord token rejection hold")
        {
            self.0.lock().expect("warning log").push(message);
        }
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

struct MessageVisit<'a>(&'a mut String);

impl tracing::field::Visit for MessageVisit<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            use std::fmt::Write;
            let _ = write!(self.0, "{value:?}");
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.0.push_str(value);
        }
    }
}

async fn with_contract_warnings<T>(
    future: impl std::future::Future<Output = T>,
) -> (T, Vec<String>) {
    let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let guard = tracing::subscriber::set_default(ContractWarnings(Arc::clone(&logs)));
    let value = future.await;
    drop(guard);
    (value, logs.lock().expect("warning log").clone())
}

async fn open_launcher_db(root: &Path) -> (Db, UpdateControl) {
    let path = root.join("core.db");
    let db = Db::open(&path, 2).unwrap();
    let opts = DbOpenOptions {
        read_only: false,
        path,
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: false,
        default_prefix: "!".into(),
        retention_days: 1,
    };
    let control = UpdateControl::open(&opts).unwrap().unwrap();
    (db, control)
}

fn uncontracted_selection(role: &str, digest_byte: char, omit_null_upgrade: bool) -> String {
    let mut release = release_at(role, digest_byte, db::CORE_SCHEMA_VERSION);
    release.launcher = None;
    if omit_null_upgrade {
        release.upgrade = None;
    }
    let mut value = serde_json::to_value(&release).unwrap();
    let object = value.as_object_mut().unwrap();
    object.remove("launcher");
    if omit_null_upgrade {
        object.remove("upgrade");
    }
    serde_json::to_string(&value).unwrap()
}

async fn store_desired_launcher(db: &Db, role: &str, raw: &str) {
    let (role, raw) = (role.to_owned(), raw.to_owned());
    let changed = db
        .with_conn(move |conn| {
            conn.execute_bounded(
                "UPDATE component_updates SET desired_launcher=?1,installed_launcher=NULL,state='idle',last_error=NULL WHERE component=?2",
                params![raw, role],
                64 * 1024,
            )
        })
        .await
        .unwrap();
    assert_eq!(changed, 1);
}

async fn assert_idle_without_desired(control: &UpdateControl, role: &str) {
    let row = control.component(role).await.unwrap();
    assert!(row.desired_launcher.is_none(), "{role}");
    assert!(row.installed_launcher.is_none(), "{role}");
    assert_eq!(row.state, "idle", "{role}");
    assert!(row.last_error.is_none(), "{role}");
}

#[tokio::test]
async fn stored_launcher_without_contract_is_cleared_once() {
    let root = std::env::temp_dir().join(format!(
        "launcher-no-contract-{:032x}",
        rand::random::<u128>()
    ));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let (db, control) = open_launcher_db(&root).await;
    // An earlier image has no launcher-revision labels. Stored JSON from before
    // that field also omits a null upgrade, which the current serializer emits.
    let historical = uncontracted_selection("bot", 'a', true);
    let parsed: ComponentRelease = serde_json::from_str(&historical).unwrap();
    assert_ne!(serde_json::to_string(&parsed).unwrap(), historical);
    assert!(parsed.launcher.is_none());
    store_desired_launcher(&db, "bot", &historical).await;
    let current = uncontracted_selection("api", 'b', false);
    assert!(
        serde_json::from_str::<ComponentRelease>(&current)
            .unwrap()
            .launcher
            .is_none()
    );
    store_desired_launcher(&db, "api", &current).await;

    let mut bot = offline_controller("bot", control.clone(), &root);
    let mut api = offline_controller("api", control.clone(), &root);
    let (_, warnings) = with_contract_warnings(async {
        assert!(matches!(
            bot.launcher(&control.component("bot").await.unwrap())
                .await
                .unwrap(),
            ControlStep::Proceed
        ));
        assert!(matches!(
            api.launcher(&control.component("api").await.unwrap())
                .await
                .unwrap(),
            ControlStep::Proceed
        ));
    })
    .await;
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert_idle_without_desired(&control, "bot").await;
    assert_idle_without_desired(&control, "api").await;
    let (_, again) = with_contract_warnings(async {
        assert!(matches!(
            bot.launcher(&control.component("bot").await.unwrap())
                .await
                .unwrap(),
            ControlStep::Proceed
        ));
        assert!(matches!(
            api.launcher(&control.component("api").await.unwrap())
                .await
                .unwrap(),
            ControlStep::Proceed
        ));
    })
    .await;
    assert!(again.is_empty(), "{again:?}");
    assert!(!root.join("jmodguard-launcher-next").exists());
    drop((bot, api, control, db));
    tokio::fs::remove_dir_all(&root).await.unwrap();
}

#[tokio::test]
async fn stored_launcher_that_is_not_newer_is_still_cleared() {
    let root = std::env::temp_dir().join(format!(
        "launcher-not-newer-{:032x}",
        rand::random::<u128>()
    ));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let (db, control) = open_launcher_db(&root).await;
    let release = release_at("bot", 'c', db::CORE_SCHEMA_VERSION);
    assert_eq!(
        release.launcher.as_ref().unwrap().revision,
        db::LAUNCHER_REVISION
    );
    store_desired_launcher(&db, "bot", &serde_json::to_string(&release).unwrap()).await;
    let mut bot = offline_controller("bot", control.clone(), &root);
    let (kept, warnings) = with_contract_warnings(async {
        bot.launcher(&control.component("bot").await.unwrap()).await
    })
    .await;
    assert!(matches!(kept.unwrap(), ControlStep::Proceed));
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_idle_without_desired(&control, "bot").await;
    assert!(!root.join("jmodguard-launcher-next").exists());
    drop((bot, control, db));
    tokio::fs::remove_dir_all(&root).await.unwrap();
}

#[tokio::test]
async fn stored_launcher_with_newer_contract_is_still_selected() {
    let root = std::env::temp_dir().join(format!("launcher-newer-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let (db, control) = open_launcher_db(&root).await;
    let mut objects = HashMap::new();
    let mut release = image(&mut objects, "bot", b"\x7fELFfixture");
    release.launcher.as_mut().unwrap().revision = db::LAUNCHER_REVISION + 1;
    store_desired_launcher(&db, "bot", &serde_json::to_string(&release).unwrap()).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let registry = Registry::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .fallback(get(object))
                .with_state(Arc::new(objects)),
        )
        .await
        .unwrap();
    });
    let mut bot = controller(
        "bot",
        root.join("baked-bot"),
        registry,
        control.clone(),
        &root,
    );
    let (selected, warnings) = with_contract_warnings(async {
        bot.launcher(&control.component("bot").await.unwrap()).await
    })
    .await;
    assert!(matches!(selected.unwrap(), ControlStep::Reexec));
    assert!(warnings.is_empty(), "{warnings:?}");
    let desired = control
        .component("bot")
        .await
        .unwrap()
        .desired_launcher
        .unwrap();
    assert_eq!(desired, release);
    let handoff = tokio::fs::read_to_string(root.join("jmodguard-launcher-next"))
        .await
        .unwrap();
    assert!(handoff.ends_with(&format!("{}\n", release.digest)));
    server.abort();
    drop((bot, control, db));
    tokio::fs::remove_dir_all(&root).await.unwrap();
}

#[tokio::test]
async fn read_only_failure_receipt_defers_retry_and_required_reexec() {
    use std::os::unix::fs::PermissionsExt;
    let root =
        std::env::temp_dir().join(format!("readonly-receipt-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let path = root.join("core.db");
    let db = Db::open(&path, 2).unwrap();
    let candidate = release_at("api", 'a', db::CORE_SCHEMA_VERSION + 5);
    let stamp = now();
    db::record_update_check(&db, "api", "latest", Some(candidate.clone()), None, stamp)
        .await
        .unwrap();
    db::request_component_install(&db, "api", &candidate.digest, stamp)
        .await
        .unwrap();
    drop(db);
    let opts = DbOpenOptions {
        read_only: true,
        path: path.clone(),
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: false,
        default_prefix: "!".into(),
        retention_days: 1,
    };
    let control = UpdateControl::open(&opts).unwrap().unwrap();
    assert!(!control.writes_locally());
    let script = root.join("sleep-child");
    tokio::fs::write(&script, "#!/bin/sh\nsleep 60\n")
        .await
        .unwrap();
    tokio::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
        .await
        .unwrap();
    let mut service = controller(
        "api",
        root.join("baked-api"),
        Registry::new("http://127.0.0.1:9").unwrap(),
        control,
        &root,
    );
    let child = launch(&script, &"b".repeat(40), None).await.unwrap();
    tokio::fs::write(&child.ready_file, "ready").await.unwrap();
    service.running = Some(child);
    service.reported = true;
    assert!(!service.tick().await.unwrap());
    let pending = service.pending_failure.as_ref().unwrap();
    assert_eq!(
        pending.component.as_ref().unwrap().0.digest,
        candidate.digest
    );
    assert!(
        !service.staging.exists()
            || std::fs::read_dir(&service.staging)
                .unwrap()
                .next()
                .is_none()
    );
    assert!(!service.tick().await.unwrap());
    assert!(service.pending_failure.is_some());
    drop(service);

    let mut objects = HashMap::new();
    let mut required = image(&mut objects, "api", b"\x7fELF\x02\x01\x01\x00required");
    required.launcher = Some(db::LauncherCompatibility {
        revision: db::LAUNCHER_REVISION + 1,
        min_revision: db::LAUNCHER_REVISION + 1,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let registry = Registry::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .fallback(get(object))
                .with_state(Arc::new(objects)),
        )
        .await
        .unwrap();
    });
    let control = UpdateControl::open(&opts).unwrap().unwrap();
    let mut service = controller("api", root.join("baked-api"), registry, control, &root);
    let step = service.required_launcher(&required, None).await.unwrap();
    assert!(matches!(step, ControlStep::Defer));
    assert!(!root.join("jmodguard-launcher-next").exists());
    server.abort();
    drop(service);
    tokio::fs::remove_dir_all(&root).await.unwrap();
}

#[tokio::test]
async fn revision_two_controller_keeps_a_revision_three_launcher_selection() {
    let root = std::env::temp_dir().join(format!(
        "launcher-rev2-keeps-rev3-{:032x}",
        rand::random::<u128>()
    ));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let (db, control) = open_launcher_db(&root).await;
    let mut objects = HashMap::new();
    let mut release = image(&mut objects, "bot", b"\x7fELFfixture");
    release.launcher = Some(db::LauncherCompatibility {
        revision: 3,
        min_revision: 3,
    });
    store_desired_launcher(&db, "bot", &serde_json::to_string(&release).unwrap()).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let registry = Registry::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .fallback(get(object))
                .with_state(Arc::new(objects)),
        )
        .await
        .unwrap();
    });
    let mut bot = controller(
        "bot",
        root.join("baked-bot"),
        registry,
        control.clone(),
        &root,
    );
    bot.launcher_revision = 2;
    let selected = bot
        .launcher(&control.component("bot").await.unwrap())
        .await
        .unwrap();
    assert!(matches!(selected, ControlStep::Reexec));
    let desired = control
        .component("bot")
        .await
        .unwrap()
        .desired_launcher
        .unwrap();
    assert_eq!(desired.digest, release.digest);
    assert_eq!(desired.launcher.unwrap().revision, 3);
    let handoff = tokio::fs::read_to_string(root.join("jmodguard-launcher-next"))
        .await
        .unwrap();
    assert!(handoff.ends_with(&format!("{}\n", release.digest)));
    server.abort();
    drop((bot, control, db));
    tokio::fs::remove_dir_all(&root).await.unwrap();
}

async fn http_application(root: &Path) -> PathBuf {
    let path = root.join("http-api");
    let source = root.join("http_api.rs");
    let code = r#"
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: i32) { STOP.store(true, Ordering::SeqCst); }
unsafe extern "C" { fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize; }
fn main() {
    unsafe { signal(15, stop); }
    let maintenance = std::env::var("JMODGUARD_UPDATE_MAINTENANCE").as_deref() == Ok("1");
    let ready = std::path::PathBuf::from(std::env::var("JMODGUARD_READY_FILE").unwrap());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::fs::write(ready.with_extension("port"), port.to_string()).unwrap();
    std::fs::write(ready.with_extension("mode"), if maintenance { "1" } else { "0" }).unwrap();
    std::fs::write(&ready, "ready").unwrap();
    while !STOP.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(200)));
                let mut buf = [0u8; 1024];
                let n = stream.read(&mut buf).unwrap_or(0);
                let line = String::from_utf8_lossy(&buf[..n]);
                let (status, body) = if line.starts_with("GET /health") {
                    ("200 OK", "ok")
                } else if line.starts_with("GET /api/me") && !maintenance {
                    ("200 OK", "{\"ok\":true}")
                } else {
                    ("404 Not Found", "missing")
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
}
"#;
    tokio::fs::write(&source, code).await.unwrap();
    let output = Command::new("rustc")
        .args(["--edition=2024", "-C", "debuginfo=0", "-o"])
        .arg(&path)
        .arg(&source)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    path
}

fn read_only_options(path: &Path) -> DbOpenOptions {
    DbOpenOptions {
        read_only: true,
        path: path.to_owned(),
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: false,
        default_prefix: "!".into(),
        retention_days: 1,
    }
}

async fn application_exchange(port_file: &Path, request_target: &str) -> String {
    let started = Instant::now();
    let port = loop {
        if let Ok(text) = tokio::fs::read_to_string(port_file).await
            && let Ok(port) = text.trim().parse::<u16>()
        {
            break port;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "application did not publish its port"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let started = Instant::now();
    loop {
        if let Ok(mut stream) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let request = format!(
                "{request_target} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut body = String::new();
            let _ = stream.read_to_string(&mut body).await;
            if body.contains("200") || body.contains("404") {
                return body;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "application route was not served"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn application_route(port_file: &Path) -> String {
    let started = Instant::now();
    loop {
        let body = application_exchange(port_file, "GET /api/me").await;
        if body.contains("200") {
            return body;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "application route was not served"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn api_without_a_writer(binary: &Path, root: &Path, db_path: &Path) {
    let control = UpdateControl::open(&read_only_options(db_path))
        .unwrap()
        .unwrap();
    assert!(!control.writes_locally());
    let hash = "ab".repeat(32);
    let planned = plan_api_startup(&control, "api", &hash, None, None)
        .await
        .unwrap();
    assert!(matches!(
        planned.first(),
        Some(StartupReceipt::BakedLauncher { .. })
    ));
    assert!(matches!(
        planned.get(1),
        Some(StartupReceipt::ObserveLauncher)
    ));
    let mut service = controller(
        "api",
        binary.to_owned(),
        Registry::new("http://127.0.0.1:9").unwrap(),
        control,
        root,
    );
    service.enabled = false;
    service.pending_startup = planned;
    let started = service.tick().await;
    assert!(started.is_ok(), "{started:?}");
    let ready = service
        .running
        .as_ref()
        .expect("normal application was not started")
        .ready_file
        .clone();
    let body = application_route(&ready.with_extension("port")).await;
    assert!(body.contains("{\"ok\":true}"), "{body}");
    let mode = tokio::fs::read_to_string(ready.with_extension("mode"))
        .await
        .unwrap();
    assert_eq!(mode.trim(), "0");
    assert_eq!(service.pending_startup.len(), 2);
    assert!(
        service
            .control
            .component("api")
            .await
            .unwrap()
            .launcher_sha256
            .is_none()
    );
    service.stop_application().await.unwrap();
}

#[tokio::test]
async fn api_role_serves_the_application_when_startup_receipts_cannot_be_saved() {
    let root = std::env::temp_dir().join(format!(
        "api-startup-nowriter-{:032x}",
        rand::random::<u128>()
    ));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let binary = http_application(&root).await;
    let db_path = root.join("core.db");
    let _db = Db::open(&db_path, 2).unwrap();
    assert!(api_writer_link("", &"x".repeat(32)).is_none());
    assert!(api_writer_link("", "").is_none());
    assert!(api_writer_link("127.0.0.1:9", "short").is_none());
    for name in ["unset", "empty", "secret"] {
        api_without_a_writer(&binary, &root.join(name), &db_path).await;
    }
    drop(_db);
    tokio::fs::remove_dir_all(&root).await.unwrap();
}

#[tokio::test]
async fn api_role_serves_immediately_and_stores_startup_receipts_once_the_bot_is_writer() {
    let root = std::env::temp_dir().join(format!(
        "api-startup-writer-{:032x}",
        rand::random::<u128>()
    ));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let binary = http_application(&root).await;
    let db_path = root.join("core.db");
    let bot_db = Db::open(&db_path, 2).unwrap();
    let secret = "x".repeat(32);
    let mut bot_cfg = jmodguard::config::BotProcessConfig::default();
    bot_cfg.db.path = db_path.clone();
    bot_cfg.bot.control_bind = "127.0.0.1:0".into();
    bot_cfg.bot.control_secret = jmodguard::config::ControlSecret::new(secret.clone());
    let bot = jmodguard_bot::state::BotState::new(bot_cfg, bot_db).unwrap();
    let bound = jmodguard_bot::control_server::spawn(Arc::clone(&bot))
        .unwrap()
        .expect("control listener");
    let link = jmodguard::control::try_writer_link(&bound.to_string(), &secret)
        .unwrap()
        .expect("writer link");
    let control = UpdateControl::open(&read_only_options(&db_path))
        .unwrap()
        .unwrap()
        .with_writer_link(link);
    let hash = "ab".repeat(32);
    let planned = plan_api_startup(&control, "api", &hash, None, None)
        .await
        .unwrap();
    let mut service = controller(
        "api",
        binary,
        Registry::new("http://127.0.0.1:9").unwrap(),
        control,
        &root,
    );
    service.enabled = false;
    service.pending_startup = planned;
    assert!(!service.tick().await.unwrap());
    let ready = service.running.as_ref().unwrap().ready_file.clone();
    let body = application_route(&ready.with_extension("port")).await;
    assert!(body.contains("{\"ok\":true}"), "{body}");
    assert_eq!(service.pending_startup.len(), 2);
    assert!(
        service
            .control
            .component("api")
            .await
            .unwrap()
            .launcher_sha256
            .is_none()
    );
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
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    assert!(!service.tick().await.unwrap());
    assert!(service.pending_startup.is_empty());
    assert_eq!(
        service
            .control
            .component("api")
            .await
            .unwrap()
            .launcher_sha256
            .as_deref(),
        Some(hash.as_str())
    );
    service.stop_application().await.unwrap();
    drop(bot);
    tokio::fs::remove_dir_all(&root).await.unwrap();
}

#[tokio::test]
async fn api_role_serves_reads_while_a_superseded_launcher_clear_waits_for_the_writer() {
    let root = std::env::temp_dir().join(format!(
        "api-superseded-clear-{:032x}",
        rand::random::<u128>()
    ));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let binary = http_application(&root).await;
    let db_path = root.join("core.db");
    let bot_db = Db::open(&db_path, 2).unwrap();
    let release = release_at("api", 'c', db::CORE_SCHEMA_VERSION);
    assert_eq!(
        release.launcher.as_ref().unwrap().revision,
        db::LAUNCHER_REVISION
    );
    store_desired_launcher(&bot_db, "api", &serde_json::to_string(&release).unwrap()).await;
    let secret = "x".repeat(32);
    let mut bot_cfg = jmodguard::config::BotProcessConfig::default();
    bot_cfg.db.path = db_path.clone();
    bot_cfg.bot.control_bind = "127.0.0.1:0".into();
    bot_cfg.bot.control_secret = jmodguard::config::ControlSecret::new(secret.clone());
    let bot = jmodguard_bot::state::BotState::new(bot_cfg, bot_db).unwrap();
    let bound = jmodguard_bot::control_server::spawn(Arc::clone(&bot))
        .unwrap()
        .expect("control listener");
    let link = jmodguard::control::try_writer_link(&bound.to_string(), &secret)
        .unwrap()
        .expect("writer link");
    let control = UpdateControl::open(&read_only_options(&db_path))
        .unwrap()
        .unwrap()
        .with_writer_link(link);
    assert!(!control.writes_locally());
    let hash = "ab".repeat(32);
    let planned = plan_api_startup(&control, "api", &hash, None, None)
        .await
        .unwrap();
    let mut service = controller(
        "api",
        binary,
        Registry::new("http://127.0.0.1:9").unwrap(),
        control,
        &root,
    );
    assert!(service.enabled);
    service.pending_startup = planned;
    assert!(!service.tick().await.unwrap());
    assert_eq!(
        service
            .pending_startup
            .iter()
            .filter(|receipt| {
                matches!(
                    receipt,
                    StartupReceipt::ClearSuperseded { release: selected }
                        if selected.digest == release.digest
                )
            })
            .count(),
        1
    );
    let ready = service
        .running
        .as_ref()
        .expect("normal application was not started")
        .ready_file
        .clone();
    let health = application_exchange(&ready.with_extension("port"), "GET /health").await;
    assert!(health.contains("200"), "{health}");
    let body = application_route(&ready.with_extension("port")).await;
    assert!(body.contains("{\"ok\":true}"), "{body}");
    let mode = tokio::fs::read_to_string(ready.with_extension("mode"))
        .await
        .unwrap();
    assert_eq!(mode.trim(), "0");
    assert_eq!(
        service
            .control
            .component("api")
            .await
            .unwrap()
            .desired_launcher
            .as_ref()
            .map(|selected| selected.digest.as_str()),
        Some(release.digest.as_str())
    );
    assert!(!root.join("jmodguard-launcher-next").exists());
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
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    assert!(!service.tick().await.unwrap());
    assert!(
        service
            .control
            .component("api")
            .await
            .unwrap()
            .desired_launcher
            .is_none()
    );
    assert!(!service.tick().await.unwrap());
    assert!(
        service
            .control
            .component("api")
            .await
            .unwrap()
            .desired_launcher
            .is_none()
    );
    service.stop_application().await.unwrap();
    drop(bot);
    tokio::fs::remove_dir_all(&root).await.unwrap();
}

#[tokio::test]
async fn api_role_serves_reads_while_an_uncontracted_launcher_clear_waits_for_the_writer() {
    let root = std::env::temp_dir().join(format!(
        "api-uncontracted-clear-{:032x}",
        rand::random::<u128>()
    ));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let binary = http_application(&root).await;
    let db_path = root.join("core.db");
    let bot_db = Db::open(&db_path, 2).unwrap();
    let raw = uncontracted_selection("api", 'd', false);
    let release: ComponentRelease = serde_json::from_str(&raw).unwrap();
    assert!(release.launcher.is_none());
    store_desired_launcher(&bot_db, "api", &raw).await;
    let secret = "x".repeat(32);
    let mut bot_cfg = jmodguard::config::BotProcessConfig::default();
    bot_cfg.db.path = db_path.clone();
    bot_cfg.bot.control_bind = "127.0.0.1:0".into();
    bot_cfg.bot.control_secret = jmodguard::config::ControlSecret::new(secret.clone());
    let bot = jmodguard_bot::state::BotState::new(bot_cfg, bot_db).unwrap();
    let bound = jmodguard_bot::control_server::spawn(Arc::clone(&bot))
        .unwrap()
        .expect("control listener");
    let link = jmodguard::control::try_writer_link(&bound.to_string(), &secret)
        .unwrap()
        .expect("writer link");
    let control = UpdateControl::open(&read_only_options(&db_path))
        .unwrap()
        .unwrap()
        .with_writer_link(link);
    assert!(!control.writes_locally());
    let hash = "ab".repeat(32);
    let planned = plan_api_startup(&control, "api", &hash, None, None)
        .await
        .unwrap();
    let mut service = controller(
        "api",
        binary,
        Registry::new("http://127.0.0.1:9").unwrap(),
        control,
        &root,
    );
    assert!(service.enabled);
    service.pending_startup = planned;
    assert!(!service.tick().await.unwrap());
    assert_eq!(
        service
            .pending_startup
            .iter()
            .filter(|receipt| {
                matches!(
                    receipt,
                    StartupReceipt::ClearUncontracted { release: selected }
                        if selected.digest == release.digest
                )
            })
            .count(),
        1
    );
    let ready = service
        .running
        .as_ref()
        .expect("normal application was not started")
        .ready_file
        .clone();
    let health = application_exchange(&ready.with_extension("port"), "GET /health").await;
    assert!(health.contains("200"), "{health}");
    let body = application_route(&ready.with_extension("port")).await;
    assert!(body.contains("{\"ok\":true}"), "{body}");
    let mode = tokio::fs::read_to_string(ready.with_extension("mode"))
        .await
        .unwrap();
    assert_eq!(mode.trim(), "0");
    assert_eq!(
        service
            .control
            .component("api")
            .await
            .unwrap()
            .desired_launcher
            .as_ref()
            .map(|selected| selected.digest.as_str()),
        Some(release.digest.as_str())
    );
    assert!(!root.join("jmodguard-launcher-next").exists());
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
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    assert!(!service.tick().await.unwrap());
    assert!(
        service
            .control
            .component("api")
            .await
            .unwrap()
            .desired_launcher
            .is_none()
    );
    assert!(!service.tick().await.unwrap());
    assert!(
        service
            .control
            .component("api")
            .await
            .unwrap()
            .desired_launcher
            .is_none()
    );
    service.stop_application().await.unwrap();
    drop(bot);
    tokio::fs::remove_dir_all(&root).await.unwrap();
}

async fn api_waiting_on_uncontracted_clear(
    label: &str,
) -> (
    PathBuf,
    Arc<jmodguard_bot::state::BotState>,
    Supervisor,
    ComponentRelease,
) {
    let root = std::env::temp_dir().join(format!("{label}-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let binary = http_application(&root).await;
    let db_path = root.join("core.db");
    let bot_db = Db::open(&db_path, 2).unwrap();
    let raw = uncontracted_selection("api", 'd', false);
    let release: ComponentRelease = serde_json::from_str(&raw).unwrap();
    assert!(release.launcher.is_none());
    store_desired_launcher(&bot_db, "api", &raw).await;
    let secret = "x".repeat(32);
    let mut bot_cfg = jmodguard::config::BotProcessConfig::default();
    bot_cfg.db.path = db_path.clone();
    bot_cfg.bot.control_bind = "127.0.0.1:0".into();
    bot_cfg.bot.control_secret = jmodguard::config::ControlSecret::new(secret.clone());
    let bot = jmodguard_bot::state::BotState::new(bot_cfg, bot_db).unwrap();
    let bound = jmodguard_bot::control_server::spawn(Arc::clone(&bot))
        .unwrap()
        .expect("control listener");
    let link = jmodguard::control::try_writer_link(&bound.to_string(), &secret)
        .unwrap()
        .expect("writer link");
    let control = UpdateControl::open(&read_only_options(&db_path))
        .unwrap()
        .unwrap()
        .with_writer_link(link);
    assert!(!control.writes_locally());
    let planned = plan_api_startup(&control, "api", &"ab".repeat(32), None, None)
        .await
        .unwrap();
    let mut service = controller(
        "api",
        binary,
        Registry::new("http://127.0.0.1:9").unwrap(),
        control,
        &root,
    );
    assert!(service.enabled);
    service.pending_startup = planned;
    assert!(!service.tick().await.unwrap());
    assert!(
        service.pending_startup.iter().any(|receipt| {
            matches!(
                receipt,
                StartupReceipt::ClearUncontracted { release: selected }
                    if selected.digest == release.digest
            )
        }),
        "uncontracted clear was not queued"
    );
    assert!(
        service.running.is_some(),
        "normal application was not started"
    );
    (root, bot, service, release)
}

async fn claim_writer(bot: &jmodguard_bot::state::BotState) {
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

async fn crash_running_child(service: &mut Supervisor) -> u32 {
    let child = service
        .running
        .as_mut()
        .expect("normal application was not started");
    let pid = child.child.id().expect("child pid");
    child.child.start_kill().expect("crash child");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if child.child.try_wait().unwrap().is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "crashed child did not exit");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    pid
}

#[tokio::test]
async fn replaced_uncontracted_clear_drains_and_restarts_a_crashed_child() {
    let (root, bot, mut service, queued) =
        api_waiting_on_uncontracted_clear("api-uncontracted-replaced").await;
    let mut replacement = release_at("api", 'e', db::CORE_SCHEMA_VERSION);
    replacement.launcher.as_mut().unwrap().revision = db::LAUNCHER_REVISION + 1;
    assert_ne!(replacement.digest, queued.digest);
    store_desired_launcher(
        &bot.db,
        "api",
        &serde_json::to_string(&replacement).unwrap(),
    )
    .await;
    let crashed = crash_running_child(&mut service).await;
    // The replacement stays selected. Self-update stays off so this tick does
    // not hand the launcher off; restarting the crashed child is the
    // supervision a stuck clear used to skip.
    service.enabled = false;
    claim_writer(&bot).await;
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let error = service.tick().await.unwrap_err();
    assert!(
        error.to_string().contains("application exited"),
        "{error:#}"
    );
    assert!(service.pending_startup.is_empty());
    assert!(service.running.is_none());
    let desired = service
        .control
        .component("api")
        .await
        .unwrap()
        .desired_launcher
        .unwrap();
    assert_eq!(desired.digest, replacement.digest);
    assert_eq!(
        desired.launcher.unwrap().revision,
        db::LAUNCHER_REVISION + 1
    );
    tokio::time::sleep(POLL).await;
    assert!(!service.tick().await.unwrap());
    let restarted = service
        .running
        .as_ref()
        .expect("crashed child was not restarted");
    assert_ne!(restarted.child.id(), Some(crashed));
    let ready = restarted.ready_file.clone();
    let body = application_route(&ready.with_extension("port")).await;
    assert!(body.contains("{\"ok\":true}"), "{body}");
    assert_eq!(
        service
            .control
            .component("api")
            .await
            .unwrap()
            .desired_launcher
            .unwrap()
            .digest,
        replacement.digest
    );
    assert!(service.pending_startup.is_empty());
    service.stop_application().await.unwrap();
    drop(bot);
    tokio::fs::remove_dir_all(&root).await.unwrap();
}

#[tokio::test]
async fn lost_uncontracted_clear_reply_drains_and_restarts_a_crashed_child() {
    let (root, bot, mut service, queued) =
        api_waiting_on_uncontracted_clear("api-uncontracted-lost-reply").await;
    let cleared = bot
        .db
        .with_conn(|conn| {
            conn.execute_bounded(
                "UPDATE component_updates SET desired_launcher=NULL WHERE component=?1 AND desired_launcher IS NOT NULL",
                params!["api"],
                64 * 1024,
            )
        })
        .await
        .unwrap();
    assert_eq!(cleared, 1);
    assert_ne!(queued.digest, "");
    let crashed = crash_running_child(&mut service).await;
    claim_writer(&bot).await;
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let error = service.tick().await.unwrap_err();
    assert!(
        error.to_string().contains("application exited"),
        "{error:#}"
    );
    assert!(service.pending_startup.is_empty());
    assert!(service.running.is_none());
    assert!(
        service
            .control
            .component("api")
            .await
            .unwrap()
            .desired_launcher
            .is_none()
    );
    tokio::time::sleep(POLL).await;
    assert!(!service.tick().await.unwrap());
    let restarted = service
        .running
        .as_ref()
        .expect("crashed child was not restarted");
    assert_ne!(restarted.child.id(), Some(crashed));
    let ready = restarted.ready_file.clone();
    let body = application_route(&ready.with_extension("port")).await;
    assert!(body.contains("{\"ok\":true}"), "{body}");
    assert!(service.pending_startup.is_empty());
    assert!(
        service
            .control
            .component("api")
            .await
            .unwrap()
            .desired_launcher
            .is_none()
    );
    service.stop_application().await.unwrap();
    drop(bot);
    tokio::fs::remove_dir_all(&root).await.unwrap();
}

async fn api_waiting_on_superseded_clear(
    label: &str,
) -> (
    PathBuf,
    Arc<jmodguard_bot::state::BotState>,
    Supervisor,
    ComponentRelease,
) {
    let root = std::env::temp_dir().join(format!("{label}-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let binary = http_application(&root).await;
    let db_path = root.join("core.db");
    let bot_db = Db::open(&db_path, 2).unwrap();
    let release = release_at("api", 'c', db::CORE_SCHEMA_VERSION);
    assert_eq!(
        release.launcher.as_ref().unwrap().revision,
        db::LAUNCHER_REVISION
    );
    store_desired_launcher(&bot_db, "api", &serde_json::to_string(&release).unwrap()).await;
    let secret = "x".repeat(32);
    let mut bot_cfg = jmodguard::config::BotProcessConfig::default();
    bot_cfg.db.path = db_path.clone();
    bot_cfg.bot.control_bind = "127.0.0.1:0".into();
    bot_cfg.bot.control_secret = jmodguard::config::ControlSecret::new(secret.clone());
    let bot = jmodguard_bot::state::BotState::new(bot_cfg, bot_db).unwrap();
    let bound = jmodguard_bot::control_server::spawn(Arc::clone(&bot))
        .unwrap()
        .expect("control listener");
    let link = jmodguard::control::try_writer_link(&bound.to_string(), &secret)
        .unwrap()
        .expect("writer link");
    let control = UpdateControl::open(&read_only_options(&db_path))
        .unwrap()
        .unwrap()
        .with_writer_link(link);
    assert!(!control.writes_locally());
    let planned = plan_api_startup(&control, "api", &"ab".repeat(32), None, None)
        .await
        .unwrap();
    let mut service = controller(
        "api",
        binary,
        Registry::new("http://127.0.0.1:9").unwrap(),
        control,
        &root,
    );
    assert!(service.enabled);
    service.pending_startup = planned;
    assert!(!service.tick().await.unwrap());
    assert!(
        service.pending_startup.iter().any(|receipt| {
            matches!(
                receipt,
                StartupReceipt::ClearSuperseded { release: selected }
                    if selected.digest == release.digest
            )
        }),
        "superseded clear was not queued"
    );
    assert!(
        service.running.is_some(),
        "normal application was not started"
    );
    (root, bot, service, release)
}

#[tokio::test]
async fn superseded_clear_receipt_retires_when_no_row_matches() {
    let (root, bot, mut service, queued) =
        api_waiting_on_superseded_clear("api-superseded-zero-rows").await;
    let replacement = release_at("api", 'f', db::CORE_SCHEMA_VERSION);
    assert_ne!(
        serde_json::to_string(&replacement).unwrap(),
        serde_json::to_string(&queued).unwrap()
    );
    store_desired_launcher(
        &bot.db,
        "api",
        &serde_json::to_string(&replacement).unwrap(),
    )
    .await;
    let crashed = crash_running_child(&mut service).await;
    // A different stored selection makes the conditional update match zero
    // rows. Leave self-update off so this tick does not clear or hand off
    // that replacement while the crashed child is restarted.
    service.enabled = false;
    claim_writer(&bot).await;
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let error = service.tick().await.unwrap_err();
    assert!(
        error.to_string().contains("application exited"),
        "{error:#}"
    );
    assert!(
        service
            .pending_startup
            .iter()
            .all(|receipt| { !matches!(receipt, StartupReceipt::ClearSuperseded { .. }) }),
        "superseded clear stayed queued after zero rows matched"
    );
    assert!(service.pending_startup.is_empty());
    assert!(service.running.is_none());
    assert_eq!(
        service
            .control
            .component("api")
            .await
            .unwrap()
            .desired_launcher
            .unwrap()
            .digest,
        replacement.digest
    );
    tokio::time::sleep(POLL).await;
    assert!(!service.tick().await.unwrap());
    let restarted = service
        .running
        .as_ref()
        .expect("crashed child was not restarted");
    assert_ne!(restarted.child.id(), Some(crashed));
    let ready = restarted.ready_file.clone();
    let body = application_route(&ready.with_extension("port")).await;
    assert!(body.contains("{\"ok\":true}"), "{body}");
    assert_eq!(
        service
            .control
            .component("api")
            .await
            .unwrap()
            .desired_launcher
            .unwrap()
            .digest,
        replacement.digest
    );
    assert!(service.pending_startup.is_empty());
    service.stop_application().await.unwrap();
    drop(bot);
    tokio::fs::remove_dir_all(&root).await.unwrap();
}

/// Reconstruct the historical schema, rather than only lowering user_version
/// on an otherwise current DB. The child is the existing ELF fixture; application
/// migrations below use the real bot-owned Db::open path after launcher admission.
#[tokio::test]
async fn baked_bot_migrates_queued_protocol3_core16_without_starting_api_early() {
    for stale_meta in [false, true] {
        let root =
            std::env::temp_dir().join(format!("queued-core16-{:032x}", rand::random::<u128>()));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let baked = executable(&root, "baked-bot").await;
        let path = root.join("core.db");
        let db = Db::open(&path, 2).unwrap();
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("fixtures/queued-core16.json")).unwrap();
        db.with_conn(move |conn| {
            let tx = conn.transaction_immediate()?;
            tx.execute_batch_bounded(
                "DROP INDEX idx_events_guild_id; DROP INDEX idx_events_guild_type_id;
                 DROP INDEX idx_bans_guild_id; DROP INDEX bot_actions_expiry;
                 DROP INDEX idx_bans_one_active; ALTER TABLE bans DROP COLUMN operation_id;
                 PRAGMA user_version=16;",
                1024 * 1024,
            )?;
            for row in fixture["component_updates"].as_array().unwrap() {
                // Column names are from this committed synthetic fixture only.
                for (column, value) in row.as_object().unwrap() {
                    if column == "component" {
                        continue;
                    }
                    let sql =
                        format!("UPDATE component_updates SET {column}=?1 WHERE component=?2");
                    let value = match value {
                        serde_json::Value::Null => None,
                        serde_json::Value::String(value) => Some(value.clone()),
                        value => Some(value.to_string()),
                    };
                    tx.execute_bounded(
                        &sql,
                        params![value, row["component"].as_str().unwrap()],
                        64 * 1024,
                    )?;
                }
            }
            for (key, value) in fixture["meta"].as_object().unwrap() {
                let value = value
                    .as_str()
                    .map_or_else(|| value.to_string(), str::to_owned);
                db::write_meta_string_conn(&tx, key, &value)?;
            }
            if stale_meta {
                for role in ["bot", "api"] {
                    // Failed newer baked boot, followed by rollback to the old app.
                    let value = json!({"source_sha": "f".repeat(40), "core": 18, "guild": 15});
                    db::write_meta_string_conn(
                        &tx,
                        &format!("component_baked_compatibility_{role}_v1"),
                        &value.to_string(),
                    )?;
                }
            }
            tx.commit()
        })
        .await
        .unwrap();
        drop(db);
        let opts = DbOpenOptions {
            path: path.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: false,
            read_only: false,
            default_prefix: "!".into(),
            retention_days: 1,
        };
        let control = UpdateControl::open(&opts).unwrap().unwrap();
        assert_eq!(control.schema_version().await.unwrap(), 16);
        assert!(control.upgrade().await.unwrap().is_none());
        let before = control.component("bot").await.unwrap();
        assert_eq!(before.observed.as_ref().unwrap().update_protocol, 3);
        assert!(before.installed.is_none() && before.previous.is_none());
        let mut bot = controller(
            "bot",
            baked.clone(),
            Registry::ghcr().unwrap(),
            control.clone(),
            &root,
        );
        // Match real baked startup: bot writes both records; read-only baked API
        // uses its compiled contract, independent of old or stale meta.
        for role in ["bot", "api"] {
            let contract = control
                .baked_compatibility(role, &bot.base_sha, true)
                .await
                .unwrap();
            assert_eq!(contract.core, 18);
            assert_eq!(contract.source_sha, bot.base_sha);
            if role == "bot" {
                bot.baked_compatibility = contract;
            }
        }
        let mut api = controller(
            "api",
            baked,
            Registry::ghcr().unwrap(),
            control.clone(),
            &root,
        );
        let error = api.tick().await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("baked application cannot open database")
        );
        assert!(api.running.is_none());
        tokio::fs::write(root.join("hold-baked-bot-ready"), "")
            .await
            .unwrap();
        bot.tick().await.unwrap();
        assert!(
            bot.running.is_some(),
            "baked migrator must start from core16"
        );
        assert!(bot.running.as_ref().unwrap().release.is_none());
        // Real application bootstrap applies migrations 17-18 and the readiness
        // marker. The ELF child itself deliberately does no DB/application work.
        let migrated = Db::open(&path, 2).unwrap();
        assert_eq!(control.schema_version().await.unwrap(), 18);
        tokio::fs::remove_file(root.join("hold-baked-bot-ready"))
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !bot.reported {
            assert!(Instant::now() < deadline);
            bot.report_ready().await.unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let after = control.component("bot").await.unwrap();
        assert_eq!(after.desired, before.desired);
        assert_eq!(after.candidate, before.candidate);
        assert!(after.installed.is_none() && after.previous.is_none());
        assert_eq!(after.state, "queued");
        assert_eq!(after.observed.unwrap().min_schema, 18);
        api.restore(&control.component("api").await.unwrap())
            .await
            .unwrap();
        assert!(api.running.is_some());
        api.stop_application().await.unwrap();
        bot.stop_application().await.unwrap();
        drop((api, bot, control, migrated));
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}

#[tokio::test]
async fn rejected_token_child_is_held_with_liveness_and_no_restart() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let root = std::env::temp_dir().join(format!("token-hold-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let script = root.join("rejected-bot");
    tokio::fs::write(
        &script,
        format!(
            "#!/bin/sh\nexit {}\n",
            jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE
        ),
    )
    .await
    .unwrap();
    tokio::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
        .await
        .unwrap();
    let opts = DbOpenOptions {
        path: root.join("core.db"),
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: true,
        read_only: false,
        default_prefix: "!".into(),
        retention_days: 1,
    };
    let db = Db::open_with(opts.clone()).unwrap();
    let control = UpdateControl::open(&opts).unwrap().unwrap();
    let mut bot = offline_controller("bot", control, &root);
    bot.baked = script.clone();
    bot.enabled = false;
    // Reserve an ephemeral health socket; production uses the configured 8081.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    bot.health_bind = Some(addr.to_string());
    bot.start(&script, &"b".repeat(40), None).await.unwrap();
    bot.running.as_mut().unwrap().child.wait().await.unwrap();
    let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let guard = tracing::subscriber::set_default(ContractWarnings(Arc::clone(&logs)));
    assert!(
        bot.report_ready().await.is_ok(),
        "token rejection must hold rather than surface a restart error"
    );
    assert!(bot.running.is_none());
    for _ in 0..3 {
        assert!(!bot.tick().await.unwrap());
        assert!(bot.running.is_none(), "held bot must never be restored");
    }
    for (path, expected) in [("/health", "HTTP/1.1 200"), ("/ready", "HTTP/1.1 503")] {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = String::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_string(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(response.starts_with(expected), "{response}");
        if path == "/ready" {
            assert!(response.contains("discord token rejected"), "{response}");
        }
    }
    assert_eq!(
        *logs.lock().unwrap(),
        ["discord token rejected; bot held, fix token and restart container"]
    );
    // Advance the policy clock to its deadline instead of waiting 30 minutes.
    bot.restart.auth_retry_at = Some(Instant::now());
    assert!(!bot.tick().await.unwrap());
    if let Some(child) = bot.running.as_mut() {
        child.child.wait().await.unwrap();
        assert!(!bot.report_ready().await.unwrap());
    }
    assert!(bot.restart.held);
    assert_eq!(bot.restart.auth_failures, 2);
    let next = bot
        .restart
        .auth_retry_at
        .unwrap()
        .duration_since(Instant::now());
    assert!(next > Duration::from_secs(3590) && next <= Duration::from_secs(3600));
    assert_eq!(
        *logs.lock().unwrap(),
        [
            "discord token rejected; bot held, fix token and restart container",
            "retrying bot after discord token rejection hold",
            "discord token rejected; bot held, fix token and restart container",
        ]
    );
    // A ready retry child resumes normal behavior and resets the auth interval.
    tokio::fs::write(
        &script,
        "#!/bin/sh\nprintf ready > \"$JMODGUARD_READY_FILE\"\nexec sleep 60\n",
    )
    .await
    .unwrap();
    bot.restart.auth_retry_at = Some(Instant::now());
    assert!(!bot.tick().await.unwrap());
    let deadline = Instant::now() + Duration::from_secs(2);
    while !bot.report_ready().await.unwrap() {
        assert!(Instant::now() < deadline, "retry child never ready");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!bot.restart.held);
    assert_eq!(bot.restart.auth_failures, 0);
    assert!(bot.restart.auth_retry_at.is_none());
    assert_eq!(
        logs.lock()
            .unwrap()
            .iter()
            .filter(|line| line.as_str() == "retrying bot after discord token rejection hold")
            .count(),
        2
    );
    drop(guard);
    bot.stop_application().await.unwrap();
    bot.end_idle().await.unwrap();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}
