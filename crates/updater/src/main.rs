//! Role-specific application supervisor with durable paired schema upgrades.
use anyhow::{Context, Result, bail, ensure};
use db::{UpdateControl, UpgradeAcknowledgement as Ack, UpgradePhase as Phase};
use jmodguard::config::{ApiProcessConfig, BotProcessConfig};
use jmodguard::db::{self, ComponentObservation, ComponentRelease};
#[cfg(test)]
use jmodguard_updater::compatible;
use jmodguard_updater::compatible_control;
use jmodguard_updater::oci::{Registry, sha256_hex};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};

const POLL: Duration = Duration::from_secs(5);
const READY_TIMEOUT: Duration = Duration::from_secs(120);

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
    launch_mode(path, source_sha, release, false).await
}

async fn launch_mode(
    path: &Path,
    source_sha: &str,
    release: Option<ComponentRelease>,
    maintenance: bool,
) -> Result<Running> {
    let ready_file = ready_path();
    let bytes = tokio::fs::read(path).await?;
    let hash = sha256_hex(&bytes);
    let child = Command::new(path)
        .env("JMODGUARD_READY_FILE", &ready_file)
        .env(
            "JMODGUARD_UPDATE_MAINTENANCE",
            if maintenance { "1" } else { "0" },
        )
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

async fn stop(running: &mut Running) -> Result<()> {
    if let Some(pid) = running.child.id() {
        // A child handles SIGTERM through the shared shutdown latch. The
        // launcher remains alive to reap it and to perform rollback.
        let rc = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    match tokio::time::timeout(Duration::from_secs(30), running.child.wait()).await {
        Ok(result) => {
            result?;
        }
        Err(_) => running.child.kill().await?,
    }
    let _ = tokio::fs::remove_file(&running.ready_file).await;
    Ok(())
}

async fn stage(
    registry: &Registry,
    release: &ComponentRelease,
    name: &str,
    dir: &Path,
) -> Result<PathBuf> {
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

fn observation(running: &Running, base_sha: &str) -> ComponentObservation {
    ComponentObservation {
        guild_schema: Some(
            running
                .release
                .as_ref()
                .and_then(|release| release.upgrade.as_ref())
                .map_or(db::guild_data::SCHEMA_VERSION, |upgrade| {
                    upgrade.guild_schema
                }),
        ),
        update_protocol: db::UPDATE_CONTROL_PROTOCOL,
        source_sha: running.source_sha.clone(),
        executable_sha256: running.executable_sha256.clone(),
        admin_sha256: None,
        base_image_sha: base_sha.into(),
        started_at: running.started_at,
        min_schema: running
            .release
            .as_ref()
            .map_or(db::CORE_SCHEMA_VERSION, |release| release.min_schema),
        max_schema: running
            .release
            .as_ref()
            .map_or(db::CORE_SCHEMA_VERSION, |release| release.max_schema),
        api_capability: 1,
        self_update_enabled: update_enabled(),
        base_abi: std::env::var("JMODGUARD_BASE_ABI").unwrap_or_else(|_| "unknown".into()),
    }
}

fn deployment_observation(
    running: &Running,
    base_sha: &str,
    base_abi: &str,
    enabled: bool,
) -> ComponentObservation {
    let mut seen = observation(running, base_sha);
    seen.base_abi = base_abi.into();
    seen.self_update_enabled = enabled;
    seen
}

/// Local process ownership plus the durable paired operation. One container per
/// role is required; an expired heartbeat never stands in for a reaped child.
struct Supervisor {
    role: String,
    base_sha: String,
    base_abi: String,
    enabled: bool,
    baked: PathBuf,
    registry: Registry,
    control: UpdateControl,
    instance: String,
    running: Option<Running>,
    maintenance: Option<Running>,
    health: Option<tokio::task::JoinHandle<Result<()>>>,
    health_bind: Option<String>,
    staging: PathBuf,
    prepared: Option<(String, PathBuf)>,
    standby: Option<(PathBuf, String, Option<ComponentRelease>)>,
    // Failed independent selection awaiting verified prior-service readiness.
    rollback: Option<(ComponentRelease, String)>,
    reported: bool,
    last_observed: Option<Instant>,
}

impl Supervisor {
    async fn idle(&mut self) -> Result<()> {
        if self.running.is_some() {
            return Ok(());
        }
        if self.role == "api" {
            if let Some(child) = self.maintenance.as_mut()
                && child.child.try_wait()?.is_some()
            {
                self.maintenance = None;
            }
            if self.maintenance.is_none() {
                self.maintenance =
                    Some(launch_mode(&self.baked, &self.base_sha, None, true).await?);
            }
        } else if self.health.is_none()
            && let Some(bind) = &self.health_bind
        {
            let listener = jmodguard::health::bind(bind).await?;
            self.health = Some(tokio::spawn(jmodguard::health::serve_listener(listener)));
        }
        Ok(())
    }

    async fn end_idle(&mut self) -> Result<()> {
        if let Some(mut child) = self.maintenance.take() {
            stop(&mut child).await?;
        }
        if let Some(task) = self.health.take() {
            task.abort();
            let _ = task.await;
        }
        Ok(())
    }

    async fn stop_application(&mut self) -> Result<()> {
        // Keep ownership until reaping succeeds. No acknowledgement on error.
        if let Some(child) = self.running.as_mut() {
            stop(child).await?;
        }
        self.running = None;
        self.reported = false;
        self.idle().await
    }

    async fn start(
        &mut self,
        path: &Path,
        sha: &str,
        release: Option<ComponentRelease>,
    ) -> Result<()> {
        self.end_idle().await?;
        self.running = Some(launch(path, sha, release).await?);
        self.reported = false;
        self.last_observed = None;
        Ok(())
    }

    async fn report_ready(&mut self) -> Result<bool> {
        let Some(child) = self.running.as_mut() else {
            return Ok(false);
        };
        if let Some(status) = child.child.try_wait()? {
            bail!("application exited: {status}");
        }
        if tokio::fs::metadata(&child.ready_file).await.is_err() {
            ensure!(
                self.rollback.is_some()
                    || child.release.is_none()
                    || now().saturating_sub(child.started_at) < READY_TIMEOUT.as_secs() as i64,
                "replacement child readiness timed out"
            );
            return Ok(false);
        }
        if self.reported {
            if self
                .last_observed
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(60))
            {
                match self
                    .control
                    .running(
                        &self.role,
                        None,
                        deployment_observation(child, &self.base_sha, &self.base_abi, self.enabled),
                        now(),
                    )
                    .await
                {
                    Ok(()) => self.last_observed = Some(Instant::now()),
                    Err(error) => tracing::warn!("release heartbeat failed: {error:#}"),
                }
            }
        } else {
            if let Some((failed, error)) = &self.rollback {
                self.control
                    .rolled_back(
                        &self.role,
                        failed,
                        child.release.clone(),
                        deployment_observation(child, &self.base_sha, &self.base_abi, self.enabled),
                        error,
                        now(),
                    )
                    .await?;
                self.rollback = None;
                self.standby = None;
            } else if child.release.is_none() {
                self.control
                    .baked_running(
                        &self.role,
                        deployment_observation(child, &self.base_sha, &self.base_abi, self.enabled),
                        now(),
                    )
                    .await?;
            } else {
                self.control
                    .running(
                        &self.role,
                        child.release.clone(),
                        deployment_observation(child, &self.base_sha, &self.base_abi, self.enabled),
                        now(),
                    )
                    .await?;
            }
            self.reported = true;
            self.last_observed = Some(Instant::now());
        }
        Ok(true)
    }

    async fn pair(&mut self, id: &str) -> Result<()> {
        let op = self
            .control
            .join_upgrade(id, &self.role, &self.instance)
            .await?;
        // Pending independent recovery can resume if this reversible pair is
        // cancelled. No prior artifacts may survive the migration fence.
        if op.irreversible || self.rollback.is_none() {
            self.standby = None;
            self.rollback = None;
        }
        if op.error.is_some() {
            self.stop_application().await?;
            if !op.service(&self.role)?.stopped {
                self.control
                    .acknowledge(id, &self.role, &self.instance, Ack::Stopped)
                    .await?;
            }
            return Ok(());
        }
        let target = op.service(&self.role)?.release.clone();
        ensure!(
            self.enabled,
            "self-update was disabled during the paired upgrade"
        );
        ensure!(
            target.base_abi == self.base_abi,
            "paired release requires a different base ABI"
        );
        if self
            .prepared
            .as_ref()
            .is_none_or(|(prepared_id, _)| prepared_id != id)
        {
            let path = stage(
                &self.registry,
                &target,
                &format!("jmodguard-{}", self.role),
                &self.staging,
            )
            .await?;
            self.prepared = Some((id.into(), path));
        }
        if !op.service(&self.role)?.prepared {
            self.control
                .acknowledge(id, &self.role, &self.instance, Ack::Prepared)
                .await?;
            return Ok(());
        }
        let must_stop = match self.role.as_str() {
            "api" => matches!(
                op.phase,
                Phase::StoppingApi | Phase::StoppingBot | Phase::StartingBot
            ),
            _ => op.phase == Phase::StoppingBot,
        };
        if must_stop {
            self.stop_application().await?;
            // Repeated acknowledgements also advance a phase whose other
            // participant restarted and has just re-established its evidence.
            self.control
                .acknowledge(id, &self.role, &self.instance, Ack::Stopped)
                .await?;
            return Ok(());
        }
        let may_start = (self.role == "bot"
            && op.phase == Phase::StartingBot
            && op.api.stopped
            && op.api.prepared)
            || (self.role == "api" && op.phase == Phase::StartingApi && op.bot.ready);
        if may_start && self.running.is_none() {
            // Atomic reservation persists the irreversible fence BEFORE spawn.
            // A recovery request cannot replace this operation until we reap.
            self.control
                .acknowledge(id, &self.role, &self.instance, Ack::Starting)
                .await?;
            self.standby = None;
            self.rollback = None;
            let path = self
                .prepared
                .as_ref()
                .context("release was not prepared")?
                .1
                .clone();
            self.start(&path, &target.source_sha, Some(target.clone()))
                .await?;
            return Ok(());
        }
        // Preparation owns desired; publishing an independent rollback here
        // could clear an explicitly retried pair with the same failed digest.
        if self.rollback.is_none() && self.report_ready().await? && may_start {
            self.control
                .acknowledge(id, &self.role, &self.instance, Ack::Ready)
                .await?;
        }
        self.idle().await
    }

    async fn restore(&mut self, state: &db::ComponentUpdate) -> Result<()> {
        if let Some((failed, error)) = self.rollback.clone() {
            return self.rollback_independent(state, failed, error).await;
        }
        // One-time deployment of this baseline replaces pre-protocol selections
        // whose metadata caused the schema15/16 lockout. Operators stop both old
        // services for this rollout. Never discard a protocol-2 managed selection.
        let legacy_baseline = state
            .observed
            .as_ref()
            .is_none_or(|seen| seen.update_protocol == 0)
            && state
                .installed
                .as_ref()
                .is_none_or(|release| release.upgrade.is_none())
            && self.control.upgrade().await?.is_none();
        let schema = self.control.schema_version().await?;
        if legacy_baseline
            && ((self.role == "bot" && schema <= db::CORE_SCHEMA_VERSION)
                || schema == db::CORE_SCHEMA_VERSION)
        {
            let path = self.baked.clone();
            let sha = self.base_sha.clone();
            return self.start(&path, &sha, None).await;
        }
        if let Some(release) = &state.installed {
            let schema = self.control.schema_version().await?;
            ensure!(
                schema >= release.min_schema && schema <= release.max_schema,
                "installed application cannot open database; use owner recovery"
            );
            ensure!(
                release.base_abi == self.base_abi,
                "installed application requires a different base ABI"
            );
            let path = stage(
                &self.registry,
                release,
                &format!("jmodguard-{}", self.role),
                &self.staging,
            )
            .await?;
            self.start(&path, &release.source_sha, Some(release.clone()))
                .await?;
        } else {
            ensure!(
                self.control.schema_version().await? == db::CORE_SCHEMA_VERSION,
                "baked application cannot open database; use owner recovery"
            );
            let path = self.baked.clone();
            let sha = self.base_sha.clone();
            self.start(&path, &sha, None).await?;
        }
        Ok(())
    }

    async fn launcher(&mut self, state: &db::ComponentUpdate) -> Result<bool> {
        let Some(release) = state
            .desired_launcher
            .as_ref()
            .or(state.installed_launcher.as_ref())
        else {
            return Ok(false);
        };
        if std::env::var("JMODGUARD_LAUNCHER_RELEASE").as_deref() == Ok(&release.digest)
            || std::env::var("JMODGUARD_LAUNCHER_FAILED").as_deref() == Ok(&release.digest)
        {
            return Ok(false);
        }
        ensure!(
            release.component == self.role
                && release.launcher_protocol == 1
                && release
                    .upgrade
                    .as_ref()
                    .is_some_and(|v| v.control_protocol == db::UPDATE_CONTROL_PROTOCOL)
                && release.base_abi == self.base_abi,
            "launcher does not support this control protocol/base runtime"
        );
        let path = stage(&self.registry, release, "jmodguard-launcher", &self.staging).await?;
        let hash = sha256_hex(&tokio::fs::read(&path).await?);
        tokio::fs::write(
            "/tmp/jmodguard-launcher-next",
            format!("{}\n{}\n{}\n", path.display(), hash, release.digest),
        )
        .await?;
        self.stop_application().await?;
        self.end_idle().await?;
        Ok(true)
    }

    async fn normal(&mut self) -> Result<bool> {
        let state = self.control.component(&self.role).await?;
        if self.rollback.is_none()
            && self.enabled
            && (state.desired_launcher.is_some() || self.running.is_none())
        {
            match self.launcher(&state).await {
                Ok(true) => return Ok(true),
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(
                        "launcher update failed; retaining stable controller: {error:#}"
                    );
                    if let Some(release) = state
                        .desired_launcher
                        .as_ref()
                        .or(state.installed_launcher.as_ref())
                    {
                        self.control
                            .launcher_failed(&self.role, &release.digest, &format!("{error:#}"))
                            .await?;
                    }
                }
            }
        }
        if self.running.is_none()
            && let Err(error) = self.restore(&state).await
        {
            if let Some(op) = self.control.upgrade().await?.filter(|op| {
                self.rollback.is_none()
                    && op.phase == Phase::Complete
                    && state.installed.as_ref().is_some_and(|release| {
                        op.service(&self.role)
                            .is_ok_and(|service| release.digest == service.release.digest)
                    })
            }) {
                self.control
                    .upgrade_error(&op.id, &format!("{} restore: {error:#}", self.role))
                    .await?;
            }
            return Err(error);
        }
        let restoring_previous = self.rollback.is_some();
        if let Err(error) = self.report_ready().await {
            // After a coordinated migration, never try a previous/baked child.
            if let Some(op) = self.control.upgrade().await?
                && op.phase == Phase::Complete
                && self.rollback.is_none()
                && self
                    .running
                    .as_ref()
                    .and_then(|child| child.release.as_ref())
                    .is_some_and(|release| {
                        op.service(&self.role)
                            .is_ok_and(|service| release.digest == service.release.digest)
                    })
            {
                self.control
                    .upgrade_error(&op.id, &format!("{}: {error:#}", self.role))
                    .await?;
                self.stop_application().await?;
                return Ok(false);
            }
            if self.rollback.is_none() && self.standby.is_some() {
                let failed = self
                    .running
                    .as_ref()
                    .and_then(|child| child.release.clone())
                    .context("failed replacement missing")?;
                self.rollback_independent(&state, failed, format!("{error:#}"))
                    .await?;
            } else {
                self.stop_application().await?;
                return Err(error);
            }
        }
        // A rollback receipt may have cleared desired after this tick's read.
        // Do not retry the stale failed selection, even if the prior is ready.
        if restoring_previous || self.rollback.is_some() {
            return Ok(false);
        }
        if !self.reported || !self.enabled {
            return Ok(false);
        }
        let Some(candidate) = state.desired.clone() else {
            return Ok(false);
        };
        if self
            .running
            .as_ref()
            .and_then(|child| child.release.as_ref())
            .is_some_and(|r| r.digest == candidate.digest)
        {
            return Ok(false);
        }
        if let Err(error) = compatible_control(&self.control, &candidate, now()).await {
            self.control
                .failed(&self.role, &candidate, &format!("compatibility: {error:#}"))
                .await?;
            return Ok(false);
        }
        let path = match stage(
            &self.registry,
            &candidate,
            &format!("jmodguard-{}", self.role),
            &self.staging,
        )
        .await
        {
            Ok(path) => path,
            Err(error) => {
                self.control
                    .failed(&self.role, &candidate, &format!("download: {error:#}"))
                    .await?;
                return Ok(false);
            }
        };
        // Admission may have changed while downloading. A paired operation
        // blocks independent work before any process is stopped.
        ensure!(
            self.control
                .upgrade()
                .await?
                .is_none_or(|op| !op.phase.active()),
            "coordinated upgrade started"
        );
        let child = self
            .running
            .as_ref()
            .context("current application missing")?;
        self.standby = Some((
            child.path.clone(),
            child.source_sha.clone(),
            child.release.clone(),
        ));
        self.stop_application().await?;
        if let Err(error) = self
            .start(&path, &candidate.source_sha, Some(candidate.clone()))
            .await
        {
            self.rollback_independent(&state, candidate, format!("spawn: {error:#}"))
                .await?;
        }
        Ok(false)
    }

    async fn rollback_independent(
        &mut self,
        state: &db::ComponentUpdate,
        failed: ComponentRelease,
        error: String,
    ) -> Result<()> {
        let (path, sha, previous) = self
            .standby
            .as_ref()
            .cloned()
            .context("rollback artifact missing")?;
        // Independent admission proved both schemas. Recheck before fallback.
        let schema = self.control.schema_version().await?;
        ensure!(
            previous
                .as_ref()
                .map_or(schema == db::CORE_SCHEMA_VERSION, |r| schema
                    >= r.min_schema
                    && schema <= r.max_schema),
            "rollback schema is incompatible"
        );
        let previous_guild = previous
            .as_ref()
            .and_then(|r| r.upgrade.as_ref())
            .map_or(db::guild_data::SCHEMA_VERSION, |contract| {
                contract.guild_schema
            });
        ensure!(
            state.observed.as_ref().and_then(|seen| seen.guild_schema) == Some(previous_guild),
            "rollback guild schema is incompatible"
        );
        // Keep the failed identity until the live prior marker permits its receipt.
        self.rollback = Some((failed, error));
        self.stop_application().await?;
        self.start(&path, &sha, previous).await
    }

    async fn tick(&mut self) -> Result<bool> {
        if let Some(op) = self.control.upgrade().await?.filter(|op| op.phase.active()) {
            if let Err(error) = self.pair(&op.id).await {
                if error.is::<db::UpgradeProgressChanged>() {
                    return Ok(false);
                }
                let _ = self
                    .control
                    .upgrade_error(&op.id, &format!("{}: {error:#}", self.role))
                    .await;
                self.stop_application().await?;
                return Err(error);
            }
            return Ok(false);
        }
        self.normal().await
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let role = role()?;
    let base_sha = std::env::var("JMODGUARD_BASE_SHA").unwrap_or_else(|_| "unknown".into());
    let (opts, health_bind) = if role == "bot" {
        let cfg = BotProcessConfig::load()?;
        jmodguard::init_tracing(&cfg.log, "launcher");
        (cfg.db_open_options(), cfg.health_bind())
    } else {
        let cfg = ApiProcessConfig::load()?;
        jmodguard::init_tracing(&cfg.log, "launcher");
        (cfg.db_open_options(), None)
    };
    let baked = binary_path(&role);
    // Only a verified empty store permits the baked bot to initialize it.
    // Neither a network error nor an unknown/newer schema means "start baked".
    let mut initial: Option<Running> = None;
    let mut maintenance = if role == "api" {
        Some(launch_mode(&baked, &base_sha, None, true).await?)
    } else {
        None
    };
    let mut boot_health = if let Some(bind) = &health_bind {
        Some(tokio::spawn(jmodguard::health::serve_listener(
            jmodguard::health::bind(bind).await?,
        )))
    } else {
        None
    };
    let control = loop {
        if let Some(child) = initial.as_mut()
            && child.child.try_wait()?.is_some()
        {
            initial = None;
            if let Some(bind) = &health_bind {
                boot_health = Some(tokio::spawn(jmodguard::health::serve_listener(
                    jmodguard::health::bind(bind).await?,
                )));
            }
        }
        let open_opts = opts.clone();
        match tokio::task::spawn_blocking(move || UpdateControl::open(&open_opts)).await? {
            Ok(Some(control)) => break control,
            Ok(None) if role == "bot" && initial.is_none() => {
                if let Some(task) = boot_health.take() {
                    task.abort();
                    let _ = task.await;
                }
                initial = Some(launch(&baked, &base_sha, None).await?);
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!("update control unavailable; application remains stopped: {error:#}")
            }
        }
        tokio::select! {
            _ = jmodguard::shutdown_signal() => {
                if let Some(child) = initial.as_mut() { stop(child).await?; }
                if let Some(child) = maintenance.as_mut() { stop(child).await?; }
                if let Some(task) = boot_health { task.abort(); let _ = task.await; }
                return Ok(());
            },
            _ = tokio::time::sleep(POLL) => {},
        }
    };
    let hash = sha256_hex(&tokio::fs::read(std::env::current_exe()?).await?);
    if let Ok(digest) = std::env::var("JMODGUARD_LAUNCHER_RELEASE") {
        let state = control.component(&role).await?;
        let release = state
            .desired_launcher
            .as_ref()
            .or(state.installed_launcher.as_ref())
            .context("launcher selection missing")?;
        ensure!(
            release.digest == digest,
            "launcher selection changed before handoff"
        );
        control
            .launcher_running(&role, release, &hash, now())
            .await?;
    } else if control
        .component(&role)
        .await?
        .installed_launcher
        .as_ref()
        .is_none_or(|release| release.upgrade.is_none())
    {
        control.baked_launcher(&role, &hash, now()).await?;
    }
    if let Ok(failed) = std::env::var("JMODGUARD_LAUNCHER_FAILED") {
        control.launcher_failed(&role, &failed, "updated launcher failed; stable baked controller is recovering the selected application").await?;
    }
    let mut supervisor = Supervisor {
        role,
        base_sha,
        base_abi: std::env::var("JMODGUARD_BASE_ABI").unwrap_or_else(|_| "unknown".into()),
        enabled: update_enabled(),
        baked,
        registry: Registry::ghcr()?,
        control,
        instance: format!("{:032x}", rand::random::<u128>()),
        running: initial,
        maintenance,
        health: boot_health,
        health_bind,
        staging: PathBuf::from("/tmp/jmodguard-updates"),
        prepared: None,
        standby: None,
        rollback: None,
        reported: false,
        last_observed: None,
    };
    loop {
        tokio::select! {
            _ = jmodguard::shutdown_signal() => break,
            result = supervisor.tick() => match result {
                Ok(true) => std::process::exit(77),
                Ok(false) => {},
                Err(error) => { tracing::warn!("update controller: {error:#}"); supervisor.idle().await?; },
            },
        }
        tokio::select! { _ = jmodguard::shutdown_signal() => break, _ = tokio::time::sleep(POLL) => {} }
    }
    supervisor.stop_application().await?;
    supervisor.end_idle().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use db::Db;

    #[tokio::test]
    async fn published_schema_and_launcher_observations_allow_only_matching_services() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let output = Command::new("bash")
            .current_dir(&root)
            .args([
                "-c",
                "JMODGUARD_PUBLISH_IMAGE_LIB=1 source .buildkite/publish-image.sh; build_args api aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa test .",
            ])
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
        let args = String::from_utf8(output.stdout).unwrap();
        let label = |name: &str| {
            args.split('\0')
                .find_map(|arg| arg.strip_prefix(&format!("io.jmodguard.{name}=")))
                .unwrap()
                .to_owned()
        };
        let candidate = ComponentRelease {
            upgrade: Some(db::UpgradeCompatibility {
                control_protocol: db::UPDATE_CONTROL_PROTOCOL,
                min_core: 15,
                min_guild: 1,
                guild_schema: db::guild_data::SCHEMA_VERSION,
            }),
            component: "api".into(),
            channel: "latest".into(),
            release_id: "test".into(),
            source_sha: "a".repeat(40),
            published_at: label("published-at"),
            image: "ghcr.io/jatmn/jmodguard-api".into(),
            digest: format!("sha256:{}", "a".repeat(64)),
            min_schema: label("min-schema").parse().unwrap(),
            max_schema: label("max-schema").parse().unwrap(),
            result_schema: label("result-schema").parse().unwrap(),
            min_api: 1,
            max_api: 1,
            launcher_protocol: 1,
            base_abi: label("base-abi"),
        };
        let dir =
            std::env::temp_dir().join(format!("jmodguard-schema-{:032x}", rand::random::<u128>()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let db = Db::open(&dir.join("core.db"), 2).unwrap();
        let schema = db::core_schema_version(&db).await.unwrap();
        assert_eq!(candidate.result_schema, schema);
        assert_eq!(candidate.min_schema, schema);
        assert_eq!(candidate.max_schema, schema);

        // An exited fixture supplies Running without leaving a child behind.
        // Real service startup is covered by dual_process_smoke.
        let mut running = launch(Path::new("/bin/true"), &candidate.source_sha, None)
            .await
            .unwrap();
        running.child.wait().await.unwrap();
        let mut baked = observation(&running, &candidate.source_sha);
        // Supply the deployment ABI normally injected by the image environment.
        baked.base_abi = candidate.base_abi.clone();
        assert_eq!((baked.min_schema, baked.max_schema), (schema, schema));
        for component in ["bot", "api"] {
            db::record_component_running(&db, component, None, baked.clone(), 100)
                .await
                .unwrap();
        }
        compatible(&db, &candidate, 100).await.unwrap();
        let mut guild_upgrade = candidate.clone();
        guild_upgrade.upgrade.as_mut().unwrap().guild_schema += 1;
        assert!(
            compatible(&db, &guild_upgrade, 100)
                .await
                .unwrap_err()
                .to_string()
                .contains("together")
        );

        // Preserve the refusal of a counterpart or rollback binary from before
        // the migration, even when the candidate matches the current database.
        let mut older = baked.clone();
        older.min_schema = schema - 1;
        older.max_schema = schema - 1;
        db::record_component_running(&db, "bot", None, older.clone(), 100)
            .await
            .unwrap();
        assert!(
            compatible(&db, &candidate, 100)
                .await
                .unwrap_err()
                .to_string()
                .contains("counterpart")
        );
        db::record_component_running(&db, "bot", None, baked, 100)
            .await
            .unwrap();
        db::record_component_running(&db, "api", None, older, 100)
            .await
            .unwrap();
        assert!(
            compatible(&db, &candidate, 100)
                .await
                .unwrap_err()
                .to_string()
                .contains("rollback")
        );

        // A managed child's observations describe that release, not its launcher.
        let mut managed = candidate;
        managed.min_schema = schema - 1;
        running.release = Some(managed);
        let observed = observation(&running, &running.source_sha);
        assert_eq!(
            (observed.min_schema, observed.max_schema),
            (schema - 1, schema)
        );
        drop(db);
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}

#[cfg(test)]
#[path = "controller_tests.rs"]
mod controller_tests;
