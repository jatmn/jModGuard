//! Role-specific application supervisor with durable paired schema upgrades.
use anyhow::{Context, Result, bail, ensure};
use db::{UpdateControl, UpgradeAcknowledgement as Ack, UpgradePhase as Phase};
use jmodguard::config::{ApiProcessConfig, BotProcessConfig};
use jmodguard::db::{self, ComponentObservation, ComponentRelease};
#[cfg(test)]
use jmodguard_updater::compatible;
use jmodguard_updater::compatible_control;
use jmodguard_updater::oci::{Registry, sha256_hex};
use jmodguard_updater::signature::{
    SCHEMA_MISMATCH, boot_repair_reason, registry_transient, signature_rejection_phrase,
};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};

const POLL: Duration = Duration::from_secs(5);
const READY_TIMEOUT: Duration = Duration::from_secs(120);

/// Process-local only: container restart is the explicit reset for a bad token.
///
/// The hold clock is not persisted. The rejected candidate itself is durable, so
/// a restart during the hold still starts that selection instead of the
/// previously installed bot.
///
/// Intended update interaction: while a rejected-token hold is active, `tick`
/// idles before control reads, so launcher updates and paired-upgrade steps
/// wait until the next auth retry or a container restart. Crash backoff
/// (5 s doubling, capped at 60 s) can likewise delay a paired `StartingBot`
/// by up to 60 s. Revisit both together if hold or backoff policy changes.
#[derive(Default)]
struct RestartPolicy {
    held: bool,
    failures: u32,
    not_before: Option<Instant>,
    auth_failures: u32,
    auth_retry_at: Option<Instant>,
}

impl RestartPolicy {
    fn exited(&mut self, bot: bool, code: Option<i32>, uptime: Duration, now: Instant) {
        if uptime >= Duration::from_secs(300) {
            self.failures = 0;
            self.recovered();
        }
        if bot && code == Some(jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE) {
            if !self.held {
                tracing::error!(
                    "discord token rejected; bot held, fix token and restart container"
                );
            }
            self.held = true;
            let delay = Duration::from_secs(30 * 60)
                .saturating_mul(1_u32 << self.auth_failures.min(4))
                .min(Duration::from_secs(6 * 3600));
            self.auth_failures = self.auth_failures.saturating_add(1);
            self.auth_retry_at = Some(now + delay);
            return;
        }
        if code == Some(0) {
            return;
        }
        let delay = POLL
            .saturating_mul(1_u32 << self.failures.min(4))
            .min(Duration::from_secs(60));
        self.failures = self.failures.saturating_add(1);
        self.not_before = Some(now + delay);
    }

    fn retry_auth(&mut self, now: Instant) -> bool {
        if self.held && self.auth_retry_at.is_some_and(|at| now >= at) {
            self.held = false;
            self.auth_retry_at = None;
            self.not_before = None;
            tracing::warn!("retrying bot after discord token rejection hold");
            return true;
        }
        false
    }

    async fn retry_auth_with_health(
        &mut self,
        now: Instant,
        health: &mut Option<tokio::task::JoinHandle<Result<()>>>,
        bind: Option<&str>,
    ) -> Result<bool> {
        if !self.retry_auth(now) {
            return Ok(false);
        }
        // Replace the captured hold reason before any fallible control work.
        // The boot loop/supervisor is the sole owner of policy transitions.
        if let Some(task) = health.take() {
            task.abort();
            let _ = task.await;
        }
        if let Some(bind) = bind {
            *health = Some(tokio::spawn(
                jmodguard::health::serve_listener_as_with_reason(
                    jmodguard::health::bind(bind).await?,
                    "launcher",
                    self.not_ready_reason(),
                ),
            ));
        }
        Ok(true)
    }

    fn recovered(&mut self) {
        self.auth_failures = 0;
        self.auth_retry_at = None;
    }

    fn not_ready_reason(&self) -> &'static str {
        if self.held {
            "discord token rejected"
        } else {
            "not ready"
        }
    }

    fn waiting(&self, now: Instant) -> bool {
        self.held || self.not_before.is_some_and(|at| now < at)
    }
}

struct Running {
    child: Child,
    ready_file: PathBuf,
    source_sha: String,
    executable_sha256: String,
    started_at: i64,
    started: Instant,
    release: Option<ComponentRelease>,
    path: PathBuf,
}

/// Last binary this supervisor started. A crash can relaunch it without a
/// control read; an intentional stop leaves `exited_for_restart` clear.
#[derive(Clone)]
struct RestartTarget {
    path: PathBuf,
    source_sha: String,
    release: Option<ComponentRelease>,
}

#[derive(Debug)]
enum ChildLiveness {
    Running,
    Exited(Option<i32>),
    Reaped,
}

/// Serves launcher health after the watched child exits. Drop aborts both the
/// watcher and a listener it bound but the supervisor has not adopted yet.
struct ChildExitWatch {
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
    bound: std::sync::Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<Result<()>>>>>,
}

impl Drop for ChildExitWatch {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        if let Ok(mut slot) = self.bound.lock()
            && let Some(serve) = slot.take()
        {
            serve.abort();
        }
    }
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

/// Retry non-timeout errors while the last confirmed fence leaves room.
async fn renew_launcher_fence_promptly<F, Fut>(deadline: Instant, mut renew: F) -> Result<bool>
where
    F: FnMut() -> Fut + Send,
    Fut: std::future::Future<Output = Result<bool>> + Send,
{
    let mut backoff = Duration::from_millis(250);
    loop {
        let requested_at = Instant::now();
        let window = deadline.saturating_duration_since(requested_at);
        ensure!(!window.is_zero(), "launcher fence authority window expired");
        let result = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), renew())
            .await
            .context("launcher fence renewal timed out")?;
        ensure!(
            Instant::now() < deadline,
            "launcher fence renewal completed too late"
        );
        match result {
            Err(error)
                if !db::is_database_operation_timeout(&error)
                    && deadline.saturating_duration_since(Instant::now()) > backoff =>
            {
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(2).min(Duration::from_secs(1));
            }
            result => return result,
        }
    }
}

fn spawn_launcher_fence_renewal(control: UpdateControl) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        renew_launcher_fence_periodically(&control, || control.renew_launcher_fence_cas()).await;
    })
}

async fn renew_launcher_fence_periodically<F, Fut>(control: &UpdateControl, mut renew: F)
where
    F: FnMut() -> Fut + Send,
    Fut: std::future::Future<Output = Result<bool>> + Send,
{
    let mut interval = tokio::time::interval(POLL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await;
    let mut reclaim_at = control.launcher_fence_deadline();
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = async {
                match reclaim_at {
                    Some(deadline) => {
                        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
                    }
                    None => std::future::pending::<()>().await,
                }
            } => {}
        }
        let result = match control.launcher_fence_deadline() {
            Some(deadline) if Instant::now() < deadline => {
                let result = renew_launcher_fence_promptly(deadline, &mut renew).await;
                // A spent window wakes the next iteration immediately, even if
                // the next regular poll is still in the future.
                reclaim_at = control.launcher_fence_deadline();
                result
            }
            // Confirm authority again through the existing fenced acquisition;
            // extending an expired local horizon would trust an unconfirmed owner.
            _ => {
                let result = control.renew_launcher_fence().await;
                // Attempt each expired horizon once; a failed acquisition must
                // fall back to polling rather than spin on the elapsed timer.
                reclaim_at = control
                    .launcher_fence_deadline()
                    .filter(|deadline| Instant::now() < *deadline);
                result
            }
        };
        match result {
            Ok(true) => {}
            Ok(false) => tracing::warn!("bot launcher fence is held by another launcher"),
            Err(error) => tracing::warn!("bot launcher fence renewal failed: {error:#}"),
        }
    }
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
        .env(
            "JMODGUARD_LAUNCHER_REVISION",
            db::LAUNCHER_REVISION.to_string(),
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
        started: Instant::now(),
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

fn open_pidfd(pid: u32) -> std::io::Result<std::fs::File> {
    // SAFETY: pidfd_open only inspects `pid`. A non-negative result is a new fd.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::c_long, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the syscall returned a unique fd. `File` closes it on drop.
    Ok(unsafe { std::fs::File::from_raw_fd(fd as std::os::fd::RawFd) })
}

/// `WNOWAIT` so the supervisor can still reap the same child with `try_wait`.
fn child_liveness(pidfd: std::os::fd::RawFd) -> std::io::Result<ChildLiveness> {
    child_liveness_wait(libc::P_PIDFD, pidfd as libc::id_t)
}

/// Same observation when `pidfd_open` is unavailable. `WNOWAIT` still does not reap.
fn child_liveness_pid(pid: u32) -> std::io::Result<ChildLiveness> {
    child_liveness_wait(libc::P_PID, pid as libc::id_t)
}

fn child_liveness_wait(kind: libc::idtype_t, id: libc::id_t) -> std::io::Result<ChildLiveness> {
    loop {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is zeroed and lives for the call. `WNOWAIT` does not reap.
        let rc = unsafe {
            libc::waitid(
                kind,
                id,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            if error.raw_os_error() == Some(libc::ECHILD) {
                return Ok(ChildLiveness::Reaped);
            }
            return Err(error);
        }
        // SAFETY: a zero `si_pid` means waitid found nobody. Otherwise this is SIGCHLD.
        if unsafe { info.si_pid() } == 0 {
            return Ok(ChildLiveness::Running);
        }
        return Ok(match info.si_code {
            libc::CLD_EXITED => ChildLiveness::Exited(Some(unsafe { info.si_status() })),
            libc::CLD_KILLED | libc::CLD_DUMPED => ChildLiveness::Exited(None),
            _ => ChildLiveness::Running,
        });
    }
}

fn readiness_for_exit(code: Option<i32>) -> &'static str {
    if code == Some(jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE) {
        "discord token rejected"
    } else {
        "not ready"
    }
}

fn addr_in_use(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::AddrInUse)
    })
}

/// Retry only while the exited child still owns the socket. Other bind errors
/// stop immediately. `WNOWAIT` observation stays in the caller.
async fn bind_health_retry(
    addr: &str,
    reason: &'static str,
    deadline: Instant,
    cancel: &mut tokio::sync::oneshot::Receiver<()>,
) -> Option<tokio::task::JoinHandle<Result<()>>> {
    let mut delay = Duration::from_millis(10);
    loop {
        match jmodguard::health::bind(addr).await {
            Ok(listener) => {
                return Some(tokio::spawn(
                    jmodguard::health::serve_listener_as_with_reason(listener, "launcher", reason),
                ));
            }
            Err(error) if addr_in_use(&error) && Instant::now() < deadline => {
                let pause = delay.min(deadline.saturating_duration_since(Instant::now()));
                if pause.is_zero() {
                    tracing::warn!("launcher health bind stayed busy: {error:#}");
                    return None;
                }
                tokio::select! {
                    _ = &mut *cancel => return None,
                    _ = tokio::time::sleep(pause) => {}
                }
                delay = delay.saturating_mul(2).min(Duration::from_millis(200));
            }
            Err(error) if addr_in_use(&error) => {
                tracing::warn!("launcher health bind stayed busy: {error:#}");
                return None;
            }
            Err(error) => {
                tracing::warn!("launcher health bind failed: {error:#}");
                return None;
            }
        }
    }
}

async fn watch_child_health(
    pid: u32,
    bind: String,
    cancel: tokio::sync::oneshot::Receiver<()>,
    bound: std::sync::Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<Result<()>>>>>,
) {
    watch_child_health_from(pid, bind, cancel, bound, true).await
}

/// `prefer_pidfd` is false only when the test is covering the pid fallback.
/// A pidfd failure keeps watching until cancel so this tick can still bind health.
async fn watch_child_health_from(
    pid: u32,
    bind: String,
    mut cancel: tokio::sync::oneshot::Receiver<()>,
    bound: std::sync::Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<Result<()>>>>>,
    prefer_pidfd: bool,
) {
    let mut pidfd = if prefer_pidfd {
        match open_pidfd(pid) {
            Ok(file) => Some(file),
            Err(error) => {
                tracing::warn!("pidfd_open failed; watching the child pid instead: {error}");
                None
            }
        }
    } else {
        None
    };
    let mut logged_pidfd_loss = false;
    loop {
        tokio::select! {
            _ = &mut cancel => return,
            _ = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
        let liveness = if let Some(file) = pidfd.as_ref() {
            match child_liveness(file.as_raw_fd()) {
                Ok(liveness) => liveness,
                Err(error) => {
                    if !logged_pidfd_loss {
                        tracing::warn!(
                            "pidfd wait failed; watching the child pid instead: {error}"
                        );
                        logged_pidfd_loss = true;
                    }
                    pidfd = None;
                    continue;
                }
            }
        } else {
            match child_liveness_pid(pid) {
                Ok(liveness) => liveness,
                Err(_) => continue,
            }
        };
        match liveness {
            ChildLiveness::Running => {}
            ChildLiveness::Exited(code) => {
                if let Some(serve) = bind_health_retry(
                    &bind,
                    readiness_for_exit(code),
                    Instant::now() + POLL,
                    &mut cancel,
                )
                .await
                {
                    *bound.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(serve);
                }
                return;
            }
            ChildLiveness::Reaped => return,
        }
    }
}

async fn discard_stale_staging_files(dir: &Path) {
    let Ok(entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    let mut entries = entries;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(".stage-") {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
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
    discard_stale_staging_files(dir).await;
    let target = dir.join(format!("{}-{hash}", release.component));
    let temp = dir.join(format!(".stage-{:032x}", rand::random::<u128>()));
    let staged: Result<PathBuf> = async {
        tokio::fs::write(&temp, binary).await?;
        tokio::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o700)).await?;
        ensure!(
            sha256_hex(&tokio::fs::read(&temp).await?) == hash,
            "staged binary hash mismatch"
        );
        tokio::fs::rename(&temp, &target).await?;
        Ok(target)
    }
    .await;
    if staged.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
    }
    staged
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

#[derive(Clone)]
enum PendingBootMismatch {
    Record {
        release: Box<ComponentRelease>,
        reason: String,
    },
    Clear,
}

/// Startup receipts the API cannot store until the bot is the writer.
/// Order is activation, then observation, then a failed handoff.
#[derive(Clone, PartialEq, Eq)]
enum StartupReceipt {
    BakedLauncher {
        hash: String,
        now: i64,
    },
    LauncherRunning {
        release: ComponentRelease,
        hash: String,
        now: i64,
    },
    ObserveLauncher,
    LauncherFailed {
        digest: String,
        message: String,
    },
    ComponentFailed {
        release: ComponentRelease,
        message: String,
    },
    ClearSuperseded {
        release: ComponentRelease,
    },
    ClearUncontracted {
        release: ComponentRelease,
    },
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
    /// Declared before `running` so a dropped supervisor cancels the watch
    /// before `kill_on_drop` stops the child.
    child_watch: Option<ChildExitWatch>,
    running: Option<Running>,
    restart: RestartPolicy,
    restart_target: Option<RestartTarget>,
    exited_for_restart: bool,
    /// `report_ready` already applied restart policy for the child still recorded
    /// in `running`. Settling that same exit must not count it again.
    exit_accounted: bool,
    maintenance: Option<Running>,
    health: Option<tokio::task::JoinHandle<Result<()>>>,
    health_bind: Option<String>,
    /// Test-only stall immediately before the post-exit `upgrade` read.
    #[cfg(test)]
    exit_control_gate: Option<tokio::sync::oneshot::Receiver<()>>,
    /// Test-only handoff after a readiness error, before exit recovery.
    #[cfg(test)]
    readiness_error_gate: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
    /// Test-only stall after the opening hold check and before `idle` in tick/recovery.
    #[cfg(test)]
    before_idle_gate: Option<tokio::sync::oneshot::Receiver<()>>,
    /// Test-only stall after `idle` and before the pre-control hold check.
    #[cfg(test)]
    after_idle_gate: Option<tokio::sync::oneshot::Receiver<()>>,
    staging: PathBuf,
    prepared: Option<(String, PathBuf)>,
    standby: Option<(PathBuf, String, Option<ComponentRelease>)>,
    // Independent replacement rejected before its activation receipt.
    // `persisted_candidate_digest` is the digest last saved for that candidate.
    // A later rejection of a different release must be saved again.
    rejected_candidate: Option<ComponentRelease>,
    persisted_candidate_digest: Option<String>,
    rejected_loaded: bool,
    #[cfg(test)]
    fail_rejected_lookups: u32,
    #[cfg(test)]
    fail_rejected_candidate_write: bool,
    // Failed independent selection awaiting verified prior-service readiness.
    rollback: Option<(ComponentRelease, String)>,
    pending_failure: Option<PendingFailure>,
    pending_boot_mismatch: Option<PendingBootMismatch>,
    pending_startup: Vec<StartupReceipt>,
    receipt_attempt: u32,
    receipt_not_before: Option<Instant>,
    reported: bool,
    last_observed: Option<Instant>,
}

impl Supervisor {
    async fn hold_rejected_token(&mut self) -> Result<bool> {
        // Copy the fields the child borrow must not hold. A saved digest keeps
        // the candidate when this process no longer has a standby path.
        let saved_digest = self.persisted_candidate_digest.clone();
        let have_standby = self.standby.is_some();
        let reported = self.reported;
        let rolling_back = self.rollback.is_some();
        let mut rejected_now = None;
        let mut stop_child = false;
        if self.role == "bot"
            && let Some(child) = self.running.as_mut()
            && let Some(status) = child.child.try_wait()?
            && status.code() == Some(jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE)
        {
            let matches_saved = child
                .release
                .as_ref()
                .is_some_and(|release| saved_digest.as_deref() == Some(release.digest.as_str()));
            if !reported && !rolling_back && (have_standby || matches_saved) {
                rejected_now = child.release.clone();
            }
            // A failed health bind retains this child for the next attempt.
            // Count its rejection only when first establishing the hold.
            if !self.restart.held {
                let code = status.code();
                let elapsed = child.started.elapsed();
                self.restart.exited(true, code, elapsed, Instant::now());
            }
            stop_child = true;
        }
        if let Some(release) = rejected_now {
            self.rejected_candidate = Some(release);
            // try_wait already reaped the child. Recover health before the
            // marker write can stall, retaining its identity until that write.
            self.disarm_child_health_watch().await;
            self.idle().await?;
            // Save before clearing the child record. A crash after the stop
            // used to lose the only copy of the candidate.
            self.persist_rejected_candidate().await;
        }
        if stop_child {
            jmodguard::set_process_ready(false);
            self.stop_application().await?;
        }
        // A failed write is retried on the next call, including later polls.
        self.persist_rejected_candidate().await;
        if self.restart.held {
            self.idle().await?;
        }
        Ok(self.restart.held)
    }

    async fn persist_rejected_candidate(&mut self) {
        if self.role != "bot" {
            return;
        }
        let Some(digest) = self
            .rejected_candidate
            .as_ref()
            .map(|release| release.digest.clone())
        else {
            return;
        };
        if self.persisted_candidate_digest.as_deref() == Some(digest.as_str()) {
            return;
        }
        #[cfg(test)]
        if self.fail_rejected_candidate_write {
            tracing::warn!("rejected-token candidate marker was not saved: injected write failure");
            return;
        }
        match self
            .control
            .remember_rejected_candidate(&self.role, &digest)
            .await
        {
            Ok(()) => self.persisted_candidate_digest = Some(digest),
            Err(error) => {
                tracing::warn!("rejected-token candidate marker was not saved: {error:#}");
            }
        }
    }

    async fn clear_rejected_marker(&mut self) {
        self.rejected_candidate = None;
        self.persisted_candidate_digest = None;
        if self.role != "bot" {
            return;
        }
        if let Err(error) = self.control.clear_rejected_candidate(&self.role).await {
            tracing::warn!("rejected-token candidate marker was not cleared: {error:#}");
        }
    }

    /// A fresh process has no standby path. Load a marker that still matches
    /// the selection; drop one whose selection changed while this process was down.
    async fn adopt_rejected_candidate(&mut self, state: &db::ComponentUpdate) -> Result<()> {
        if self.role != "bot" || self.rejected_loaded {
            return Ok(());
        }
        #[cfg(test)]
        if self.fail_rejected_lookups > 0 {
            self.fail_rejected_lookups -= 1;
            anyhow::bail!("rejected-candidate lookup failed");
        }
        let digest = self.control.rejected_candidate_digest(&self.role).await?;
        // An absent marker is a successful read. A failed read must retry.
        self.rejected_loaded = true;
        let Some(digest) = digest else {
            return Ok(());
        };
        if state
            .desired
            .as_ref()
            .is_some_and(|desired| desired.digest == digest)
        {
            self.rejected_candidate = state.desired.clone();
            self.persisted_candidate_digest = Some(digest);
            return Ok(());
        }
        self.clear_rejected_marker().await;
        Ok(())
    }

    async fn installed_fallback(
        &mut self,
        state: &db::ComponentUpdate,
    ) -> Result<(PathBuf, String, Option<ComponentRelease>)> {
        if let Some(release) = state.installed.clone() {
            let path = stage(
                &self.registry,
                &release,
                &format!("jmodguard-{}", self.role),
                &self.staging,
            )
            .await?;
            let sha = release.source_sha.clone();
            Ok((path, sha, Some(release)))
        } else {
            Ok((self.baked.clone(), self.base_sha.clone(), None))
        }
    }

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

    /// Saves a pending failure receipt when its backoff has elapsed.
    /// `Ok(())` never means "skip the child". Callers restart first.
    async fn flush_pending_failure(&mut self) -> Result<()> {
        if self.pending_failure.is_none() || !self.receipt_wait_elapsed() {
            return Ok(());
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
                    return Ok(());
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
                    return Ok(());
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
        Ok(())
    }

    async fn apply_startup_receipt(&self, receipt: &StartupReceipt) -> Result<()> {
        match receipt {
            StartupReceipt::BakedLauncher { hash, now } => {
                self.control.baked_launcher(&self.role, hash, *now).await
            }
            StartupReceipt::LauncherRunning { release, hash, now } => {
                self.control
                    .launcher_running(&self.role, release, hash, *now)
                    .await
            }
            StartupReceipt::ObserveLauncher => {
                self.control
                    .observe_launcher(&self.role, db::LAUNCHER_REVISION)
                    .await
            }
            StartupReceipt::LauncherFailed { digest, message } => {
                self.control
                    .launcher_failed(&self.role, digest, message)
                    .await
            }
            StartupReceipt::ComponentFailed { release, message } => {
                self.control.failed(&self.role, release, message).await
            }
            StartupReceipt::ClearSuperseded { release } => {
                self.control
                    .clear_superseded_launcher(&self.role, release)
                    .await
            }
            StartupReceipt::ClearUncontracted { release } => {
                self.control
                    .clear_uncontracted_launcher(&self.role, release)
                    .await
            }
        }
    }

    /// Send the head of the startup queue. A missing writer waits on the same
    /// capped backoff as other receipts and does not skip the application start.
    async fn flush_startup_receipts(&mut self) -> Result<()> {
        while !self.pending_startup.is_empty() && self.receipt_wait_elapsed() {
            let receipt = self.pending_startup[0].clone();
            match self.apply_startup_receipt(&receipt).await {
                Ok(()) => {
                    self.pending_startup.remove(0);
                    self.reset_receipt_backoff();
                }
                Err(error) if Self::receipt_not_saved(&error) => {
                    tracing::warn!("launcher startup receipt was not saved: {error:#}");
                    self.note_receipt_delay();
                    return Ok(());
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    async fn flush_pending_boot_mismatch(&mut self) -> Result<()> {
        if self.pending_boot_mismatch.is_none() || !self.receipt_wait_elapsed() {
            return Ok(());
        }
        let pending = self.pending_boot_mismatch.clone();
        match pending {
            Some(PendingBootMismatch::Record { release, reason }) => {
                let on_baked = self
                    .running
                    .as_ref()
                    .is_some_and(|child| child.path == self.baked);
                if !on_baked {
                    return Ok(());
                }
                match self
                    .control
                    .record_boot_mismatch(db::BootMismatchRecord {
                        component: self.role.clone(),
                        running: self.base_sha.clone(),
                        expected: db::BootMismatchExpected {
                            release_id: release.release_id.clone(),
                            source_sha: release.source_sha.clone(),
                        },
                        reason: reason.clone(),
                    })
                    .await
                {
                    Ok(()) => {
                        self.pending_boot_mismatch = None;
                        self.reset_receipt_backoff();
                    }
                    Err(error) if Self::receipt_not_saved(&error) => {
                        tracing::warn!("boot mismatch receipt was not saved: {error:#}");
                        self.note_receipt_delay();
                    }
                    Err(error) => return Err(error),
                }
            }
            Some(PendingBootMismatch::Clear) => {
                match self.control.clear_boot_mismatch(&self.role).await {
                    Ok(()) => {
                        self.pending_boot_mismatch = None;
                        self.reset_receipt_backoff();
                    }
                    Err(error) if Self::receipt_not_saved(&error) => {
                        tracing::warn!("boot mismatch clear was not saved: {error:#}");
                        self.note_receipt_delay();
                    }
                    Err(error) => return Err(error),
                }
            }
            None => {}
        }
        Ok(())
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
        // Retain an exited bot's identity for readiness/rollback handling, but
        // take over its health socket without waiting for control reads.
        // Token rejection must establish its hold reason before binding, but
        // its child record may remain while the rejected candidate is saved.
        if let Some(child) = self.running.as_mut()
            && (self.role == "api"
                || child.child.try_wait()?.is_none_or(|status| {
                    !self.restart.held
                        && status.code()
                            == Some(jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE)
                }))
        {
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
            self.health = Some(tokio::spawn(
                jmodguard::health::serve_listener_as_with_reason(
                    listener,
                    "launcher",
                    self.restart.not_ready_reason(),
                ),
            ));
        }
        Ok(())
    }

    fn arm_child_health_watch(&mut self) {
        if self.child_watch.is_some() || self.health.is_some() || self.role != "bot" {
            return;
        }
        let Some(bind) = self.health_bind.clone() else {
            return;
        };
        let Some(pid) = self.running.as_ref().and_then(|child| child.child.id()) else {
            return;
        };
        let (cancel, cancel_rx) = tokio::sync::oneshot::channel();
        let bound = std::sync::Arc::new(std::sync::Mutex::new(None));
        let slot = std::sync::Arc::clone(&bound);
        self.child_watch = Some(ChildExitWatch {
            cancel: Some(cancel),
            task: Some(tokio::spawn(async move {
                watch_child_health(pid, bind, cancel_rx, slot).await;
            })),
            bound,
        });
    }

    async fn disarm_child_health_watch(&mut self) {
        let Some(mut watch) = self.child_watch.take() else {
            return;
        };
        if let Some(cancel) = watch.cancel.take() {
            let _ = cancel.send(());
        }
        // Take the task before drop so `Drop` does not abort it mid-bind.
        if let Some(task) = watch.task.take() {
            let _ = task.await;
        }
        let serve = watch
            .bound
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        drop(watch);
        if let Some(serve) = serve {
            if self.health.is_none() {
                self.health = Some(serve);
            } else {
                serve.abort();
                let _ = serve.await;
            }
        }
    }

    async fn end_idle(&mut self) -> Result<()> {
        self.disarm_child_health_watch().await;
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
        self.stop_child(false).await
    }

    /// `restart_after_exit` keeps the local binary for the next poll. Every
    /// other stop is intentional and must not relaunch that process.
    async fn stop_child(&mut self, restart_after_exit: bool) -> Result<()> {
        // Cancel before SIGTERM so the watcher does not treat our stop as a crash.
        self.disarm_child_health_watch().await;
        if restart_after_exit && let Some(child) = &self.running {
            self.restart_target = Some(RestartTarget {
                path: child.path.clone(),
                source_sha: child.source_sha.clone(),
                release: child.release.clone(),
            });
        }
        // Keep ownership until reaping succeeds. No acknowledgement on error.
        if let Some(child) = self.running.as_mut() {
            stop(child).await?;
        }
        self.running = None;
        self.reported = false;
        self.exited_for_restart = restart_after_exit;
        self.idle().await
    }

    /// `hold_rejected_token` reaps every exit and only binds health for code 78.
    /// Any other reaped exit must bind launcher health and record restart policy
    /// before a receipt or control await. `running` stays set so this tick can
    /// still report the exit after that await. A live child is left for the watcher.
    async fn recover_reaped_ordinary_exit(&mut self) -> Result<()> {
        if self.role != "bot" {
            return Ok(());
        }
        let (code, uptime, target) = {
            let Some(child) = self.running.as_mut() else {
                return Ok(());
            };
            if child.child.id().is_some() {
                return Ok(());
            }
            let Some(status) = child.child.try_wait()? else {
                return Ok(());
            };
            if status.code() == Some(jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE) {
                return Ok(());
            }
            (
                status.code(),
                child.started.elapsed(),
                RestartTarget {
                    path: child.path.clone(),
                    source_sha: child.source_sha.clone(),
                    release: child.release.clone(),
                },
            )
        };
        self.idle().await?;
        if self.standby.is_some() || self.rollback.is_some() {
            return Ok(());
        }
        self.restart_target.get_or_insert(target);
        if !self.exit_accounted {
            self.restart.exited(true, code, uptime, Instant::now());
            self.exit_accounted = true;
        }
        self.exited_for_restart = true;
        Ok(())
    }

    /// Relaunch the last local child before any receipt or control await.
    async fn restart_exited_child(&mut self) -> Result<()> {
        if !self.exited_for_restart
            || self.running.is_some()
            || self.restart.waiting(Instant::now())
        {
            return Ok(());
        }
        let Some(target) = self.restart_target.clone() else {
            self.exited_for_restart = false;
            return Ok(());
        };
        tracing::info!("restarting child before the failure receipt is saved");
        self.start(&target.path, &target.source_sha, target.release)
            .await
    }

    /// Record an exit that `report_ready` did not reach. The watcher only binds
    /// health; restart policy stays here. A replacement with a standby binary
    /// is left in place so a later successful read can roll it back.
    async fn settle_exited_child(&mut self) -> Result<()> {
        if self.role != "bot" {
            self.exit_accounted = false;
            return Ok(());
        }
        let (code, uptime) = {
            let Some(child) = self.running.as_mut() else {
                self.exit_accounted = false;
                return Ok(());
            };
            let Some(status) = child.child.try_wait()? else {
                self.exit_accounted = false;
                return Ok(());
            };
            (status.code(), child.started.elapsed())
        };
        if code == Some(jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE) {
            self.exit_accounted = false;
            self.hold_rejected_token().await?;
            return Ok(());
        }
        if self.standby.is_some() || self.rollback.is_some() {
            return Ok(());
        }
        if !self.exit_accounted {
            self.restart.exited(true, code, uptime, Instant::now());
        }
        self.exit_accounted = false;
        self.stop_child(true).await
    }

    /// The exit may already have been reaped, so the watcher can observe
    /// `Reaped` and stop. Bind launcher health before any later control read.
    async fn bind_health_after_observed_exit(&mut self) -> Result<()> {
        if self.health.is_some() || self.role != "bot" {
            return Ok(());
        }
        if self.hold_rejected_token().await? {
            return Ok(());
        }
        #[cfg(test)]
        if let Some(gate) = self.before_idle_gate.take() {
            let _ = gate.await;
        }
        if let Some(child) = self.running.as_mut() {
            let Some(status) = child.child.try_wait()? else {
                // Keep watching a live child during the following control read.
                return Ok(());
            };
            if status.code() == Some(jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE) {
                self.hold_rejected_token().await?;
                return Ok(());
            }
        }
        // An ordinary exit may already have a watcher-owned listener.
        self.disarm_child_health_watch().await;
        self.idle().await
    }

    async fn start(
        &mut self,
        path: &Path,
        sha: &str,
        release: Option<ComponentRelease>,
    ) -> Result<()> {
        ensure!(
            !self.restart.held,
            "bot held after Discord token rejection; restart container"
        );
        if self.running.is_some() {
            anyhow::bail!("application process is already running");
        }
        if let Some(release) = &release {
            release.supports_launcher(self.launcher_revision)?;
        }
        self.end_idle().await?;
        let child = match launch(path, sha, release).await {
            Ok(child) => child,
            Err(error) => {
                // The listener was released before spawn. Put it back on this
                // tick; a later poll must not be what makes /health exist again.
                if let Err(idle_error) = self.idle().await {
                    tracing::warn!(
                        "launcher health was not restored after a failed start: {idle_error:#}"
                    );
                }
                return Err(error);
            }
        };
        self.restart_target = Some(RestartTarget {
            path: child.path.clone(),
            source_sha: child.source_sha.clone(),
            release: child.release.clone(),
        });
        self.exited_for_restart = false;
        self.exit_accounted = false;
        self.running = Some(child);
        self.rejected_candidate = None;
        self.reported = false;
        self.last_observed = None;
        self.arm_child_health_watch();
        Ok(())
    }

    async fn report_ready(&mut self) -> Result<bool> {
        // Check again here for exits racing the tick's initial check, before
        // normal/paired startup can interpret rejection as a rollback trigger.
        if self.hold_rejected_token().await? {
            return Ok(false);
        }
        let (release, seen, running_from_baked) = {
            let Some(child) = self.running.as_mut() else {
                return Ok(false);
            };
            if let Some(status) = child.child.try_wait()? {
                // A 401 can race the check above. Keep it out of generic errors.
                if self.role == "bot"
                    && status.code()
                        == Some(jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE)
                {
                    self.hold_rejected_token().await?;
                    return Ok(false);
                }
                // The pre-control recover path may already have counted this exit.
                if !self.exit_accounted {
                    self.restart.exited(
                        self.role == "bot",
                        status.code(),
                        child.started.elapsed(),
                        Instant::now(),
                    );
                    self.exit_accounted = true;
                }
                bail!("application exited: {status}");
            }
            if child.started.elapsed() >= Duration::from_secs(300) {
                self.restart.recovered();
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
                child.path == self.baked,
            )
        };
        self.restart.recovered();
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
                    self.clear_rejected_marker().await;
                }
                Err(error) if Self::receipt_not_saved(&error) => {
                    tracing::warn!("rollback receipt was not saved: {error:#}");
                    self.note_receipt_delay();
                    return Ok(false);
                }
                Err(error) => return Err(error),
            }
        } else {
            let installed_digest = release.as_ref().map(|release| release.digest.clone());
            let saved = if release.is_none() {
                self.control.baked_running(&self.role, seen, now()).await
            } else {
                self.control.running(&self.role, release, seen, now()).await
            };
            match saved {
                Ok(()) => {
                    self.standby = None;
                    self.reported = true;
                    self.last_observed = Some(Instant::now());
                    self.reset_receipt_backoff();
                    self.clear_rejected_marker().await;
                    if let Some(digest) = installed_digest.as_ref() {
                        let state = self.control.component(&self.role).await?;
                        if !running_from_baked
                            && state
                                .installed
                                .as_ref()
                                .is_some_and(|installed| &installed.digest == digest)
                        {
                            match self.clear_boot_mismatch_when_saved().await {
                                Ok(()) => {}
                                Err(error) if Self::receipt_not_saved(&error) => {
                                    tracing::warn!("boot mismatch clear was not saved: {error:#}");
                                    self.pending_boot_mismatch = Some(PendingBootMismatch::Clear);
                                    self.note_receipt_delay();
                                }
                                Err(error) => return Err(error),
                            }
                        }
                    }
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
        self.clear_rejected_marker().await;
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

    fn baked_supports_schema(&self, schema: i64) -> bool {
        if self.role == "bot" {
            (15..=self.baked_compatibility.core).contains(&schema)
        } else {
            schema == self.baked_compatibility.core
        }
    }

    async fn required_guild_schema_for_fallback(
        &self,
        state: &db::ComponentUpdate,
        release: &ComponentRelease,
    ) -> Result<i64> {
        let mut required = release
            .upgrade
            .as_ref()
            .context("installed release lacks guild schema metadata")?
            .guild_schema;
        if let Some(observed) = state.observed.as_ref().and_then(|seen| seen.guild_schema) {
            required = required.max(observed);
        }
        let counterpart = if self.role == "bot" { "api" } else { "bot" };
        let other = self.control.component(counterpart).await?;
        if let Some(observed) = other.observed.as_ref().and_then(|seen| seen.guild_schema) {
            required = required.max(observed);
        }
        if let Some(installed) = other
            .installed
            .as_ref()
            .and_then(|candidate| candidate.upgrade.as_ref())
        {
            required = required.max(installed.guild_schema);
        }
        Ok(required)
    }

    async fn baked_supports_guild_for_fallback(
        &self,
        state: &db::ComponentUpdate,
        release: &ComponentRelease,
    ) -> Result<bool> {
        let required = self
            .required_guild_schema_for_fallback(state, release)
            .await?;
        Ok(self.baked_compatibility.guild >= required)
    }

    async fn record_boot_mismatch_for_installed(
        &mut self,
        release: &ComponentRelease,
        reason: &str,
    ) -> Result<()> {
        match self
            .control
            .record_boot_mismatch(db::BootMismatchRecord {
                component: self.role.clone(),
                running: self.base_sha.clone(),
                expected: db::BootMismatchExpected {
                    release_id: release.release_id.clone(),
                    source_sha: release.source_sha.clone(),
                },
                reason: reason.to_owned(),
            })
            .await
        {
            Ok(()) => Ok(()),
            Err(error) if Self::receipt_not_saved(&error) => {
                tracing::warn!("boot mismatch receipt was not saved: {error:#}");
                self.pending_boot_mismatch = Some(PendingBootMismatch::Record {
                    release: Box::new(release.clone()),
                    reason: reason.to_owned(),
                });
                self.note_receipt_delay();
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    async fn clear_boot_mismatch_when_saved(&mut self) -> Result<()> {
        match self.control.clear_boot_mismatch(&self.role).await {
            Ok(()) => {
                self.pending_boot_mismatch = None;
                Ok(())
            }
            Err(error) if Self::receipt_not_saved(&error) => {
                tracing::warn!("boot mismatch clear was not saved: {error:#}");
                self.pending_boot_mismatch = Some(PendingBootMismatch::Clear);
                self.note_receipt_delay();
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    async fn boot_fallback_from_installed(
        &mut self,
        state: &db::ComponentUpdate,
        release: &ComponentRelease,
        reason: &str,
    ) -> Result<()> {
        let schema = self.control.schema_version().await?;
        if !self.baked_supports_schema(schema) {
            bail!("baked application cannot open database; use owner recovery");
        }
        if !self
            .baked_supports_guild_for_fallback(state, release)
            .await?
        {
            self.record_boot_mismatch_for_installed(release, SCHEMA_MISMATCH)
                .await?;
            bail!("baked application cannot open database; use owner recovery");
        }
        tracing::error!(
            component = %self.role,
            digest = %release.digest,
            baked_sha = %self.base_sha,
            reason = reason,
            "installed release could not be restored from the registry; starting baked application"
        );
        self.record_boot_mismatch_for_installed(release, reason)
            .await?;
        let baked = self.baked.clone();
        let base_sha = self.base_sha.clone();
        self.start(&baked, &base_sha, None).await?;
        Ok(())
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
            match stage(
                &self.registry,
                release,
                &format!("jmodguard-{}", self.role),
                &self.staging,
            )
            .await
            {
                Ok(path) => {
                    self.start(&path, &release.source_sha, Some(release.clone()))
                        .await?;
                }
                Err(error) => {
                    if let Some(reason) = boot_repair_reason(&error) {
                        return self
                            .boot_fallback_from_installed(state, release, reason)
                            .await;
                    }
                    return Err(error);
                }
            }
        } else {
            // A baked bot owns migrations. A protocol-aware observation alone
            // does not make an uninstalled, queued release a managed baseline.
            // Keep that queue intact, but never replace managed history or a
            // durable paired operation. Published baked migration origins start
            // at core15; the API still requires its exact runtime schema.
            let baked_migration = self.role == "bot"
                && (15..=self.baked_compatibility.core).contains(&schema)
                && state.previous.is_none()
                && (state.desired.is_none() || state.state == "queued")
                && state
                    .observed
                    .as_ref()
                    .is_none_or(|seen| seen.max_schema <= self.baked_compatibility.core)
                && self.control.upgrade().await?.is_none();
            ensure!(
                schema == self.baked_compatibility.core || baked_migration,
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
                if self.control.writes_locally() {
                    let cleared = self
                        .control
                        .clear_superseded_launcher(&self.role, release)
                        .await;
                    return match cleared {
                        Ok(()) => Ok(ControlStep::Proceed),
                        Err(error) => self.defer_unsaved(error),
                    };
                }
                let receipt = StartupReceipt::ClearSuperseded {
                    release: release.clone(),
                };
                if !self.pending_startup.contains(&receipt) {
                    self.pending_startup.push(receipt);
                }
                return Ok(ControlStep::Proceed);
            }
        } else if state.desired_launcher.is_some() {
            // An image from before launcher-revision labels has nothing to run.
            // Drop it once, the same way a contract that is not newer is dropped,
            // and keep the baked controller. The row is gone on the next boot.
            tracing::warn!(
                "stored launcher selection {} has no capability contract; clearing it and keeping the baked controller",
                release.digest
            );
            if self.control.writes_locally() {
                let cleared = self
                    .control
                    .clear_uncontracted_launcher(&self.role, release)
                    .await;
                return match cleared {
                    Ok(()) => Ok(ControlStep::Proceed),
                    Err(error) => self.defer_unsaved(error),
                };
            }
            let receipt = StartupReceipt::ClearUncontracted {
                release: release.clone(),
            };
            if !self.pending_startup.contains(&receipt) {
                self.pending_startup.push(receipt);
            }
            return Ok(ControlStep::Proceed);
        } else {
            return Ok(ControlStep::Proceed);
        }
        self.handoff(release, None, false).await
    }

    async fn normal(&mut self) -> Result<bool> {
        let state = self.control.component(&self.role).await?;
        self.adopt_rejected_candidate(&state).await?;
        // Only an unactivated auth-rejected independent selection skips restore.
        // A restarted launcher has no standby path; the durable marker is enough
        // to retry that same selection. Reuse candidate admission below.
        let saved_for_candidate = self.persisted_candidate_digest.as_ref()
            == self
                .rejected_candidate
                .as_ref()
                .map(|release| &release.digest);
        let retrying_rejected = self.running.is_none()
            && self.enabled
            && self.rollback.is_none()
            && self.rejected_candidate.is_some()
            && self.rejected_candidate == state.desired
            && (self.standby.is_some() || saved_for_candidate);
        if self.rollback.is_some()
            || (self.rejected_candidate.is_some() && self.rejected_candidate != state.desired)
        {
            self.clear_rejected_marker().await;
        }
        if !retrying_rejected {
            self.rejected_candidate = None;
        }
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
        if self.running.is_none() && !retrying_rejected {
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
            #[cfg(test)]
            if let Some((entered, gate)) = self.readiness_error_gate.take() {
                let _ = entered.send(());
                let _ = gate.await;
            }
            // report_ready reaps before this read. Bind health first so a stall
            // here cannot leave /health down after the watcher sees Reaped.
            self.bind_health_after_observed_exit().await?;
            if self.restart.held {
                return Ok(false);
            }
            #[cfg(test)]
            if let Some(gate) = self.exit_control_gate.take() {
                let _ = gate.await;
            }
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
                    Ok(()) => {
                        // The receipt cleared the saved candidate. Drop the
                        // in-memory digest so a later rejection is saved again.
                        self.persisted_candidate_digest = None;
                        return Ok(false);
                    }
                    Err(save) if Self::receipt_not_saved(&save) => {
                        tracing::warn!("rollback receipt was not saved: {save:#}");
                        self.rollback = Some((failed, message));
                        self.note_receipt_delay();
                        return Ok(false);
                    }
                    Err(save) => return Err(save),
                }
            } else {
                self.stop_child(true).await?;
                return Err(error);
            }
        }
        // A rollback receipt may have cleared desired after this tick's read.
        // Do not retry the stale failed selection until the next read.
        if restoring_previous || self.rollback.is_some() {
            return Ok(false);
        }
        if (!self.reported && !retrying_rejected) || !self.enabled {
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
        if !self.pending_startup.is_empty()
            || self.pending_failure.as_ref().is_some_and(|pending| {
                pending
                    .component
                    .as_ref()
                    .is_some_and(|(release, _)| release.digest == candidate.digest)
                    || pending
                        .launcher
                        .as_ref()
                        .is_some_and(|(digest, _)| digest == &candidate.digest)
            })
        {
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
                if registry_transient(&error) {
                    tracing::warn!(
                        component = %self.role,
                        digest = %candidate.digest,
                        "registry download stalled on transient error: {error:#}"
                    );
                    return Ok(false);
                }
                let message = signature_rejection_phrase(&error)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("download: {error:#}"));
                self.record_component_failure(candidate, message).await?;
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
        if retrying_rejected {
            // A selection can change during staging; never revive that artifact.
            if self.control.component(&self.role).await?.desired.as_ref() != Some(&candidate) {
                self.clear_rejected_marker().await;
                return Ok(false);
            }
            // A recovered candidate has no live standby child. Stage the
            // installed release before spawn so a later non-auth exit or
            // readiness timeout rolls back through the same gates.
            if self.standby.is_none() {
                self.standby = Some(self.installed_fallback(&state).await?);
            }
        } else {
            let child = self
                .running
                .as_ref()
                .context("current application missing")?;
            self.standby = Some((
                child.path.clone(),
                child.source_sha.clone(),
                child.release.clone(),
            ));
        }
        self.stop_application().await?;
        if let Err(error) = self
            .start(&path, &candidate.source_sha, Some(candidate.clone()))
            .await
        {
            // Launcher gating already ran. A failed application spawn must
            // restart the captured standby and settle the failed selection.
            // A restarted launcher no longer has that path, so stage the
            // installed release (or the baked binary) before rolling back.
            if self.standby.is_none() {
                self.standby = Some(self.installed_fallback(&state).await?);
            }
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
        // The rejected-candidate marker must not outlive this rollback.
        self.clear_rejected_marker().await;
        self.rollback = Some((failed, error));
        self.stop_application().await?;
        self.start(&path, &sha, previous).await
    }

    async fn tick(&mut self) -> Result<bool> {
        self.disarm_child_health_watch().await;
        // Hold before control reads, restore, update selection or rollback.
        let held = self.hold_rejected_token().await?;
        // Exit 78 can land after the check above. The test parks here so that
        // ordering is observable; production falls through with no wait.
        #[cfg(test)]
        if let Some(gate) = self.before_idle_gate.take() {
            let _ = gate.await;
        }
        if (held
            && !self
                .restart
                .retry_auth_with_health(
                    Instant::now(),
                    &mut self.health,
                    self.health_bind.as_deref(),
                )
                .await?)
            || (self.running.is_none() && self.restart.waiting(Instant::now()))
        {
            self.idle().await?;
            return Ok(false);
        }
        if self.role == "bot" {
            self.idle().await?;
        }
        // An ordinary exit can land after idle saw the child alive. The test
        // parks here so that ordering is observable; production does not wait.
        #[cfg(test)]
        if let Some(gate) = self.after_idle_gate.take() {
            let _ = gate.await;
        }
        // idle reaps exit 78 and leaves the socket unbound, and the reaped pid
        // cannot be watched. Hold before restart or any control await so the
        // token listener is up for the whole stall and the child is not restarted.
        if self.hold_rejected_token().await?
            && !self
                .restart
                .retry_auth_with_health(
                    Instant::now(),
                    &mut self.health,
                    self.health_bind.as_deref(),
                )
                .await?
        {
            return Ok(false);
        }
        // That hold reaps a non-78 exit without binding. Restore health and
        // record the restart before the control await, including exits reaped
        // by the opening hold check. A child that is still alive is watched
        // below, so its exit during the stall binds inside the same window.
        self.recover_reaped_ordinary_exit().await?;
        // Restart from the retained binary before any receipt or control await.
        // The receipt is retried below, or on a later poll, and must not gate this.
        self.restart_exited_child().await?;
        self.arm_child_health_watch();
        let result = self.tick_control().await;
        self.disarm_child_health_watch().await;
        // A control error can return before report_ready. Record the exit
        // anyway so the next poll can restart without another control read.
        self.settle_exited_child().await?;
        result
    }

    async fn tick_control(&mut self) -> Result<bool> {
        self.flush_startup_receipts().await?;
        self.flush_pending_failure().await?;
        self.flush_pending_boot_mismatch().await?;
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

/// Read-only startup checks for the API launcher. Receipts are returned in the
/// order they must be stored: baked or handoff activation, then the observation,
/// then a failed handoff. Nothing here writes.
async fn plan_api_startup(
    control: &UpdateControl,
    role: &str,
    hash: &str,
    handoff_digest: Option<&str>,
    launcher_failed: Option<&str>,
) -> Result<Vec<StartupReceipt>> {
    let state = control.component(role).await?;
    let mut receipts = Vec::new();
    if let Some(digest) = handoff_digest {
        let release = state
            .desired_launcher
            .clone()
            .or(state.installed_launcher.clone())
            .context("launcher selection missing")?;
        ensure!(
            release.digest == digest,
            "launcher selection changed before handoff"
        );
        ensure!(
            release
                .launcher
                .as_ref()
                .is_some_and(|contract| contract.revision == db::LAUNCHER_REVISION),
            "launcher binary capability does not match its release metadata"
        );
        receipts.push(StartupReceipt::LauncherRunning {
            release,
            hash: hash.to_owned(),
            now: now(),
        });
    } else if state
        .installed_launcher
        .as_ref()
        .is_none_or(|release| release.upgrade.is_none())
    {
        receipts.push(StartupReceipt::BakedLauncher {
            hash: hash.to_owned(),
            now: now(),
        });
    }
    receipts.push(StartupReceipt::ObserveLauncher);
    if let Some(digest) = launcher_failed {
        receipts.push(StartupReceipt::LauncherFailed {
            digest: digest.to_owned(),
            message: "updated launcher failed; stable baked controller is recovering the selected application".into(),
        });
        if let Some(pending) = state.desired.filter(|release| {
            release.digest == digest && release.required_launcher() > db::LAUNCHER_REVISION
        }) {
            receipts.push(StartupReceipt::ComponentFailed {
                release: pending,
                message: "required launcher failed; application was not updated; retry from Administration".into(),
            });
        }
    }
    Ok(receipts)
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
    let mut restart = RestartPolicy::default();
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
            && let Some(status) = child.child.try_wait()?
        {
            restart.exited(true, status.code(), child.started.elapsed(), Instant::now());
            let _ = tokio::fs::remove_file(&child.ready_file).await;
            initial = None;
            if let Some(bind) = &health_bind {
                boot_health = Some(tokio::spawn(
                    jmodguard::health::serve_listener_as_with_reason(
                        jmodguard::health::bind(bind).await?,
                        "launcher",
                        restart.not_ready_reason(),
                    ),
                ));
            }
        }
        restart
            .retry_auth_with_health(Instant::now(), &mut boot_health, health_bind.as_deref())
            .await?;
        let open_opts = opts.clone();
        match tokio::task::spawn_blocking(move || UpdateControl::open(&open_opts)).await? {
            Ok(Some(control)) => {
                break control;
            }
            Ok(None) if role == "bot" && initial.is_none() && !restart.waiting(Instant::now()) => {
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
    // Capability is checked before the application starts. The API queues the
    // receipts and serves immediately; the bot still waits until they are saved.
    // A rejected handoff must not leave Administration treating this binary as active.
    let pending_startup = if role == "api" {
        let failed = std::env::var("JMODGUARD_LAUNCHER_FAILED").ok();
        let planned = plan_api_startup(
            &control,
            &role,
            &hash,
            handoff_digest.as_deref(),
            failed.as_deref(),
        )
        .await?;
        if let Some(digest) = failed.as_deref()
            && let Some(op) = control.upgrade().await?.filter(|op| op.phase.active())
            && op.service(&role)?.release.digest == digest
        {
            control
                .upgrade_error(
                    &op.id,
                    "required launcher failed; retry the selected pair from recovery",
                )
                .await?;
        }
        planned
    } else {
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
                let note = "required launcher failed; application was not updated; retry from Administration";
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
        Vec::new()
    };
    let staging = PathBuf::from("/tmp/jmodguard-updates");
    discard_stale_staging_files(&staging).await;
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
        child_watch: None,
        running: initial,
        restart,
        restart_target: None,
        exited_for_restart: false,
        exit_accounted: false,
        maintenance,
        health: boot_health,
        health_bind,
        #[cfg(test)]
        exit_control_gate: None,
        #[cfg(test)]
        readiness_error_gate: None,
        #[cfg(test)]
        before_idle_gate: None,
        #[cfg(test)]
        after_idle_gate: None,
        staging,
        prepared: None,
        standby: None,
        rejected_candidate: None,
        persisted_candidate_digest: None,
        rejected_loaded: false,
        #[cfg(test)]
        fail_rejected_lookups: 0,
        #[cfg(test)]
        fail_rejected_candidate_write: false,
        rollback: None,
        pending_failure: None,
        pending_boot_mismatch: None,
        pending_startup,
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
                Err(error) => {
                    tracing::warn!("update controller: {error:#}");
                    match supervisor.idle().await {
                        Err(error) if supervisor.role == "bot" => {
                            tracing::warn!("launcher idle: {error:#}");
                        }
                        result => result?,
                    }
                },
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
    #[tokio::test]
    async fn launcher_fence_retries_a_transient_failure_promptly_and_stops_at_zero_rows() {
        let path =
            std::env::temp_dir().join(format!("launcher_retry_{:032x}.db", rand::random::<u128>()));
        let db = db::Db::open(&path, 2).unwrap();
        let control = UpdateControl::from_application(&db);
        control.arm_launcher_fence("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert!(control.renew_launcher_fence().await.unwrap());
        let started = Instant::now();
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let renewed =
            renew_launcher_fence_promptly(Instant::now() + Duration::from_secs(10), || async {
                let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if attempt == 0 {
                    Err(anyhow::anyhow!("connection reset"))
                } else {
                    control.renew_launcher_fence_cas().await
                }
            })
            .await
            .unwrap();
        assert!(renewed);
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(
            started.elapsed() < POLL,
            "retry must precede the next 5 s tick"
        );
        control.arm_launcher_fence("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        attempts.store(0, std::sync::atomic::Ordering::SeqCst);
        assert!(
            !renew_launcher_fence_promptly(Instant::now() + Duration::from_secs(10), || async {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                control.renew_launcher_fence_cas().await
            })
            .await
            .unwrap()
        );
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "zero rows must not retry"
        );
        drop(control);
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn launcher_rejects_ready_late_reply() {
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let result =
            renew_launcher_fence_promptly(Instant::now() + Duration::from_millis(100), || async {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(150));
                Ok(true)
            })
            .await;
        assert!(result.is_err(), "late success must never count");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn slow_launcher_error_retries_and_extends_the_fence() {
        let path =
            std::env::temp_dir().join(format!("launcher_slow_{:032x}.db", rand::random::<u128>()));
        let db = Db::open(&path, 2).unwrap();
        let control = UpdateControl::from_application(&db);
        control.arm_launcher_fence(&"a".repeat(32));
        assert!(control.renew_launcher_fence().await.unwrap());
        let before = control.launcher_fence_deadline().unwrap();
        let read_fence = || {
            db.with_conn_sync(|conn| {
                conn.query_row(
                    "SELECT value FROM meta WHERE key='launcher_lease'",
                    (),
                    |row| Ok(row.get::<String>(0)?),
                )
            })
            .unwrap()
        };
        let before_row = read_fence();
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let began = Instant::now();
        let result = renew_launcher_fence_promptly(before, || async {
            if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_secs(3)).await;
                Err(anyhow::anyhow!("provider unavailable"))
            } else {
                control.renew_launcher_fence_cas().await
            }
        })
        .await;
        assert!(
            matches!(result, Ok(true)),
            "3s non-timeout error must retry: {result:?}"
        );
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(control.launcher_fence_deadline().unwrap() > before);
        let expires = |row: &str| row.rsplit_once(' ').unwrap().1.parse::<i64>().unwrap();
        assert!(
            expires(&read_fence()) > expires(&before_row),
            "fence renewed durably"
        );
        assert!(began.elapsed() >= Duration::from_secs(3));
        eprintln!("3s injected provider error; production launcher retry owner extended fence");
        drop(control);
        drop(db);
        remove_launcher_test_db(&path);
    }

    #[tokio::test]
    async fn instant_launcher_errors_stop_before_the_deadline_with_the_last_error() {
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let began = Instant::now();
        let deadline = began + Duration::from_secs(10);
        let error = renew_launcher_fence_promptly(deadline, || async {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(anyhow::anyhow!("connection refused"))
        })
        .await
        .unwrap_err();
        let count = attempts.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            (9..=12).contains(&count),
            "250ms to 1s backoff: {count} attempts"
        );
        assert!(Instant::now() < deadline, "must not sleep across deadline");
        assert!(began.elapsed() >= Duration::from_secs(8));
        assert!(error.to_string().contains("connection refused"));
        eprintln!(
            "instant launcher errors: {count} attempts; stopped before deadline with last error"
        );
    }

    fn remove_launcher_test_db(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn launcher_recovers_at_expiry_after_a_hung_tick_and_resumes_cas() {
        assert_launcher_recovers_at_expiry(true).await;
    }

    #[tokio::test]
    async fn launcher_recovers_at_expiry_after_renewal_errors_and_resumes_cas() {
        assert_launcher_recovers_at_expiry(false).await;
    }

    async fn assert_launcher_recovers_at_expiry(hung: bool) {
        let path = std::env::temp_dir().join(format!(
            "launcher_recover_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = Db::open(&path, 2).unwrap();
        let control = UpdateControl::from_application(&db);
        control.arm_launcher_fence(&"a".repeat(32));
        assert!(control.renew_launcher_fence().await.unwrap());
        let before = control.launcher_fence_deadline().unwrap();
        // Offset the poll schedule from the real 20s authority deadline.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let renewing = control.clone();
        let renewal = tokio::spawn(async move {
            renew_launcher_fence_periodically(&renewing, || async {
                if renewing.launcher_fence_deadline() == Some(before) {
                    if hung {
                        std::future::pending::<()>().await;
                    }
                    return Err(anyhow::anyhow!("provider unavailable"));
                }
                renewing.renew_launcher_fence_cas().await
            })
            .await;
        });
        let recovered = tokio::time::timeout_at(
            tokio::time::Instant::from_std(before + Duration::from_secs(1)),
            async {
                while control.launcher_fence_deadline() == Some(before) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            },
        )
        .await;
        if recovered.is_err() {
            renewal.abort();
            let _ = renewal.await;
            panic!("spent-window renewal must reclaim before the next regular poll");
        }
        let rearmed = control.launcher_fence_deadline().unwrap();
        let resumed = tokio::time::timeout(POLL + Duration::from_secs(3), async {
            while control.launcher_fence_deadline() == Some(rearmed) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        renewal.abort();
        let _ = renewal.await;
        resumed.expect("CAS renewal must resume on the following tick");
        assert!(rearmed > before);
        assert!(control.launcher_fence_deadline().unwrap() > rearmed);
        let row: String = db
            .with_conn_sync(|conn| {
                conn.query_row(
                    "SELECT value FROM meta WHERE key='launcher_lease'",
                    (),
                    |row| Ok(row.get(0)?),
                )
            })
            .unwrap();
        assert!(row.starts_with(&"a".repeat(32)));
        assert!(
            row.rsplit_once(' ').unwrap().1.parse::<i64>().unwrap() > jmodguard::control::now_ms()
        );
        drop(control);
        drop(db);
        remove_launcher_test_db(&path);
    }

    #[tokio::test]
    async fn launcher_reclaims_an_already_expired_window_without_waiting_for_a_tick() {
        let path = std::env::temp_dir().join(format!(
            "launcher_expired_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = Db::open(&path, 2).unwrap();
        let control = UpdateControl::from_application(&db);
        control.arm_launcher_fence(&"a".repeat(32));
        assert!(control.renew_launcher_fence().await.unwrap());
        let before = control.launcher_fence_deadline().unwrap();
        tokio::time::sleep_until(tokio::time::Instant::from_std(before)).await;
        let renewal = spawn_launcher_fence_renewal(control.clone());
        let recovered = tokio::time::timeout(Duration::from_secs(1), async {
            while control.launcher_fence_deadline() == Some(before) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        renewal.abort();
        assert!(renewal.await.unwrap_err().is_cancelled());
        recovered.expect("an expired window must trigger immediate fenced acquisition");
        assert!(control.launcher_fence_deadline().unwrap() > before);
        drop(control);
        drop(db);
        remove_launcher_test_db(&path);
    }

    #[tokio::test]
    async fn launcher_periodic_tick_never_steals_a_live_foreign_fence_after_expiry() {
        let path = std::env::temp_dir().join(format!(
            "launcher_no_steal_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = Db::open(&path, 2).unwrap();
        let control = UpdateControl::from_application(&db);
        control.arm_launcher_fence(&"a".repeat(32));
        assert!(control.renew_launcher_fence().await.unwrap());
        let before = control.launcher_fence_deadline().unwrap();
        tokio::time::sleep_until(tokio::time::Instant::from_std(
            before + Duration::from_millis(50),
        ))
        .await;
        let other_db = Db::open(&path, 2).unwrap();
        let other = UpdateControl::from_application(&other_db);
        other.arm_launcher_fence(&"b".repeat(32));
        assert!(other.renew_launcher_fence().await.unwrap());
        let row = || {
            db.with_conn_sync(|conn| {
                conn.query_row(
                    "SELECT value FROM meta WHERE key='launcher_lease'",
                    (),
                    |row| Ok(row.get::<String>(0)?),
                )
            })
            .unwrap()
        };
        let held = row();
        let renewal = spawn_launcher_fence_renewal(control.clone());
        tokio::time::sleep(POLL + Duration::from_millis(500)).await;
        renewal.abort();
        let _ = renewal.await;
        assert_eq!(
            row(),
            held,
            "periodic recovery must leave another live owner untouched"
        );
        assert_eq!(
            control.launcher_fence_deadline(),
            Some(before),
            "lost fence must not rearm"
        );
        assert!(
            !control.renew_launcher_fence().await.unwrap(),
            "existing lost-fence result is false"
        );
        eprintln!("expired launcher did not steal live foreign fence; lost-fence result false");
        drop(other);
        drop(other_db);
        drop(control);
        drop(db);
        remove_launcher_test_db(&path);
    }

    use super::*;
    use db::{Db, SqlExecutor};

    #[test]
    fn crash_restarts_back_off_and_a_stable_child_resets_the_delay() {
        let mut policy = RestartPolicy::default();
        let now = Instant::now();
        for seconds in [5, 10, 20, 40, 60, 60, 60] {
            policy.exited(true, Some(2), Duration::from_secs(1), now);
            let deadline = now + Duration::from_secs(seconds);
            assert!(policy.waiting(deadline - Duration::from_millis(1)));
            assert!(!policy.waiting(deadline));
        }
        policy.exited(true, Some(1), Duration::from_secs(300), now);
        assert_eq!(policy.not_before, Some(now + POLL));
        assert!(!policy.held);
    }

    #[test]
    fn fresh_restart_policy_has_no_process_local_hold_reason() {
        assert_eq!(RestartPolicy::default().not_ready_reason(), "not ready");
    }

    #[test]
    fn retry_and_recovery_clear_the_ready_hold_reason() {
        let mut policy = RestartPolicy::default();
        let now = Instant::now();
        policy.exited(
            true,
            Some(jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE),
            Duration::ZERO,
            now,
        );
        assert_eq!(policy.not_ready_reason(), "discord token rejected");
        assert!(policy.retry_auth(policy.auth_retry_at.unwrap()));
        assert_eq!(policy.not_ready_reason(), "not ready");
        // Recovery occurs after a retry child starts, never while still held.
        policy.recovered();
        assert_eq!(policy.not_ready_reason(), "not ready");
    }

    #[test]
    fn auth_fallback_doubles_to_six_hours_and_recovery_resets_it() {
        let mut policy = RestartPolicy::default();
        let now = Instant::now();
        let code = Some(jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE);
        for seconds in [1800, 3600, 7200, 14400, 21600, 21600] {
            policy.exited(true, code, Duration::ZERO, now);
            let deadline = now + Duration::from_secs(seconds);
            assert_eq!(policy.auth_retry_at, Some(deadline));
            assert!(!policy.retry_auth(deadline - Duration::from_millis(1)));
            assert!(policy.retry_auth(deadline));
            assert!(!policy.retry_auth(deadline));
            assert!(!policy.waiting(deadline));
        }
        // A retry that stays alive five minutes also resets, even before readiness.
        policy.exited(true, code, Duration::from_secs(300), now);
        assert_eq!(policy.auth_retry_at, Some(now + Duration::from_secs(1800)));
        policy.retry_auth(now + Duration::from_secs(1800));
        policy.recovered();
        policy.exited(true, code, Duration::ZERO, now);
        assert_eq!(policy.auth_retry_at, Some(now + Duration::from_secs(1800)));
    }

    #[test]
    fn lost_lease_restarts_on_the_first_poll_and_only_bot_auth_is_held() {
        let now = Instant::now();
        for code in [Some(1), Some(2), None] {
            let mut policy = RestartPolicy::default();
            policy.exited(true, code, Duration::ZERO, now);
            assert!(!policy.held);
            assert!(!policy.waiting(now + POLL));
        }
        let code = Some(jmodguard::launcher_child::DISCORD_TOKEN_REJECTED_EXIT_CODE);
        let mut bot = RestartPolicy::default();
        bot.exited(true, code, Duration::ZERO, now);
        assert!(bot.waiting(now + Duration::from_secs(3600)));
        let mut api = RestartPolicy::default();
        api.exited(false, code, Duration::ZERO, now);
        assert!(!api.held);
        assert!(!api.waiting(now + POLL));
        let mut clean = RestartPolicy::default();
        clean.exited(true, Some(0), Duration::ZERO, now);
        assert!(!clean.waiting(now));
    }

    #[tokio::test]
    async fn published_schema_and_launcher_observations_allow_only_matching_services() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let output = Command::new("bash")
            .current_dir(&root)
            .args([
                "-c",
                "JMODGUARD_PUBLISH_IMAGE_LIB=1 source scripts/publish-image.sh; build_args api aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa test . \"\" current \"$(publish_staging_ref ghcr.io/jatmn/jmodguard-api aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa)\"",
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
        assert_eq!(
            compatible(&db, &guild_upgrade, 100)
                .await
                .unwrap_err()
                .to_string(),
            db::APP_ONLY_SCHEMA_CHANGE_REFUSED
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
