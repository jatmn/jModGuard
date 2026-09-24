//! Local process adapter and optional provider HTTP fixture recording.
//! This is not live Bunny cutover proof.
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::Config;

pub const COMPONENTS: [&str; 4] = ["frontend", "api", "bot", "conductor"];
pub const RECOVER_FORMAT: &str = "JMODGUARD-UPDATE-RECOVER-1";
pub const INVENTORY_FORMAT: &str = "JMODGUARD-UPDATE-INVENTORY-1";

const DRAIN_SECS: u64 = 5;
const BOT_DRAIN_SECS: u64 = 10;
const READY_SECS: u64 = 8;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ComponentRecord {
    pub digest: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub generation: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Inventory {
    pub format: String,
    pub release_id: String,
    pub components: BTreeMap<String, ComponentRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecoverInventory {
    pub format: String,
    pub job_id: String,
    pub release_id: String,
    pub previous_release_id: String,
    pub previous: BTreeMap<String, String>,
    pub desired: BTreeMap<String, String>,
    pub phase: String,
    pub component: String,
    pub status: String,
}

#[derive(Clone)]
pub struct Adapter {
    pub root: PathBuf,
    pub artifact_root: PathBuf,
    provider: Option<String>,
    http: reqwest::Client,
}

pub enum DrainContext {
    /// Apply/update job: SIGKILL that was required is ownership-uncertain even if the pid exited.
    Apply,
    /// recover-update: a SIGKILL that actually terminated the process is acceptable.
    Recover,
}

impl Adapter {
    pub fn from_config(cfg: &Config, http: reqwest::Client) -> Result<Self> {
        let root = cfg
            .update_root
            .clone()
            .context("JMODGUARD_CONDUCTOR_UPDATE_ROOT is not configured")?;
        let artifact_root = cfg
            .artifact_root
            .clone()
            .unwrap_or_else(|| root.join("cache"));
        Ok(Self {
            root,
            artifact_root,
            provider: cfg.update_provider.clone(),
            http,
        })
    }

    pub fn recover_path(root: &Path) -> PathBuf {
        root.join("recover.json")
    }

    pub fn inventory_path(root: &Path) -> PathBuf {
        root.join("inventory.json")
    }

    pub fn load_inventory(&self) -> Result<Inventory> {
        let path = Self::inventory_path(&self.root);
        if !path.exists() {
            return Ok(Inventory {
                format: INVENTORY_FORMAT.into(),
                release_id: String::new(),
                components: BTreeMap::new(),
            });
        }
        let raw = std::fs::read_to_string(&path).context("update inventory")?;
        let inventory: Inventory = serde_json::from_str(&raw).context("update inventory JSON")?;
        ensure!(
            inventory.format == INVENTORY_FORMAT,
            "update inventory format is not recognized"
        );
        Ok(inventory)
    }

    pub fn save_inventory(&self, inventory: &Inventory) -> Result<()> {
        std::fs::create_dir_all(&self.root)?;
        let tmp = self.root.join("inventory.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(inventory)?)?;
        std::fs::rename(tmp, Self::inventory_path(&self.root))?;
        Ok(())
    }

    pub fn load_recover(root: &Path) -> Result<Option<RecoverInventory>> {
        let path = Self::recover_path(root);
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path).context("recover inventory")?;
        let body: RecoverInventory =
            serde_json::from_str(&raw).context("recover inventory JSON")?;
        ensure!(
            body.format == RECOVER_FORMAT,
            "recover inventory format is not recognized"
        );
        Ok(Some(body))
    }

    pub fn write_recover(&self, body: &RecoverInventory) -> Result<()> {
        std::fs::create_dir_all(&self.root)?;
        let tmp = self.root.join("recover.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(body)?)?;
        std::fs::rename(tmp, Self::recover_path(&self.root))?;
        Ok(())
    }

    pub fn digest_key(digest: &str) -> String {
        digest.trim_start_matches("sha256:").to_string()
    }

    pub fn artifact_for(&self, digest: &str) -> PathBuf {
        self.artifact_root.join(Self::digest_key(digest))
    }

    pub fn staged_for(&self, digest: &str) -> PathBuf {
        self.root.join("staged").join(Self::digest_key(digest))
    }

    pub fn missing_artifact(&self, digest: &str) -> bool {
        !self.artifact_for(digest).exists()
    }

    pub fn stage(&self, digest: &str) -> Result<PathBuf> {
        let src = self.artifact_for(digest);
        ensure!(src.exists(), "release artifact is missing for {digest}");
        let dest = self.staged_for(digest);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if dest.exists() {
            if dest.is_dir() {
                std::fs::remove_dir_all(&dest)?;
            } else {
                std::fs::remove_file(&dest)?;
            }
        }
        copy_artifact(&src, &dest)?;
        Ok(dest)
    }

    pub async fn drain(
        &self,
        name: &str,
        record: &ComponentRecord,
        context: DrainContext,
    ) -> Result<()> {
        if name == "frontend" {
            return Ok(());
        }
        let Some(pid) = record.pid else {
            return Ok(());
        };
        signal_pid(pid, true)?;
        // The bot spends up to three seconds on shutdown notices before
        // Serenity closes its shards, which can take five seconds for one shard.
        let grace = if name == "bot" {
            BOT_DRAIN_SECS
        } else {
            DRAIN_SECS
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(grace);
        while std::time::Instant::now() < deadline {
            reap(pid);
            if !pid_alive(pid) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        signal_pid(pid, false)?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        reap(pid);
        if pid_alive(pid) {
            anyhow::bail!(
                "ownership uncertain; {name} pid {pid} survived SIGKILL and must be resolved before activating a replacement"
            );
        }
        match context {
            DrainContext::Recover => Ok(()),
            DrainContext::Apply => anyhow::bail!(
                "ownership uncertain; {name} required SIGKILL so the old instance may still restart"
            ),
        }
    }

    pub async fn confirm_dead(&self, name: &str, record: &ComponentRecord) -> Result<()> {
        if name == "frontend" {
            return Ok(());
        }
        if let Some(pid) = record.pid {
            reap(pid);
            ensure!(
                !pid_alive(pid),
                "old {name} process {pid} is still running; exclusive ownership is not confirmed"
            );
        }
        if let Some(port) = record.port
            && self.health_ok(port).await
        {
            anyhow::bail!(
                "old {name} instance is still serving health on port {port}; exclusive ownership is not confirmed"
            );
        }
        Ok(())
    }

    pub async fn activate(
        &self,
        name: &str,
        digest: &str,
        previous: Option<&ComponentRecord>,
    ) -> Result<ComponentRecord> {
        let staged = self.staged_for(digest);
        ensure!(staged.exists(), "staged {name} artifact is missing");
        if name == "frontend" {
            let dest = self.root.join("current").join("frontend");
            if dest.exists() {
                std::fs::remove_dir_all(&dest)?;
            }
            copy_artifact(&staged, &dest)?;
            self.provider_set(name, digest, true).await?;
            return Ok(ComponentRecord {
                digest: digest.to_string(),
                kind: "files".into(),
                pid: None,
                port: None,
                generation: previous.map(|row| row.generation).unwrap_or(0) + 1,
            });
        }
        let port = match previous.and_then(|row| row.port) {
            Some(port) => port,
            None => free_port()?,
        };
        if previous.is_none() || previous.is_some_and(|row| row.port != Some(port)) {
            // first bind uses the chosen port
        }
        let generation = previous.map(|row| row.generation).unwrap_or(0) + 1;
        let run = self.root.join("run");
        std::fs::create_dir_all(&run)?;
        let owner = run.join(format!("{name}.generation"));
        std::fs::write(&owner, generation.to_string())?;
        let exec_path = run.join(format!("{name}-{generation}.bin"));
        copy_artifact(&staged, &exec_path)?;
        if let Ok(file) = std::fs::File::open(&exec_path) {
            let _ = file.sync_all();
        }
        let mut last_error = None;
        let child = {
            let mut spawned = None;
            for _ in 0..20 {
                let mut cmd = std::process::Command::new(&exec_path);
                cmd.env("JMODGUARD_COMPONENT_NAME", name)
                    .env("JMODGUARD_COMPONENT_PORT", port.to_string())
                    .env("JMODGUARD_OWNER_FILE", &owner)
                    .env("JMODGUARD_OWNER_GENERATION", generation.to_string())
                    .env("JMODGUARD_STANDIN_DIGEST", digest)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                if let Ok(raw) =
                    std::fs::read_to_string(self.artifact_for(digest).with_extension("env"))
                {
                    for line in raw.lines() {
                        if let Some((key, value)) = line.split_once('=') {
                            cmd.env(key.trim(), value.trim());
                        }
                    }
                }
                match cmd.spawn() {
                    Ok(child) => {
                        spawned = Some(child);
                        break;
                    }
                    Err(error)
                        if error.raw_os_error() == Some(libc::ETXTBSY)
                            || error.kind() == std::io::ErrorKind::ExecutableFileBusy =>
                    {
                        last_error = Some(error);
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                    Err(error) => {
                        return Err(error).with_context(|| format!("failed to start {name}"));
                    }
                }
            }
            spawned.with_context(|| {
                format!(
                    "failed to start {name}: {}",
                    last_error
                        .map(|error| error.to_string())
                        .unwrap_or_else(|| "unknown".into())
                )
            })?
        };
        let pid = child.id();
        std::mem::forget(child);
        self.provider_set(name, digest, false).await?;
        Ok(ComponentRecord {
            digest: digest.to_string(),
            kind: "process".into(),
            pid: Some(pid),
            port: Some(port),
            generation,
        })
    }

    pub async fn wait_ready(&self, name: &str, record: &ComponentRecord) -> Result<()> {
        if name == "frontend" {
            self.provider_set(name, &record.digest, true).await?;
            return Ok(());
        }
        let port = record
            .port
            .context("process component has no health port")?;
        let deadline = std::time::Instant::now() + Duration::from_secs(READY_SECS);
        while std::time::Instant::now() < deadline {
            if self.health_ok(port).await {
                self.provider_set(name, &record.digest, true).await?;
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        anyhow::bail!("{name} failed readiness on port {port}")
    }

    async fn health_ok(&self, port: u16) -> bool {
        let url = format!("http://127.0.0.1:{port}/health");
        match self.http.get(url).send().await {
            Ok(response) => response.status().is_success(),
            Err(_) => false,
        }
    }

    async fn provider_set(&self, name: &str, digest: &str, ready: bool) -> Result<()> {
        let Some(base) = &self.provider else {
            return Ok(());
        };
        let url = format!("{}/apps/{name}/desired", base.trim_end_matches('/'));
        let response = self
            .http
            .post(&url)
            .json(&json!({ "digest": digest, "ready": ready }))
            .send()
            .await
            .context("update provider desired request failed")?;
        ensure!(
            response.status().is_success(),
            "update provider rejected desired state for {name}"
        );
        let body: Value = self
            .http
            .get(format!("{}/apps/{name}", base.trim_end_matches('/')))
            .send()
            .await
            .context("update provider status request failed")?
            .error_for_status()
            .context("update provider status request failed")?
            .json()
            .await
            .context("update provider status is not JSON")?;
        if ready {
            ensure!(
                body.get("desired").and_then(Value::as_str) == Some(digest)
                    && body.get("observed").and_then(Value::as_str) == Some(digest)
                    && body.get("ready").and_then(Value::as_bool) == Some(true),
                "update provider did not observe ready {name}"
            );
        }
        Ok(())
    }
}

fn copy_artifact(src: &Path, dest: &Path) -> Result<()> {
    if src.is_dir() {
        copy_dir(src, dest)
    } else {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dest).context("copy release artifact")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(dest)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(dest, perms)?;
        }
        Ok(())
    }
}

fn copy_dir(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dest.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

fn signal_pid(pid: u32, terminate: bool) -> Result<()> {
    if pid == 0 {
        return Ok(());
    }
    #[cfg(unix)]
    {
        let sig = if terminate {
            libc::SIGTERM
        } else {
            libc::SIGKILL
        };
        let rc = unsafe { libc::kill(pid as i32, sig) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ESRCH) {
                return Ok(());
            }
            return Err(err.into());
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, terminate);
        anyhow::bail!("local update adapter requires unix process signals")
    }
}

fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            && let Some(state) = stat.split_whitespace().nth(2)
            && (state == "Z" || state == "X")
        {
            return false;
        }
        let mut status: i32 = 0;
        let waited = unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) };
        if waited == pid as i32 {
            return false;
        }
        let rc = unsafe { libc::kill(pid as i32, 0) };
        if rc == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn reap(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG);
    }
    #[cfg(not(unix))]
    let _ = pid;
}
