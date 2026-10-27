//! Real supervisor, OCI extraction, SQLite operation state and reaped OS children.
//! Tiny ELF fixtures stand in for Discord/application work; their readiness and
//! exit behavior are real, while the test explicitly simulates a future migration.
use super::*;
use axum::{Router, body::Body, extract::State, http::Uri, response::Response, routing::get};
use db::{Db, DbOpenOptions, SqlExecutor, params};
use jmodguard_updater::signing_fixture::{SigningFixture, SigningKeyChoice};
use jmodguard_updater::signature::{
    UNSIGNED_IMAGE, signature_tag_for_digest,
};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

fn test_signing() -> &'static SigningFixture {
    static FIXTURE: OnceLock<SigningFixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let fixture = SigningFixture::new();
        fixture.apply_env();
        fixture
    })
}

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
    test_signing().sign(
        objects,
        role,
        &manifest_hash,
        "2027-12-31T00:00:00Z",
        SigningKeyChoice::Current,
    );
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

fn complete_exit_code(raw: &str) -> Option<i32> {
    if !raw.ends_with('\n') {
        return None;
    }
    let text = raw.trim();
    if text.is_empty() {
        return None;
    }
    Some(text.parse().expect("exit-code record is not an integer"))
}

fn exit_code_bytes(code: impl std::fmt::Display) -> String {
    format!("{code}\n")
}

fn publish_exit_code_sync(path: &std::path::Path, code: impl std::fmt::Display) {
    let pending = path.with_extension("tmp");
    std::fs::write(&pending, exit_code_bytes(&code)).unwrap();
    std::fs::rename(&pending, path).unwrap();
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
fn complete_exit_code(raw: &str) -> Option<i32> {{
    if !raw.ends_with('\n') {{
        return None;
    }}
    let text = raw.trim();
    if text.is_empty() {{
        return None;
    }}
    Some(text.parse().expect("exit-code record is not an integer"))
}}
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
        let _health = if root.join("bind-health-{name}").exists() {{
            let bind = std::fs::read_to_string(root.join("bind-health-{name}")).unwrap();
            let listener = std::net::TcpListener::bind(&bind).unwrap();
            let bound = root.join("health-bound-{name}");
            let pending = bound.with_extension("tmp");
            std::fs::write(&pending, listener.local_addr().unwrap().to_string()).unwrap();
            std::fs::rename(pending, bound).unwrap();
            while !root.join("exit-code-{name}").exists() && !STOP.load(Ordering::SeqCst) {{
                std::thread::sleep(std::time::Duration::from_millis(5));
            }}
            Some(listener)
        }} else {{ None }};
        if let Ok(mut raw) = std::fs::read_to_string(root.join("exit-code-{name}")) {{
            if let Some(code) = complete_exit_code(&raw) {{
                std::process::exit(code);
            }} else {{
                event("exit-partial");
                let incomplete_start = std::time::Instant::now();
                while !STOP.load(Ordering::SeqCst)
                    && incomplete_start.elapsed() < std::time::Duration::from_millis(200)
                {{
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    match std::fs::read_to_string(root.join("exit-code-{name}")) {{
                        Ok(updated) => {{
                            raw = updated;
                            if let Some(code) = complete_exit_code(&raw) {{
                                std::process::exit(code);
                            }}
                            event("exit-partial");
                        }}
                        Err(_) => break,
                    }}
                }}
            }}
        }}
        if root.join("fail-{name}").exists() {{
            std::fs::write(std::env::var("JMODGUARD_READY_FILE").unwrap(), "stale marker").unwrap();
            std::process::exit(7);
        }}
        while root.join("hold-{name}-ready").exists() && !STOP.load(Ordering::SeqCst) {{ std::thread::sleep(std::time::Duration::from_millis(5)); }}
        if STOP.load(Ordering::SeqCst) {{ event("stop"); return; }}
        std::fs::write(std::env::var("JMODGUARD_READY_FILE").unwrap(), "ready").unwrap();
        event("ready");
    }}
    while !STOP.load(Ordering::SeqCst) {{
        if !maintenance && let Ok(raw) = std::fs::read_to_string(root.join("exit-code-{name}")) {{
            if let Some(code) = complete_exit_code(&raw) {{
                std::process::exit(code);
            }}
        }}
        std::thread::sleep(std::time::Duration::from_millis(5));
    }}
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

/// Publish an exit code by renaming a complete `{code}\n` record onto the path
/// the fixture child polls.
async fn publish_exit_code(path: impl AsRef<std::path::Path>, code: impl std::fmt::Display) {
    let path = path.as_ref();
    let pending = path.with_extension("tmp");
    tokio::fs::write(&pending, exit_code_bytes(&code))
        .await
        .unwrap();
    tokio::fs::rename(&pending, path).await.unwrap();
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
        child_watch: None,
        running: None,
        restart: RestartPolicy::default(),
        restart_target: None,
        exited_for_restart: false,
        exit_accounted: false,
        maintenance: None,
        health: None,
        health_bind: None,
        exit_control_gate: None,
        readiness_error_gate: None,
        before_idle_gate: None,
        after_idle_gate: None,
        staging: root.join(format!("staged-{role}")),
        prepared: None,
        standby: None,
        rejected_candidate: None,
        persisted_candidate_digest: None,
        rejected_loaded: false,
        fail_rejected_lookups: 0,
        fail_rejected_candidate_write: false,
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
    service.rejected_candidate = None;
    service.persisted_candidate_digest = None;
    service.rollback = None;
    service.pending_failure = None;
    service.pending_startup.clear();
    service.receipt_attempt = 0;
    service.receipt_not_before = None;
    service.exited_for_restart = false;
    service.restart_target = None;
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
    release.launcher.as_mut().unwrap().revision = db::LAUNCHER_REVISION;
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
    bot.launcher_revision = db::LAUNCHER_REVISION - 1;
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
    assert_eq!(
        desired.launcher.as_ref().unwrap().revision,
        db::LAUNCHER_REVISION
    );
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
                "DROP TABLE gateway_ban_deliveries; DROP TABLE known_bad_images; DROP INDEX idx_events_guild_id; DROP INDEX idx_events_guild_type_id;
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
            assert_eq!(contract.core, db::CORE_SCHEMA_VERSION);
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
        // Real application bootstrap applies the remaining migrations and the readiness
        // marker. The ELF child itself deliberately does no DB/application work.
        let migrated = Db::open(&path, 2).unwrap();
        assert_eq!(
            control.schema_version().await.unwrap(),
            db::CORE_SCHEMA_VERSION
        );
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
        assert_eq!(after.observed.unwrap().min_schema, db::CORE_SCHEMA_VERSION);
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

/// Historical desired rows omit `upgrade`. Migration keeps them queued; the
/// next updater pass must refuse each one and later passes must leave it.
#[tokio::test]
async fn stale_schema_selections_without_upgrade_are_refused_once_after_migration() {
    let root = std::env::temp_dir().join(format!(
        "stale-schema-selection-{:032x}",
        rand::random::<u128>()
    ));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let baked = executable(&root, "baked-bot").await;
    let next_bot = executable(&root, "next-bot").await;
    let path = root.join("core.db");
    let db = Db::open(&path, 2).unwrap();
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/queued-core16.json")).unwrap();
    db.with_conn({
        let fixture = fixture.clone();
        move |conn| {
            let tx = conn.transaction_immediate()?;
            tx.execute_batch_bounded(
                "DROP TABLE gateway_ban_deliveries; DROP TABLE known_bad_images; DROP INDEX idx_events_guild_id; DROP INDEX idx_events_guild_type_id;
                 DROP INDEX idx_bans_guild_id; DROP INDEX bot_actions_expiry;
                 DROP INDEX idx_bans_one_active; ALTER TABLE bans DROP COLUMN operation_id;
                 PRAGMA user_version=16;",
                1024 * 1024,
            )?;
            for row in fixture["component_updates"].as_array().unwrap() {
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
            tx.commit()
        }
    })
    .await
    .unwrap();
    let raw_desired = db
        .with_conn(|conn| {
            let tx = conn.transaction_primary_snapshot()?;
            let mut rows = Vec::new();
            for role in ["bot", "api"] {
                let raw: String = tx.query_row(
                    "SELECT desired FROM component_updates WHERE component=?1",
                    params![role],
                    |row| Ok(row.get(0)?),
                )?;
                rows.push(raw);
            }
            tx.finish_read()?;
            Ok(rows)
        })
        .await
        .unwrap();
    for raw in &raw_desired {
        let parsed: ComponentRelease = serde_json::from_str(raw).unwrap();
        assert_ne!(serde_json::to_string(&parsed).unwrap(), *raw);
        assert!(parsed.upgrade.is_none(), "{raw}");
        assert_eq!(parsed.max_schema, 15, "{raw}");
    }
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
    let mut bot = controller(
        "bot",
        baked.clone(),
        Registry::ghcr().unwrap(),
        control.clone(),
        &root,
    );
    for role in ["bot", "api"] {
        let contract = control
            .baked_compatibility(role, &bot.base_sha, true)
            .await
            .unwrap();
        assert_eq!(contract.core, db::CORE_SCHEMA_VERSION);
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
    tokio::fs::write(root.join("hold-baked-bot-ready"), "")
        .await
        .unwrap();
    bot.tick().await.unwrap();
    assert!(bot.running.is_some());
    let migrated = Db::open(&path, 2).unwrap();
    assert_eq!(
        control.schema_version().await.unwrap(),
        db::CORE_SCHEMA_VERSION
    );
    api.tick().await.unwrap();
    assert!(api.running.is_some());
    tokio::fs::remove_file(root.join("hold-baked-bot-ready"))
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !bot.reported || !api.reported {
        assert!(Instant::now() < deadline);
        bot.report_ready().await.unwrap();
        api.report_ready().await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    for role in ["bot", "api"] {
        let queued = control.component(role).await.unwrap();
        assert!(
            queued.desired.is_some(),
            "{role} queue cleared during migration"
        );
        assert_eq!(queued.state, "queued", "{role}");
        assert!(queued.installed.is_none(), "{role}");
    }
    assert!(!bot.tick().await.unwrap());
    assert!(!api.tick().await.unwrap());
    let mut refused_at = Vec::new();
    for role in ["bot", "api"] {
        let row = control.component(role).await.unwrap();
        assert!(
            row.desired.is_none(),
            "{role} selection was rechecked without settling"
        );
        assert_eq!(row.state, "failed", "{role}");
        let error = row.last_error.unwrap();
        assert!(
            error.contains("candidate cannot open current database schema"),
            "{role}: {error}"
        );
        refused_at.push(error);
        assert!(row.installed.is_none(), "{role}");
    }
    let bot_pid = bot.running.as_ref().unwrap().child.id();
    let api_pid = api.running.as_ref().unwrap().child.id();
    assert!(bot.running.as_ref().unwrap().release.is_none());
    assert!(api.running.as_ref().unwrap().release.is_none());
    for _ in 0..3 {
        assert!(!bot.tick().await.unwrap());
        assert!(!api.tick().await.unwrap());
    }
    assert_eq!(bot.running.as_ref().unwrap().child.id(), bot_pid);
    assert_eq!(api.running.as_ref().unwrap().child.id(), api_pid);
    for (role, error) in ["bot", "api"].into_iter().zip(refused_at) {
        let row = control.component(role).await.unwrap();
        assert!(row.desired.is_none(), "{role}");
        assert_eq!(row.state, "failed", "{role}");
        assert_eq!(row.last_error.as_deref(), Some(error.as_str()), "{role}");
    }

    let mut objects = HashMap::new();
    let bytes = tokio::fs::read(&next_bot).await.unwrap();
    let mut release = image(&mut objects, "bot", &bytes);
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
    bot.registry = registry;
    control
        .checked("bot", "latest", Some(release.clone()), None, now())
        .await
        .unwrap();
    let install_db = Db::open(&path, 2).unwrap();
    db::request_component_install(&install_db, "bot", &release.digest, now())
        .await
        .unwrap();
    let apply_deadline = Instant::now() + Duration::from_secs(10);
    while control
        .component("bot")
        .await
        .unwrap()
        .installed
        .as_ref()
        .map(|installed| &installed.digest)
        != Some(&release.digest)
    {
        assert!(
            Instant::now() < apply_deadline,
            "compatible selection did not apply"
        );
        assert!(!bot.tick().await.unwrap());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let applied = control.component("bot").await.unwrap();
    assert_eq!(applied.state, "running");
    assert_eq!(applied.installed.unwrap().digest, release.digest);
    assert_eq!(applied.desired.unwrap().digest, release.digest);
    assert!(applied.last_error.is_none());
    assert_ne!(bot.running.as_ref().unwrap().child.id(), bot_pid);
    assert_eq!(
        bot.running
            .as_ref()
            .unwrap()
            .release
            .as_ref()
            .unwrap()
            .digest,
        release.digest
    );
    assert_eq!(api.running.as_ref().unwrap().child.id(), api_pid);
    assert!(control.component("api").await.unwrap().desired.is_none());
    bot.stop_application().await.unwrap();
    api.stop_application().await.unwrap();
    server.abort();
    drop((api, bot, control, migrated, install_db));
    tokio::fs::remove_dir_all(&root).await.unwrap();
}

async fn launcher_health_responds(addr: std::net::SocketAddr) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    tokio::time::timeout(Duration::from_millis(200), async {
        let Ok(mut stream) = tokio::net::TcpStream::connect(addr).await else {
            return false;
        };
        if stream
            .write_all(b"GET /health HTTP/1.0\r\n\r\n")
            .await
            .is_err()
        {
            return false;
        }
        let mut buf = [0u8; 64];
        let Ok(n) = stream.read(&mut buf).await else {
            return false;
        };
        std::str::from_utf8(&buf[..n]).is_ok_and(|text| text.starts_with("HTTP/1.1 200"))
    })
    .await
    .unwrap_or(false)
}

async fn assert_launcher_health(addr: std::net::SocketAddr, reason: &str) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for (path, status, body) in [
        ("/health", "HTTP/1.1 200 OK", "ok"),
        ("/ready", "HTTP/1.1 503 Service Unavailable", reason),
    ] {
        let response = tokio::time::timeout(Duration::from_secs(2), async {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            stream
                .write_all(format!("GET {path} HTTP/1.0\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).await.unwrap();
            response
        })
        .await
        .unwrap();
        assert!(response.starts_with(status), "{response}");
        assert_eq!(response.split_once("\r\n\r\n").unwrap().1, body);
    }
}

#[tokio::test]
async fn exited_bot_serves_health_while_tick_database_is_stalled() {
    let root = std::env::temp_dir().join(format!("health-stall-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let db = Db::open(&root.join("core.db"), 2).unwrap();
    let mut bot = offline_controller("bot", UpdateControl::from_application(&db), &root);
    let child = executable(&root, "health-child").await;
    let bind_request = root.join("bind-health-health-child");
    tokio::fs::write(
        &bind_request,
        std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)).to_string(),
    )
    .await
    .unwrap();
    bot.start(&child, &"b".repeat(40), None).await.unwrap();
    let bound = root.join("health-bound-health-child");
    tokio::time::timeout(POLL, async {
        while !bound.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child never bound health");
    let addr: std::net::SocketAddr = tokio::fs::read_to_string(&bound)
        .await
        .unwrap()
        .parse()
        .unwrap();
    bot.health_bind = Some(addr.to_string());
    tokio::fs::write(&bind_request, addr.to_string())
        .await
        .unwrap();
    bot.idle().await.unwrap();
    assert!(bot.health.is_none(), "live child owns health");
    let exit = root.join("exit-code-health-child");
    publish_exit_code(&exit, 1).await;
    tokio::time::timeout(POLL, bot.running.as_mut().unwrap().child.wait())
        .await
        .unwrap()
        .unwrap();

    // Occupy both real database connections. The production tick cannot finish
    // its first control read until these channels are released, even on panic.
    let mut releases = Vec::new();
    let mut blockers = Vec::new();
    for _ in 0..2 {
        let db = db.clone();
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let (entered, ready) = tokio::sync::oneshot::channel();
        releases.push(release);
        blockers.push(tokio::spawn(async move {
            db.with_conn(move |_| {
                let _ = entered.send(());
                let _ = wait.recv();
                Ok(())
            })
            .await
            .unwrap();
        }));
        ready.await.unwrap();
    }
    let tick = tokio::spawn(async move {
        let result = bot.tick().await;
        (bot, result)
    });
    tokio::time::timeout(POLL, async {
        loop {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_launcher_health(addr, "not ready").await;
    })
    .await
    .expect("health remained unbound behind the stalled database step");
    tokio::time::sleep(POLL).await;
    assert!(!tick.is_finished(), "database step must still be stalled");
    assert_launcher_health(addr, "not ready").await;
    drop(releases);
    for blocker in blockers {
        blocker.await.unwrap();
    }
    let (mut bot, result) = tokio::time::timeout(POLL, tick).await.unwrap().unwrap();
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("application exited")
    );
    assert_launcher_health(addr, "not ready").await;

    // Failed binds return to the poll loop without losing the next retry.
    bot.end_idle().await.unwrap();
    let occupied = tokio::net::TcpListener::bind(addr).await.unwrap();
    for _ in 0..2 {
        let error = tokio::time::timeout(POLL, bot.tick())
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::AddrInUse
        );
        assert!(bot.health.is_none());
    }
    drop(occupied);
    // Backoff has elapsed. A bad control row must not skip the restart, and the
    // live child keeps the health socket while that read fails.
    bot.restart.not_before = None;
    tokio::fs::remove_file(&exit).await.unwrap();
    tokio::fs::remove_file(&bound).await.unwrap();
    db.with_conn_sync(|conn| {
        let tx = conn.transaction_immediate()?;
        tx.execute_bounded(
            "INSERT OR REPLACE INTO meta(key,value) VALUES('component_upgrade_v1','invalid')",
            (),
            64 * 1024,
        )?;
        tx.commit()
    })
    .unwrap();
    let error = bot.tick().await.unwrap_err();
    assert!(error.to_string().contains("expected value"), "{error:#}");
    assert!(
        bot.running.is_some(),
        "control error must not skip the local restart"
    );
    assert!(bot.health.is_none(), "live child owns health");
    tokio::time::timeout(POLL, async {
        while !bound.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("restarted child never bound health");
    assert_eq!(
        tokio::net::TcpListener::bind(addr)
            .await
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AddrInUse
    );

    // A replacement must acquire the same socket after the launcher releases it.
    bot.stop_application().await.unwrap();
    assert_launcher_health(addr, "not ready").await;
    tokio::fs::remove_file(&bound).await.unwrap();
    bot.start(&child, &"b".repeat(40), None).await.unwrap();
    assert!(bot.health.is_none());
    tokio::time::timeout(POLL, async {
        while !bound.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("replacement never acquired health");
    assert_eq!(
        tokio::net::TcpListener::bind(addr)
            .await
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AddrInUse
    );
    bot.stop_application().await.unwrap();
    assert_launcher_health(addr, "not ready").await;
    bot.end_idle().await.unwrap();
    let released = tokio::net::TcpListener::bind(addr).await.unwrap();
    drop((released, bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn running_bot_exit_during_stalled_tick_restores_health() {
    let root =
        std::env::temp_dir().join(format!("health-stall-live-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let db = Db::open(&root.join("core.db"), 2).unwrap();
    let mut bot = offline_controller("bot", UpdateControl::from_application(&db), &root);
    let child = executable(&root, "health-live").await;
    let bind_request = root.join("bind-health-health-live");
    tokio::fs::write(
        &bind_request,
        std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)).to_string(),
    )
    .await
    .unwrap();
    bot.start(&child, &"b".repeat(40), None).await.unwrap();
    let bound = root.join("health-bound-health-live");
    tokio::time::timeout(POLL, async {
        while !bound.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child never bound health");
    let addr: std::net::SocketAddr = tokio::fs::read_to_string(&bound)
        .await
        .unwrap()
        .parse()
        .unwrap();
    bot.health_bind = Some(addr.to_string());
    tokio::fs::write(&bind_request, addr.to_string())
        .await
        .unwrap();
    bot.idle().await.unwrap();
    assert!(bot.health.is_none(), "live child owns health");

    // Hold every pool permit before the tick. One current-thread yield then
    // polls tick() through its live-child check and parks it on the database
    // acquire, before this test is allowed to exit the child.
    let mut releases = Vec::new();
    let mut blockers = Vec::new();
    for _ in 0..2 {
        let db = db.clone();
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let (entered, ready) = tokio::sync::oneshot::channel();
        releases.push(release);
        blockers.push(tokio::spawn(async move {
            db.with_conn(move |_| {
                let _ = entered.send(());
                let _ = wait.recv();
                Ok(())
            })
            .await
            .unwrap();
        }));
        ready.await.unwrap();
    }
    let tick = tokio::spawn(async move {
        let result = bot.tick().await;
        (bot, result)
    });
    tokio::task::yield_now().await;
    assert!(
        !tick.is_finished(),
        "database step must stall while the child is still running"
    );
    assert!(
        !launcher_health_responds(addr).await,
        "launcher must not own health before the live child exits"
    );
    publish_exit_code(root.join("exit-code-health-live"), 1).await;
    let recovered = tokio::time::timeout(POLL, async {
        loop {
            if launcher_health_responds(addr).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        recovered.is_ok(),
        "health remained unbound after the child exited during the stalled database step"
    );
    assert_launcher_health(addr, "not ready").await;
    assert!(
        !tick.is_finished(),
        "database step must still be stalled after health returns"
    );
    drop(releases);
    for blocker in blockers {
        blocker.await.unwrap();
    }
    let (mut bot, result) = tokio::time::timeout(POLL, tick).await.unwrap().unwrap();
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("application exited")
    );
    assert_launcher_health(addr, "not ready").await;
    bot.end_idle().await.unwrap();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

fn child_start_count(root: &std::path::Path, name: &str) -> usize {
    let Ok(text) = std::fs::read_to_string(root.join("events")) else {
        return 0;
    };
    text.lines()
        .filter(|line| *line == format!("{name}.start"))
        .count()
}

#[tokio::test(flavor = "current_thread")]
async fn rejected_token_exit_during_stalled_tick_keeps_hold_reason() {
    let root = std::env::temp_dir().join(format!(
        "health-stall-token-{:032x}",
        rand::random::<u128>()
    ));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let db = Db::open(&root.join("core.db"), 2).unwrap();
    let mut bot = offline_controller("bot", UpdateControl::from_application(&db), &root);
    let child = executable(&root, "health-token").await;
    let bind_request = root.join("bind-health-health-token");
    tokio::fs::write(&bind_request, "127.0.0.1:0")
        .await
        .unwrap();
    bot.start(&child, &"b".repeat(40), None).await.unwrap();
    let bound = root.join("health-bound-health-token");
    tokio::time::timeout(POLL, async {
        while !bound.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child never bound health");
    let addr: std::net::SocketAddr = tokio::fs::read_to_string(&bound)
        .await
        .unwrap()
        .parse()
        .unwrap();
    bot.health_bind = Some(addr.to_string());
    tokio::fs::write(&bind_request, addr.to_string())
        .await
        .unwrap();
    bot.idle().await.unwrap();
    assert!(bot.health.is_none());
    let mut releases = Vec::new();
    let mut blockers = Vec::new();
    for _ in 0..2 {
        let db = db.clone();
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let (entered, ready) = tokio::sync::oneshot::channel();
        releases.push(release);
        blockers.push(tokio::spawn(async move {
            db.with_conn(move |_| {
                let _ = entered.send(());
                let _ = wait.recv();
                Ok(())
            })
            .await
            .unwrap();
        }));
        ready.await.unwrap();
    }
    let tick = tokio::spawn(async move {
        let result = bot.tick().await;
        (bot, result)
    });
    tokio::task::yield_now().await;
    assert!(
        !tick.is_finished(),
        "database step must stall while the child is running"
    );
    publish_exit_code(
        root.join("exit-code-health-token"),
        jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE,
    )
    .await;
    tokio::time::timeout(POLL, async {
        loop {
            if launcher_health_responds(addr).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("health remained unbound after token rejection during the stalled step");
    assert_launcher_health(addr, "discord token rejected").await;
    assert!(!tick.is_finished());
    drop(releases);
    for blocker in blockers {
        blocker.await.unwrap();
    }
    let (mut bot, result) = tokio::time::timeout(POLL, tick).await.unwrap().unwrap();
    assert!(!result.unwrap());
    assert!(bot.restart.held);
    assert!(bot.running.is_none());
    assert_eq!(child_start_count(&root, "health-token"), 1);
    assert!(!bot.tick().await.unwrap());
    assert!(
        bot.running.is_none(),
        "token hold must not restart the child"
    );
    assert_eq!(child_start_count(&root, "health-token"), 1);
    assert_launcher_health(addr, "discord token rejected").await;
    bot.end_idle().await.unwrap();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

/// Exit 78 lands after the opening hold check and before `idle`. The token
/// listener has to be up while a control read is still stalled, and the child
/// must not be restarted.
#[tokio::test(flavor = "current_thread")]
async fn token_exit_before_idle_keeps_health_during_stalled_control() {
    let name = "health-token-idle";
    let root =
        std::env::temp_dir().join(format!("health-token-idle-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let db = Db::open(&root.join("core.db"), 2).unwrap();
    let mut bot = offline_controller("bot", UpdateControl::from_application(&db), &root);
    let child = executable(&root, name).await;
    let bind_request = root.join(format!("bind-health-{name}"));
    tokio::fs::write(&bind_request, "127.0.0.1:0")
        .await
        .unwrap();
    bot.start(&child, &"b".repeat(40), None).await.unwrap();
    let bound = root.join(format!("health-bound-{name}"));
    tokio::time::timeout(POLL, async {
        while !bound.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child never bound health");
    let addr: std::net::SocketAddr = tokio::fs::read_to_string(&bound)
        .await
        .unwrap()
        .parse()
        .unwrap();
    bot.health_bind = Some(addr.to_string());
    tokio::fs::write(&bind_request, addr.to_string())
        .await
        .unwrap();
    bot.idle().await.unwrap();
    assert!(bot.health.is_none(), "live child owns health");
    let pid = bot
        .running
        .as_ref()
        .unwrap()
        .child
        .id()
        .expect("live child pid");
    let (release_gate, gate) = tokio::sync::oneshot::channel();
    bot.before_idle_gate = Some(gate);
    let control = bot.control.clone();

    let mut releases = Vec::new();
    let mut blockers = Vec::new();
    for _ in 0..2 {
        let db = db.clone();
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let (entered, ready) = tokio::sync::oneshot::channel();
        releases.push(release);
        blockers.push(tokio::spawn(async move {
            db.with_conn(move |_| {
                let _ = entered.send(());
                let _ = wait.recv();
                Ok(())
            })
            .await
            .unwrap();
        }));
        ready.await.unwrap();
    }
    let tick = tokio::spawn(async move {
        let result = bot.tick().await;
        (bot, result)
    });
    tokio::task::yield_now().await;
    assert!(
        !tick.is_finished(),
        "tick must wait after the hold check while the child is still alive"
    );
    assert!(
        !launcher_health_responds(addr).await,
        "launcher must not own health before the token exit"
    );
    publish_exit_code(
        root.join(format!("exit-code-{name}")),
        jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE,
    )
    .await;
    tokio::time::timeout(POLL, async {
        loop {
            if child_is_zombie(pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child did not exit 78 before idle");
    let stalled = tokio::spawn(async move { control.component("bot").await });
    tokio::task::yield_now().await;
    assert!(
        !stalled.is_finished(),
        "control read must be stalled before idle runs"
    );
    release_gate.send(()).unwrap();
    tokio::time::timeout(POLL, async {
        loop {
            if launcher_health_responds(addr).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("health stayed down after exit 78 while the control read was stalled");
    assert!(
        !stalled.is_finished(),
        "control read must still be stalled after health returns"
    );
    assert_launcher_health(addr, "discord token rejected").await;
    let (mut bot, result) = tokio::time::timeout(POLL, tick).await.unwrap().unwrap();
    assert!(!result.unwrap());
    assert!(
        !stalled.is_finished(),
        "binding health must not wait for the stalled control read"
    );
    assert!(bot.restart.held);
    assert!(bot.running.is_none());
    assert!(!bot.exited_for_restart);
    assert_eq!(child_start_count(&root, name), 1);
    assert!(!bot.tick().await.unwrap());
    assert!(
        bot.running.is_none(),
        "token hold must not restart the child"
    );
    assert_eq!(child_start_count(&root, name), 1);
    assert_launcher_health(addr, "discord token rejected").await;
    drop(releases);
    for blocker in blockers {
        blocker.await.unwrap();
    }
    stalled.await.unwrap().unwrap();
    bot.end_idle().await.unwrap();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

/// Hold both pooled connections. The next supervisor read parks until `releases` is dropped.
async fn block_sqlite_pool(
    db: &Db,
) -> (
    Vec<std::sync::mpsc::Sender<()>>,
    Vec<tokio::task::JoinHandle<()>>,
) {
    let mut releases = Vec::new();
    let mut blockers = Vec::new();
    for _ in 0..2 {
        let db = db.clone();
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let (entered, ready) = tokio::sync::oneshot::channel();
        releases.push(release);
        blockers.push(tokio::spawn(async move {
            db.with_conn(move |_| {
                let _ = entered.send(());
                let _ = wait.recv();
                Ok(())
            })
            .await
            .unwrap();
        }));
        ready.await.unwrap();
    }
    (releases, blockers)
}

/// Exit 1 lands after `idle` saw the child alive and before the pre-control hold
/// check. That check reaps it. `/health` has to answer within one poll while the
/// SQLite pool is still blocked, and the exit is counted once for restart.
#[tokio::test(flavor = "current_thread")]
async fn ordinary_exit_after_idle_keeps_health_during_stalled_control() {
    let name = "health-ordinary-idle";
    let root = std::env::temp_dir().join(format!(
        "health-ordinary-idle-{:032x}",
        rand::random::<u128>()
    ));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let db = Db::open(&root.join("core.db"), 2).unwrap();
    let mut bot = offline_controller("bot", UpdateControl::from_application(&db), &root);
    let child = executable(&root, name).await;
    let bind_request = root.join(format!("bind-health-{name}"));
    tokio::fs::write(&bind_request, "127.0.0.1:0")
        .await
        .unwrap();
    bot.start(&child, &"b".repeat(40), None).await.unwrap();
    let bound = root.join(format!("health-bound-{name}"));
    tokio::time::timeout(POLL, async {
        while !bound.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child never bound health");
    let addr: std::net::SocketAddr = tokio::fs::read_to_string(&bound)
        .await
        .unwrap()
        .parse()
        .unwrap();
    bot.health_bind = Some(addr.to_string());
    tokio::fs::write(&bind_request, addr.to_string())
        .await
        .unwrap();
    bot.idle().await.unwrap();
    assert!(bot.health.is_none(), "live child owns health");
    let pid = bot
        .running
        .as_ref()
        .unwrap()
        .child
        .id()
        .expect("live child pid");
    let (release_gate, gate) = tokio::sync::oneshot::channel();
    bot.after_idle_gate = Some(gate);
    let (releases, blockers) = block_sqlite_pool(&db).await;
    let tick = tokio::spawn(async move {
        let result = bot.tick().await;
        (bot, result)
    });
    tokio::task::yield_now().await;
    assert!(
        !tick.is_finished(),
        "tick must wait after idle while the child is still alive"
    );
    assert!(
        !launcher_health_responds(addr).await,
        "launcher must not own health before the ordinary exit"
    );
    let exit = root.join(format!("exit-code-{name}"));
    publish_exit_code(&exit, 1).await;
    tokio::time::timeout(POLL, async {
        loop {
            if child_is_zombie(pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child did not exit 1 before the pre-control hold check");
    release_gate.send(()).unwrap();
    tokio::time::timeout(POLL, async {
        loop {
            if launcher_health_responds(addr).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("health stayed down after exit 1 while the database was blocked");
    assert!(
        !tick.is_finished(),
        "database must still be blocked after health returns"
    );
    assert_launcher_health(addr, "not ready").await;
    drop(releases);
    for blocker in blockers {
        blocker.await.unwrap();
    }
    let (mut bot, result) = tokio::time::timeout(POLL, tick).await.unwrap().unwrap();
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("application exited")
    );
    assert_launcher_health(addr, "not ready").await;
    assert!(bot.exited_for_restart);
    assert!(bot.restart.waiting(Instant::now()));
    assert_eq!(bot.restart.failures, 1);
    assert!(bot.running.is_none());
    assert_eq!(child_start_count(&root, name), 1);
    assert!(!bot.tick().await.unwrap());
    assert_eq!(
        child_start_count(&root, name),
        1,
        "backoff must hold the restart"
    );
    bot.restart.not_before = None;
    tokio::fs::remove_file(&exit).await.unwrap();
    tokio::fs::remove_file(&bound).await.unwrap();
    bot.tick().await.unwrap();
    assert_eq!(child_start_count(&root, name), 2);
    bot.stop_application().await.unwrap();
    bot.end_idle().await.unwrap();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

/// The child is still alive when the tick parks on a blocked SQLite pool, then
/// exits 1. `/health` has to answer within one poll before that read finishes.
#[tokio::test(flavor = "current_thread")]
async fn ordinary_exit_during_blocked_pool_restores_health() {
    let name = "health-ordinary-pool";
    let root = std::env::temp_dir().join(format!(
        "health-ordinary-pool-{:032x}",
        rand::random::<u128>()
    ));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let db = Db::open(&root.join("core.db"), 2).unwrap();
    let mut bot = offline_controller("bot", UpdateControl::from_application(&db), &root);
    let child = executable(&root, name).await;
    let bind_request = root.join(format!("bind-health-{name}"));
    tokio::fs::write(&bind_request, "127.0.0.1:0")
        .await
        .unwrap();
    bot.start(&child, &"b".repeat(40), None).await.unwrap();
    let bound = root.join(format!("health-bound-{name}"));
    tokio::time::timeout(POLL, async {
        while !bound.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child never bound health");
    let addr: std::net::SocketAddr = tokio::fs::read_to_string(&bound)
        .await
        .unwrap()
        .parse()
        .unwrap();
    bot.health_bind = Some(addr.to_string());
    tokio::fs::write(&bind_request, addr.to_string())
        .await
        .unwrap();
    bot.idle().await.unwrap();
    assert!(bot.health.is_none(), "live child owns health");
    let pid = bot
        .running
        .as_ref()
        .unwrap()
        .child
        .id()
        .expect("live child pid");
    let (releases, blockers) = block_sqlite_pool(&db).await;
    let tick = tokio::spawn(async move {
        let result = bot.tick().await;
        (bot, result)
    });
    tokio::task::yield_now().await;
    assert!(
        !tick.is_finished(),
        "database step must stall while the child is still running"
    );
    assert!(
        !child_is_zombie(pid),
        "child must still be alive when the database stalls"
    );
    assert!(
        !launcher_health_responds(addr).await,
        "launcher must not own health before the live child exits"
    );
    publish_exit_code(root.join(format!("exit-code-{name}")), 1).await;
    tokio::time::timeout(POLL, async {
        loop {
            if launcher_health_responds(addr).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("health stayed down after exit 1 while the database was blocked");
    assert!(
        !tick.is_finished(),
        "database must still be blocked after health returns"
    );
    assert_launcher_health(addr, "not ready").await;
    drop(releases);
    for blocker in blockers {
        blocker.await.unwrap();
    }
    let (mut bot, result) = tokio::time::timeout(POLL, tick).await.unwrap().unwrap();
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("application exited")
    );
    assert_launcher_health(addr, "not ready").await;
    assert!(bot.exited_for_restart);
    assert!(bot.restart.waiting(Instant::now()));
    assert_eq!(bot.restart.failures, 1);
    assert!(bot.running.is_none());
    assert_eq!(child_start_count(&root, name), 1);
    bot.end_idle().await.unwrap();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

fn child_is_zombie(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().next())
        == Some("Z")
}

#[tokio::test(flavor = "current_thread")]
async fn unsaved_failure_receipt_restarts_child_while_database_is_stalled() {
    let root =
        std::env::temp_dir().join(format!("receipt-restart-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let db = Db::open(&root.join("core.db"), 2).unwrap();
    let mut bot = offline_controller("bot", UpdateControl::from_application(&db), &root);
    let child = executable(&root, "health-receipt").await;
    let bind_request = root.join("bind-health-health-receipt");
    tokio::fs::write(&bind_request, "127.0.0.1:0")
        .await
        .unwrap();
    bot.start(&child, &"b".repeat(40), None).await.unwrap();
    let bound = root.join("health-bound-health-receipt");
    tokio::time::timeout(POLL, async {
        while !bound.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child never bound health");
    let addr: std::net::SocketAddr = tokio::fs::read_to_string(&bound)
        .await
        .unwrap()
        .parse()
        .unwrap();
    bot.health_bind = Some(addr.to_string());
    tokio::fs::write(&bind_request, addr.to_string())
        .await
        .unwrap();
    let exit = root.join("exit-code-health-receipt");
    publish_exit_code(&exit, 1).await;
    tokio::time::timeout(POLL, bot.running.as_mut().unwrap().child.wait())
        .await
        .unwrap()
        .unwrap();
    let error = bot.tick().await.unwrap_err();
    assert!(
        error.to_string().contains("application exited"),
        "{error:#}"
    );
    assert!(bot.exited_for_restart);
    assert!(bot.running.is_none());
    assert_launcher_health(addr, "not ready").await;
    bot.pending_failure = Some(PendingFailure {
        component: Some((
            release_at("bot", 'd', db::CORE_SCHEMA_VERSION),
            "component failed".into(),
        )),
        launcher: None,
    });
    bot.receipt_not_before = Some(Instant::now() + Duration::from_secs(60));
    assert!(
        !bot.tick().await.unwrap(),
        "crash backoff still holds the restart"
    );
    assert!(bot.running.is_none());
    assert_eq!(child_start_count(&root, "health-receipt"), 1);
    assert_launcher_health(addr, "not ready").await;

    bot.restart.not_before = None;
    tokio::fs::remove_file(&exit).await.unwrap();
    tokio::fs::remove_file(&bound).await.unwrap();
    let mut releases = Vec::new();
    let mut blockers = Vec::new();
    for _ in 0..2 {
        let db = db.clone();
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let (entered, ready) = tokio::sync::oneshot::channel();
        releases.push(release);
        blockers.push(tokio::spawn(async move {
            db.with_conn(move |_| {
                let _ = entered.send(());
                let _ = wait.recv();
                Ok(())
            })
            .await
            .unwrap();
        }));
        ready.await.unwrap();
    }
    let started = Instant::now();
    let tick = tokio::spawn(async move {
        let result = bot.tick().await;
        (bot, result)
    });
    tokio::time::timeout(POLL, async {
        while !bound.exists() {
            assert!(
                !tick.is_finished(),
                "database step finished before the child restarted"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child was not restarted while the receipt database call was stalled");
    assert!(started.elapsed() < POLL);
    assert!(!tick.is_finished());
    assert_eq!(
        tokio::net::TcpListener::bind(addr)
            .await
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AddrInUse
    );
    drop(releases);
    for blocker in blockers {
        blocker.await.unwrap();
    }
    let (mut bot, result) = tokio::time::timeout(POLL, tick).await.unwrap().unwrap();
    assert!(!result.unwrap());
    assert!(bot.running.as_ref().unwrap().child.id().is_some());
    assert!(bot.health.is_none(), "restarted child owns health");
    assert!(
        bot.pending_failure.is_some(),
        "the unsaved receipt stays pending for a later retry"
    );
    assert!(bot.receipt_not_before.is_some());
    assert!(!bot.tick().await.unwrap());
    assert_eq!(child_start_count(&root, "health-receipt"), 2);
    bot.stop_application().await.unwrap();
    bot.end_idle().await.unwrap();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn control_link_unavailable_receipt_restarts_without_waiting() {
    let root = std::env::temp_dir().join(format!("receipt-link-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let path = root.join("core.db");
    let db = Db::open(&path, 2).unwrap();
    drop(db);
    let opts = DbOpenOptions {
        read_only: true,
        path,
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: false,
        default_prefix: "!".into(),
        retention_days: 1,
    };
    let control = UpdateControl::open(&opts).unwrap().unwrap();
    assert!(!control.writes_locally());
    let mut bot = offline_controller("bot", control, &root);
    let child = executable(&root, "health-link").await;
    let listener = bind_outside_ephemeral_range().await;
    let addr = listener.local_addr().unwrap();
    drop(listener);
    bot.health_bind = Some(addr.to_string());
    tokio::fs::write(root.join("bind-health-health-link"), addr.to_string())
        .await
        .unwrap();
    bot.restart_target = Some(RestartTarget {
        path: child,
        source_sha: "b".repeat(40),
        release: None,
    });
    bot.exited_for_restart = true;
    bot.pending_failure = Some(PendingFailure {
        component: Some((
            release_at("bot", 'e', db::CORE_SCHEMA_VERSION),
            "component failed".into(),
        )),
        launcher: None,
    });
    bot.idle().await.unwrap();
    assert_launcher_health(addr, "not ready").await;
    let started = Instant::now();
    assert!(!bot.tick().await.unwrap());
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "receipt failure delayed restart by {:?}",
        started.elapsed()
    );
    assert!(bot.running.as_ref().unwrap().child.id().is_some());
    assert!(bot.health.is_none());
    assert!(bot.pending_failure.is_some());
    let bound = root.join("health-bound-health-link");
    tokio::time::timeout(POLL, async {
        while !bound.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("restarted child never bound health");
    assert_eq!(
        tokio::net::TcpListener::bind(addr)
            .await
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AddrInUse
    );
    assert!(!bot.tick().await.unwrap());
    assert_eq!(child_start_count(&root, "health-link"), 1);
    assert!(
        bot.pending_failure.is_some(),
        "control link failure stays pending"
    );
    bot.stop_application().await.unwrap();
    assert_launcher_health(addr, "not ready").await;
    bot.end_idle().await.unwrap();
    drop(bot);
    tokio::fs::remove_dir_all(root).await.unwrap();
}

fn poison_upgrade(db: &Db) {
    db.with_conn_sync(|conn| {
        let tx = conn.transaction_immediate()?;
        tx.execute_bounded(
            "INSERT OR REPLACE INTO meta(key,value) VALUES('component_upgrade_v1','invalid')",
            (),
            64 * 1024,
        )?;
        tx.commit()
    })
    .unwrap();
}

/// The child is still running when `tick` parks on the upgrade read, then exits
/// while that read is blocked. The row is already invalid, so the read fails
/// before `report_ready`.
async fn exit_during_invalid_upgrade(
    name: &str,
    code: &str,
) -> (Supervisor, Db, std::path::PathBuf, std::net::SocketAddr) {
    let root = std::env::temp_dir().join(format!(
        "health-ctrl-{name}-{:032x}",
        rand::random::<u128>()
    ));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let db = Db::open(&root.join("core.db"), 2).unwrap();
    poison_upgrade(&db);
    let mut bot = offline_controller("bot", UpdateControl::from_application(&db), &root);
    let child = executable(&root, name).await;
    let bind_request = root.join(format!("bind-health-{name}"));
    tokio::fs::write(
        &bind_request,
        std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)).to_string(),
    )
    .await
    .unwrap();
    bot.start(&child, &"b".repeat(40), None).await.unwrap();
    let bound = root.join(format!("health-bound-{name}"));
    tokio::time::timeout(POLL, async {
        while !bound.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child never bound health");
    let addr: std::net::SocketAddr = tokio::fs::read_to_string(&bound)
        .await
        .unwrap()
        .parse()
        .unwrap();
    bot.health_bind = Some(addr.to_string());
    tokio::fs::write(&bind_request, addr.to_string())
        .await
        .unwrap();
    bot.idle().await.unwrap();
    assert!(bot.health.is_none(), "live child owns health");
    let mut releases = Vec::new();
    let mut blockers = Vec::new();
    for _ in 0..2 {
        let db = db.clone();
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let (entered, ready) = tokio::sync::oneshot::channel();
        releases.push(release);
        blockers.push(tokio::spawn(async move {
            db.with_conn(move |_| {
                let _ = entered.send(());
                let _ = wait.recv();
                Ok(())
            })
            .await
            .unwrap();
        }));
        ready.await.unwrap();
    }
    let tick = tokio::spawn(async move {
        let result = bot.tick().await;
        (bot, result)
    });
    tokio::task::yield_now().await;
    assert!(
        !tick.is_finished(),
        "database step must stall while the child is still running"
    );
    assert!(
        !launcher_health_responds(addr).await,
        "launcher must not own health before the live child exits"
    );
    publish_exit_code(root.join(format!("exit-code-{name}")), code).await;
    tokio::time::timeout(POLL, async {
        loop {
            if launcher_health_responds(addr).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("health remained unbound after the child exited during the failed read");
    assert!(!tick.is_finished(), "database step must still be stalled");
    drop(releases);
    for blocker in blockers {
        blocker.await.unwrap();
    }
    let (bot, result) = tokio::time::timeout(POLL, tick).await.unwrap().unwrap();
    let error = result.unwrap_err();
    assert!(error.to_string().contains("expected value"), "{error:#}");
    assert_eq!(child_start_count(&root, name), 1);
    (bot, db, root, addr)
}

#[tokio::test(flavor = "current_thread")]
async fn control_error_after_exit_during_stall_still_restarts() {
    let name = "health-ctrl";
    let (mut bot, db, root, addr) = exit_during_invalid_upgrade(name, "1").await;
    assert!(bot.running.is_none());
    assert!(bot.exited_for_restart);
    assert!(bot.restart.waiting(Instant::now()));
    assert_launcher_health(addr, "not ready").await;
    // Backoff has elapsed. The same control error must not block the restart.
    bot.restart.not_before = None;
    tokio::fs::remove_file(root.join(format!("exit-code-{name}")))
        .await
        .unwrap();
    tokio::fs::remove_file(root.join(format!("health-bound-{name}")))
        .await
        .unwrap();
    let error = bot.tick().await.unwrap_err();
    assert!(error.to_string().contains("expected value"), "{error:#}");
    assert!(bot.running.as_ref().unwrap().child.id().is_some());
    // Spawn returns before the child writes its start line. Wait for that
    // event instead of assuming the process has been scheduled.
    tokio::time::timeout(POLL, async {
        while child_start_count(&root, name) < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("restarted child never published its start");
    assert_eq!(child_start_count(&root, name), 2);
    let bound = root.join(format!("health-bound-{name}"));
    tokio::time::timeout(POLL, async {
        while !bound.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("restarted child never bound health");
    assert!(bot.health.is_none(), "restarted child owns health");
    // A later failed read must leave that child running.
    let error = bot.tick().await.unwrap_err();
    assert!(error.to_string().contains("expected value"), "{error:#}");
    assert_eq!(child_start_count(&root, name), 2);
    assert!(bot.running.as_ref().unwrap().child.id().is_some());
    bot.stop_application().await.unwrap();
    assert_launcher_health(addr, "not ready").await;
    bot.end_idle().await.unwrap();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn control_error_after_token_rejection_does_not_restart() {
    let name = "health-ctrl-token";
    let (mut bot, db, root, addr) = exit_during_invalid_upgrade(
        name,
        &jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE.to_string(),
    )
    .await;
    assert!(bot.restart.held);
    assert!(!bot.exited_for_restart);
    assert!(bot.running.is_none());
    assert_launcher_health(addr, "discord token rejected").await;
    assert!(!bot.tick().await.unwrap());
    assert_eq!(child_start_count(&root, name), 1);
    assert!(bot.running.is_none());
    assert_launcher_health(addr, "discord token rejected").await;
    bot.end_idle().await.unwrap();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn standby_child_is_not_restarted_when_control_fails() {
    let name = "health-standby";
    let root = std::env::temp_dir().join(format!("health-standby-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let db = Db::open(&root.join("core.db"), 2).unwrap();
    poison_upgrade(&db);
    let mut bot = offline_controller("bot", UpdateControl::from_application(&db), &root);
    let child = executable(&root, name).await;
    let bind_request = root.join(format!("bind-health-{name}"));
    tokio::fs::write(&bind_request, "127.0.0.1:0")
        .await
        .unwrap();
    bot.start(&child, &"b".repeat(40), None).await.unwrap();
    let bound = root.join(format!("health-bound-{name}"));
    tokio::time::timeout(POLL, async {
        while !bound.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child never bound health");
    let addr: std::net::SocketAddr = tokio::fs::read_to_string(&bound)
        .await
        .unwrap()
        .parse()
        .unwrap();
    bot.health_bind = Some(addr.to_string());
    tokio::fs::write(&bind_request, addr.to_string())
        .await
        .unwrap();
    bot.standby = Some((child, "b".repeat(40), None));
    bot.disarm_child_health_watch().await;
    publish_exit_code(root.join(format!("exit-code-{name}")), 1).await;
    tokio::time::timeout(POLL, bot.running.as_mut().unwrap().child.wait())
        .await
        .unwrap()
        .unwrap();
    let error = bot.tick().await.unwrap_err();
    assert!(error.to_string().contains("expected value"), "{error:#}");
    assert!(
        bot.running.is_some(),
        "standby child stays recorded for rollback"
    );
    assert!(!bot.exited_for_restart);
    assert_eq!(child_start_count(&root, name), 1);
    assert_launcher_health(addr, "not ready").await;
    assert!(bot.tick().await.is_err());
    assert_eq!(child_start_count(&root, name), 1);
    bot.standby = None;
    bot.stop_application().await.unwrap();
    bot.end_idle().await.unwrap();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn failed_restart_keeps_launcher_health() {
    let root = std::env::temp_dir().join(format!(
        "health-restart-fail-{:032x}",
        rand::random::<u128>()
    ));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let db = Db::open(&root.join("core.db"), 2).unwrap();
    let mut bot = offline_controller("bot", UpdateControl::from_application(&db), &root);
    let child = executable(&root, "health-re").await;
    let listener = bind_outside_ephemeral_range().await;
    let addr = listener.local_addr().unwrap();
    bot.health = Some(tokio::spawn(
        jmodguard::health::serve_listener_as_with_reason(listener, "launcher", "not ready"),
    ));
    bot.health_bind = Some(addr.to_string());
    tokio::fs::write(root.join("bind-health-health-re"), addr.to_string())
        .await
        .unwrap();
    assert_launcher_health(addr, "not ready").await;
    bot.restart_target = Some(RestartTarget {
        path: root.join("missing-child"),
        source_sha: "b".repeat(40),
        release: None,
    });
    bot.exited_for_restart = true;
    let error = bot.tick().await.unwrap_err();
    assert!(error.to_string().contains("os error 2"), "{error:#}");
    assert!(bot.running.is_none());
    assert!(bot.exited_for_restart, "a failed spawn can be retried");
    assert_launcher_health(addr, "not ready").await;
    bot.restart_target.as_mut().unwrap().path = child;
    assert!(!bot.tick().await.unwrap());
    assert!(bot.running.as_ref().unwrap().child.id().is_some());
    assert_eq!(child_start_count(&root, "health-re"), 1);
    let bound = root.join("health-bound-health-re");
    tokio::time::timeout(POLL, async {
        while !bound.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("retried child never bound health");
    assert!(bot.health.is_none(), "retried child owns health");
    bot.stop_application().await.unwrap();
    assert_launcher_health(addr, "not ready").await;
    bot.end_idle().await.unwrap();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn child_exit_without_pidfd_still_binds_health() {
    let listener = bind_outside_ephemeral_range().await;
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = child.id();
    let (cancel, cancel_rx) = tokio::sync::oneshot::channel();
    let bound = std::sync::Arc::new(std::sync::Mutex::new(None));
    let slot = std::sync::Arc::clone(&bound);
    let watch = tokio::spawn(async move {
        watch_child_health_from(pid, addr.to_string(), cancel_rx, slot, false).await;
    });
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
    tokio::time::timeout(POLL, async {
        loop {
            if launcher_health_responds(addr).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("pid fallback did not bind health");
    assert_launcher_health(addr, "not ready").await;
    assert!(
        child.try_wait().unwrap().is_some(),
        "pid fallback must not reap"
    );
    assert!(matches!(
        child_liveness_pid(pid).unwrap(),
        ChildLiveness::Reaped
    ));
    let _ = cancel.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), watch).await;
    let serve = bound.lock().unwrap().take();
    if let Some(serve) = serve {
        serve.abort();
        let _ = serve.await;
    }
}

/// The supervisor can reap before the watcher's next poll. `Reaped` does not
/// bind, so the post-exit control read must restore /health before it stalls.
#[tokio::test(flavor = "current_thread")]
async fn supervisor_reap_binds_health_before_stalled_exit_control_read() {
    let name = "health-reap";
    let root = std::env::temp_dir().join(format!("health-reap-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let db = Db::open(&root.join("core.db"), 2).unwrap();
    let mut bot = offline_controller("bot", UpdateControl::from_application(&db), &root);
    let child = executable(&root, name).await;
    let reserved = bind_outside_ephemeral_range().await;
    let addr = reserved.local_addr().unwrap();
    drop(reserved);
    let bind_request = root.join(format!("bind-health-{name}"));
    tokio::fs::write(&bind_request, addr.to_string())
        .await
        .unwrap();
    bot.start(&child, &"b".repeat(40), None).await.unwrap();
    let bound = root.join(format!("health-bound-{name}"));
    tokio::time::timeout(POLL, async {
        while !bound.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child never bound health");
    bot.health_bind = Some(addr.to_string());
    bot.arm_child_health_watch();
    assert!(
        bot.child_watch.is_some(),
        "watcher must be armed while the child still owns the socket"
    );
    assert!(
        !launcher_health_responds(addr).await,
        "the child socket is not the launcher health server"
    );

    let pid = bot
        .running
        .as_ref()
        .unwrap()
        .child
        .id()
        .expect("live child pid");
    // Block this thread so the watcher cannot observe the exit before the reap.
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if bot
            .running
            .as_mut()
            .unwrap()
            .child
            .try_wait()
            .unwrap()
            .is_some()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "supervisor did not reap the killed child"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    tokio::time::timeout(POLL, async {
        loop {
            if bot
                .child_watch
                .as_ref()
                .unwrap()
                .task
                .as_ref()
                .unwrap()
                .is_finished()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("watcher did not exit after the supervisor reaped first");
    assert!(
        !launcher_health_responds(addr).await,
        "a reaped observation must not bind health"
    );

    let (release, gate) = tokio::sync::oneshot::channel();
    bot.exit_control_gate = Some(gate);
    let normal = tokio::spawn(async move {
        let result = bot.normal().await;
        (bot, result)
    });
    tokio::time::timeout(POLL, async {
        loop {
            if launcher_health_responds(addr).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("health stays unbound after normal() reaps the child and stalls on the error-path upgrade lookup");
    assert!(
        !normal.is_finished(),
        "post-exit control read must still be stalled"
    );
    release.send(()).unwrap();
    let (mut bot, result) = tokio::time::timeout(POLL, normal).await.unwrap().unwrap();
    let error = result.unwrap_err();
    assert!(
        error.to_string().contains("application exited"),
        "{error:#}"
    );
    assert_launcher_health(addr, "not ready").await;
    assert!(bot.running.is_none());
    assert!(bot.exited_for_restart);
    bot.end_idle().await.unwrap();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[test]
fn exit_code_publish_is_absent_or_complete() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;

    assert_eq!(complete_exit_code(""), None);
    assert_eq!(complete_exit_code("7"), None);
    assert_eq!(complete_exit_code("78"), None);
    assert_eq!(complete_exit_code("\n"), None);
    assert_eq!(complete_exit_code(" \n"), None);
    assert_eq!(complete_exit_code("7\n"), Some(7));
    assert_eq!(complete_exit_code("78\n"), Some(78));
    assert_eq!(complete_exit_code(" 78\n"), Some(78));
    assert_eq!(complete_exit_code("78\r\n"), Some(78));
    assert!(catch_unwind(AssertUnwindSafe(|| complete_exit_code("nope\n"))).is_err());

    let root = std::env::temp_dir().join(format!("exit-publish-{:032x}", rand::random::<u128>()));
    std::fs::create_dir_all(&root).unwrap();
    let exit = root.join("exit-code-stress");
    let empty = Arc::new(AtomicUsize::new(0));
    let seven = Arc::new(AtomicUsize::new(0));
    let bare = Arc::new(AtomicUsize::new(0));
    for _ in 0..2000 {
        let _ = std::fs::remove_file(&exit);
        let published = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let readers_sampled = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(5));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let exit = exit.clone();
                let barrier = Arc::clone(&barrier);
                let published = Arc::clone(&published);
                let readers_sampled = Arc::clone(&readers_sampled);
                let empty = Arc::clone(&empty);
                let seven = Arc::clone(&seven);
                let bare = Arc::clone(&bare);
                thread::spawn(move || {
                    barrier.wait();
                    let mut sampled = false;
                    while !published.load(Ordering::Acquire) {
                        if !sampled {
                            sampled = true;
                            readers_sampled.fetch_add(1, Ordering::Release);
                        }
                        if let Ok(bytes) = std::fs::read(&exit) {
                            if bytes.as_slice() == b"78\n" {
                                continue;
                            }
                            if bytes.is_empty() {
                                empty.fetch_add(1, Ordering::Relaxed);
                            } else if bytes.as_slice() == b"7" {
                                seven.fetch_add(1, Ordering::Relaxed);
                            } else if bytes.as_slice() == b"78" {
                                bare.fetch_add(1, Ordering::Relaxed);
                            } else {
                                panic!("unexpected read: {:?}", bytes);
                            }
                        }
                    }
                    let bytes = std::fs::read(&exit).unwrap();
                    assert_eq!(bytes, b"78\n");
                })
            })
            .collect();
        barrier.wait();
        while readers_sampled.load(Ordering::Acquire) < 4 {
            std::hint::spin_loop();
        }
        publish_exit_code_sync(&exit, 78);
        published.store(true, Ordering::Release);
        for handle in threads {
            handle.join().unwrap();
        }
    }
    assert_eq!(empty.load(Ordering::Relaxed), 0);
    assert_eq!(seven.load(Ordering::Relaxed), 0);
    assert_eq!(bare.load(Ordering::Relaxed), 0);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn exit_code_child_retries_empty_and_partial_records() {
    let root = std::env::temp_dir().join(format!("exit-record-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let name = "exit-record";
    let binary = executable(&root, name).await;
    let exit = root.join(format!("exit-code-{name}"));
    let ready = root.join("ready");
    let events = root.join("events");

    async fn spawn_child(binary: &Path, ready: &Path) -> tokio::process::Child {
        use std::process::Stdio;
        Command::new(binary)
            .env("JMODGUARD_READY_FILE", ready)
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    async fn stderr_has_parse_error(child: &mut tokio::process::Child) -> bool {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_end(&mut buf).await;
        }
        String::from_utf8_lossy(&buf).contains("ParseIntError")
    }

    tokio::fs::write(&exit, "").await.unwrap();
    let mut child = spawn_child(&binary, &ready).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(child.try_wait().unwrap().is_none());
    child.kill().await.unwrap();
    let _ = child.wait().await;
    assert!(!stderr_has_parse_error(&mut child).await);

    let _ = tokio::fs::remove_file(&events).await;
    tokio::fs::write(&exit, "7").await.unwrap();
    let hold = root.join("hold-exit-record-ready");
    tokio::fs::write(&hold, "hold").await.unwrap();
    let mut child = spawn_child(&binary, &ready).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(text) = tokio::fs::read_to_string(&events).await
                && text.lines().any(|line| line == "exit-record.exit-partial")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("fixture never observed the partial exit record");
    assert!(child.try_wait().unwrap().is_none());
    publish_exit_code_sync(&exit, 78);
    let status = tokio::time::timeout(Duration::from_secs(2), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.code(), Some(78));
    assert!(!stderr_has_parse_error(&mut child).await);
    let _ = tokio::fs::remove_file(&hold).await;

    for _ in 0..40 {
        tokio::fs::write(&exit, "").await.unwrap();
        let mut child = spawn_child(&binary, &ready).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        publish_exit_code_sync(&exit, 78);
        let status = child.wait().await.unwrap();
        assert_eq!(status.code(), Some(78));
        assert!(!stderr_has_parse_error(&mut child).await);
        let _ = tokio::fs::remove_file(&exit).await;
        let _ = tokio::fs::remove_file(&ready).await;
        let _ = tokio::fs::remove_file(&events).await;
    }

    tokio::fs::remove_dir_all(&root).await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn readiness_recovery_holds_token_exit_between_status_checks() {
    use std::future::Future as _;
    let root = std::env::temp_dir().join(format!("ready-race-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let db = Db::open(&root.join("core.db"), 2).unwrap();
    let mut bot = offline_controller("bot", UpdateControl::from_application(&db), &root);
    let name = "ready-race";
    let child = executable(&root, name).await;
    let hold = root.join(format!("hold-{name}-ready"));
    tokio::fs::write(&hold, "hold").await.unwrap();
    let candidate = image(&mut HashMap::new(), "bot", b"ready-race");
    bot.start(&child, &candidate.source_sha, Some(candidate.clone()))
        .await
        .unwrap();
    bot.running.as_mut().unwrap().started_at = now() - READY_TIMEOUT.as_secs() as i64 - 1;
    assert!(
        bot.report_ready()
            .await
            .unwrap_err()
            .to_string()
            .contains("readiness timed out")
    );
    let reserved = bind_outside_ephemeral_range().await;
    let addr = reserved.local_addr().unwrap();
    drop(reserved);
    bot.health_bind = Some(addr.to_string());
    bot.arm_child_health_watch();
    let pid = bot.running.as_ref().unwrap().child.id().unwrap();
    let (release, gate) = tokio::sync::oneshot::channel();
    bot.before_idle_gate = Some(gate);
    {
        let recovery = bot.bind_health_after_observed_exit();
        tokio::pin!(recovery);
        // Poll only recovery: the first status check sees a live child, then
        // the gate parks it without scheduling the watcher.
        assert!(
            std::future::poll_fn(|cx| { std::task::Poll::Ready(recovery.as_mut().poll(cx)) })
                .await
                .is_pending()
        );
        publish_exit_code_sync(&root.join(format!("exit-code-{name}")), 78);
        std::fs::remove_file(&hold).unwrap();
        let deadline = Instant::now() + POLL;
        while !child_is_zombie(pid) {
            assert!(Instant::now() < deadline, "fixture child did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
        release.send(()).unwrap();
        // The second check must reap/classify the exit before the watcher runs.
        tokio::time::timeout(POLL, recovery).await.unwrap().unwrap();
    }
    assert!(
        bot.restart.held,
        "second status check must establish the token hold"
    );
    assert!(bot.running.is_none());
    assert_launcher_health(addr, "discord token rejected").await;
    bot.end_idle().await.unwrap();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn readiness_recovery_keeps_unsaved_candidate_through_auth_retry() {
    let (root, db, mut bot, candidate, server) =
        independent_exiting_bot_prepared(78, |bot| bot.fail_rejected_candidate_write = true).await;
    bot.tick().await.unwrap();
    // Retry into a live, unready replacement with its original standby.
    let exit = root.join("exit-code-desired-bot");
    tokio::fs::remove_file(&exit).await.unwrap();
    let hold = root.join("hold-desired-bot-ready");
    tokio::fs::write(&hold, "hold").await.unwrap();
    bot.restart.auth_retry_at = Some(Instant::now());
    bot.tick().await.unwrap();
    assert!(bot.standby.is_some());
    assert!(
        bot.running
            .as_mut()
            .unwrap()
            .child
            .try_wait()
            .unwrap()
            .is_none()
    );
    bot.running.as_mut().unwrap().started_at = now() - READY_TIMEOUT.as_secs() as i64 - 1;
    let reserved = bind_reclaimable_health_port().await;
    let addr = reserved.local_addr().unwrap();
    drop(reserved);
    bot.health_bind = Some(addr.to_string());
    let pid = bot.running.as_ref().unwrap().child.id().unwrap();
    let (entered, reached) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    bot.readiness_error_gate = Some((entered, gate));
    let normal = tokio::spawn(async move {
        let result = bot.normal().await;
        (bot, result)
    });
    tokio::time::timeout(POLL, reached).await.unwrap().unwrap();
    publish_exit_code(&exit, 78).await;
    tokio::fs::remove_file(&hold).await.unwrap();
    tokio::time::timeout(POLL, async {
        while !child_is_zombie(pid) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    release.send(()).unwrap();
    let (mut bot, result) = tokio::time::timeout(POLL, normal).await.unwrap().unwrap();
    assert!(bot.restart.held);
    assert!(
        bot.standby.is_some(),
        "token hold must not consume the standby"
    );
    assert!(!result.unwrap());
    assert!(bot.running.is_none());
    assert_eq!(bot.rejected_candidate.as_ref(), Some(&candidate));
    assert!(
        bot.control
            .rejected_candidate_digest("bot")
            .await
            .unwrap()
            .is_none()
    );
    assert_launcher_health(addr, "discord token rejected").await;

    // Marker writes still fail: retry must use retained candidate/standby,
    // not restore the installed or baked release.
    tokio::fs::remove_file(&exit).await.unwrap();
    bot.restart.auth_retry_at = Some(Instant::now());
    bot.tick().await.unwrap();
    assert_eq!(
        bot.running.as_ref().unwrap().release.as_ref(),
        Some(&candidate)
    );
    wait_for_bot_ready(&mut bot).await;
    assert_eq!(
        independent_exit_starts(&root).await,
        [
            "desired-bot.start",
            "desired-bot.start",
            "desired-bot.start",
        ]
    );
    bot.stop_application().await.unwrap();
    bot.end_idle().await.unwrap();
    server.abort();
    let _ = server.await;
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn readiness_error_then_token_exit_binds_health_before_control_read() {
    readiness_error_then_token_exit(false, false).await;
}

#[tokio::test]
async fn readiness_error_then_token_exit_binds_health_before_candidate_write() {
    readiness_error_then_token_exit(true, false).await;
}

#[tokio::test]
async fn readiness_error_then_token_exit_retries_failed_health_bind_before_candidate_write() {
    readiness_error_then_token_exit(true, true).await;
}

async fn readiness_error_then_token_exit(block_candidate_write: bool, fail_first_bind: bool) {
    let root = std::env::temp_dir().join(format!("ready-token-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let db = Db::open(&root.join("core.db"), 2).unwrap();
    let mut bot = offline_controller("bot", UpdateControl::from_application(&db), &root);
    let name = "ready-token";
    let child = executable(&root, name).await;
    tokio::fs::write(root.join(format!("hold-{name}-ready")), "hold")
        .await
        .unwrap();
    let release = image(&mut HashMap::new(), "bot", b"readiness-timeout");
    bot.start(&child, &release.source_sha, Some(release.clone()))
        .await
        .unwrap();
    bot.running.as_mut().unwrap().started_at = now() - READY_TIMEOUT.as_secs() as i64 - 1;
    let error = bot.report_ready().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("replacement child readiness timed out"),
        "{error:#}"
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
    assert!(!bot.restart.held);

    // Reproduce the handoff in normal(): readiness has failed, then exit 78
    // arrives before health recovery and the following control read.
    publish_exit_code(root.join(format!("exit-code-{name}")), 78).await;
    // The fixture waits for readiness before checking its requested exit.
    tokio::fs::remove_file(root.join(format!("hold-{name}-ready")))
        .await
        .unwrap();
    let status = tokio::time::timeout(POLL, bot.running.as_mut().unwrap().child.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.code(), Some(78));
    let reserved = bind_outside_ephemeral_range().await;
    let addr = reserved.local_addr().unwrap();
    bot.health_bind = Some(addr.to_string());
    let mut first_retry_at = None;
    if block_candidate_write {
        bot.standby = Some((child, bot.base_sha.clone(), None));
        let (releases, blockers) = block_sqlite_pool(&db).await;
        if fail_first_bind {
            // Keep the port occupied and the database blocked. Failed health
            // recovery must return so another attempt can run once it is free.
            let recovery = tokio::time::timeout(POLL, bot.bind_health_after_observed_exit()).await;
            assert!(
                recovery.is_ok(),
                "failed health bind must return before rejected-candidate persistence"
            );
            let error = recovery.unwrap().unwrap_err();
            assert!(
                error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::AddrInUse),
                "{error:#}"
            );
            assert!(bot.restart.held);
            assert!(bot.health.is_none());
            assert_eq!(
                bot.running.as_ref().unwrap().release.as_ref(),
                Some(&release)
            );
            assert_eq!(bot.rejected_candidate.as_ref(), Some(&release));
            assert!(bot.persisted_candidate_digest.is_none());
            assert_eq!(bot.restart.auth_failures, 1);
            first_retry_at = bot.restart.auth_retry_at;
        }
        drop(reserved);
        let recovery = tokio::spawn(async move {
            bot.bind_health_after_observed_exit().await.unwrap();
            bot
        });
        let healthy = tokio::time::timeout(POLL, async {
            while !launcher_health_responds(addr).await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        let still_blocked = !recovery.is_finished();
        if healthy.is_ok() {
            assert_launcher_health(addr, "discord token rejected").await;
        }
        drop(releases);
        for blocker in blockers {
            blocker.await.unwrap();
        }
        bot = tokio::time::timeout(POLL, recovery).await.unwrap().unwrap();
        assert!(
            healthy.is_ok(),
            "health must bind before rejected-candidate persistence"
        );
        assert!(still_blocked, "candidate persistence must still be stalled");
        assert_eq!(bot.rejected_candidate.as_ref(), Some(&release));
        assert_eq!(
            bot.control.rejected_candidate_digest("bot").await.unwrap(),
            Some(release.digest)
        );
    } else {
        drop(reserved);
        bot.bind_health_after_observed_exit().await.unwrap();
    }

    // Inspect the boundary before allowing any control read to run.
    assert!(bot.restart.held, "token hold must precede the control read");
    assert_eq!(
        bot.restart.auth_failures, 1,
        "count each rejected child once"
    );
    if let Some(first_retry_at) = first_retry_at {
        assert_eq!(bot.restart.auth_retry_at, Some(first_retry_at));
    }
    assert!(bot.running.is_none());
    assert_launcher_health(addr, "discord token rejected").await;
    bot.end_idle().await.unwrap();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

/// `report_ready` already counted this exit. A later control error must not
/// make `settle_exited_child` count it again and stretch the backoff.
#[tokio::test]
async fn settled_exit_keeps_the_backoff_report_ready_already_recorded() {
    let root = std::env::temp_dir().join(format!("health-once-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let db = Db::open(&root.join("core.db"), 2).unwrap();
    let mut bot = offline_controller("bot", UpdateControl::from_application(&db), &root);
    let child = root.join("exit-child");
    tokio::fs::write(&child, b"#!/bin/sh\nexit 1\n")
        .await
        .unwrap();
    std::fs::set_permissions(
        &child,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
    )
    .unwrap();
    bot.start(&child, &"b".repeat(40), None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if bot
                .running
                .as_mut()
                .unwrap()
                .child
                .try_wait()
                .unwrap()
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child did not exit");
    let error = bot.report_ready().await.unwrap_err();
    assert!(
        error.to_string().contains("application exited"),
        "{error:#}"
    );
    assert_eq!(bot.restart.failures, 1);
    assert!(bot.exit_accounted);
    let not_before = bot.restart.not_before;
    bot.settle_exited_child().await.unwrap();
    assert_eq!(
        bot.restart.failures, 1,
        "settle must not count the exit again"
    );
    assert_eq!(bot.restart.not_before, not_before);
    assert!(!bot.exit_accounted);
    assert!(bot.running.is_none());
    assert!(bot.exited_for_restart);
    bot.end_idle().await.unwrap();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[test]
fn child_liveness_observes_exit_without_reaping() {
    use std::os::fd::AsRawFd;
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = child.id();
    let pidfd = open_pidfd(pid).unwrap();
    assert!(matches!(
        child_liveness(pidfd.as_raw_fd()).unwrap(),
        ChildLiveness::Running
    ));
    assert!(child.try_wait().unwrap().is_none());
    let rc = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    assert_eq!(rc, 0);
    let deadline = Instant::now() + Duration::from_secs(2);
    let code = loop {
        match child_liveness(pidfd.as_raw_fd()).unwrap() {
            ChildLiveness::Exited(code) => break code,
            ChildLiveness::Running if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            other => panic!("unexpected liveness after kill: {other:?}"),
        }
    };
    assert!(code.is_none(), "signal death is not an exit code");
    assert!(
        child.try_wait().unwrap().is_some(),
        "supervisor can still reap"
    );
    assert!(matches!(
        child_liveness(pidfd.as_raw_fd()).unwrap(),
        ChildLiveness::Reaped
    ));
}

struct PortScan {
    origin: std::sync::OnceLock<u32>,
    cursor: std::sync::atomic::AtomicU32,
}

static OUTSIDE_EPHEMERAL_SCAN: PortScan = PortScan {
    origin: std::sync::OnceLock::new(),
    cursor: std::sync::atomic::AtomicU32::new(0),
};
static RECLAIMABLE_SCAN: PortScan = PortScan {
    origin: std::sync::OnceLock::new(),
    cursor: std::sync::atomic::AtomicU32::new(0),
};

struct Reservation {
    port: u16,
    pid: u32,
    start: u64,
}

/// `bind(0)` draws from the ephemeral range. A fixture that closes a port and
/// binds that same address later races every other `bind(0)`. Candidates in
/// that range, or below the unprivileged bind floor, are skipped.
///
/// Each process starts at a random offset and walks every slot in the span
/// once. A port is claimed in this user's private reservation directory
/// before the bind. The claim outlives the socket, so another live process
/// of the same user skips a port this process already handed out and then
/// closed. A pid that is gone is dropped. A pid whose `/proc` entry cannot
/// be read is kept. A bind that loses to some other socket releases the
/// claim and tries the next port.
///
/// Registry updates are a synced snapshot renamed into place. The lock is a
/// different file, so a writer killed before the rename cannot erase another
/// live owner's claim or replace the lock inode. The directory is mode 0700,
/// owned by this user, and neither it nor the lock or claims entries may be
/// a symlink.
async fn bind_outside_ephemeral_range() -> tokio::net::TcpListener {
    bind_scanned(20_000, 10_000, &OUTSIDE_EPHEMERAL_SCAN).await
}

/// Same allocator as [`bind_outside_ephemeral_range`], in a disjoint range.
/// Rebinding a port just closed in the other range must not take this one
/// while a test is between close and the later bind.
async fn bind_reclaimable_health_port() -> tokio::net::TcpListener {
    bind_scanned(31_000, 1_000, &RECLAIMABLE_SCAN).await
}

fn scan_origin(scan: &'static PortScan) -> u32 {
    // The closed-port regression sets this so both children walk one sequence.
    // Ordinary fixture allocation leaves it unset and keeps a random origin.
    if let Ok(raw) = std::env::var("JMODGUARD_PORT_SCAN_ORIGIN")
        && let Ok(origin) = raw.parse::<u32>()
    {
        return origin;
    }
    *scan
        .origin
        .get_or_init(|| rand::random::<u32>() ^ std::process::id())
}

/// `origin % span + step` stays inside `span` values. Adding before the modulo
/// repeats slots once `origin` wraps through `u32::MAX`.
fn scan_slot(origin: u32, step: u32, span: u32) -> u32 {
    (origin % span + step) % span
}

#[derive(Clone, Copy)]
struct PortPolicy {
    ephemeral_low: u16,
    ephemeral_high: u16,
    unprivileged_start: u16,
    root: bool,
}

fn port_policy() -> PortPolicy {
    static POLICY: std::sync::OnceLock<PortPolicy> = std::sync::OnceLock::new();
    *POLICY.get_or_init(|| {
        let text = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
            .unwrap_or_else(|_| panic!("cannot read the ephemeral port range"));
        let mut parts = text.split_whitespace();
        let ephemeral_low = parts
            .next()
            .and_then(|part| part.parse().ok())
            .unwrap_or_else(|| panic!("cannot parse the ephemeral port range"));
        let ephemeral_high = parts
            .next()
            .and_then(|part| part.parse().ok())
            .unwrap_or_else(|| panic!("cannot parse the ephemeral port range"));
        let unprivileged_start =
            std::fs::read_to_string("/proc/sys/net/ipv4/ip_unprivileged_port_start")
                .ok()
                .and_then(|text| text.split_whitespace().next()?.parse().ok())
                .unwrap_or(1024);
        PortPolicy {
            ephemeral_low,
            ephemeral_high,
            unprivileged_start,
            root: unsafe { libc::geteuid() } == 0,
        }
    })
}

fn port_is_selectable(port: u16, policy: PortPolicy) -> bool {
    let outside_ephemeral = port < policy.ephemeral_low || port > policy.ephemeral_high;
    let bindable = policy.root || port >= policy.unprivileged_start;
    outside_ephemeral && bindable
}

async fn bind_scanned(first: u16, span: u16, scan: &'static PortScan) -> tokio::net::TcpListener {
    let policy = port_policy();
    let origin = scan_origin(scan);
    let span = u32::from(span);
    let mut selectable = 0u32;
    for _ in 0..span {
        let step = scan
            .cursor
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if step >= span {
            break;
        }
        let port = first + scan_slot(origin, step, span) as u16;
        if !port_is_selectable(port, policy) {
            continue;
        }
        selectable += 1;
        if !claim_port(port) {
            continue;
        }
        match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
            Ok(listener) => return listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                release_port(port);
                panic!(
                    "loopback port {port} is not bindable by this user (ip_unprivileged_port_start={})",
                    policy.unprivileged_start
                );
            }
            Err(_) => release_port(port),
        }
    }
    if selectable == 0 {
        panic!(
            "no bindable port outside the ephemeral range in {first}..{}; ephemeral {}-{}, unprivileged start {}",
            u32::from(first) + span,
            policy.ephemeral_low,
            policy.ephemeral_high,
            policy.unprivileged_start
        );
    }
    panic!("no free loopback port outside the ephemeral range");
}

fn our_uid() -> u32 {
    unsafe { libc::geteuid() }
}

fn reservation_dir_for_bind() -> std::path::PathBuf {
    std::path::PathBuf::from(format!(
        "/tmp/jmodguard-updater-port-reservations-{}",
        our_uid()
    ))
}

fn reservation_dir() -> std::path::PathBuf {
    match std::env::var("JMODGUARD_PORT_RESERVATION_DIR") {
        Ok(value) if !value.is_empty() => std::path::PathBuf::from(value),
        _ => reservation_dir_for_bind(),
    }
}

fn reservation_metadata_trust(meta: &std::fs::Metadata, uid: u32) -> Result<(), &'static str> {
    use std::os::unix::fs::MetadataExt;
    if meta.file_type().is_symlink() {
        return Err("port reservation path must not be a symlink");
    }
    if !meta.is_dir() {
        return Err("port reservation path is not a directory");
    }
    if meta.uid() != uid {
        return Err("port reservation directory is owned by another user");
    }
    Ok(())
}

fn prepare_reservation_dir_for(dir: &std::path::Path, uid: u32) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    match std::fs::symlink_metadata(dir) {
        Ok(meta) => reservation_metadata_trust(&meta, uid).map_err(str::to_string)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut bytes = dir.as_os_str().as_bytes().to_vec();
            if bytes.contains(&0) {
                return Err("port reservation path is not a usable directory name".into());
            }
            bytes.push(0);
            let rc = unsafe { libc::mkdir(bytes.as_ptr().cast(), 0o700) };
            if rc != 0 {
                let create_error = std::io::Error::last_os_error();
                if create_error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(format!(
                        "cannot create the port reservation directory: {create_error}"
                    ));
                }
            }
            let meta = std::fs::symlink_metadata(dir).map_err(|error| {
                format!("cannot inspect the port reservation directory: {error}")
            })?;
            reservation_metadata_trust(&meta, uid).map_err(str::to_string)?;
        }
        Err(error) => {
            return Err(format!(
                "cannot inspect the port reservation directory: {error}"
            ));
        }
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(dir)
        .map_err(|error| {
            if error.raw_os_error() == Some(libc::ELOOP) {
                "port reservation path must not be a symlink".to_string()
            } else {
                format!("cannot open the port reservation directory: {error}")
            }
        })?;
    let meta = file
        .metadata()
        .map_err(|error| format!("cannot inspect the port reservation directory: {error}"))?;
    if meta.uid() != uid || !meta.is_dir() {
        return Err("port reservation directory is owned by another user".into());
    }
    if meta.permissions().mode() & 0o777 != 0o700 {
        let rc = unsafe { libc::fchmod(file.as_raw_fd(), 0o700) };
        if rc != 0 {
            return Err(format!(
                "cannot set the port reservation directory mode: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

fn prepare_reservation_dir(dir: &std::path::Path) {
    prepare_reservation_dir_for(dir, our_uid()).unwrap_or_else(|error| panic!("{error}"));
}

fn open_reservation_lock(path: &std::path::Path) -> Result<std::fs::File, String> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| {
            if error.raw_os_error() == Some(libc::ELOOP) {
                "port reservation lock is a symlink".to_string()
            } else {
                format!("cannot open the port reservation lock: {error}")
            }
        })?;
    if file.metadata().map_err(|error| error.to_string())?.uid() != our_uid() {
        return Err("port reservation lock is owned by another user".into());
    }
    Ok(file)
}

fn lock_reservations(file: &std::fs::File) {
    lock_reservations_until(file, Instant::now() + Duration::from_secs(30))
        .unwrap_or_else(|_| panic!("timed out waiting for the port reservation lock"));
}

fn lock_reservations_until(file: &std::fs::File, deadline: Instant) -> Result<(), ()> {
    use std::os::fd::AsRawFd;
    loop {
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        let blocked = error.raw_os_error() == Some(libc::EAGAIN)
            || error.raw_os_error() == Some(libc::EWOULDBLOCK);
        if !blocked {
            panic!("port reservation lock failed: {error}");
        }
        if Instant::now() >= deadline {
            return Err(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn unlock_reservations(file: &std::fs::File) {
    use std::os::fd::AsRawFd;
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
}

#[derive(Debug)]
enum ProcLookup {
    Gone,
    Unreadable,
}

fn process_start(pid: u32) -> Result<u64, ProcLookup> {
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ProcLookup::Gone);
        }
        Err(_) => return Err(ProcLookup::Unreadable),
    };
    let Some(rest) = stat.rsplit_once(')') else {
        return Err(ProcLookup::Unreadable);
    };
    rest.1
        .split_whitespace()
        .nth(19)
        .and_then(|field| field.parse().ok())
        .ok_or(ProcLookup::Unreadable)
}

fn owner_still_reserved(observed: Result<u64, ProcLookup>, recorded: u64) -> bool {
    match observed {
        Ok(start) => start == recorded,
        Err(ProcLookup::Gone) => false,
        Err(ProcLookup::Unreadable) => true,
    }
}

fn our_start() -> u64 {
    static START: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *START.get_or_init(|| process_start(std::process::id()).unwrap_or(0))
}

fn read_reservation_claims(dir: &std::path::Path) -> Result<String, String> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let path = dir.join("claims");
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            return Err("port reservation claims entry is a symlink".into());
        }
        Err(error) => return Err(format!("cannot read the port reservation file: {error}")),
    };
    if file.metadata().map_err(|error| error.to_string())?.uid() != our_uid() {
        return Err("port reservation claims file is owned by another user".into());
    }
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|error| format!("cannot read the port reservation file: {error}"))?;
    Ok(text)
}

fn load_reservations(dir: &std::path::Path) -> Vec<Reservation> {
    let text = read_reservation_claims(dir).unwrap_or_else(|error| panic!("{error}"));
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let port = parts.next()?.parse().ok()?;
            let pid = parts.next()?.parse().ok()?;
            let start = parts.next()?.parse().ok()?;
            if !owner_still_reserved(process_start(pid), start) {
                return None;
            }
            Some(Reservation { port, pid, start })
        })
        .collect()
}

fn reservation_text(rows: &[Reservation]) -> String {
    let mut text = String::new();
    for row in rows {
        text.push_str(&format!("{} {} {}\n", row.port, row.pid, row.start));
    }
    text
}

fn store_reservations(dir: &std::path::Path, rows: &[Reservation]) {
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let snapshot = dir.join(format!(
        ".claims.{}.{}.tmp",
        std::process::id(),
        rand::random::<u64>()
    ));
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&snapshot)
            .unwrap_or_else(|error| panic!("cannot create the port reservation snapshot: {error}"));
        if file.metadata().map(|meta| meta.uid()).ok() != Some(our_uid()) {
            panic!("port reservation snapshot is owned by another user");
        }
        file.write_all(reservation_text(rows).as_bytes()).unwrap();
        file.sync_all()
            .unwrap_or_else(|_| panic!("cannot sync the port reservation snapshot"));
    }
    // The interrupted-rewrite regression sets this after the snapshot is durable
    // and kills the writer before rename. The claims file and the lock stay put.
    if std::env::var_os("JMODGUARD_PORT_RESERVATION_CRASH_BEFORE_RENAME").is_some() {
        unsafe { libc::raise(libc::SIGKILL) };
    }
    std::fs::rename(&snapshot, dir.join("claims"))
        .unwrap_or_else(|_| panic!("cannot publish the port reservation snapshot"));
    let directory = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(dir)
            .unwrap_or_else(|error| panic!("cannot open the port reservation directory: {error}"))
    };
    directory
        .sync_all()
        .unwrap_or_else(|_| panic!("cannot sync the port reservation directory"));
}

fn with_reservations_in<T>(dir: &std::path::Path, body: impl FnOnce(&std::path::Path) -> T) -> T {
    // flock is per process on Linux, so threads in this process also need a mutex.
    static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _gate = GATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    prepare_reservation_dir(dir);
    let lock = open_reservation_lock(&dir.join("lock")).unwrap_or_else(|error| panic!("{error}"));
    lock_reservations(&lock);
    let result = body(dir);
    unlock_reservations(&lock);
    result
}

fn with_reservations<T>(body: impl FnOnce(&std::path::Path) -> T) -> T {
    with_reservations_in(&reservation_dir_for_bind(), body)
}

fn claim_port(port: u16) -> bool {
    claim_port_in(&reservation_dir_for_bind(), port)
}

fn claim_port_in(dir: &std::path::Path, port: u16) -> bool {
    with_reservations_in(dir, |dir| {
        let mut rows = load_reservations(dir);
        let free = rows.iter().all(|row| row.port != port);
        if free {
            rows.push(Reservation {
                port,
                pid: std::process::id(),
                start: our_start(),
            });
        }
        store_reservations(dir, &rows);
        free
    })
}

fn release_port(port: u16) {
    with_reservations(|dir| {
        let pid = std::process::id();
        let start = our_start();
        let mut rows = load_reservations(dir);
        rows.retain(|row| !(row.port == port && row.pid == pid && row.start == start));
        store_reservations(dir, &rows);
    });
}

/// A second process is killed after its replacement snapshot is synced and
/// before that snapshot is renamed. The unrelated live owner's claim remains,
/// and the lock file keeps the inode the other process was waiting on.
#[test]
fn interrupted_reservation_rewrite_keeps_a_live_owners_claim() {
    if std::env::var_os("JMODGUARD_PORT_RESERVATION_CRASH_BEFORE_RENAME").is_some() {
        crash_before_reservation_rename();
        return;
    }

    let dir = std::env::temp_dir().join(format!(
        "port-reservation-crash-{:032x}",
        rand::random::<u128>()
    ));
    let _cleanup = RemoveDir(dir.clone());
    std::fs::create_dir_all(&dir).unwrap();
    let owner = KillChildren(vec![
        std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap(),
    ]);
    let pid = owner.0[0].id();
    let start = process_start(pid).expect("live reservation owner");
    let port = 25_001u16;
    with_reservations_in(&dir, |dir| {
        store_reservations(dir, &[Reservation { port, pid, start }]);
    });
    let claims = dir.join("claims");
    let before = std::fs::read(&claims).unwrap();
    use std::os::unix::fs::MetadataExt;
    let lock_inode = std::fs::metadata(dir.join("lock")).unwrap().ino();
    let claims_inode = std::fs::metadata(&claims).unwrap().ino();

    let mut writer = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "controller_tests::interrupted_reservation_rewrite_keeps_a_live_owners_claim",
            "--exact",
            "--test-threads=1",
        ])
        .env("JMODGUARD_PORT_RESERVATION_CRASH_BEFORE_RENAME", "1")
        .env("JMODGUARD_PORT_RESERVATION_DIR", &dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap();
    let status = wait_for_exit(&mut writer);
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        status.signal(),
        Some(libc::SIGKILL),
        "writer finished without being killed before rename"
    );

    assert_eq!(
        std::fs::read(&claims).unwrap(),
        before,
        "killing the writer replaced a live owner's claim"
    );
    assert_eq!(std::fs::metadata(&claims).unwrap().ino(), claims_inode);
    assert_eq!(
        std::fs::metadata(dir.join("lock")).unwrap().ino(),
        lock_inode
    );
    let snapshots = unpublished_snapshots(&dir);
    assert_eq!(snapshots.len(), 1, "crash left no unpublished snapshot");
    let snapshot = std::fs::read_to_string(&snapshots[0]).unwrap();
    assert!(
        snapshot.contains("25002 "),
        "unpublished snapshot did not contain the replacement"
    );
    assert!(
        !snapshot.contains(&format!("{port} {pid} {start}")),
        "unpublished snapshot still carried the live claim"
    );
    let loaded = with_reservations_in(&dir, load_reservations);
    assert!(
        loaded
            .iter()
            .any(|row| row.port == port && row.pid == pid && row.start == start),
        "live owner disappeared from the registry"
    );
    assert!(
        !claim_port_in(&dir, port),
        "live owner's reserved port was handed out after the crashed rewrite"
    );
    assert_eq!(
        std::fs::metadata(dir.join("lock")).unwrap().ino(),
        lock_inode
    );
    drop(owner);
}

fn crash_before_reservation_rename() {
    let dir = reservation_dir();
    with_reservations_in(&dir, |dir| {
        store_reservations(
            dir,
            &[Reservation {
                port: 25_002,
                pid: std::process::id(),
                start: our_start(),
            }],
        );
    });
    panic!("reservation writer renamed the registry after the crash point");
}

fn unpublished_snapshots(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if name.starts_with(".claims.") && name.ends_with(".tmp") {
            paths.push(path);
        }
    }
    paths.sort();
    paths
}

struct RemoveDir(std::path::PathBuf);

impl Drop for RemoveDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Many threads, and two processes, must not be handed the same port.
/// A second process also allocates after the first has closed its sockets:
/// the fixed 20000 counter used to bind those same ports again.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_port_allocations_never_return_the_same_port() {
    if let Ok(role) = std::env::var("JMODGUARD_PORT_ALLOC_CHILD") {
        let dir = std::path::PathBuf::from(
            std::env::var("JMODGUARD_PORT_ALLOC_DIR").expect("child directory"),
        );
        match role.as_str() {
            "hold" => hold_allocated_ports(&dir).await,
            "gap-owner" => gap_owner(&dir).await,
            "gap-other" => gap_other(&dir).await,
            other => panic!("unknown port allocation child role: {other}"),
        }
        return;
    }

    let listeners = threaded_listeners(24);
    let mut outside = Vec::new();
    let mut reclaimable = Vec::new();
    for (index, listener) in listeners.iter().enumerate() {
        let port = listener.local_addr().unwrap().port();
        if index % 2 == 0 {
            outside.push(port);
        } else {
            reclaimable.push(port);
        }
    }
    assert_unique_ports(&outside);
    assert_unique_ports(&reclaimable);
    assert_outside_ephemeral(&outside);
    assert_outside_ephemeral(&reclaimable);
    assert!(
        outside.iter().all(|port| (20_000..30_000).contains(port)),
        "outside-ephemeral allocator left its range"
    );
    assert!(
        reclaimable
            .iter()
            .all(|port| (31_000..32_000).contains(port)),
        "reclaimable allocator left its range"
    );
    drop(listeners);

    let hold = std::env::temp_dir().join(format!("port-hold-{:032x}", rand::random::<u128>()));
    let hold_a = hold.join("a");
    let hold_b = hold.join("b");
    std::fs::create_dir_all(&hold_a).unwrap();
    std::fs::create_dir_all(&hold_b).unwrap();
    let mut held_children = KillChildren(vec![
        spawn_port_child("hold", &hold_a),
        spawn_port_child("hold", &hold_b),
    ]);
    wait_for_file(&hold_a.join("ready"));
    wait_for_file(&hold_b.join("ready"));
    let mut held = read_ports(&hold_a.join("ports"));
    held.extend(read_ports(&hold_b.join("ports")));
    assert_unique_ports(&held);
    assert_outside_ephemeral(&held);
    std::fs::write(hold_a.join("release"), "ok").unwrap();
    std::fs::write(hold_b.join("release"), "ok").unwrap();
    assert!(
        held_children.wait_all(),
        "hold children failed while keeping their listeners"
    );

    let gap = std::env::temp_dir().join(format!("port-gap-{:032x}", rand::random::<u128>()));
    std::fs::create_dir_all(&gap).unwrap();
    let mut gap_children = KillChildren(vec![
        spawn_port_child("gap-owner", &gap),
        spawn_port_child("gap-other", &gap),
    ]);
    wait_for_file(&gap.join("owner-ready"));
    wait_for_file(&gap.join("other-ready"));
    let owner_ports = read_ports(&gap.join("owner-ports"));
    let other_ports = read_ports(&gap.join("other-ports"));
    let mut combined = owner_ports.clone();
    combined.extend(&other_ports);
    assert_unique_ports(&combined);
    assert_outside_ephemeral(&combined);
    std::fs::write(gap.join("rebind"), "ok").unwrap();
    wait_for_file(&gap.join("rebound"));
    std::fs::write(gap.join("release"), "ok").unwrap();
    assert!(
        gap_children.wait_all(),
        "gap children failed while rebinding a closed port"
    );
    let _ = std::fs::remove_dir_all(&hold);
    let _ = std::fs::remove_dir_all(&gap);
}

fn threaded_listeners(count: usize) -> Vec<std::net::TcpListener> {
    // A runtime built on this async test's worker thread is rejected, and
    // blocking that worker while it waits for itself deadlocks. Allocate on
    // fresh threads and keep the sockets after those runtimes shut down.
    std::thread::scope(|scope| {
        let mut joins = Vec::with_capacity(count);
        for _ in 0..count {
            joins.push(scope.spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .build()
                    .unwrap();
                runtime.block_on(async {
                    let outside = bind_outside_ephemeral_range().await.into_std().unwrap();
                    let reclaimable = bind_reclaimable_health_port().await.into_std().unwrap();
                    (outside, reclaimable)
                })
            }));
        }
        let mut listeners = Vec::with_capacity(count * 2);
        for join in joins {
            let (outside, reclaimable) = join.join().unwrap();
            listeners.push(outside);
            listeners.push(reclaimable);
        }
        listeners
    })
}

async fn hold_allocated_ports(dir: &std::path::Path) {
    let listeners = threaded_listeners(16);
    let ports = listeners
        .iter()
        .map(|listener| listener.local_addr().unwrap().port())
        .collect::<Vec<_>>();
    write_port_list(&dir.join("ports"), &ports);
    std::fs::write(dir.join("ready"), "ok").unwrap();
    wait_for_file(&dir.join("release"));
}

async fn gap_owner(dir: &std::path::Path) {
    let mut listeners = Vec::new();
    for _ in 0..8 {
        listeners.push(bind_outside_ephemeral_range().await);
    }
    let ports: Vec<u16> = listeners
        .iter()
        .map(|listener| listener.local_addr().unwrap().port())
        .collect();
    drop(listeners);
    write_port_list(&dir.join("owner-ports"), &ports);
    std::fs::write(dir.join("owner-ready"), "ok").unwrap();
    wait_for_file(&dir.join("rebind"));
    let mut rebound = Vec::new();
    for port in &ports {
        rebound.push(
            tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, *port))
                .await
                .expect("closed port was handed to another live process"),
        );
    }
    std::fs::write(dir.join("rebound"), "ok").unwrap();
    wait_for_file(&dir.join("release"));
    drop(rebound);
}

async fn gap_other(dir: &std::path::Path) {
    wait_for_file(&dir.join("owner-ready"));
    let mut listeners = Vec::new();
    for _ in 0..8 {
        listeners.push(bind_outside_ephemeral_range().await);
    }
    write_ports(&dir.join("other-ports"), &listeners);
    std::fs::write(dir.join("other-ready"), "ok").unwrap();
    wait_for_file(&dir.join("release"));
}

fn spawn_port_child(role: &str, dir: &std::path::Path) -> std::process::Child {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "controller_tests::concurrent_port_allocations_never_return_the_same_port",
            "--exact",
            "--test-threads=1",
        ])
        .env("JMODGUARD_PORT_ALLOC_CHILD", role)
        .env("JMODGUARD_PORT_ALLOC_DIR", dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit());
    // Random origins make these two children miss each other, and the regression
    // then passes with reservations disabled. Pin one shared walk for this pair.
    if role == "gap-owner" || role == "gap-other" {
        command.env("JMODGUARD_PORT_SCAN_ORIGIN", "0");
    }
    command.spawn().unwrap()
}

struct KillChildren(Vec<std::process::Child>);

impl KillChildren {
    fn wait_all(&mut self) -> bool {
        self.0.iter_mut().all(wait_child)
    }
}

impl Drop for KillChildren {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn wait_child(child: &mut std::process::Child) -> bool {
    wait_for_exit(child).success()
}

fn wait_for_exit(child: &mut std::process::Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "port allocation child timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_file(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("marker")
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn write_ports(path: &std::path::Path, listeners: &[tokio::net::TcpListener]) {
    let ports = listeners
        .iter()
        .map(|listener| listener.local_addr().unwrap().port())
        .collect::<Vec<_>>();
    write_port_list(path, &ports);
}

fn write_port_list(path: &std::path::Path, ports: &[u16]) {
    let mut text = String::new();
    for port in ports {
        text.push_str(&port.to_string());
        text.push('\n');
    }
    std::fs::write(path, text).unwrap();
}

fn read_ports(path: &std::path::Path) -> Vec<u16> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| line.parse().unwrap())
        .collect()
}

fn assert_unique_ports(ports: &[u16]) {
    let mut seen = HashSet::new();
    for port in ports {
        assert!(seen.insert(*port), "port {port} was allocated twice");
    }
    assert!(!ports.is_empty(), "allocation returned no ports");
}

fn assert_outside_ephemeral(ports: &[u16]) {
    let policy = port_policy();
    for port in ports {
        assert!(
            *port < policy.ephemeral_low || *port > policy.ephemeral_high,
            "port {port} is inside the ephemeral range"
        );
    }
}

#[test]
fn scan_near_u32_max_visits_every_slot() {
    let span = 10_000u32;
    let origin = u32::MAX - 17;
    let mut seen = vec![false; span as usize];
    for step in 0..span {
        seen[scan_slot(origin, step, span) as usize] = true;
    }
    assert!(seen.iter().all(|slot| *slot));
}

#[test]
fn selectable_ports_skip_the_ephemeral_range_and_privileged_ports() {
    let policy = PortPolicy {
        ephemeral_low: 32_768,
        ephemeral_high: 60_999,
        unprivileged_start: 1024,
        root: false,
    };
    assert!(port_is_selectable(20_000, policy));
    assert!(port_is_selectable(31_000, policy));
    assert!(!port_is_selectable(40_000, policy));
    assert!(!port_is_selectable(80, policy));
    let root = PortPolicy {
        root: true,
        ..policy
    };
    assert!(port_is_selectable(80, root));
    assert!(!port_is_selectable(40_000, root));
    let shifted = PortPolicy {
        ephemeral_low: 10_000,
        ephemeral_high: 25_000,
        ..policy
    };
    assert!(!port_is_selectable(20_000, shifted));
    assert!(port_is_selectable(31_000, shifted));
}

fn unix_mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn port_claims_use_a_private_per_user_directory() {
    assert_eq!(
        reservation_dir_for_bind(),
        std::path::PathBuf::from(format!(
            "/tmp/jmodguard-updater-port-reservations-{}",
            our_uid()
        ))
    );
}

#[test]
fn reservation_lock_symlink_does_not_change_the_target_mode() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("port-lock-link-{:032x}", rand::random::<u128>()));
    let _cleanup = RemoveDir(dir.clone());
    prepare_reservation_dir(&dir);
    let sentinel = dir.join("sentinel");
    let claims_target = dir.join("claims-target");
    for path in [&sentinel, &claims_target] {
        std::fs::File::create(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    std::os::unix::fs::symlink(&sentinel, dir.join("lock")).unwrap();
    std::os::unix::fs::symlink(&claims_target, dir.join("claims")).unwrap();
    let lock_error = open_reservation_lock(&dir.join("lock")).unwrap_err();
    assert!(
        lock_error.contains("symlink"),
        "lock symlink was opened: {lock_error}"
    );
    let claims_error = read_reservation_claims(&dir).unwrap_err();
    assert!(
        claims_error.contains("symlink"),
        "claims symlink was followed: {claims_error}"
    );
    assert_eq!(unix_mode(&sentinel), 0o600);
    assert_eq!(unix_mode(&claims_target), 0o600);
}

#[test]
fn reservation_directory_owned_by_another_user_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("port-foreign-{:032x}", rand::random::<u128>()));
    let _cleanup = RemoveDir(dir.clone());
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let sentinel = dir.join("lock");
    std::fs::File::create(&sentinel).unwrap();
    std::fs::set_permissions(&sentinel, std::fs::Permissions::from_mode(0o600)).unwrap();
    let error = prepare_reservation_dir_for(&dir, our_uid().wrapping_add(1)).unwrap_err();
    assert!(
        error.contains("another user"),
        "foreign reservation directory was accepted: {error}"
    );
    assert_eq!(unix_mode(&dir), 0o755, "foreign directory mode changed");
    assert_eq!(unix_mode(&sentinel), 0o600, "foreign file mode changed");
}

#[test]
fn private_reservation_directory_tightens_only_its_own_mode() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("port-mode-{:032x}", rand::random::<u128>()));
    let _cleanup = RemoveDir(dir.clone());
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let sibling = dir.join("sibling");
    std::fs::File::create(&sibling).unwrap();
    std::fs::set_permissions(&sibling, std::fs::Permissions::from_mode(0o640)).unwrap();
    prepare_reservation_dir(&dir);
    assert_eq!(unix_mode(&dir), 0o700);
    assert_eq!(unix_mode(&sibling), 0o640);
    let lock = dir.join("lock");
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&lock)
        .unwrap();
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).unwrap();
    open_reservation_lock(&lock).unwrap();
    assert_eq!(unix_mode(&lock), 0o600);
}

#[test]
fn reservation_directory_rejects_a_symlink() {
    let root = std::env::temp_dir().join(format!("port-link-{:032x}", rand::random::<u128>()));
    let _cleanup = RemoveDir(root.clone());
    let target = root.join("target");
    let link = root.join("link");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("claims"), "keep").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let error = prepare_reservation_dir_for(&link, our_uid()).unwrap_err();
    assert!(
        error.contains("symlink"),
        "symlink reservation directory was accepted: {error}"
    );
    assert_eq!(
        std::fs::read_to_string(target.join("claims")).unwrap(),
        "keep"
    );
}

#[test]
fn reservation_lock_times_out_while_another_process_holds_it() {
    use std::os::fd::AsRawFd;
    let dir = std::env::temp_dir().join(format!("port-lock-{:032x}", rand::random::<u128>()));
    let _cleanup = RemoveDir(dir.clone());
    std::fs::create_dir_all(&dir).unwrap();
    let lock_path = dir.join("lock");
    std::fs::File::create(&lock_path).unwrap();
    let holder = KillChildren(vec![
        std::process::Command::new("flock")
            .arg("-x")
            .arg(&lock_path)
            .arg("sleep")
            .arg("30")
            .spawn()
            .unwrap(),
    ]);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    let blocked = Instant::now() + Duration::from_secs(5);
    loop {
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            break;
        }
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        assert!(Instant::now() < blocked, "holder never took the lock");
        std::thread::sleep(Duration::from_millis(10));
    }
    let result = lock_reservations_until(&file, Instant::now() + Duration::from_millis(200));
    assert!(result.is_err(), "lock wait should time out");
    drop(holder);
}

#[test]
fn a_dead_owner_is_dropped_and_an_unreadable_owner_is_kept() {
    assert!(owner_still_reserved(Ok(4), 4));
    assert!(!owner_still_reserved(Ok(5), 4));
    assert!(!owner_still_reserved(Err(ProcLookup::Gone), 1));
    assert!(owner_still_reserved(Err(ProcLookup::Unreadable), 1));

    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = child.id();
    let start = process_start(pid).expect("live owner");
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(matches!(process_start(pid), Err(ProcLookup::Gone)));

    let dir = std::env::temp_dir().join(format!("port-dead-{:032x}", rand::random::<u128>()));
    let _cleanup = RemoveDir(dir.clone());
    prepare_reservation_dir(&dir);
    std::fs::write(dir.join("claims"), format!("25010 {pid} {start}\n")).unwrap();
    assert!(load_reservations(&dir).is_empty());
}

#[tokio::test]
async fn health_bind_retries_only_while_the_port_is_busy() {
    let listener = bind_outside_ephemeral_range().await;
    let addr = listener.local_addr().unwrap();
    let (_cancel, mut cancel_rx) = tokio::sync::oneshot::channel::<()>();
    let started = Instant::now();
    let missed = bind_health_retry(
        &addr.to_string(),
        "not ready",
        Instant::now() + Duration::from_millis(80),
        &mut cancel_rx,
    )
    .await;
    assert!(missed.is_none());
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(50),
        "retry returned too quickly: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "retry ran too long: {elapsed:?}"
    );

    let (cancel, mut cancel_rx) = tokio::sync::oneshot::channel::<()>();
    let retry = tokio::spawn(async move {
        bind_health_retry(
            &addr.to_string(),
            "not ready",
            Instant::now() + Duration::from_secs(5),
            &mut cancel_rx,
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    cancel.send(()).unwrap();
    let cancelled = tokio::time::timeout(Duration::from_millis(500), retry)
        .await
        .unwrap()
        .unwrap();
    assert!(cancelled.is_none(), "cancel must not leave a listener");

    drop(listener);
    let (_cancel, mut cancel_rx) = tokio::sync::oneshot::channel::<()>();
    let serve = bind_health_retry(
        &addr.to_string(),
        "discord token rejected",
        Instant::now() + POLL,
        &mut cancel_rx,
    )
    .await
    .expect("bind after the port is released");
    assert_launcher_health(addr, "discord token rejected").await;
    serve.abort();
    let _ = serve.await;

    let (_cancel, mut cancel_rx) = tokio::sync::oneshot::channel::<()>();
    let started = Instant::now();
    let invalid = bind_health_retry(
        "invalid-address",
        "not ready",
        Instant::now() + Duration::from_secs(5),
        &mut cancel_rx,
    )
    .await;
    assert!(invalid.is_none());
    assert!(started.elapsed() < Duration::from_millis(500));
}

async fn held_launcher_health(
    restart: &mut RestartPolicy,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<Result<()>>) {
    restart.exited(
        true,
        Some(jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE),
        Duration::ZERO,
        Instant::now(),
    );
    // The server keeps this socket. An OS-assigned port stays ours while it does.
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(jmodguard::health::serve_listener_as_with_reason(
        listener,
        "launcher",
        restart.not_ready_reason(),
    ));
    assert_launcher_health(addr, "discord token rejected").await;
    // Move only the deadline: preserve the production hold/retry transition.
    restart.auth_retry_at = Some(Instant::now());
    (addr, task)
}

#[tokio::test]
async fn failed_auth_retry_tick_refreshes_ready_reason_before_control_error() {
    let root = std::env::temp_dir().join(format!("retry-tick-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let db = Db::open(&root.join("core.db"), 2).unwrap();
    let mut bot = offline_controller("bot", UpdateControl::from_application(&db), &root);
    // The tick aborts whatever `health` holds, then binds `health_bind`.
    // Those must be different sockets: rebinding the port just closed races
    // other listeners in this process.
    let bind_listener = bind_reclaimable_health_port().await;
    let addr = bind_listener.local_addr().unwrap();
    drop(bind_listener);
    let (held_addr, task) = held_launcher_health(&mut bot.restart).await;
    assert_ne!(held_addr, addr);
    bot.health = Some(task);
    bot.health_bind = Some(addr.to_string());
    db.with_conn_sync(|conn| {
        let tx = conn.transaction_immediate()?;
        tx.execute_bounded(
            "INSERT OR REPLACE INTO meta(key,value) VALUES('component_upgrade_v1','invalid')",
            (),
            64 * 1024,
        )?;
        tx.commit()
    })
    .unwrap();
    let error = bot.tick().await.unwrap_err();
    assert!(error.to_string().contains("expected value"), "{error:#}");
    assert!(!bot.restart.held);
    assert!(bot.running.is_none());
    assert_launcher_health(addr, "not ready").await;
    // Later failed polls must not restore the stale reason, either.
    assert!(bot.tick().await.is_err());
    assert_launcher_health(addr, "not ready").await;
    bot.end_idle().await.unwrap();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn failed_auth_retry_boot_open_keeps_current_ready_reason() {
    let root = std::env::temp_dir().join(format!("retry-boot-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
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
    db.with_conn_sync(|conn| {
        let tx = conn.transaction_immediate()?;
        tx.execute_bounded(
            "UPDATE meta SET value=?1 WHERE key=?2",
            params!["foreign", "schema_family"],
            64 * 1024,
        )?;
        tx.commit()
    })
    .unwrap();
    let mut restart = RestartPolicy::default();
    restart.exited(
        true,
        Some(jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE),
        Duration::ZERO,
        Instant::now(),
    );
    // The retry drops this listener and binds the same address. A reserved
    // port stays outside the ephemeral range across that gap.
    let listener = bind_reclaimable_health_port().await;
    let addr = listener.local_addr().unwrap();
    assert_outside_ephemeral(&[addr.port()]);
    assert!(
        (31_000..32_000).contains(&addr.port()),
        "retry fixture left the reserved range"
    );
    let port = addr.port().to_string();
    let reserved = read_reservation_claims(&reservation_dir_for_bind()).unwrap();
    assert!(
        reserved
            .lines()
            .any(|line| line.split_whitespace().next() == Some(port.as_str())),
        "retry fixture did not reserve its port"
    );
    let task = tokio::spawn(jmodguard::health::serve_listener_as_with_reason(
        listener,
        "launcher",
        restart.not_ready_reason(),
    ));
    assert_launcher_health(addr, "discord token rejected").await;
    restart.auth_retry_at = Some(Instant::now());
    let mut health = Some(task);
    assert!(
        restart
            .retry_auth_with_health(Instant::now(), &mut health, Some(&addr.to_string()))
            .await
            .unwrap()
    );
    // Same retry/open ordering as the boot loop; the database failure is real.
    let error = UpdateControl::open(&opts)
        .err()
        .expect("foreign store must fail");
    assert!(
        error.to_string().contains("unsupported update-control"),
        "{error:#}"
    );
    assert!(!restart.held);
    assert_launcher_health(addr, "not ready").await;
    let task = health.take().unwrap();
    task.abort();
    let _ = task.await;
    drop(db);
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn rejected_token_child_is_held_with_liveness_and_no_restart() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // Exercise direct executable launch with characters that require shell quoting.
    let root = std::env::temp_dir().join(format!(
        "token-hold-quoted ' path-{:032x}",
        rand::random::<u128>()
    ));
    tokio::fs::create_dir_all(&root).await.unwrap();
    // Let the child allocate and retain its socket, avoiding a reserve/drop/bind
    // race before the handoff. Its published address configures both processes.
    let child = executable(&root, "health-bot").await;
    let bind_request = root.join("bind-health-health-bot");
    tokio::fs::write(&bind_request, "127.0.0.1:0")
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
    bot.baked = child.clone();
    bot.enabled = false;
    bot.start(&child, &"b".repeat(40), None).await.unwrap();
    let bound = root.join("health-bound-health-bot");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !bound.exists() {
            assert!(
                bot.running
                    .as_mut()
                    .unwrap()
                    .child
                    .try_wait()
                    .unwrap()
                    .is_none(),
                "child exited before binding the configured health address"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child never bound the configured health address");
    let addr: std::net::SocketAddr = tokio::fs::read_to_string(bound)
        .await
        .unwrap()
        .parse()
        .unwrap();
    bot.health_bind = Some(addr.to_string());
    // Retries must use the same concrete address, too.
    tokio::fs::write(&bind_request, addr.to_string())
        .await
        .unwrap();
    assert_eq!(
        tokio::net::TcpListener::bind(addr)
            .await
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AddrInUse
    );
    bot.idle().await.unwrap();
    assert!(
        bot.health.is_none(),
        "launcher must not listen while the child runs"
    );
    // Publish the complete code before its path releases the waiting child.
    publish_exit_code(
        root.join("exit-code-health-bot"),
        jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE,
    )
    .await;
    let status = tokio::time::timeout(
        Duration::from_secs(5),
        bot.running.as_mut().unwrap().child.wait(),
    )
    .await
    .expect("child did not exit after token rejection")
    .unwrap();
    assert_eq!(status.code(), Some(78));
    // If exit races the tick's initial check, generic idle must not capture
    // the ordinary readiness reason before the auth hold is established.
    bot.idle().await.unwrap();
    assert!(bot.health.is_none());
    let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let guard = tracing::subscriber::set_default(ContractWarnings(Arc::clone(&logs)));
    // Exactly one production poll must reap the child and bind the same socket.
    assert!(!bot.tick().await.unwrap());
    assert!(bot.running.is_none());
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
    for _ in 0..3 {
        assert!(!bot.tick().await.unwrap());
        assert!(bot.running.is_none(), "held bot must never be restored");
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
    assert_launcher_health(addr, "discord token rejected").await;
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
    tokio::fs::remove_file(bind_request).await.unwrap();
    tokio::fs::remove_file(root.join("exit-code-health-bot"))
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
    assert!(bot.health.is_none(), "start must release the idle listener");
    let released = tokio::net::TcpListener::bind(addr).await.unwrap();
    drop(released);
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
    assert_launcher_health(addr, "not ready").await;
    bot.end_idle().await.unwrap();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

async fn independent_exiting_bot(
    code: i32,
) -> (
    PathBuf,
    Db,
    Supervisor,
    ComponentRelease,
    tokio::task::JoinHandle<()>,
) {
    independent_exiting_bot_prepared(code, |_| {}).await
}

/// `prepare` runs after the exit file is in place and before the candidate
/// start tick, which is the first poll that can settle that exit.
async fn independent_exiting_bot_prepared(
    code: i32,
    prepare: impl FnOnce(&mut Supervisor),
) -> (
    PathBuf,
    Db,
    Supervisor,
    ComponentRelease,
    tokio::task::JoinHandle<()>,
) {
    selected_exiting_bot(code, false, false, prepare).await
}

async fn selected_exiting_bot(
    code: i32,
    restored: bool,
    after_ready: bool,
    prepare: impl FnOnce(&mut Supervisor),
) -> (
    PathBuf,
    Db,
    Supervisor,
    ComponentRelease,
    tokio::task::JoinHandle<()>,
) {
    let root =
        std::env::temp_dir().join(format!("independent-exit-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let baked = executable(&root, "baked-bot").await;
    let installed = executable(&root, "installed-bot").await;
    let desired = executable(&root, "desired-bot").await;
    let mut objects = HashMap::new();
    let mut releases = Vec::new();
    for path in [installed, desired] {
        let mut release = image(&mut objects, "bot", &tokio::fs::read(path).await.unwrap());
        release.min_schema = db::CORE_SCHEMA_VERSION;
        release.max_schema = db::CORE_SCHEMA_VERSION;
        release.result_schema = db::CORE_SCHEMA_VERSION;
        release.upgrade.as_mut().unwrap().guild_schema = db::guild_data::SCHEMA_VERSION;
        releases.push(release);
    }
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
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
    let mut bot = controller("bot", baked, registry, control.clone(), &root);
    wait_for_bot_ready(&mut bot).await;
    let seen = control.component("bot").await.unwrap().observed.unwrap();
    control.running("api", None, seen, now()).await.unwrap();
    let installed = &releases[0];
    control
        .checked("bot", "latest", Some(installed.clone()), None, now())
        .await
        .unwrap();
    db::request_component_install(&db, "bot", &installed.digest, now())
        .await
        .unwrap();
    bot.tick().await.unwrap();
    wait_for_bot_ready(&mut bot).await;
    assert_eq!(
        control
            .component("bot")
            .await
            .unwrap()
            .installed
            .unwrap()
            .digest,
        installed.digest
    );
    let (candidate, fixture) = if restored {
        restart(&mut bot).await;
        (installed.clone(), "installed-bot")
    } else {
        let candidate = releases.pop().unwrap();
        control
            .checked("bot", "latest", Some(candidate.clone()), None, now())
            .await
            .unwrap();
        db::request_component_install(&db, "bot", &candidate.digest, now())
            .await
            .unwrap();
        (candidate, "desired-bot")
    };
    let exit_file = root.join(format!("exit-code-{fixture}"));
    if !after_ready {
        publish_exit_code(&exit_file, code).await;
    }
    // Record only replacement/recovery starts, after both baselines ran.
    tokio::fs::write(root.join("events"), "").await.unwrap();
    prepare(&mut bot);
    bot.tick().await.unwrap();
    if after_ready {
        assert_eq!(
            bot.running
                .as_ref()
                .unwrap()
                .release
                .as_ref()
                .unwrap()
                .digest,
            candidate.digest
        );
        wait_for_bot_ready(&mut bot).await;
        // Publish the whole code before the child can read it.
        publish_exit_code(&exit_file, code).await;
        bot.running.as_mut().unwrap().child.wait().await.unwrap();
        assert!(bot.running.as_ref().unwrap().ready_file.exists());
    } else if let Some(child) = bot.running.as_mut() {
        assert_eq!(child.release.as_ref().unwrap().digest, candidate.digest);
        let ready_file = child.ready_file.clone();
        child.child.wait().await.unwrap();
        assert!(!ready_file.exists());
    } else {
        // settle_exited_child already reaped an immediate token rejection.
        // The hold, not a still-running child, is the record of that exit.
        assert_eq!(
            code,
            jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE
        );
        assert!(bot.restart.held);
        assert!(!bot.exited_for_restart);
        let recorded = bot.rejected_candidate.clone().or_else(|| {
            bot.restart_target
                .as_ref()
                .and_then(|target| target.release.clone())
        });
        assert_eq!(
            recorded.expect("held exit recorded the candidate").digest,
            candidate.digest
        );
        let events = tokio::fs::read_to_string(root.join("events"))
            .await
            .unwrap();
        assert!(
            !events.lines().any(|line| line.ends_with(".ready")),
            "an immediate token rejection must not publish readiness"
        );
    }
    (root, db, bot, candidate, server)
}

async fn wait_for_bot_ready(bot: &mut Supervisor) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !bot.reported {
        assert!(
            Instant::now() < deadline,
            "fixture bot did not become ready"
        );
        bot.tick().await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn independent_exit_starts(root: &Path) -> Vec<String> {
    tokio::fs::read_to_string(root.join("events"))
        .await
        .unwrap()
        .lines()
        .filter(|line| line.ends_with(".start"))
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn independent_rejected_token_retries_desired_before_installed_and_doubles_hold() {
    let (root, db, mut bot, candidate, server) =
        independent_exiting_bot(jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE).await;
    bot.tick().await.unwrap();
    assert!(bot.restart.held);
    assert!(bot.running.is_none());
    assert_eq!(bot.restart.auth_failures, 1);
    let first = bot
        .restart
        .auth_retry_at
        .unwrap()
        .duration_since(Instant::now());
    assert!(first > Duration::from_secs(1790) && first <= Duration::from_secs(1800));
    assert_ne!(
        bot.control
            .component("bot")
            .await
            .unwrap()
            .installed
            .unwrap()
            .digest,
        candidate.digest
    );
    bot.restart.auth_retry_at = Some(Instant::now());
    bot.tick().await.unwrap();
    // The exit file is already present, so this child can exit before tick
    // returns. Settlement then holds the token and clears `running`. A quiet
    // run can still return while the child is recorded. Either result is valid.
    if let Some(running) = bot.running.as_mut() {
        assert_eq!(
            running.release.as_ref().unwrap().digest,
            candidate.digest,
            "timed retry must start the desired candidate before any installed/baked child"
        );
        tokio::time::timeout(POLL, running.child.wait())
            .await
            .unwrap()
            .unwrap();
    } else {
        // settle_exited_child already reaped this immediate token rejection.
        // The hold, not a still-running child, is the record of that exit.
        assert!(bot.restart.held);
        assert!(!bot.exited_for_restart);
        let recorded = bot.rejected_candidate.clone().or_else(|| {
            bot.restart_target
                .as_ref()
                .and_then(|target| target.release.clone())
        });
        assert_eq!(
            recorded.expect("held exit recorded the candidate").digest,
            candidate.digest,
            "timed retry must start the desired candidate before any installed/baked child"
        );
    }
    assert_eq!(
        independent_exit_starts(&root).await,
        ["desired-bot.start", "desired-bot.start"]
    );
    bot.tick().await.unwrap();
    assert!(bot.restart.held);
    assert!(bot.running.is_none());
    assert_eq!(bot.restart.auth_failures, 2);
    let second = bot
        .restart
        .auth_retry_at
        .unwrap()
        .duration_since(Instant::now());
    assert!(second > Duration::from_secs(3590) && second <= Duration::from_secs(3600));
    assert_eq!(
        bot.control
            .component("bot")
            .await
            .unwrap()
            .desired
            .unwrap()
            .digest,
        candidate.digest
    );
    bot.end_idle().await.unwrap();
    server.abort();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn independent_non_auth_exit_before_readiness_still_rolls_back() {
    let (root, db, mut bot, candidate, server) = independent_exiting_bot(7).await;
    let installed = bot
        .control
        .component("bot")
        .await
        .unwrap()
        .installed
        .unwrap();
    bot.tick().await.unwrap();
    assert!(!bot.restart.held);
    assert_eq!(
        bot.running
            .as_ref()
            .unwrap()
            .release
            .as_ref()
            .unwrap()
            .digest,
        installed.digest
    );
    assert!(
        bot.control
            .component("bot")
            .await
            .unwrap()
            .desired
            .is_none()
    );
    wait_for_bot_ready(&mut bot).await;
    for _ in 0..3 {
        bot.tick().await.unwrap();
    }
    assert_ne!(
        bot.running
            .as_ref()
            .unwrap()
            .release
            .as_ref()
            .unwrap()
            .digest,
        candidate.digest
    );
    assert_eq!(
        independent_exit_starts(&root).await,
        ["desired-bot.start", "installed-bot.start"]
    );
    bot.stop_application().await.unwrap();
    bot.end_idle().await.unwrap();
    server.abort();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn independent_ready_update_clears_standby_before_later_exit() {
    let root =
        std::env::temp_dir().join(format!("independent-ready-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let baked = executable(&root, "baked-bot").await;
    let installed = executable(&root, "installed-bot").await;
    let desired = executable(&root, "desired-bot").await;
    let mut objects = HashMap::new();
    let mut releases = Vec::new();
    for path in [installed, desired] {
        let mut release = image(&mut objects, "bot", &tokio::fs::read(path).await.unwrap());
        release.min_schema = db::CORE_SCHEMA_VERSION;
        release.max_schema = db::CORE_SCHEMA_VERSION;
        release.result_schema = db::CORE_SCHEMA_VERSION;
        release.upgrade.as_mut().unwrap().guild_schema = db::guild_data::SCHEMA_VERSION;
        releases.push(release);
    }
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
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
    let mut bot = controller("bot", baked, registry, control.clone(), &root);
    wait_for_bot_ready(&mut bot).await;
    let seen = control.component("bot").await.unwrap().observed.unwrap();
    control.running("api", None, seen, now()).await.unwrap();
    let installed_release = releases[0].clone();
    control
        .checked(
            "bot",
            "latest",
            Some(installed_release.clone()),
            None,
            now(),
        )
        .await
        .unwrap();
    db::request_component_install(&db, "bot", &installed_release.digest, now())
        .await
        .unwrap();
    bot.tick().await.unwrap();
    wait_for_bot_ready(&mut bot).await;
    let candidate = releases[1].clone();
    control
        .checked("bot", "latest", Some(candidate.clone()), None, now())
        .await
        .unwrap();
    db::request_component_install(&db, "bot", &candidate.digest, now())
        .await
        .unwrap();
    tokio::fs::write(root.join("events"), "").await.unwrap();
    bot.tick().await.unwrap();
    wait_for_bot_ready(&mut bot).await;
    assert!(
        bot.standby.is_none(),
        "a ready independent update must retire the prior standby build"
    );
    assert_eq!(
        bot.running
            .as_ref()
            .unwrap()
            .release
            .as_ref()
            .unwrap()
            .digest,
        candidate.digest
    );
    bot.running.as_mut().unwrap().child.start_kill().unwrap();
    let _ = bot.running.as_mut().unwrap().child.wait().await;
    assert!(
        bot.tick().await.is_err(),
        "an unexpected child exit should surface as a tick error, not a standby rollback"
    );
    assert!(
        bot.running.is_none(),
        "a later exit must not restart the retired standby"
    );
    assert_eq!(
        independent_exit_starts(&root).await,
        ["desired-bot.start"],
        "must not roll back to the prior build after activation"
    );
    bot.end_idle().await.unwrap();
    server.abort();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn independent_rejected_token_cleared_selection_restores_installed() {
    let (root, db, mut bot, candidate, server) =
        independent_exiting_bot(jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE).await;
    bot.tick().await.unwrap();
    bot.control
        .failed("bot", &candidate, "selection cleared")
        .await
        .unwrap();
    let installed = bot
        .control
        .component("bot")
        .await
        .unwrap()
        .installed
        .unwrap();
    bot.restart.auth_retry_at = Some(Instant::now());
    bot.tick().await.unwrap();
    assert_eq!(
        bot.running
            .as_ref()
            .unwrap()
            .release
            .as_ref()
            .unwrap()
            .digest,
        installed.digest
    );
    wait_for_bot_ready(&mut bot).await;
    assert_eq!(
        independent_exit_starts(&root).await,
        ["desired-bot.start", "installed-bot.start"]
    );
    bot.stop_application().await.unwrap();
    bot.end_idle().await.unwrap();
    server.abort();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

/// Container restart: drop the supervisor and open another on the same database.
async fn reopen_supervisor(mut bot: Supervisor) -> Supervisor {
    let role = bot.role.clone();
    let baked = bot.baked.clone();
    let registry = bot.registry.clone();
    let control = bot.control.clone();
    let root = bot
        .staging
        .parent()
        .expect("fixture staging directory")
        .to_path_buf();
    bot.end_idle().await.expect("idle listener released");
    drop(bot);
    controller(&role, baked, registry, control, &root)
}

#[tokio::test]
async fn rejected_token_marker_survives_launcher_restart_during_hold() {
    let (root, db, mut bot, candidate, server) =
        independent_exiting_bot(jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE).await;
    bot.tick().await.unwrap();
    assert!(
        bot.restart.held,
        "desired candidate must be inside the hold"
    );
    assert!(bot.running.is_none());
    assert_eq!(
        bot.control
            .rejected_candidate_digest("bot")
            .await
            .unwrap()
            .as_deref(),
        Some(candidate.digest.as_str())
    );
    // The hold clock is process-local. Reopening during the hold is the restart.
    let mut bot = reopen_supervisor(bot).await;
    if bot.restart.held {
        bot.restart.auth_retry_at = Some(Instant::now());
    }
    tokio::fs::remove_file(root.join("exit-code-desired-bot"))
        .await
        .unwrap();
    bot.tick().await.unwrap();
    assert_eq!(
        bot.running
            .as_ref()
            .unwrap()
            .release
            .as_ref()
            .unwrap()
            .digest,
        candidate.digest,
        "restart during the hold must start the desired candidate"
    );
    wait_for_bot_ready(&mut bot).await;
    assert_eq!(
        independent_exit_starts(&root).await,
        ["desired-bot.start", "desired-bot.start"],
        "restart must not start the installed or baked bot"
    );
    assert!(
        bot.control
            .rejected_candidate_digest("bot")
            .await
            .unwrap()
            .is_none(),
        "readiness clears the rejected-candidate marker"
    );
    bot.stop_application().await.unwrap();
    bot.end_idle().await.unwrap();
    server.abort();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn cleared_rejected_token_selection_is_not_revived_after_restart() {
    let (root, db, mut bot, candidate, server) =
        independent_exiting_bot(jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE).await;
    bot.tick().await.unwrap();
    assert!(bot.restart.held);
    bot.control
        .failed("bot", &candidate, "selection cleared")
        .await
        .unwrap();
    assert!(
        bot.control
            .rejected_candidate_digest("bot")
            .await
            .unwrap()
            .is_none(),
        "clearing the selection removes the rejected-candidate marker"
    );
    let installed = bot
        .control
        .component("bot")
        .await
        .unwrap()
        .installed
        .unwrap();
    let mut bot = reopen_supervisor(bot).await;
    if bot.restart.held {
        bot.restart.auth_retry_at = Some(Instant::now());
    }
    bot.tick().await.unwrap();
    assert_eq!(
        bot.running
            .as_ref()
            .unwrap()
            .release
            .as_ref()
            .unwrap()
            .digest,
        installed.digest,
        "a cleared selection must not revive the rejected candidate"
    );
    assert_ne!(installed.digest, candidate.digest);
    wait_for_bot_ready(&mut bot).await;
    assert_eq!(
        independent_exit_starts(&root).await,
        ["desired-bot.start", "installed-bot.start"]
    );
    bot.stop_application().await.unwrap();
    bot.end_idle().await.unwrap();
    server.abort();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn coordinated_upgrade_clears_rejected_token_marker() {
    let (root, db, mut bot, candidate, server) =
        independent_exiting_bot(jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE).await;
    bot.tick().await.unwrap();
    assert_eq!(
        bot.control
            .rejected_candidate_digest("bot")
            .await
            .unwrap()
            .as_deref(),
        Some(candidate.digest.as_str())
    );
    let error = bot.pair("absent-upgrade").await.unwrap_err();
    assert!(
        error.to_string().contains("no coordinated upgrade"),
        "{error:#}"
    );
    assert!(
        bot.control
            .rejected_candidate_digest("bot")
            .await
            .unwrap()
            .is_none(),
        "a coordinated upgrade drops the rejected-candidate marker"
    );
    bot.end_idle().await.unwrap();
    server.abort();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn transient_rejected_candidate_lookup_is_retried() {
    let (root, db, mut bot, candidate, server) =
        independent_exiting_bot(jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE).await;
    bot.tick().await.unwrap();
    assert_eq!(
        bot.control
            .rejected_candidate_digest("bot")
            .await
            .unwrap()
            .as_deref(),
        Some(candidate.digest.as_str())
    );
    let mut bot = reopen_supervisor(bot).await;
    bot.fail_rejected_lookups = 1;
    let error = bot.tick().await.unwrap_err();
    assert!(
        format!("{error:#}").contains("rejected-candidate lookup failed"),
        "{error:#}"
    );
    assert!(
        !bot.rejected_loaded,
        "a failed lookup must be retried on the next tick"
    );
    assert_eq!(
        bot.control
            .rejected_candidate_digest("bot")
            .await
            .unwrap()
            .as_deref(),
        Some(candidate.digest.as_str())
    );
    tokio::fs::remove_file(root.join("exit-code-desired-bot"))
        .await
        .unwrap();
    bot.tick().await.unwrap();
    assert_eq!(
        bot.running
            .as_ref()
            .unwrap()
            .release
            .as_ref()
            .unwrap()
            .digest,
        candidate.digest,
        "the saved candidate must start after the lookup recovers"
    );
    wait_for_bot_ready(&mut bot).await;
    assert_eq!(
        independent_exit_starts(&root).await,
        ["desired-bot.start", "desired-bot.start"]
    );
    bot.stop_application().await.unwrap();
    bot.end_idle().await.unwrap();
    server.abort();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn second_token_rejection_after_restart_keeps_candidate() {
    let (root, db, mut bot, candidate, server) =
        independent_exiting_bot(jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE).await;
    bot.tick().await.unwrap();
    let mut bot = reopen_supervisor(bot).await;
    bot.tick().await.unwrap();
    if bot.running.is_some() {
        bot.running.as_mut().unwrap().child.wait().await.unwrap();
        bot.tick().await.unwrap();
    }
    assert!(bot.restart.held);
    assert!(bot.running.is_none());
    assert_eq!(
        bot.control
            .rejected_candidate_digest("bot")
            .await
            .unwrap()
            .as_deref(),
        Some(candidate.digest.as_str()),
        "a second token rejection must keep the saved candidate"
    );
    assert_eq!(
        bot.control
            .component("bot")
            .await
            .unwrap()
            .desired
            .unwrap()
            .digest,
        candidate.digest
    );
    tokio::fs::remove_file(root.join("exit-code-desired-bot"))
        .await
        .unwrap();
    bot.restart.auth_retry_at = Some(Instant::now());
    bot.tick().await.unwrap();
    assert_eq!(
        bot.running
            .as_ref()
            .unwrap()
            .release
            .as_ref()
            .unwrap()
            .digest,
        candidate.digest,
        "the timed retry must start the candidate, not the installed or baked bot"
    );
    wait_for_bot_ready(&mut bot).await;
    assert_eq!(
        independent_exit_starts(&root).await,
        [
            "desired-bot.start",
            "desired-bot.start",
            "desired-bot.start"
        ]
    );
    bot.stop_application().await.unwrap();
    bot.end_idle().await.unwrap();
    server.abort();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn recovered_candidate_non_auth_exit_rolls_back() {
    let (root, db, mut bot, candidate, server) =
        independent_exiting_bot(jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE).await;
    bot.tick().await.unwrap();
    let installed = bot
        .control
        .component("bot")
        .await
        .unwrap()
        .installed
        .unwrap();
    let mut bot = reopen_supervisor(bot).await;
    publish_exit_code(root.join("exit-code-desired-bot"), 7).await;
    bot.tick().await.unwrap();
    if bot.running.as_ref().is_some_and(|child| {
        child
            .release
            .as_ref()
            .is_some_and(|release| release.digest == candidate.digest)
    }) {
        bot.running.as_mut().unwrap().child.wait().await.unwrap();
        bot.tick().await.unwrap();
    }
    assert_eq!(
        bot.running
            .as_ref()
            .unwrap()
            .release
            .as_ref()
            .unwrap()
            .digest,
        installed.digest,
        "a non-auth exit after a recovered spawn must roll back"
    );
    assert!(
        bot.control
            .component("bot")
            .await
            .unwrap()
            .desired
            .is_none()
    );
    assert!(
        bot.control
            .rejected_candidate_digest("bot")
            .await
            .unwrap()
            .is_none()
    );
    wait_for_bot_ready(&mut bot).await;
    assert_eq!(
        independent_exit_starts(&root).await,
        [
            "desired-bot.start",
            "desired-bot.start",
            "installed-bot.start"
        ]
    );
    bot.stop_application().await.unwrap();
    bot.end_idle().await.unwrap();
    server.abort();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn recovered_candidate_readiness_timeout_rolls_back() {
    let (root, db, mut bot, candidate, server) =
        independent_exiting_bot(jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE).await;
    bot.tick().await.unwrap();
    let installed = bot
        .control
        .component("bot")
        .await
        .unwrap()
        .installed
        .unwrap();
    let mut bot = reopen_supervisor(bot).await;
    tokio::fs::remove_file(root.join("exit-code-desired-bot"))
        .await
        .unwrap();
    tokio::fs::write(root.join("hold-desired-bot-ready"), "")
        .await
        .unwrap();
    bot.tick().await.unwrap();
    assert_eq!(
        bot.running
            .as_ref()
            .unwrap()
            .release
            .as_ref()
            .unwrap()
            .digest,
        candidate.digest
    );
    let elapsed = READY_TIMEOUT + Duration::from_secs(1);
    let child = bot.running.as_mut().unwrap();
    child.started_at = now() - elapsed.as_secs() as i64;
    child.started = Instant::now() - elapsed;
    bot.tick().await.unwrap();
    assert_eq!(
        bot.running
            .as_ref()
            .unwrap()
            .release
            .as_ref()
            .unwrap()
            .digest,
        installed.digest,
        "a readiness timeout after a recovered spawn must roll back"
    );
    assert!(
        bot.control
            .component("bot")
            .await
            .unwrap()
            .desired
            .is_none()
    );
    assert!(
        bot.control
            .rejected_candidate_digest("bot")
            .await
            .unwrap()
            .is_none()
    );
    wait_for_bot_ready(&mut bot).await;
    assert_eq!(
        independent_exit_starts(&root).await,
        [
            "desired-bot.start",
            "desired-bot.start",
            "installed-bot.start"
        ]
    );
    bot.stop_application().await.unwrap();
    bot.end_idle().await.unwrap();
    server.abort();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn failed_rejected_marker_write_is_retried_before_restart() {
    let (root, db, mut bot, candidate, server) = independent_exiting_bot_prepared(
        jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE,
        |bot| bot.fail_rejected_candidate_write = true,
    )
    .await;
    bot.tick().await.unwrap();
    assert!(bot.restart.held);
    assert!(bot.running.is_none());
    assert!(
        bot.control
            .rejected_candidate_digest("bot")
            .await
            .unwrap()
            .is_none(),
        "a failed marker write leaves the candidate unsaved"
    );
    bot.fail_rejected_candidate_write = false;
    bot.tick().await.unwrap();
    assert!(bot.restart.held);
    assert!(bot.running.is_none());
    assert_eq!(
        bot.control
            .rejected_candidate_digest("bot")
            .await
            .unwrap()
            .as_deref(),
        Some(candidate.digest.as_str()),
        "the next poll must save the marker while the hold is still active"
    );
    let mut bot = reopen_supervisor(bot).await;
    tokio::fs::remove_file(root.join("exit-code-desired-bot"))
        .await
        .unwrap();
    bot.tick().await.unwrap();
    assert_eq!(
        bot.running
            .as_ref()
            .unwrap()
            .release
            .as_ref()
            .unwrap()
            .digest,
        candidate.digest
    );
    bot.stop_application().await.unwrap();
    bot.end_idle().await.unwrap();
    server.abort();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn installed_running_receipt_keeps_rejected_candidate() {
    let (root, db, mut bot, candidate, server) =
        independent_exiting_bot(jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE).await;
    bot.tick().await.unwrap();
    let state = bot.control.component("bot").await.unwrap();
    let installed = state.installed.clone().unwrap();
    let seen = state.observed.clone().unwrap();
    bot.control
        .running("bot", Some(installed.clone()), seen, now())
        .await
        .unwrap();
    assert_eq!(
        bot.control
            .rejected_candidate_digest("bot")
            .await
            .unwrap()
            .as_deref(),
        Some(candidate.digest.as_str()),
        "a running receipt for the installed release must keep the saved candidate"
    );
    assert_eq!(
        bot.control
            .component("bot")
            .await
            .unwrap()
            .desired
            .unwrap()
            .digest,
        candidate.digest
    );
    let mut bot = reopen_supervisor(bot).await;
    tokio::fs::remove_file(root.join("exit-code-desired-bot"))
        .await
        .unwrap();
    bot.tick().await.unwrap();
    assert_eq!(
        bot.running
            .as_ref()
            .unwrap()
            .release
            .as_ref()
            .unwrap()
            .digest,
        candidate.digest
    );
    bot.stop_application().await.unwrap();
    bot.end_idle().await.unwrap();
    server.abort();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn selection_change_away_and_back_does_not_revive_rejected_candidate() {
    let (root, db, mut bot, candidate, server) =
        independent_exiting_bot(jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE).await;
    bot.tick().await.unwrap();
    let installed = bot
        .control
        .component("bot")
        .await
        .unwrap()
        .installed
        .unwrap();
    let mut bytes = tokio::fs::read(root.join("desired-bot")).await.unwrap();
    bytes.push(0);
    let mut objects = HashMap::new();
    let mut other = image(&mut objects, "bot", &bytes);
    other.min_schema = db::CORE_SCHEMA_VERSION;
    other.max_schema = db::CORE_SCHEMA_VERSION;
    other.result_schema = db::CORE_SCHEMA_VERSION;
    other.upgrade.as_mut().unwrap().guild_schema = db::guild_data::SCHEMA_VERSION;
    bot.control
        .checked("bot", "latest", Some(other.clone()), None, now())
        .await
        .unwrap();
    db::request_component_install(&db, "bot", &other.digest, now())
        .await
        .unwrap();
    assert!(
        bot.control
            .rejected_candidate_digest("bot")
            .await
            .unwrap()
            .is_none(),
        "selecting a different release clears the saved candidate"
    );
    bot.control
        .checked("bot", "latest", Some(candidate.clone()), None, now())
        .await
        .unwrap();
    db::request_component_install(&db, "bot", &candidate.digest, now())
        .await
        .unwrap();
    assert!(
        bot.control
            .rejected_candidate_digest("bot")
            .await
            .unwrap()
            .is_none(),
        "selecting the rejected release again must not restore its marker"
    );
    tokio::fs::write(root.join("hold-installed-bot-ready"), "")
        .await
        .unwrap();
    tokio::fs::remove_file(root.join("exit-code-desired-bot"))
        .await
        .unwrap();
    let mut bot = reopen_supervisor(bot).await;
    bot.tick().await.unwrap();
    assert_eq!(
        bot.running
            .as_ref()
            .unwrap()
            .release
            .as_ref()
            .unwrap()
            .digest,
        installed.digest,
        "without the marker, restart restores the installed release first"
    );
    assert!(
        bot.control
            .rejected_candidate_digest("bot")
            .await
            .unwrap()
            .is_none()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let starts = loop {
        let starts = independent_exit_starts(&root).await;
        if starts.len() >= 2 || Instant::now() >= deadline {
            break starts;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(starts, ["desired-bot.start", "installed-bot.start"]);
    bot.stop_application().await.unwrap();
    bot.end_idle().await.unwrap();
    server.abort();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

async fn exiting_bot_with_alternate() -> (
    PathBuf,
    Db,
    Supervisor,
    ComponentRelease,
    ComponentRelease,
    tokio::task::JoinHandle<()>,
) {
    let root = std::env::temp_dir().join(format!(
        "rejected-alternate-{:032x}",
        rand::random::<u128>()
    ));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let baked = executable(&root, "baked-bot").await;
    let installed_path = executable(&root, "installed-bot").await;
    let desired_path = executable(&root, "desired-bot").await;
    let alternate_path = executable(&root, "other-bot").await;
    let mut objects = HashMap::new();
    let mut releases = Vec::new();
    for path in [installed_path, desired_path, alternate_path] {
        let mut release = image(&mut objects, "bot", &tokio::fs::read(path).await.unwrap());
        release.min_schema = db::CORE_SCHEMA_VERSION;
        release.max_schema = db::CORE_SCHEMA_VERSION;
        release.result_schema = db::CORE_SCHEMA_VERSION;
        release.upgrade.as_mut().unwrap().guild_schema = db::guild_data::SCHEMA_VERSION;
        releases.push(release);
    }
    let alternate = releases.pop().unwrap();
    let candidate = releases.pop().unwrap();
    let installed = releases.pop().unwrap();
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
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
    let mut bot = controller("bot", baked, registry, control.clone(), &root);
    wait_for_bot_ready(&mut bot).await;
    let seen = control.component("bot").await.unwrap().observed.unwrap();
    control.running("api", None, seen, now()).await.unwrap();
    control
        .checked("bot", "latest", Some(installed.clone()), None, now())
        .await
        .unwrap();
    db::request_component_install(&db, "bot", &installed.digest, now())
        .await
        .unwrap();
    bot.tick().await.unwrap();
    wait_for_bot_ready(&mut bot).await;
    control
        .checked("bot", "latest", Some(candidate.clone()), None, now())
        .await
        .unwrap();
    db::request_component_install(&db, "bot", &candidate.digest, now())
        .await
        .unwrap();
    publish_exit_code(
        root.join("exit-code-desired-bot"),
        jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE,
    )
    .await;
    tokio::fs::write(root.join("events"), "").await.unwrap();
    bot.tick().await.unwrap();
    // The candidate exits 78 as soon as it starts. This tick can leave that
    // child running, or settle_exited_child can already have held it and
    // cleared `running`.
    if let Some(child) = bot.running.as_mut() {
        assert_eq!(
            child.release.as_ref().unwrap().digest,
            candidate.digest,
            "the immediate exit must belong to the desired candidate"
        );
        child.child.wait().await.unwrap();
    } else {
        assert!(
            bot.restart.held,
            "a settled token exit must be inside the rejected-token hold"
        );
        assert!(!bot.exited_for_restart);
        let recorded = bot.rejected_candidate.clone().or_else(|| {
            bot.restart_target
                .as_ref()
                .and_then(|target| target.release.clone())
        });
        assert_eq!(
            recorded.expect("held exit recorded the candidate").digest,
            candidate.digest
        );
        let events = tokio::fs::read_to_string(root.join("events"))
            .await
            .unwrap();
        assert!(
            !events.lines().any(|line| line.ends_with(".ready")),
            "an immediate token rejection must not publish readiness"
        );
    }
    (root, db, bot, candidate, alternate, server)
}

#[tokio::test]
async fn rejected_marker_tracks_the_candidate_digest() {
    let (root, db, mut bot, candidate, alternate, server) = exiting_bot_with_alternate().await;
    bot.tick().await.unwrap();
    assert_eq!(
        bot.persisted_candidate_digest.as_deref(),
        Some(candidate.digest.as_str())
    );
    tokio::fs::remove_file(root.join("exit-code-desired-bot"))
        .await
        .unwrap();
    tokio::fs::write(root.join("hold-desired-bot-ready"), "")
        .await
        .unwrap();
    bot.restart.auth_retry_at = Some(Instant::now());
    bot.tick().await.unwrap();
    assert_eq!(
        bot.running
            .as_ref()
            .unwrap()
            .release
            .as_ref()
            .unwrap()
            .digest,
        candidate.digest
    );
    assert!(bot.rejected_candidate.is_none());
    assert_eq!(
        bot.persisted_candidate_digest.as_deref(),
        Some(candidate.digest.as_str()),
        "starting the saved candidate must not forget which digest was saved"
    );
    bot.stop_application().await.unwrap();
    bot.control
        .checked("bot", "latest", Some(alternate.clone()), None, now())
        .await
        .unwrap();
    db::request_component_install(&db, "bot", &alternate.digest, now())
        .await
        .unwrap();
    assert!(
        bot.control
            .rejected_candidate_digest("bot")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        bot.persisted_candidate_digest.as_deref(),
        Some(candidate.digest.as_str())
    );
    publish_exit_code(
        root.join("exit-code-other-bot"),
        jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE,
    )
    .await;
    let state = bot.control.component("bot").await.unwrap();
    bot.standby = Some(bot.installed_fallback(&state).await.unwrap());
    let registry = bot.registry.clone();
    let staging = bot.staging.clone();
    let path = stage(&registry, &alternate, "jmodguard-bot", &staging)
        .await
        .unwrap();
    bot.start(&path, &alternate.source_sha, Some(alternate.clone()))
        .await
        .unwrap();
    bot.running.as_mut().unwrap().child.wait().await.unwrap();
    bot.tick().await.unwrap();
    assert!(bot.restart.held);
    assert_eq!(
        bot.control
            .rejected_candidate_digest("bot")
            .await
            .unwrap()
            .as_deref(),
        Some(alternate.digest.as_str()),
        "a rejection of a different release must replace the saved digest"
    );
    assert_eq!(
        bot.persisted_candidate_digest.as_deref(),
        Some(alternate.digest.as_str())
    );
    let mut bot = reopen_supervisor(bot).await;
    tokio::fs::remove_file(root.join("exit-code-other-bot"))
        .await
        .unwrap();
    bot.tick().await.unwrap();
    assert_eq!(
        bot.running
            .as_ref()
            .unwrap()
            .release
            .as_ref()
            .unwrap()
            .digest,
        alternate.digest,
        "restart must start the replacement candidate, not the installed bot"
    );
    assert!(
        !independent_exit_starts(&root)
            .await
            .iter()
            .any(|line| line == "installed-bot.start" || line == "baked-bot.start")
    );
    bot.stop_application().await.unwrap();
    bot.end_idle().await.unwrap();
    server.abort();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn selected_auth_rejection_survives_legacy_readiness_deadline() {
    for restored in [false, true] {
        for after_ready in [false, true] {
            let (root, db, mut bot, _, server) = selected_exiting_bot(
                jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE,
                restored,
                after_ready,
                |_| {},
            )
            .await;
            let before = bot.control.component("bot").await.unwrap();
            let starts = independent_exit_starts(&root).await;
            let ready_file = bot.running.as_ref().map(|child| child.ready_file.clone());
            let elapsed = READY_TIMEOUT + Duration::from_secs(1);
            if let Some(child) = bot.running.as_mut() {
                // Advance both child ages beyond the historical 120s deadline.
                child.started_at = now() - elapsed.as_secs() as i64;
                child.started = Instant::now() - elapsed;
            } else {
                assert!(
                    bot.restart.held,
                    "an already settled token exit must be held"
                );
            }
            assert!(!bot.tick().await.unwrap());
            assert!(bot.restart.held);
            // Advance the hold clock by the same interval, still before fallback retry.
            if ready_file.is_some() {
                bot.restart.auth_retry_at = bot.restart.auth_retry_at.map(|at| at - elapsed);
            }
            for _ in 0..3 {
                assert!(!bot.tick().await.unwrap());
                assert!(bot.running.is_none());
                assert!(bot.restart.held);
            }
            let after = bot.control.component("bot").await.unwrap();
            assert_eq!(after.installed, before.installed);
            assert_eq!(after.desired, before.desired);
            assert_eq!(after.state, before.state);
            assert_eq!(after.last_error, before.last_error);
            assert_eq!(independent_exit_starts(&root).await, starts);
            if let Some(ready_file) = ready_file {
                assert!(!ready_file.exists(), "hold must not fabricate readiness");
            } else {
                let events = tokio::fs::read_to_string(root.join("events"))
                    .await
                    .unwrap();
                assert!(
                    !events.lines().any(|line| line.ends_with(".ready")),
                    "hold must not fabricate readiness"
                );
            }
            bot.end_idle().await.unwrap();
            server.abort();
            let _ = server.await;
            drop((bot, db));
            tokio::fs::remove_dir_all(root).await.unwrap();
        }
    }
}

#[tokio::test]
async fn selected_auth_hold_requires_capable_launcher_before_spawn() {
    let root = std::env::temp_dir().join(format!("auth-admission-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let (db, control) = open_launcher_db(&root).await;
    let mut bot = offline_controller("bot", control, &root);
    let release = release_at("bot", 'c', db::CORE_SCHEMA_VERSION);
    for revision in 0..7 {
        bot.launcher_revision = revision;
        // A nonexistent executable ensures rejection occurs at admission, before spawn.
        let error = bot
            .start(
                &root.join("must-not-start"),
                &release.source_sha,
                Some(release.clone()),
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("launcher update required before application start"),
            "{error:#}"
        );
        assert!(bot.running.is_none());
    }
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn baked_auth_rejection_after_ready_is_held() {
    let root = std::env::temp_dir().join(format!("baked-auth-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let child = executable(&root, "baked-bot").await;
    let (db, control) = open_launcher_db(&root).await;
    let mut bot = offline_controller("bot", control, &root);
    bot.start(&child, &"b".repeat(40), None).await.unwrap();
    wait_for_bot_ready(&mut bot).await;
    assert!(bot.running.as_ref().unwrap().release.is_none());
    let ready_file = bot.running.as_ref().unwrap().ready_file.clone();
    assert!(ready_file.exists());
    publish_exit_code(
        root.join("exit-code-baked-bot"),
        jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE,
    )
    .await;
    bot.running.as_mut().unwrap().child.wait().await.unwrap();
    for _ in 0..3 {
        assert!(!bot.tick().await.unwrap());
        assert!(bot.restart.held);
        assert!(bot.running.is_none());
    }
    assert!(!ready_file.exists());
    assert_eq!(independent_exit_starts(&root).await, ["baked-bot.start"]);
    bot.end_idle().await.unwrap();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

async fn registry_with_objects(
    objects: HashMap<String, Vec<u8>>,
) -> (Registry, tokio::task::JoinHandle<()>) {
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
    (registry, server)
}

#[tokio::test]
async fn bad_signature_leaves_running_child_without_rollback() {
    let root = std::env::temp_dir().join(format!("sig-rollback-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let running = executable(&root, "running-bot").await;
    let candidate_bin = executable(&root, "candidate-bot").await;
    let mut objects = HashMap::new();
    let release = image(
        &mut objects,
        "bot",
        &tokio::fs::read(candidate_bin).await.unwrap(),
    );
    let tag = signature_tag_for_digest(&release.digest).unwrap();
    objects.insert(tag, b"broken-signature".to_vec());
    let (registry, server) = registry_with_objects(objects).await;
    let (db, control) = open_launcher_db(&root).await;
    control
        .checked("bot", "latest", Some(release.clone()), None, now())
        .await
        .unwrap();
    db::request_component_install(&db, "bot", &release.digest, now())
        .await
        .unwrap();
    let mut bot = controller("bot", running.clone(), registry, control.clone(), &root);
    bot.start(&running, &"b".repeat(40), None).await.unwrap();
    wait_for_bot_ready(&mut bot).await;
    let seen = control.component("bot").await.unwrap().observed.unwrap();
    control.running("api", None, seen, now()).await.unwrap();
    bot.reported = true;
    for _ in 0..10 {
        bot.tick().await.unwrap();
    }
    assert!(bot.running.is_some());
    assert!(bot.rollback.is_none());
    server.abort();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn restart_ignores_leftover_stage_file() {
    let root = std::env::temp_dir().join(format!("sig-stage-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let baked = executable(&root, "baked-bot").await;
    let mut objects = HashMap::new();
    let mut release = image(
        &mut objects,
        "bot",
        &tokio::fs::read(baked.clone()).await.unwrap(),
    );
    release.min_schema = db::CORE_SCHEMA_VERSION;
    release.max_schema = db::CORE_SCHEMA_VERSION;
    release.result_schema = db::CORE_SCHEMA_VERSION;
    release.upgrade.as_mut().unwrap().guild_schema = db::guild_data::SCHEMA_VERSION;
    let (registry, server) = registry_with_objects(objects).await;
    let (db, control) = open_launcher_db(&root).await;
    let release_json = serde_json::to_string(&release).unwrap();
    db.with_conn(|conn| {
        conn.execute_bounded(
            "UPDATE component_updates SET installed=?1, state='running' WHERE component='bot'",
            params![release_json],
            64 * 1024,
        )?;
        Ok(())
    })
    .await
    .unwrap();
    let staging = root.join("staged-bot");
    tokio::fs::create_dir_all(&staging).await.unwrap();
    tokio::fs::write(staging.join(".stage-deadbeef"), b"short")
        .await
        .unwrap();
    let mut bot = controller("bot", baked.clone(), registry, control.clone(), &root);
    let stage_marker = staging.join(".stage-deadbeef");
    bot.staging = staging;
    bot.running = None;
    for _ in 0..20 {
        if bot.tick().await.unwrap() {
            break;
        }
    }
    assert!(bot.running.is_some());
    assert!(!tokio::fs::try_exists(stage_marker).await.unwrap());
    server.abort();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn boot_repair_starts_baked_when_signature_missing_and_schema_fits() {
    let root = std::env::temp_dir().join(format!("sig-boot-{:032x}", rand::random::<u128>()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let baked = executable(&root, "baked-bot").await;
    let mut objects = HashMap::new();
    let release = image(
        &mut objects,
        "bot",
        &tokio::fs::read(baked.clone()).await.unwrap(),
    );
    let tag = signature_tag_for_digest(&release.digest).unwrap();
    objects.remove(&tag);
    let (registry, server) = registry_with_objects(objects).await;
    let (db, control) = open_launcher_db(&root).await;
    let mut release = release;
    release.min_schema = db::CORE_SCHEMA_VERSION;
    release.max_schema = db::CORE_SCHEMA_VERSION;
    release.result_schema = db::CORE_SCHEMA_VERSION;
    release.upgrade.as_mut().unwrap().guild_schema = db::guild_data::SCHEMA_VERSION;
    let release_json = serde_json::to_string(&release).unwrap();
    let observed = db::ComponentObservation {
        guild_schema: Some(db::guild_data::SCHEMA_VERSION),
        update_protocol: db::UPDATE_CONTROL_PROTOCOL,
        launcher_revision: db::LAUNCHER_REVISION,
        source_sha: "b".repeat(40),
        executable_sha256: "a".repeat(64),
        admin_sha256: None,
        base_image_sha: "b".repeat(40),
        started_at: 100,
        min_schema: db::CORE_SCHEMA_VERSION,
        max_schema: db::CORE_SCHEMA_VERSION,
        api_capability: 1,
        self_update_enabled: true,
        base_abi: "debian-bookworm-amd64".into(),
    };
    let observed_json = serde_json::to_string(&observed).unwrap();
    db.with_conn(move |conn| {
        conn.execute_bounded(
            "UPDATE component_updates SET installed=?1, observed=?2, observed_at=100, state='running' WHERE component='bot'",
            params![release_json, observed_json],
            64 * 1024,
        )?;
        Ok(())
    })
    .await
    .unwrap();
    let mut bot = controller("bot", baked, registry, control.clone(), &root);
    bot.running = None;
    for _ in 0..20 {
        bot.tick().await.unwrap();
    }
    assert!(bot.running.is_some());
    let mismatches = db::read_boot_mismatch_map(&db).await.unwrap();
    assert_eq!(mismatches["bot"].reason, UNSIGNED_IMAGE);
    server.abort();
    drop((bot, db));
    tokio::fs::remove_dir_all(root).await.unwrap();
}
