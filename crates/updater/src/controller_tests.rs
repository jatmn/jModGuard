//! Real supervisor, OCI extraction, SQLite operation state and reaped OS children.
//! Tiny ELF fixtures stand in for Discord/application work; their readiness and
//! exit behavior are real, while the test explicitly simulates a future migration.
use super::*;
use axum::{Router, body::Body, extract::State, http::Uri, response::Response, routing::get};
use db::{Db, DbOpenOptions};
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
        let inventory = format!("{}  bin/jmodguard-{role}\n", sha256_hex(binary));
        for (path, bytes) in [
            (name.as_str(), binary),
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
        baked,
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
