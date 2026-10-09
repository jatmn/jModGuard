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

fn api_writer_link(target: &str, secret: &str) -> Option<jmodguard::control::WriterLink> {
    if target.is_empty() {
        tracing::warn!(
            "API launcher control target is unset; component observations will not be recorded"
        );
        return None;
    }
    match jmodguard::control::try_writer_link(target, secret) {
        Ok(Some(link)) => Some(link),
        Ok(None) => {
            tracing::warn!(
                "API launcher control secret is missing or too short; component observations will not be recorded"
            );
            None
        }
        Err(error) => {
            tracing::warn!(
                "API launcher control link is unusable; component observations will not be recorded: {error:#}"
            );
            None
        }
    }
}

fn spawn_launcher_fence_renewal(control: UpdateControl) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            match control.renew_launcher_fence().await {
                Ok(true) => {}
                Ok(false) => tracing::warn!("bot launcher fence is held by another launcher"),
                Err(error) => tracing::warn!("bot launcher fence renewal failed: {error:#}"),
            }
        }
    })
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

fn observation(
    running: &Running,
    base_sha: &str,
    baked: &db::BakedCompatibility,
) -> ComponentObservation {
    ComponentObservation {
        guild_schema: Some(
            running
                .release
                .as_ref()
                .and_then(|release| release.upgrade.as_ref())
                .map_or(baked.guild, |upgrade| upgrade.guild_schema),
        ),
        update_protocol: db::UPDATE_CONTROL_PROTOCOL,
        launcher_revision: db::LAUNCHER_REVISION,
        source_sha: running.source_sha.clone(),
        executable_sha256: running.executable_sha256.clone(),
        admin_sha256: None,
        base_image_sha: base_sha.into(),
        started_at: running.started_at,
        min_schema: running
            .release
            .as_ref()
            .map_or(baked.core, |release| release.min_schema),
        max_schema: running
            .release
            .as_ref()
            .map_or(baked.core, |release| release.max_schema),
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
    baked: &db::BakedCompatibility,
) -> ComponentObservation {
    let mut seen = observation(running, base_sha, baked);
    seen.base_abi = base_abi.into();
    seen.self_update_enabled = enabled;
    seen
}

enum ControlStep {
    Reexec,
    Proceed,
    Defer,
}

struct PendingFailure {
    component: Option<(ComponentRelease, String)>,
    launcher: Option<(String, String)>,
}

/// Local process ownership plus the durable paired operation. One container per
/// role is required; an expired heartbeat never stands in for a reaped child.
struct Supervisor {
    role: String,
    base_sha: String,
    base_abi: String,
    enabled: bool,
    launcher_revision: u32,
    baked: PathBuf,
    baked_compatibility: db::BakedCompatibility,
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
    pending_failure: Option<PendingFailure>,
    receipt_attempt: u32,
    receipt_not_before: Option<Instant>,
    reported: bool,
    last_observed: Option<Instant>,
}

impl Supervisor {
    fn receipt_not_saved(error: &anyhow::Error) -> bool {
        error.is::<db::ReceiptNotSaved>()
    }

    fn note_receipt_delay(&mut self) {
        let delay =
            Duration::from_secs(1_u64 << self.receipt_attempt.min(6)).min(Duration::from_secs(60));
        self.receipt_attempt = self.receipt_attempt.saturating_add(1);
        self.receipt_not_before = Some(Instant::now() + delay);
    }

    fn reset_receipt_backoff(&mut self) {
        self.receipt_attempt = 0;
        self.receipt_not_before = None;
    }

    fn receipt_wait_elapsed(&self) -> bool {
        self.receipt_not_before
            .is_none_or(|at| Instant::now() >= at)
    }

    fn remember_pending(&mut self, pending: PendingFailure) {
        tracing::warn!("launcher receipt was not saved; retrying before any new candidate");
        self.pending_failure = Some(pending);
        self.note_receipt_delay();
    }

    async fn flush_pending_failure(&mut self) -> Result<bool> {
        if self.pending_failure.is_none() {
            return Ok(false);
        }
        if !self.receipt_wait_elapsed() {
            return Ok(true);
        }
        if let Some((digest, message)) = self
            .pending_failure
            .as_ref()
            .and_then(|pending| pending.launcher.clone())
        {
            match self
                .control
                .launcher_failed(&self.role, &digest, &message)
                .await
            {
                Ok(()) => {
                    if let Some(pending) = self.pending_failure.as_mut() {
                        pending.launcher = None;
                    }
                }
                Err(error) if Self::receipt_not_saved(&error) => {
                    tracing::warn!("launcher failure receipt was not saved: {error:#}");
                    self.note_receipt_delay();
                    return Ok(true);
                }
                Err(error) => return Err(error),
            }
        }
        if let Some((release, message)) = self
            .pending_failure
            .as_ref()
            .and_then(|pending| pending.component.clone())
        {
            match self.control.failed(&self.role, &release, &message).await {
                Ok(()) => {
                    if let Some(pending) = self.pending_failure.as_mut() {
                        pending.component = None;
                    }
                }
                Err(error) if Self::receipt_not_saved(&error) => {
                    tracing::warn!("component failure receipt was not saved: {error:#}");
                    self.note_receipt_delay();
                    return Ok(true);
                }
                Err(error) => return Err(error),
            }
        }
        if self
            .pending_failure
            .as_ref()
            .is_some_and(|pending| pending.component.is_none() && pending.launcher.is_none())
        {
            self.pending_failure = None;
            self.reset_receipt_backoff();
        }
        Ok(false)
    }

    async fn record_component_failure(
        &mut self,
        release: ComponentRelease,
        message: String,
    ) -> Result<()> {
        match self.control.failed(&self.role, &release, &message).await {
            Ok(()) => self.reset_receipt_backoff(),
            Err(error) if Self::receipt_not_saved(&error) => {
                self.remember_pending(PendingFailure {
                    component: Some((release, message)),
                    launcher: None,
                });
            }
            Err(error) => return Err(error),
        }
        Ok(())
    }

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
            self.health = Some(tokio::spawn(jmodguard::health::serve_listener_as(
                listener, "launcher",
            )));
        }
        Ok(())
    }

    async fn end_idle(&mut self) -> Result<()> {
        if let Some(mut child) = self.maintenance.take() {
            tracing::info!("stopping temporary maintenance API before the application starts");
            stop(&mut child).await?;
        }
        if let Some(task) = self.health.take() {
            task.abort();
            let _ = task.await;
            tracing::info!("launcher released health listener");
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
        if self.running.is_some() {
            anyhow::bail!("application process is already running");
        }
        if let Some(release) = &release {
            release.supports_launcher(self.launcher_revision)?;
        }
        self.end_idle().await?;
        self.running = Some(launch(path, sha, release).await?);
        self.reported = false;
        self.last_observed = None;
        Ok(())
    }

    async fn report_ready(&mut self) -> Result<bool> {
        let (release, seen) = {
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
            (
                child.release.clone(),
                deployment_observation(
                    child,
                    &self.base_sha,
                    &self.base_abi,
                    self.enabled,
                    &self.baked_compatibility,
                ),
            )
        };
        if self.reported {
            if self
                .last_observed
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(60))
            {
                match self.control.running(&self.role, None, seen, now()).await {
                    Ok(()) => self.last_observed = Some(Instant::now()),
                    Err(error) => {
                        tracing::warn!("release heartbeat failed: {error:#}");
                        self.last_observed = Some(Instant::now());
                    }
                }
            }
            return Ok(true);
        }
        if !self.receipt_wait_elapsed() {
            return Ok(false);
        }
        if let Some((failed, error)) = self.rollback.clone() {
            match self
                .control
                .rolled_back(&self.role, &failed, release.clone(), seen, &error, now())
                .await
            {
                Ok(()) => {
                    self.rollback = None;
                    self.standby = None;
                    self.reported = true;
                    self.last_observed = Some(Instant::now());
                    self.reset_receipt_backoff();
                }
                Err(error) if Self::receipt_not_saved(&error) => {
                    tracing::warn!("rollback receipt was not saved: {error:#}");
                    self.note_receipt_delay();
                    return Ok(false);
                }
                Err(error) => return Err(error),
            }
        } else {
            let saved = if release.is_none() {
                self.control.baked_running(&self.role, seen, now()).await
            } else {
                self.control.running(&self.role, release, seen, now()).await
            };
            match saved {
                Ok(()) => {
                    self.reported = true;
                    self.last_observed = Some(Instant::now());
                    self.reset_receipt_backoff();
                }
                Err(error) if Self::receipt_not_saved(&error) => {
                    tracing::warn!("activation receipt was not saved: {error:#}");
                    self.note_receipt_delay();
                    return Ok(false);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(true)
    }

    async fn pair(&mut self, id: &str) -> Result<bool> {
        // Independent rollback artifacts cannot survive a potential core OR
        // guild migration, even after later operations replace the status row.
        self.standby = None;
        self.rollback = None;
        let op = self
            .control
            .join_upgrade(id, &self.role, &self.instance)
            .await?;
        if op.error.is_some() {
            self.stop_application().await?;
            if !op.service(&self.role)?.stopped {
                self.control
                    .acknowledge(id, &self.role, &self.instance, Ack::Stopped)
                    .await?;
            }
            return Ok(false);
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
        match self.required_launcher(&target, Some(id)).await? {
            ControlStep::Reexec => return Ok(true),
            ControlStep::Proceed => {}
            ControlStep::Defer => return Ok(false),
        }
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
            return Ok(false);
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
            return Ok(false);
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
            let path = self
                .prepared
                .as_ref()
                .context("release was not prepared")?
                .1
                .clone();
            self.start(&path, &target.source_sha, Some(target.clone()))
                .await?;
            return Ok(false);
        }
        if self.report_ready().await? && may_start {
            self.control
                .acknowledge(id, &self.role, &self.instance, Ack::Ready)
                .await?;
        }
        self.idle().await?;
        Ok(false)
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
            && ((self.role == "bot" && schema <= self.baked_compatibility.core)
                || schema == self.baked_compatibility.core)
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
                self.control.schema_version().await? == self.baked_compatibility.core,
                "baked application cannot open database; use owner recovery"
            );
            let path = self.baked.clone();
            let sha = self.base_sha.clone();
            self.start(&path, &sha, None).await?;
        }
        Ok(())
    }

    async fn handoff(
        &mut self,
        release: &ComponentRelease,
        operation: Option<&str>,
        required: bool,
    ) -> Result<ControlStep> {
        if !self.control.writes_locally() && !self.receipt_wait_elapsed() {
            return Ok(ControlStep::Defer);
        }
        ensure!(
            self.enabled,
            "launcher update requires self-update to be enabled"
        );
        ensure!(
            release.component == self.role,
            "launcher component mismatch"
        );
        jmodguard_updater::launcher_compatible(release, &self.base_abi)?;
        let path = stage(&self.registry, release, "jmodguard-launcher", &self.staging).await?;
        let hash = sha256_hex(&tokio::fs::read(&path).await?);
        if required
            && let Err(error) = self
                .control
                .select_required_launcher(release, operation)
                .await
        {
            if !self.control.writes_locally() {
                tracing::warn!(
                    "required launcher selection was not saved; handoff deferred: {error:#}"
                );
                self.note_receipt_delay();
                return Ok(ControlStep::Defer);
            }
            return Err(error);
        }
        // Production staging is /tmp/jmodguard-updates and PID1 uses its sibling.
        let control_file = self
            .staging
            .parent()
            .context("missing staging parent")?
            .join("jmodguard-launcher-next");
        tokio::fs::write(
            control_file,
            format!("{}\n{}\n{}\n", path.display(), hash, release.digest),
        )
        .await?;
        self.stop_application().await?;
        self.end_idle().await?;
        Ok(ControlStep::Reexec)
    }

    async fn required_launcher(
        &mut self,
        release: &ComponentRelease,
        operation: Option<&str>,
    ) -> Result<ControlStep> {
        release.validate()?;
        if self.launcher_revision >= release.required_launcher() {
            return Ok(ControlStep::Proceed);
        }
        if operation.is_none()
            && std::env::var("JMODGUARD_LAUNCHER_FAILED").as_deref() == Ok(&release.digest)
            && self
                .control
                .component(&self.role)
                .await?
                .desired_launcher
                .is_none()
        {
            bail!("required launcher failed; retry the selected pair from owner recovery");
        }
        self.handoff(release, operation, true).await
    }

    fn defer_unsaved(&mut self, error: anyhow::Error) -> Result<ControlStep> {
        if Self::receipt_not_saved(&error) {
            tracing::warn!("launcher clear was not saved: {error:#}");
            self.note_receipt_delay();
            Ok(ControlStep::Defer)
        } else {
            Err(error)
        }
    }

    async fn launcher(&mut self, state: &db::ComponentUpdate) -> Result<ControlStep> {
        let Some(release) = state
            .desired_launcher
            .as_ref()
            .or(state.installed_launcher.as_ref())
        else {
            return Ok(ControlStep::Proceed);
        };
        if std::env::var("JMODGUARD_LAUNCHER_RELEASE").as_deref() == Ok(&release.digest)
            || (std::env::var("JMODGUARD_LAUNCHER_FAILED").as_deref() == Ok(&release.digest)
                && state.desired_launcher.is_none())
        {
            return Ok(ControlStep::Proceed);
        }
        if let Some(contract) = &release.launcher {
            if contract.revision <= self.launcher_revision {
                let cleared = self
                    .control
                    .clear_superseded_launcher(&self.role, release)
                    .await;
                return match cleared {
                    Ok(()) => Ok(ControlStep::Proceed),
                    Err(error) => self.defer_unsaved(error),
                };
            }
        } else if state.desired_launcher.is_some() {
            // An image from before launcher-revision labels has nothing to run.
            // Drop it once, the same way a contract that is not newer is dropped,
            // and keep the baked controller. The row is gone on the next boot.
            tracing::warn!(
                "stored launcher selection {} has no capability contract; clearing it and keeping the baked controller",
                release.digest
            );
            let cleared = self
                .control
                .clear_uncontracted_launcher(&self.role, release)
                .await;
            return match cleared {
                Ok(()) => Ok(ControlStep::Proceed),
                Err(error) => self.defer_unsaved(error),
            };
        } else {
            return Ok(ControlStep::Proceed);
        }
        self.handoff(release, None, false).await
    }

    async fn normal(&mut self) -> Result<bool> {
        let state = self.control.component(&self.role).await?;
        if self.enabled && (state.desired_launcher.is_some() || self.running.is_none()) {
            match self.launcher(&state).await {
                Ok(ControlStep::Reexec) => return Ok(true),
                Ok(ControlStep::Proceed) => {}
                Ok(ControlStep::Defer) => return Ok(false),
                Err(error) => {
                    tracing::warn!(
                        "launcher update failed; retaining stable controller: {error:#}"
                    );
                    if let Some(release) = state
                        .desired_launcher
                        .as_ref()
                        .or(state.installed_launcher.as_ref())
                    {
                        let message = format!("{error:#}");
                        match self
                            .control
                            .launcher_failed(&self.role, &release.digest, &message)
                            .await
                        {
                            Ok(()) => {}
                            Err(save) if Self::receipt_not_saved(&save) => {
                                self.remember_pending(PendingFailure {
                                    component: None,
                                    launcher: Some((release.digest.clone(), message)),
                                });
                                return Ok(false);
                            }
                            Err(save) => return Err(save),
                        }
                    }
                }
            }
        }
        if self.running.is_none() {
            let restored = async {
                if let Some(installed) = &state.installed {
                    match self.required_launcher(installed, None).await? {
                        ControlStep::Reexec => return Ok(ControlStep::Reexec),
                        ControlStep::Proceed => {}
                        ControlStep::Defer => return Ok(ControlStep::Defer),
                    }
                }
                self.restore(&state).await?;
                Ok::<_, anyhow::Error>(ControlStep::Proceed)
            }
            .await;
            match restored {
                Ok(ControlStep::Reexec) => return Ok(true),
                Ok(ControlStep::Defer) => return Ok(false),
                Ok(ControlStep::Proceed) => {}
                Err(error) => {
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
            }
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
            if let Some((path, sha, previous)) = self.standby.take() {
                // Independent switches passed the same core AND guild schema
                // gate. Recheck the current schema before using that fallback.
                let schema = self.control.schema_version().await?;
                ensure!(
                    previous
                        .as_ref()
                        .map_or(schema == self.baked_compatibility.core, |r| schema
                            >= r.min_schema
                            && schema <= r.max_schema),
                    "rollback schema is incompatible"
                );
                let previous_guild = previous
                    .as_ref()
                    .and_then(|r| r.upgrade.as_ref())
                    .map_or(self.baked_compatibility.guild, |contract| {
                        contract.guild_schema
                    });
                ensure!(
                    state.observed.as_ref().and_then(|seen| seen.guild_schema)
                        == Some(previous_guild),
                    "rollback guild schema is incompatible"
                );
                let failed = self
                    .running
                    .as_ref()
                    .and_then(|r| r.release.clone())
                    .context("failed replacement missing")?;
                self.stop_application().await?;
                self.start(&path, &sha, previous.clone()).await?;
                let child = self.running.as_ref().context("rollback did not start")?;
                let message = format!("{error:#}");
                match self
                    .control
                    .rolled_back(
                        &self.role,
                        &failed,
                        previous,
                        deployment_observation(
                            child,
                            &self.base_sha,
                            &self.base_abi,
                            self.enabled,
                            &self.baked_compatibility,
                        ),
                        &message,
                        now(),
                    )
                    .await
                {
                    Ok(()) => {}
                    Err(save) if Self::receipt_not_saved(&save) => {
                        tracing::warn!("rollback receipt was not saved: {save:#}");
                        self.rollback = Some((failed, message));
                        self.note_receipt_delay();
                        return Ok(false);
                    }
                    Err(save) => return Err(save),
                }
            } else {
                self.stop_application().await?;
                return Err(error);
            }
        }
        // A rollback receipt may have cleared desired after this tick's read.
        // Do not retry the stale failed selection until the next read.
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
        if self.pending_failure.as_ref().is_some_and(|pending| {
            pending
                .component
                .as_ref()
                .is_some_and(|(release, _)| release.digest == candidate.digest)
                || pending
                    .launcher
                    .as_ref()
                    .is_some_and(|(digest, _)| digest == &candidate.digest)
        }) {
            return Ok(false);
        }
        if let Err(error) = compatible_control(&self.control, &candidate, now()).await {
            self.record_component_failure(candidate, format!("compatibility: {error:#}"))
                .await?;
            return Ok(false);
        }
        match self.required_launcher(&candidate, None).await {
            Ok(ControlStep::Reexec) => return Ok(true),
            Ok(ControlStep::Proceed) => {}
            Ok(ControlStep::Defer) => return Ok(false),
            Err(error) => {
                let launcher_message = format!("required launcher failed: {error:#}");
                let app_message =
                    format!("required launcher failed: {error:#}; application was not updated");
                match self
                    .control
                    .launcher_failed(&self.role, &candidate.digest, &launcher_message)
                    .await
                {
                    Ok(()) => {}
                    Err(save) if Self::receipt_not_saved(&save) => {
                        let digest = candidate.digest.clone();
                        self.remember_pending(PendingFailure {
                            component: Some((candidate, app_message)),
                            launcher: Some((digest, launcher_message)),
                        });
                        return Ok(false);
                    }
                    Err(save) => return Err(save),
                }
                self.record_component_failure(candidate, app_message)
                    .await?;
                return Ok(false);
            }
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
                self.record_component_failure(candidate, format!("download: {error:#}"))
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
            // Launcher gating already ran. A failed application spawn must
            // restart the captured standby and settle the failed selection.
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
        let schema = self.control.schema_version().await?;
        ensure!(
            previous
                .as_ref()
                .map_or(schema == self.baked_compatibility.core, |r| {
                    schema >= r.min_schema && schema <= r.max_schema
                }),
            "rollback schema is incompatible"
        );
        let previous_guild = previous
            .as_ref()
            .and_then(|r| r.upgrade.as_ref())
            .map_or(self.baked_compatibility.guild, |contract| {
                contract.guild_schema
            });
        ensure!(
            state.observed.as_ref().and_then(|seen| seen.guild_schema) == Some(previous_guild),
            "rollback guild schema is incompatible"
        );
        // Keep the failed identity until the restarted prior is ready. A later
        // report_ready records the receipt; running() must not overwrite it.
        self.rollback = Some((failed, error));
        self.stop_application().await?;
        self.start(&path, &sha, previous).await
    }

    async fn tick(&mut self) -> Result<bool> {
        if self.flush_pending_failure().await? {
            return Ok(false);
        }
        if let Some(op) = self.control.upgrade().await?.filter(|op| op.phase.active()) {
            let result = self.pair(&op.id).await;
            if let Err(error) = result {
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
            return result;
        }
        self.normal().await
    }
}

async fn activate_launcher(
    control: &UpdateControl,
    role: &str,
    release: &ComponentRelease,
    hash: &str,
) -> Result<()> {
    ensure!(
        release
            .launcher
            .as_ref()
            .is_some_and(|v| v.revision == db::LAUNCHER_REVISION),
        "launcher binary capability does not match its release metadata"
    );
    control.launcher_running(role, release, hash, now()).await
}

/// Verify a replacement (or record the baked controller), then publish compiled
/// capability. Observation is the owner-visible "running launcher"; it must not
/// precede a failed activation.
async fn establish_launcher(
    control: &UpdateControl,
    role: &str,
    hash: &str,
    handoff_digest: Option<&str>,
) -> Result<()> {
    if let Some(digest) = handoff_digest {
        let state = control.component(role).await?;
        let release = state
            .desired_launcher
            .as_ref()
            .or(state.installed_launcher.as_ref())
            .context("launcher selection missing")?;
        ensure!(
            release.digest == digest,
            "launcher selection changed before handoff"
        );
        activate_launcher(control, role, release, hash).await?;
    } else if control
        .component(role)
        .await?
        .installed_launcher
        .as_ref()
        .is_none_or(|release| release.upgrade.is_none())
    {
        control.baked_launcher(role, hash, now()).await?;
    }
    control.observe_launcher(role, db::LAUNCHER_REVISION).await
}

async fn stop_pre_supervisor(
    initial: &mut Option<Running>,
    maintenance: &mut Option<Running>,
    boot_health: &mut Option<tokio::task::JoinHandle<Result<()>>>,
    fence_renewal: &Option<tokio::task::JoinHandle<()>>,
) -> Result<()> {
    if let Some(task) = fence_renewal {
        task.abort();
    }
    if let Some(child) = initial.as_mut() {
        stop(child).await?;
    }
    if let Some(child) = maintenance.as_mut() {
        tracing::info!("stopping temporary maintenance API before the application starts");
        stop(child).await?;
    }
    if let Some(task) = boot_health.take() {
        task.abort();
        let _ = task.await;
        tracing::info!("launcher released health listener");
    }
    jmodguard::log_shutdown_finished("launcher");
    Ok(())
}

fn startup_receipt_missing(error: &anyhow::Error) -> bool {
    error.is::<db::ReceiptNotSaved>()
}

/// `Ok(false)` is shutdown. A compare failure is still returned.
async fn retry_startup_receipt<F, Fut>(label: &str, mut op: F) -> Result<bool>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let mut attempt = 0u32;
    loop {
        match op().await {
            Ok(()) => return Ok(true),
            Err(error) if startup_receipt_missing(&error) => {
                let status = error
                    .downcast_ref::<db::ReceiptNotSaved>()
                    .map(|saved| saved.status.as_str())
                    .unwrap_or("not saved");
                tracing::warn!("{label} was not saved ({status}); retrying");
                let delay =
                    Duration::from_secs(1_u64 << attempt.min(6)).min(Duration::from_secs(60));
                attempt = attempt.saturating_add(1);
                tokio::select! {
                    _ = jmodguard::shutdown_signal() => return Ok(false),
                    _ = tokio::time::sleep(delay) => {}
                }
            }
            Err(error) => return Err(error),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let role = role()?;
    let base_sha = std::env::var("JMODGUARD_BASE_SHA").unwrap_or_else(|_| "unknown".into());
    let mut api_control: Option<(String, String)> = None;
    let (opts, health_bind) = if role == "bot" {
        let cfg = BotProcessConfig::load()?;
        jmodguard::init_tracing(&cfg.log, "launcher");
        jmodguard::set_shutdown_context("launcher");
        (cfg.db_open_options(), cfg.health_bind())
    } else {
        let cfg = ApiProcessConfig::load()?;
        jmodguard::init_tracing(&cfg.log, "launcher");
        jmodguard::set_shutdown_context("launcher");
        api_control = Some((
            cfg.bot.control_target.clone(),
            cfg.bot.control_secret.as_str().to_owned(),
        ));
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
        Some(tokio::spawn(jmodguard::health::serve_listener_as(
            jmodguard::health::bind(bind).await?,
            "launcher",
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
                boot_health = Some(tokio::spawn(jmodguard::health::serve_listener_as(
                    jmodguard::health::bind(bind).await?,
                    "launcher",
                )));
            }
        }
        let open_opts = opts.clone();
        match tokio::task::spawn_blocking(move || UpdateControl::open(&open_opts)).await? {
            Ok(Some(control)) => {
                break control;
            }
            Ok(None) if role == "bot" && initial.is_none() => {
                if let Some(task) = boot_health.take() {
                    task.abort();
                    let _ = task.await;
                    tracing::info!("launcher released health listener");
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
                if let Some(child) = maintenance.as_mut() {
                    tracing::info!(
                        "stopping temporary maintenance API before the application starts"
                    );
                    stop(child).await?;
                }
                if let Some(task) = boot_health {
                    task.abort();
                    let _ = task.await;
                    tracing::info!("launcher released health listener");
                }
                jmodguard::log_shutdown_finished("launcher");
                return Ok(());
            },
            _ = tokio::time::sleep(POLL) => {},
        }
    };
    let control = if let Some((target, secret)) = api_control.as_ref() {
        match api_writer_link(target, secret) {
            Some(link) => control.with_writer_link(link),
            None => control,
        }
    } else {
        control
    };
    let instance = format!("{:032x}", rand::random::<u128>());
    let fence_renewal = if role == "bot" {
        control.arm_launcher_fence(&instance);
        let acquired = control
            .wait_for_launcher_fence(async || {
                tokio::select! {
                    _ = jmodguard::shutdown_signal() => true,
                    _ = tokio::time::sleep(POLL) => false,
                }
            })
            .await;
        if !acquired {
            if let Some(child) = initial.as_mut() {
                stop(child).await?;
            }
            if let Some(task) = boot_health {
                task.abort();
                let _ = task.await;
                tracing::info!("launcher released health listener");
            }
            jmodguard::log_shutdown_finished("launcher");
            return Ok(());
        }
        Some(spawn_launcher_fence_renewal(control.clone()))
    } else {
        None
    };
    let handoff_digest = std::env::var("JMODGUARD_LAUNCHER_RELEASE").ok();
    let baked_controller = handoff_digest.is_none();
    let baked_compatibility = control
        .baked_compatibility(&role, &base_sha, baked_controller)
        .await?;
    // The API process is read-only, so the baked bot controller stores both rows.
    if role == "bot" && baked_controller {
        control.baked_compatibility("api", &base_sha, true).await?;
    }
    let hash = sha256_hex(&tokio::fs::read(std::env::current_exe()?).await?);
    // Capability is published only after the pinned release is verified.
    // A rejected handoff must not leave Administration treating this binary as active.
    if !retry_startup_receipt("launcher startup", || {
        establish_launcher(&control, &role, &hash, handoff_digest.as_deref())
    })
    .await?
    {
        return stop_pre_supervisor(
            &mut initial,
            &mut maintenance,
            &mut boot_health,
            &fence_renewal,
        )
        .await;
    }
    if let Ok(failed) = std::env::var("JMODGUARD_LAUNCHER_FAILED") {
        let launcher_note = "updated launcher failed; stable baked controller is recovering the selected application";
        if !retry_startup_receipt("launcher failure", || {
            control.launcher_failed(&role, &failed, launcher_note)
        })
        .await?
        {
            return stop_pre_supervisor(
                &mut initial,
                &mut maintenance,
                &mut boot_health,
                &fence_renewal,
            )
            .await;
        }
        if let Some(pending) = control
            .component(&role)
            .await?
            .desired
            .filter(|r| r.digest == failed && r.required_launcher() > db::LAUNCHER_REVISION)
        {
            let note =
                "required launcher failed; application was not updated; retry from Administration";
            if !retry_startup_receipt("application failure", || {
                control.failed(&role, &pending, note)
            })
            .await?
            {
                return stop_pre_supervisor(
                    &mut initial,
                    &mut maintenance,
                    &mut boot_health,
                    &fence_renewal,
                )
                .await;
            }
        }
        if let Some(op) = control.upgrade().await?.filter(|op| op.phase.active())
            && op.service(&role)?.release.digest == failed
        {
            control
                .upgrade_error(
                    &op.id,
                    "required launcher failed; retry the selected pair from recovery",
                )
                .await?;
        }
    }
    let mut supervisor = Supervisor {
        role,
        base_sha,
        base_abi: std::env::var("JMODGUARD_BASE_ABI").unwrap_or_else(|_| "unknown".into()),
        enabled: update_enabled(),
        launcher_revision: db::LAUNCHER_REVISION,
        baked,
        baked_compatibility,
        registry: Registry::ghcr()?,
        control,
        instance,
        running: initial,
        maintenance,
        health: boot_health,
        health_bind,
        staging: PathBuf::from("/tmp/jmodguard-updates"),
        prepared: None,
        standby: None,
        rollback: None,
        pending_failure: None,
        receipt_attempt: 0,
        receipt_not_before: None,
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
    if let Some(task) = fence_renewal {
        task.abort();
    }
    supervisor.stop_application().await?;
    supervisor.end_idle().await?;
    jmodguard::log_shutdown_finished("launcher");
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
            launcher: Some(db::LauncherCompatibility {
                revision: db::LAUNCHER_REVISION,
                min_revision: db::REQUIRED_LAUNCHER_REVISION,
            }),
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
        let mut baked = observation(
            &running,
            &candidate.source_sha,
            &db::BakedCompatibility::compiled(&candidate.source_sha),
        );
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
        let observed = observation(
            &running,
            &running.source_sha,
            &db::BakedCompatibility::compiled(&running.source_sha),
        );
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
