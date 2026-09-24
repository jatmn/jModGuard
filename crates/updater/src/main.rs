//! Role-specific application supervisor. Bunny still selects the base image
//! manually; this process can replace only the child executable after the
//! explicit deployment gate is enabled.
use anyhow::{Context, Result, bail, ensure};
use jmodguard::config::{ApiProcessConfig, BotProcessConfig};
use jmodguard::db::{self, ComponentObservation, ComponentRelease, Db};
use jmodguard_updater::compatible;
use jmodguard_updater::oci::{Registry, sha256_hex};
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};

const POLL: Duration = Duration::from_secs(5);
const READY_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, PartialEq, Eq)]
enum ServiceReadiness {
    Ready,
    Shutdown,
}

struct Running {
    child: Child,
    ready_file: PathBuf,
    source_sha: String,
    executable_sha256: String,
    started_at: i64,
    release: Option<ComponentRelease>,
    path: PathBuf,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn role() -> Result<String> {
    let value = std::env::var("JMODGUARD_ROLE").context("JMODGUARD_ROLE is required")?;
    ensure!(
        matches!(value.as_str(), "bot" | "api"),
        "invalid launcher role"
    );
    Ok(value)
}

fn update_enabled() -> bool {
    std::env::var("JMODGUARD_SELF_UPDATE").as_deref() == Ok("1")
}

fn binary_path(role: &str) -> PathBuf {
    PathBuf::from(format!("/usr/local/bin/jmodguard-{role}"))
}

fn ready_path() -> PathBuf {
    std::env::temp_dir().join(format!("jmodguard-ready-{:032x}", rand::random::<u128>()))
}

async fn launch(
    path: &Path,
    source_sha: &str,
    release: Option<ComponentRelease>,
) -> Result<Running> {
    let ready_file = ready_path();
    let bytes = tokio::fs::read(path).await?;
    let hash = sha256_hex(&bytes);
    let child = Command::new(path)
        .env("JMODGUARD_READY_FILE", &ready_file)
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to launch {}", path.display()))?;
    Ok(Running {
        child,
        ready_file,
        source_sha: source_sha.to_owned(),
        executable_sha256: hash,
        started_at: now(),
        release,
        path: path.to_owned(),
    })
}

async fn wait_ready(running: &mut Running, timeout: Option<Duration>) -> Result<()> {
    let start = Instant::now();
    loop {
        if let Some(status) = running.child.try_wait()? {
            bail!("child exited before readiness: {status}");
        }
        if tokio::fs::metadata(&running.ready_file).await.is_ok() {
            return Ok(());
        }
        if let Some(limit) = timeout {
            ensure!(
                start.elapsed() < limit,
                "replacement child readiness timed out"
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The baked or prior service already owns the container's process-local
/// health listener. Wait for its real readiness without a cutover deadline;
/// a delayed Discord Gateway Ready must not kill an otherwise live bot.
async fn wait_service_ready(
    running: &mut Running,
    shutdown: impl Future<Output = ()>,
) -> Result<ServiceReadiness> {
    let outcome = tokio::select! {
        result = wait_ready(running, None) => { result?; ServiceReadiness::Ready },
        _ = shutdown => ServiceReadiness::Shutdown,
    };
    if outcome == ServiceReadiness::Shutdown {
        stop(running).await?;
    }
    Ok(outcome)
}

async fn stop(running: &mut Running) -> Result<()> {
    if let Some(pid) = running.child.id() {
        // A child handles SIGTERM through the shared shutdown latch. The
        // launcher remains alive to reap it and to perform rollback.
        let rc = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    if tokio::time::timeout(Duration::from_secs(30), running.child.wait())
        .await
        .is_err()
    {
        running.child.kill().await?;
    }
    let _ = tokio::fs::remove_file(&running.ready_file).await;
    Ok(())
}

async fn stage(registry: &Registry, release: &ComponentRelease, name: &str) -> Result<PathBuf> {
    let path = format!("usr/local/bin/{name}");
    let files = registry
        .files(release, &[&path, "usr/local/share/jmodguard/files.sha256"])
        .await?;
    let binary = files
        .get(&path)
        .context("image did not include expected executable")?;
    ensure!(
        binary.starts_with(b"\x7fELF") && binary.len() <= 64 * 1024 * 1024,
        "invalid linux/amd64 executable"
    );
    let manifest = std::str::from_utf8(
        files
            .get("usr/local/share/jmodguard/files.sha256")
            .context("image file inventory is missing")?,
    )?;
    let hash = sha256_hex(binary);
    ensure!(
        manifest
            .lines()
            .any(|line| line == format!("{hash}  bin/{name}")),
        "executable does not match image file inventory"
    );
    // PID 1 reads only this fixed directory; TMPDIR is allowed to differ.
    let dir = PathBuf::from("/tmp/jmodguard-updates");
    tokio::fs::create_dir_all(&dir).await?;
    let target = dir.join(format!("{}-{hash}", release.component));
    let temp = dir.join(format!(".stage-{:032x}", rand::random::<u128>()));
    tokio::fs::write(&temp, binary).await?;
    tokio::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o700)).await?;
    tokio::fs::rename(&temp, &target).await?;
    ensure!(
        sha256_hex(&tokio::fs::read(&target).await?) == hash,
        "staged binary hash mismatch"
    );
    Ok(target)
}

async fn database(role: &str) -> Result<Db> {
    let opts = if role == "bot" {
        BotProcessConfig::load()?.db_open_options()
    } else {
        ApiProcessConfig::load()?.db_open_options()
    };
    let (attempts, delay) = db::startup_open_retry(&opts);
    Db::open_with_until_ready(opts, attempts, delay).await
}

fn observation(running: &Running, base_sha: &str) -> ComponentObservation {
    ComponentObservation {
        source_sha: running.source_sha.clone(),
        executable_sha256: running.executable_sha256.clone(),
        admin_sha256: None,
        base_image_sha: base_sha.into(),
        started_at: running.started_at,
        min_schema: 15,
        max_schema: 15,
        api_capability: 1,
        self_update_enabled: update_enabled(),
        base_abi: std::env::var("JMODGUARD_BASE_ABI").unwrap_or_else(|_| "unknown".into()),
    }
}

async fn maybe_update_launcher(
    db: &Db,
    registry: &Registry,
    role: &str,
    state: &db::ComponentUpdate,
) -> Result<bool> {
    let Some(release) = &state.desired_launcher else {
        return Ok(false);
    };
    if state
        .installed_launcher
        .as_ref()
        .is_some_and(|v| v.digest == release.digest)
    {
        return Ok(false);
    }
    ensure!(
        release.component == role && release.launcher_protocol == 1,
        "incompatible release role or launcher protocol"
    );
    compatible(db, release, now()).await?;
    let path = stage(registry, release, "jmodguard-launcher").await?;
    let hash = sha256_hex(&tokio::fs::read(&path).await?);
    let control = PathBuf::from("/tmp/jmodguard-launcher-next");
    let contents = format!("{}\n{}\n{}\n", path.display(), hash, release.digest);
    tokio::fs::write(&control, contents).await?;
    Ok(true)
}

#[tokio::main]
async fn main() -> Result<()> {
    let role = role()?;
    let base_sha = std::env::var("JMODGUARD_BASE_SHA").unwrap_or_else(|_| "unknown".into());
    let baked = binary_path(&role);
    let mut running = launch(&baked, &base_sha, None).await?;
    let db = tokio::select! {
        opened=database(&role) => opened?,
        exited=running.child.wait() => bail!("baked service exited before database opened: {}",exited?),
        _=jmodguard::shutdown_signal() => { stop(&mut running).await?; return Ok(()); },
    };
    if wait_service_ready(&mut running, jmodguard::shutdown_signal()).await?
        == ServiceReadiness::Shutdown
    {
        return Ok(());
    }
    db::record_baked_component_running(&db, &role, observation(&running, &base_sha), now()).await?;
    let registry = Registry::ghcr()?;
    let mut standby: Option<(PathBuf, String, Option<ComponentRelease>)> = None;
    if let Ok(digest) = std::env::var("JMODGUARD_LAUNCHER_RELEASE") {
        let state = db::component_update(&db, &role).await?;
        let release = state
            .desired_launcher
            .context("launcher request was cleared before handoff")?;
        ensure!(
            release.digest == digest,
            "launcher request changed before handoff"
        );
        let hash = sha256_hex(&tokio::fs::read(std::env::current_exe()?).await?);
        db::record_launcher_running(&db, &role, &release, &hash, now()).await?;
    } else {
        let hash = sha256_hex(&tokio::fs::read(std::env::current_exe()?).await?);
        db::record_baked_launcher(&db, &role, &hash, now()).await?;
    }
    if let Ok(failed) = std::env::var("JMODGUARD_LAUNCHER_FAILED") {
        db::record_launcher_failure(
            &db,
            &role,
            &failed,
            "updated launcher failed; using baked launcher",
        )
        .await?;
    }
    let mut tick = tokio::time::interval(POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_observed: Option<Instant> = None;
    loop {
        tokio::select! {
            _=jmodguard::shutdown_signal() => { stop(&mut running).await?; return Ok(()); }
            result=running.child.wait() => {
                let status=result?;
                if running.release.is_some() {
                    let failed=running.release.clone().expect("updated child has release");
                    let (path,sha,release)=standby.take().unwrap_or_else(||(baked.clone(),base_sha.clone(),None));
                    running=launch(&path,&sha,release.clone()).await?;
                    if wait_service_ready(&mut running, jmodguard::shutdown_signal()).await? == ServiceReadiness::Shutdown { return Ok(()); }
                    db::record_component_rollback(&db,&role,&failed,release,observation(&running,&base_sha),&format!("updated child exited: {status}"),now()).await?;
                } else { bail!("baked service exited: {status}"); }
            }
            _=tick.tick() => {
                if last_observed.is_none_or(|at|at.elapsed()>=Duration::from_secs(60)) {
                    if let Err(error)=db::record_component_running(&db,&role,None,observation(&running,&base_sha),now()).await {
                        tracing::warn!("release heartbeat failed: {error:#}");
                    } else { last_observed=Some(Instant::now()); }
                }
                if !update_enabled() { continue; }
                let state=match db::component_update(&db,&role).await { Ok(v)=>v,Err(error)=>{tracing::warn!("update state unavailable: {error:#}");continue;} };
                match maybe_update_launcher(&db,&registry,&role,&state).await {
                    Ok(true)=>{ stop(&mut running).await?; std::process::exit(77); },
                    Ok(false)=>{},
                    Err(error)=>{
                        tracing::warn!("launcher update rejected: {error:#}");
                        if let Some(release)=&state.desired_launcher {
                            db::record_launcher_failure(&db,&role,&release.digest,&format!("launcher update: {error:#}")).await?;
                        }
                    },
                }
                let Some(candidate)=state.desired else { continue; };
                if state.installed.as_ref().is_some_and(|release| release.digest==candidate.digest)
                    && running.release.as_ref().is_some_and(|release| release.digest==candidate.digest) { continue; }
                if candidate.component!=role || candidate.launcher_protocol!=1 {
                    db::record_component_failure(&db,&role,&candidate,"incompatible release role or launcher protocol").await?;
                    continue;
                }
                if let Err(error)=compatible(&db,&candidate,now()).await {
                    db::record_component_failure(&db,&role,&candidate,&format!("compatibility check: {error:#}")).await?;
                    continue;
                }
                let new_path=match stage(&registry,&candidate,&format!("jmodguard-{role}")).await {
                    Ok(path)=>path,
                    Err(error)=>{db::record_component_failure(&db,&role,&candidate,&format!("download: {error:#}")).await?;continue;}
                };
                let previous_path=running.path.clone();
                let previous_sha=running.source_sha.clone();
                let previous_release=running.release.clone();
                stop(&mut running).await?;
                let replacement=launch(&new_path,&candidate.source_sha,Some(candidate.clone())).await;
                let switched=match replacement {
                    Ok(mut child)=>match wait_ready(&mut child, Some(READY_TIMEOUT)).await {
                        Ok(())=>Ok(child),
                        Err(error)=>{let _=stop(&mut child).await;Err(error)}
                    },
                    Err(error)=>Err(error),
                };
                match switched {
                    Ok(child)=>{
                        running=child;
                        if let Err(error)=db::record_component_running(&db,&role,Some(candidate.clone()),observation(&running,&base_sha),now()).await {
                            stop(&mut running).await?;
                            running=launch(&previous_path,&previous_sha,previous_release).await?;
                            if wait_service_ready(&mut running, jmodguard::shutdown_signal()).await? == ServiceReadiness::Shutdown { return Ok(()); }
                            db::record_component_rollback(&db,&role,&candidate,running.release.clone(),observation(&running,&base_sha),&format!("activation record: {error:#}"),now()).await?;
                        } else {
                            standby=Some((previous_path,previous_sha,previous_release));
                        }
                    }
                    Err(error)=>{
                        running=launch(&previous_path,&previous_sha,previous_release).await?;
                        if wait_service_ready(&mut running, jmodguard::shutdown_signal()).await? == ServiceReadiness::Shutdown { return Ok(()); }
                        db::record_component_rollback(&db,&role,&candidate,running.release.clone(),observation(&running,&base_sha),&format!("readiness: {error:#}"),now()).await?;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn readiness_requires_live_child_marker_and_stop_reaps_it() {
        let dir = std::env::temp_dir().join(format!(
            "jmodguard-launch-test-{:032x}",
            rand::random::<u128>()
        ));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let bad = dir.join("bad");
        tokio::fs::write(&bad, b"#!/bin/sh\nexit 1\n")
            .await
            .unwrap();
        tokio::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o700))
            .await
            .unwrap();
        let mut first = launch(&bad, "test", None).await.unwrap();
        assert!(wait_ready(&mut first, Some(READY_TIMEOUT)).await.is_err());
        let good = dir.join("good");
        tokio::fs::write(
            &good,
            b"#!/bin/sh\nprintf ready > \"$JMODGUARD_READY_FILE\"\nexec sleep 10\n",
        )
        .await
        .unwrap();
        tokio::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o700))
            .await
            .unwrap();
        let mut second = launch(&good, "test", None).await.unwrap();
        wait_ready(&mut second, Some(READY_TIMEOUT)).await.unwrap();
        stop(&mut second).await.unwrap();
        assert!(second.child.try_wait().unwrap().is_some());
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn baked_wait_survives_delayed_ready_but_replacement_has_a_deadline() {
        let dir = std::env::temp_dir().join(format!(
            "jmodguard-delayed-ready-{:032x}",
            rand::random::<u128>()
        ));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let delayed = dir.join("delayed");
        let allow_ready = dir.join("allow-ready");
        tokio::fs::write(
            &delayed,
            format!(
                "#!/bin/sh\nwhile [ ! -f \"{}\" ]; do sleep 0.05; done\nprintf ready > \"$JMODGUARD_READY_FILE\"\nexec sleep 10\n",
                allow_ready.display()
            ),
        )
        .await
        .unwrap();
        tokio::fs::set_permissions(&delayed, std::fs::Permissions::from_mode(0o700))
            .await
            .unwrap();

        let mut replacement = launch(&delayed, "test", None).await.unwrap();
        assert!(
            wait_ready(&mut replacement, Some(Duration::from_millis(100)))
                .await
                .is_err()
        );
        stop(&mut replacement).await.unwrap();

        let mut baked = launch(&delayed, "test", None).await.unwrap();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(350),
                wait_service_ready(&mut baked, std::future::pending::<()>())
            )
            .await
            .is_err()
        );
        assert!(baked.child.try_wait().unwrap().is_none());
        tokio::fs::write(&allow_ready, b"go").await.unwrap();
        assert_eq!(
            wait_service_ready(&mut baked, std::future::pending::<()>())
                .await
                .unwrap(),
            ServiceReadiness::Ready
        );
        stop(&mut baked).await.unwrap();
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn baked_wait_reaps_on_shutdown_and_reports_child_exit() {
        let dir = std::env::temp_dir().join(format!(
            "jmodguard-baked-wait-{:032x}",
            rand::random::<u128>()
        ));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let sleeping = dir.join("sleeping");
        tokio::fs::write(&sleeping, b"#!/bin/sh\nexec sleep 10\n")
            .await
            .unwrap();
        tokio::fs::set_permissions(&sleeping, std::fs::Permissions::from_mode(0o700))
            .await
            .unwrap();
        let mut baked = launch(&sleeping, "test", None).await.unwrap();
        assert_eq!(
            wait_service_ready(&mut baked, tokio::time::sleep(Duration::from_millis(50)))
                .await
                .unwrap(),
            ServiceReadiness::Shutdown
        );
        assert!(baked.child.try_wait().unwrap().is_some());

        let exiting = dir.join("exiting");
        tokio::fs::write(&exiting, b"#!/bin/sh\nexit 1\n")
            .await
            .unwrap();
        tokio::fs::set_permissions(&exiting, std::fs::Permissions::from_mode(0o700))
            .await
            .unwrap();
        let mut failed = launch(&exiting, "test", None).await.unwrap();
        assert!(
            wait_service_ready(&mut failed, std::future::pending::<()>())
                .await
                .is_err()
        );
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}
