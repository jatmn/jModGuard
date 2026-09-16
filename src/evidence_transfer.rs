//! Opt-in local Unix evidence transfer. No runtime scheduler or remote-provider adapter.
use crate::db::guild_data::*;
use anyhow::{Context, Result, ensure};
use ring::digest::{Context as Digest, SHA256};
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Semaphore;

#[derive(Clone)]
pub struct EvidenceTarget {
    pub scope: GuildDataScope,
    pub case_id: CaseId,
    pub evidence_id: CaseId,
}
pub struct EvidenceUpload {
    pub target: EvidenceTarget,
    pub request: EvidenceRequest,
    pub limits: EvidenceLimits,
}

/// Dedicated private local resource. Never use a backup directory, network filesystem,
/// or directory writable by untrusted actors. All writers must use this protocol.
#[derive(Clone)]
pub struct LocalEvidenceStore {
    root: PathBuf,
    storage_id: CaseId,
    buffer_bytes: usize,
    admission: Arc<Semaphore>,
}

fn directory(path: &Path, create: bool) -> Result<()> {
    if create && !path.try_exists()? {
        match fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {
                File::open(path.parent().context("directory has no parent")?)?.sync_all()?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.is_dir() && !m.file_type().is_symlink() && m.mode() & 0o077 == 0,
        "evidence directory must be private and not a symlink"
    );
    Ok(())
}
fn regular(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(m) => {
            ensure!(
                m.is_file() && m.nlink() == 1 && m.mode() & 0o077 == 0,
                "evidence file must be private, regular and unlinked elsewhere"
            );
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
fn new_file(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?)
}
fn small_file(path: &Path) -> Result<Option<String>> {
    if !regular(path)? {
        return Ok(None);
    }
    let mut text = String::new();
    File::open(path)?.take(129).read_to_string(&mut text)?;
    ensure!(text.len() <= 128, "invalid evidence marker size");
    Ok(Some(text))
}
fn sync_parent(path: &Path) -> Result<()> {
    File::open(path.parent().context("object has no parent")?)?.sync_all()?;
    Ok(())
}

impl LocalEvidenceStore {
    /// Startup/blocking operation. Only create a new/empty private resource, never adopt files.
    pub fn initialize(root: PathBuf, storage_id: CaseId, buffer_bytes: usize) -> Result<Self> {
        Self::connect(root, storage_id, buffer_bytes, true)
    }
    /// Reopen an existing exact resource identity. The stable lock file must already exist.
    pub fn open(root: PathBuf, storage_id: CaseId, buffer_bytes: usize) -> Result<Self> {
        Self::connect(root, storage_id, buffer_bytes, false)
    }
    fn identity(&self) -> String {
        format!("jmodguard-evidence-local-1\n{}\n", self.storage_id.as_str())
    }
    fn connect(
        root: PathBuf,
        storage_id: CaseId,
        buffer_bytes: usize,
        create: bool,
    ) -> Result<Self> {
        ensure!(
            root.is_absolute() && buffer_bytes > 0,
            "absolute private root and positive buffer required"
        );
        directory(&root, create)?;
        let this = Self {
            root,
            storage_id,
            buffer_bytes,
            admission: Arc::new(Semaphore::new(1)),
        };
        let identity = this.root.join("identity");
        let lock = this.root.join("lock");
        if !regular(&identity)? {
            ensure!(create, "unmarked evidence resource");
            for entry in fs::read_dir(&this.root)? {
                ensure!(
                    entry?.file_name() == "lock",
                    "refusing nonempty unmarked evidence resource"
                );
            }
            if !regular(&lock)? {
                match new_file(&lock) {
                    Ok(f) => {
                        f.sync_all()?;
                        sync_parent(&lock)?;
                    }
                    Err(e)
                        if e.downcast_ref::<std::io::Error>()
                            .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists) => {}
                    Err(e) => return Err(e),
                }
            }
        }
        let _lock = this.lock()?;
        if small_file(&identity)?.is_none() {
            ensure!(create, "unmarked evidence resource");
            for entry in fs::read_dir(&this.root)? {
                ensure!(
                    entry?.file_name() == "lock",
                    "refusing nonempty unmarked evidence resource"
                );
            }
            let mut f = new_file(&identity)?;
            f.write_all(this.identity().as_bytes())?;
            f.sync_all()?;
            sync_parent(&identity)?;
        }
        this.verify()?;
        Ok(this)
    }
    fn lock(&self) -> Result<File> {
        directory(&self.root, false)?;
        let path = self.root.join("lock");
        ensure!(regular(&path)?, "missing stable resource lock");
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        file.try_lock()
            .context("evidence resource busy or locking unsupported")?;
        Ok(file)
    }
    fn verify(&self) -> Result<()> {
        ensure!(
            small_file(&self.root.join("identity"))?.as_deref() == Some(self.identity().as_str()),
            "evidence resource identity mismatch"
        );
        Ok(())
    }

    async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(Self, tokio::runtime::Handle) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let permit = self
            .admission
            .clone()
            .try_acquire_owned()
            .context("evidence transfer already running")?;
        let this = self.clone();
        let runtime = tokio::runtime::Handle::current();
        // A cancelled async waiter must not release the lock while blocking I/O continues.
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _lock = this.lock()?;
            this.verify()?;
            operation(this, runtime)
        })
        .await?
    }
    fn paths(&self, job: &EvidenceJob, create: bool) -> Result<[PathBuf; 3]> {
        ensure!(
            job.storage_id == self.storage_id,
            "evidence resource alias mismatch"
        );
        let key = Path::new(&job.object_key);
        ensure!(
            key.components().all(|c| matches!(c, Component::Normal(_))),
            "invalid derived object key"
        );
        let mut parent = self.root.clone();
        let mut missing = false;
        for part in key.parent().context("missing object parent")?.components() {
            parent.push(part.as_os_str());
            if create {
                directory(&parent, true)?;
            } else if !missing {
                match fs::symlink_metadata(&parent) {
                    Ok(_) => directory(&parent, false)?,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => missing = true,
                    Err(e) => return Err(e.into()),
                }
            }
        }
        let base = self.root.join(key);
        let paths = [
            base.with_extension("data"),
            base.with_extension("complete"),
            base.with_extension("complete-tmp"),
        ];
        for path in &paths {
            regular(path)?;
        }
        Ok(paths)
    }
    fn buffer(&self) -> Result<Vec<u8>> {
        let mut b = Vec::new();
        b.try_reserve_exact(self.buffer_bytes)?;
        b.resize(self.buffer_bytes, 0);
        Ok(b)
    }
    fn stream(
        &self,
        reader: &mut impl Read,
        mut writer: Option<&mut File>,
        bytes: u64,
    ) -> Result<String> {
        let mut buffer = self.buffer()?;
        let mut digest = Digest::new(&SHA256);
        let mut total = 0u64;
        loop {
            let wanted = (bytes - total).saturating_add(1).min(buffer.len() as u64) as usize;
            let n = match reader.read(&mut buffer[..wanted]) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                other => other?,
            };
            if n == 0 {
                break;
            }
            ensure!(
                n as u64 <= bytes - total,
                "evidence stream exceeds reservation"
            );
            if let Some(w) = writer.as_mut() {
                w.write_all(&buffer[..n])?;
            }
            digest.update(&buffer[..n]);
            total += n as u64;
        }
        ensure!(total == bytes, "evidence stream shorter than reservation");
        let mut encoded = String::with_capacity(64);
        for byte in digest.finish().as_ref() {
            write!(&mut encoded, "{byte:02x}")?;
        }
        Ok(encoded)
    }
    fn write_object(&self, job: &EvidenceJob, source: &mut impl Read) -> Result<String> {
        let [data, complete, temp] = self.paths(job, true)?;
        ensure!(
            !regular(&data)? && !regular(&complete)? && !regular(&temp)?,
            "object artifacts already exist; reconcile instead"
        );
        let digest = {
            let mut file = new_file(&data)?;
            let digest = self.stream(source, Some(&mut file), job.reserved_bytes)?;
            file.sync_all()?;
            digest
        };
        ensure!(
            self.stream(&mut File::open(&data)?, None, job.reserved_bytes)? == digest,
            "stored evidence checksum mismatch"
        );
        let mut receipt = new_file(&temp)?;
        writeln!(receipt, "{}\n{}", job.reserved_bytes, digest)?;
        receipt.sync_all()?;
        drop(receipt);
        fs::rename(&temp, &complete)?;
        sync_parent(&complete)?;
        Ok(digest)
    }
    fn verified_digest(&self, job: &EvidenceJob) -> Result<Option<String>> {
        let [data, complete, _] = self.paths(job, false)?;
        let Some(receipt) = small_file(&complete)? else {
            return Ok(None);
        };
        let mut lines = receipt.lines();
        let size = lines
            .next()
            .context("invalid completion receipt")?
            .parse::<u64>()?;
        let hash = lines.next().context("missing completion checksum")?;
        ensure!(
            lines.next().is_none()
                && size == job.reserved_bytes
                && hash.len() == 64
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid completion receipt"
        );
        ensure!(regular(&data)?, "missing completed evidence");
        let mut file = File::open(data)?;
        let actual = self.stream(&mut file, None, size)?;
        ensure!(actual == hash, "stored evidence checksum mismatch");
        // A previous process may have died after rename but before syncing the
        // directory. Recovery must persist the verified artifacts before SQL settlement.
        file.sync_all()?;
        File::open(&complete)?.sync_all()?;
        sync_parent(&complete)?;
        Ok(Some(actual))
    }

    /// Verified private object bytes for an authorized dashboard/API reader.
    pub fn read_available(&self, job: &EvidenceJob, expected_sha: &str) -> Result<Vec<u8>> {
        let _lock = self.lock()?;
        self.verify()?;
        let digest = self
            .verified_digest(job)?
            .context("evidence is not available")?;
        ensure!(digest == expected_sha, "stored evidence checksum mismatch");
        let [data, _, _] = self.paths(job, false)?;
        let mut buf = Vec::new();
        File::open(data)?.read_to_end(&mut buf)?;
        ensure!(
            buf.len() as u64 == job.reserved_bytes,
            "stored evidence size mismatch"
        );
        Ok(buf)
    }

    pub fn storage_id(&self) -> &CaseId {
        &self.storage_id
    }

    pub async fn capture<R: Read + Send + 'static>(
        &self,
        store: GuildDataStore,
        upload: EvidenceUpload,
        mut source: R,
    ) -> Result<EvidenceState> {
        ensure!(
            upload.request.storage_id == self.storage_id,
            "evidence resource alias mismatch"
        );
        self.run(move |this, rt| {
            let t = upload.target;
            let reserved = rt.block_on(store.reserve_case_evidence(
                t.scope.clone(),
                t.case_id.clone(),
                t.evidence_id.clone(),
                upload.request,
                upload.limits,
            ))?;
            ensure!(
                reserved.created,
                "existing reservation requires reconciliation, never a repeated upload"
            );
            let result = this.write_object(&reserved.job, &mut source);
            match result {
                Ok(sha256) => rt.block_on(store.settle_evidence_capture(
                    t.scope,
                    t.case_id,
                    t.evidence_id,
                    EvidenceCapture::Stored {
                        bytes: reserved.job.reserved_bytes,
                        sha256,
                    },
                )),
                Err(error) => {
                    rt.block_on(store.settle_evidence_capture(
                        t.scope,
                        t.case_id,
                        t.evidence_id,
                        EvidenceCapture::Discarded,
                    ))
                    .context("capture failed and settlement remains unresolved")?;
                    Err(error)
                }
            }
        })
        .await
    }
    /// Recover without reading the original source again. The resource lock excludes
    /// all cooperating local writes, unlike an HTTP timeout or a remote object 404.
    pub async fn reconcile(
        &self,
        store: GuildDataStore,
        target: EvidenceTarget,
    ) -> Result<EvidenceState> {
        self.run(move |this, rt| {
            let job = rt.block_on(store.evidence_transfer_job(
                target.scope.clone(),
                target.case_id.clone(),
                target.evidence_id.clone(),
            ))?;
            this.paths(&job, false)?;
            if job.state != EvidenceState::Capturing {
                return Ok(job.state);
            }
            let verified = if job.cancelled {
                Ok(None)
            } else {
                this.verified_digest(&job)
            };
            let capture = match &verified {
                Ok(Some(sha256)) => EvidenceCapture::Stored {
                    bytes: job.reserved_bytes,
                    sha256: sha256.clone(),
                },
                _ => EvidenceCapture::Discarded,
            };
            let state = rt.block_on(store.settle_evidence_capture(
                target.scope,
                target.case_id,
                target.evidence_id,
                capture,
            ))?;
            verified?;
            Ok(state)
        })
        .await
    }
    pub async fn cleanup(&self, store: GuildDataStore, target: EvidenceTarget) -> Result<bool> {
        self.run(move |this, rt| {
            let job = rt.block_on(store.evidence_transfer_job(
                target.scope.clone(),
                target.case_id.clone(),
                target.evidence_id.clone(),
            ))?;
            let paths = this.paths(&job, false)?;
            let state = if job.state == EvidenceState::Capturing && job.cancelled {
                rt.block_on(store.settle_evidence_capture(
                    target.scope.clone(),
                    target.case_id.clone(),
                    target.evidence_id.clone(),
                    EvidenceCapture::Discarded,
                ))?
            } else {
                job.state
            };
            ensure!(
                matches!(state, EvidenceState::CleanupPending | EvidenceState::Erased),
                "evidence has not been cancelled and settled"
            );
            for path in &paths {
                if regular(path)? {
                    fs::remove_file(path)?;
                    sync_parent(path)?;
                }
            }
            // Remove only empty owned key directories, retaining the resource marker/lock.
            let mut parent = paths[0].parent();
            while let Some(path) = parent.filter(|p| *p != this.root) {
                match fs::remove_dir(path) {
                    Ok(()) => sync_parent(path)?,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                        // A prior attempt may have unlinked artifacts/directories but
                        // failed their sync. Persist absence even on an otherwise no-op retry.
                        File::open(path)?.sync_all()?;
                        break;
                    }
                    Err(e) => return Err(e.into()),
                }
                parent = path.parent();
            }
            File::open(&this.root)?.sync_all()?;
            for path in &paths {
                ensure!(!regular(path)?, "evidence artifact remains");
            }
            rt.block_on(store.confirm_evidence_deleted(
                target.scope,
                target.case_id,
                target.evidence_id,
            ))
        })
        .await
    }
}

#[cfg(test)]
mod tests;
