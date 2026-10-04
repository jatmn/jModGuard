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
        maintenance: None,
        health: None,
        health_bind: None,
        staging: root.join(format!("staged-{role}")),
        prepared: None,
        standby: None,
        rollback: None,
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
        .request_upgrade(&bot_release.digest, &api_release.digest, false, now())
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
            .request_upgrade(&bot_release.digest, &api_release.digest, true, now())
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
    assert!(!bot.launcher(&state).await.unwrap());
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
    assert!(!bot.required_launcher(&optional, None).await.unwrap());
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
        .request_upgrade(&target.digest, &api.digest, false, now())
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
        .request_upgrade(&bot.digest, &api.digest, false, stamp)
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
        if message.contains("no capability contract") {
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
        assert!(
            !bot.launcher(&control.component("bot").await.unwrap())
                .await
                .unwrap()
        );
        assert!(
            !api.launcher(&control.component("api").await.unwrap())
                .await
                .unwrap()
        );
    })
    .await;
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert_idle_without_desired(&control, "bot").await;
    assert_idle_without_desired(&control, "api").await;
    let (_, again) = with_contract_warnings(async {
        assert!(
            !bot.launcher(&control.component("bot").await.unwrap())
                .await
                .unwrap()
        );
        assert!(
            !api.launcher(&control.component("api").await.unwrap())
                .await
                .unwrap()
        );
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
    assert!(!kept.unwrap());
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
    assert!(selected.unwrap());
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
