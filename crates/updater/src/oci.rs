//! Anonymous, digest-pinned reads of the project's public GHCR OCI images.
//! Registry tags are discovery pointers; a queued install keeps its digest.
use crate::{http_client_builder, signature};
use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use jmodguard::db::ComponentRelease;
use reqwest::header::{ACCEPT, CONTENT_LENGTH};
use ring::digest::{Context as DigestContext, SHA256, digest};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MAX_MANIFEST: usize = 2 * 1024 * 1024;
const MAX_CONFIG: usize = 256 * 1024;
const MAX_LAYER: usize = 128 * 1024 * 1024;
const MAX_UNPACKED: u64 = 256 * 1024 * 1024;
const MAX_FILE: u64 = 64 * 1024 * 1024;
const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(digest(&SHA256, bytes).as_ref())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Hash a file incrementally instead of reading it into memory.
pub async fn file_sha256(path: &Path) -> Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut context = DigestContext::new(&SHA256);
    let mut buffer = vec![0; STREAM_BUFFER];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            return Ok(hex(context.finish().as_ref()));
        }
        context.update(&buffer[..read]);
    }
}

const STREAM_BUFFER: usize = 64 * 1024;
const EXECUTABLE_HEADER: usize = 4;

/// A file in the launcher staging directory. It is removed on drop, including
/// when staging fails or its future is cancelled, unless it was renamed into place.
struct TempFile(Option<PathBuf>);

impl TempFile {
    fn new(dir: &Path, prefix: &str) -> Self {
        Self(Some(
            dir.join(format!(".{prefix}-{:032x}", rand::random::<u128>())),
        ))
    }

    fn path(&self) -> &Path {
        self.0
            .as_deref()
            .expect("temporary file was already persisted")
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The requested executable, streamed from a digest-verified release layer to
/// an unpublished file in the staging directory, plus the image file inventory.
pub struct StagedExecutable {
    file: TempFile,
    sha256: String,
    size: u64,
    header: Vec<u8>,
    inventory: Vec<u8>,
}

impl StagedExecutable {
    pub fn path(&self) -> &Path {
        self.file.path()
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// The executable's first bytes, enough for its ELF magic.
    pub fn header(&self) -> &[u8] {
        &self.header
    }

    pub fn inventory(&self) -> &[u8] {
        &self.inventory
    }

    /// Atomically rename the executable to `target`; it is no longer removed on drop.
    pub async fn persist(mut self, target: &Path) -> Result<()> {
        tokio::fs::rename(self.file.path(), target).await?;
        self.file.0 = None;
        Ok(())
    }
}

struct HashingWriter<W> {
    inner: W,
    context: DigestContext,
    size: u64,
    header: Vec<u8>,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        let bytes = &buf[..written];
        self.context.update(bytes);
        let missing = EXECUTABLE_HEADER.saturating_sub(self.header.len());
        self.header
            .extend_from_slice(&bytes[..missing.min(bytes.len())]);
        self.size += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn valid_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// A registry request failed in a way that permits outage fallback.
/// Connection failure, request timeout, HTTP 408/429, and server errors are
/// unreachable. Response-body failures (including truncated downloads), signature
/// refusals, and other responses do not qualify, even after earlier fetches succeeded.
pub fn registry_unreachable(error: &anyhow::Error) -> bool {
    if signature::signature_rejection_phrase(error).is_some() {
        return false;
    }
    // A response body failure can retain a nested connection reset. Once the
    // response is being read, that reset indicates a failed download.
    if error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|request| request.is_body() || request.is_decode())
    }) {
        return false;
    }
    if error.chain().any(|cause| {
        if let Some(request) = cause.downcast_ref::<reqwest::Error>()
            && (request.is_connect()
                || (request.is_request() && request.is_timeout())
                || request
                    .status()
                    .is_some_and(|status| registry_outage_status(status.as_u16())))
        {
            return true;
        }
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::NotConnected
                    | std::io::ErrorKind::HostUnreachable
                    | std::io::ErrorKind::NetworkUnreachable
            )
        })
    }) {
        return true;
    }
    let message = error.to_string();
    message
        .split_once("registry returned HTTP ")
        .and_then(|(_, response)| response.split_whitespace().next())
        .and_then(|status| status.parse().ok())
        .is_some_and(registry_outage_status)
}

fn registry_outage_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 500..=599)
}

fn repository(component: &str) -> Result<&'static str> {
    match component {
        "bot" => Ok("jatmn/jmodguard-bot"),
        "api" => Ok("jatmn/jmodguard-api"),
        "webui" => Ok("jatmn/jmodguard-webui"),
        _ => bail!("unsupported update component"),
    }
}

#[derive(Debug, Deserialize)]
struct Descriptor {
    digest: String,
    size: u64,
    #[serde(rename = "mediaType")]
    media_type: String,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
}

pub(crate) struct VerifiedReleaseLayer {
    repo: &'static str,
    descriptor: Descriptor,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    config: Descriptor,
    layers: Vec<Descriptor>,
}

#[derive(Debug, Deserialize)]
struct ImageConfig {
    architecture: String,
    os: String,
    config: ImageConfigInner,
}

#[derive(Debug, Deserialize)]
struct ImageConfigInner {
    #[serde(rename = "Labels", default)]
    labels: BTreeMap<String, String>,
}

#[derive(Clone)]
pub struct Registry {
    http: reqwest::Client,
    base: String,
}

impl Registry {
    pub fn ghcr() -> Result<Self> {
        Self::new("https://ghcr.io")
    }

    /// The base override is for local registry fixtures; production callers
    /// construct only `ghcr()` and choose among fixed project repositories.
    pub fn new(base: &str) -> Result<Self> {
        let parsed = reqwest::Url::parse(base)?;
        ensure!(
            matches!(parsed.scheme(), "https" | "http")
                && parsed.host_str().is_some()
                && parsed.path() == "/"
                && parsed.query().is_none(),
            "invalid registry base"
        );
        Ok(Self {
            http: http_client_builder()
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::limited(2))
                .build()?,
            base: base.trim_end_matches('/').to_owned(),
        })
    }

    async fn token(&self, repo: &str) -> Result<Option<String>> {
        if self.base != "https://ghcr.io" {
            return Ok(None);
        }
        #[derive(Deserialize)]
        struct Token {
            token: String,
        }
        let url = format!(
            "{}/token?scope=repository:{repo}:pull&service=ghcr.io",
            self.base
        );
        let token: Token = self
            .http
            .get(url)
            .send()
            .await
            .map_err(reqwest::Error::without_url)?
            .error_for_status()
            .map_err(reqwest::Error::without_url)?
            .json()
            .await
            .map_err(reqwest::Error::without_url)?;
        ensure!(!token.token.is_empty(), "GHCR returned an empty pull token");
        Ok(Some(token.token))
    }

    async fn get_limited(
        &self,
        url: String,
        token: Option<&str>,
        accept: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<u8>, reqwest::header::HeaderMap)> {
        let mut response = self.open_limited(url, token, accept, limit).await?;
        let headers = response.headers().clone();
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(reqwest::Error::without_url)?
        {
            ensure!(
                body.len().saturating_add(chunk.len()) <= limit,
                "registry object exceeds size limit"
            );
            body.extend_from_slice(&chunk);
        }
        Ok((body, headers))
    }

    async fn open_limited(
        &self,
        url: String,
        token: Option<&str>,
        accept: Option<&str>,
        limit: usize,
    ) -> Result<reqwest::Response> {
        let mut request = self.http.get(url);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(accept) = accept {
            request = request.header(ACCEPT, accept);
        }
        let response = request.send().await.map_err(reqwest::Error::without_url)?;
        ensure!(
            response.status().is_success(),
            "registry returned HTTP {}",
            response.status()
        );
        if let Some(raw) = response.headers().get(CONTENT_LENGTH)
            && let Ok(length) = raw.to_str()?.parse::<u64>()
        {
            ensure!(length <= limit as u64, "registry object exceeds size limit");
        }
        Ok(response)
    }

    async fn signature_body(&self, repo: &str, manifest_digest: &str) -> Result<Vec<u8>> {
        let tag = signature::signature_tag_for_digest(manifest_digest)?;
        let token = self.token(repo).await?;
        let url = format!("{}/v2/{repo}/manifests/{tag}", self.base);
        match self
            .get_limited(url, token.as_deref(), Some(MANIFEST_ACCEPT), MAX_MANIFEST)
            .await
        {
            Ok((body, _)) => Ok(body),
            Err(error) if error.to_string().contains("registry returned HTTP 404") => {
                bail!(signature::ImageSignatureRejection {
                    phrase: signature::UNSIGNED_IMAGE,
                });
            }
            Err(error) => Err(error),
        }
    }

    async fn verify_manifest(
        &self,
        component: &str,
        repo: &str,
        manifest_digest: &str,
        manifest_bytes: &[u8],
        expiry: signature::SignatureExpiry,
    ) -> Result<()> {
        let signature = self.signature_body(repo, manifest_digest).await?;
        if serde_json::from_slice::<serde_json::Value>(&signature)
            .ok()
            .is_some_and(|value| {
                value.get("payloadType").is_some() && value.get("payload").is_some()
            })
        {
            return signature::verify_manifest_signature(
                component,
                manifest_digest,
                manifest_bytes,
                &signature,
                Utc::now(),
                expiry,
            );
        }
        let manifest: Manifest =
            serde_json::from_slice(&signature).map_err(|_| signature::ImageSignatureRejection {
                phrase: signature::BAD_IMAGE_SIGNATURE,
            })?;
        ensure!(
            manifest.schema_version == 2 && !manifest.layers.is_empty(),
            signature::ImageSignatureRejection {
                phrase: signature::BAD_IMAGE_SIGNATURE,
            }
        );
        let mut last = None;
        for layer in manifest.layers.iter().rev() {
            let Some(signature_b64) = layer
                .annotations
                .get("dev.cosignproject.cosign/signature")
                .map(String::as_str)
            else {
                continue;
            };
            let payload = match self.blob(repo, layer, MAX_MANIFEST).await {
                Ok(payload) => payload,
                Err(error) if registry_unreachable(&error) => return Err(error),
                Err(_) => {
                    last = Some(
                        signature::ImageSignatureRejection {
                            phrase: signature::BAD_IMAGE_SIGNATURE,
                        }
                        .into(),
                    );
                    continue;
                }
            };
            let envelope = match signature::envelope_from_cosign_payload(&payload, signature_b64) {
                Ok(envelope) => envelope,
                Err(error) => {
                    last = Some(error);
                    continue;
                }
            };
            match signature::verify_manifest_signature(
                component,
                manifest_digest,
                manifest_bytes,
                &envelope,
                Utc::now(),
                expiry,
            ) {
                Ok(()) => return Ok(()),
                Err(error) => last = Some(error),
            }
        }
        Err(last.unwrap_or_else(|| {
            signature::ImageSignatureRejection {
                phrase: signature::BAD_IMAGE_SIGNATURE,
            }
            .into()
        }))
    }

    async fn manifest(&self, repo: &str, reference: &str) -> Result<(Manifest, String, Vec<u8>)> {
        ensure!(
            !reference.contains('/') && !reference.contains('?') && !reference.contains('#'),
            "invalid image reference"
        );
        let token = self.token(repo).await?;
        let url = format!("{}/v2/{repo}/manifests/{reference}", self.base);
        let (bytes, headers) = self
            .get_limited(url, token.as_deref(), Some(MANIFEST_ACCEPT), MAX_MANIFEST)
            .await?;
        let digest = format!("sha256:{}", sha256_hex(&bytes));
        if valid_digest(reference) {
            ensure!(digest == reference, "manifest digest mismatch");
        }
        if let Some(value) = headers.get("docker-content-digest") {
            ensure!(
                value.to_str()? == digest,
                "registry manifest digest header mismatch"
            );
        }
        let parsed: Manifest = serde_json::from_slice(&bytes)?;
        ensure!(
            parsed.schema_version == 2 && !parsed.layers.is_empty() && parsed.layers.len() <= 32,
            "unsupported image manifest"
        );
        ensure!(
            valid_digest(&parsed.config.digest) && parsed.config.size as usize <= MAX_CONFIG,
            "invalid image config descriptor"
        );
        for layer in &parsed.layers {
            ensure!(
                valid_digest(&layer.digest) && layer.size as usize <= MAX_LAYER,
                "invalid image layer descriptor"
            );
        }
        Ok((parsed, digest, bytes))
    }

    async fn blob(&self, repo: &str, descriptor: &Descriptor, limit: usize) -> Result<Vec<u8>> {
        ensure!(
            valid_digest(&descriptor.digest) && descriptor.size <= limit as u64,
            "invalid blob descriptor"
        );
        let token = self.token(repo).await?;
        let url = format!("{}/v2/{repo}/blobs/{}", self.base, descriptor.digest);
        let (bytes, _) = self.get_limited(url, token.as_deref(), None, limit).await?;
        ensure!(bytes.len() as u64 == descriptor.size, "blob size mismatch");
        ensure!(
            format!("sha256:{}", sha256_hex(&bytes)) == descriptor.digest,
            "blob digest mismatch"
        );
        Ok(bytes)
    }

    /// `blob` written to `path` with incremental hashing. The caller trusts
    /// nothing in the file unless this returns success.
    async fn blob_to_file(
        &self,
        repo: &str,
        descriptor: &Descriptor,
        limit: usize,
        dir: &Path,
    ) -> Result<TempFile> {
        ensure!(
            valid_digest(&descriptor.digest) && descriptor.size <= limit as u64,
            "invalid blob descriptor"
        );
        // The blocking task owns the cleanup guard from creation, so a cancelled
        // caller cannot drop the guard before a queued creation runs.
        let dir = dir.to_owned();
        let (file, blob) = tokio::task::spawn_blocking(move || -> Result<_> {
            let blob = TempFile::new(&dir, "layer");
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(blob.path())?;
            Ok((file, blob))
        })
        .await??;
        let mut file = tokio::fs::File::from_std(file);
        let token = self.token(repo).await?;
        let url = format!("{}/v2/{repo}/blobs/{}", self.base, descriptor.digest);
        let mut response = self
            .open_limited(url, token.as_deref(), None, limit)
            .await?;
        let mut context = DigestContext::new(&SHA256);
        let mut received = 0u64;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(reqwest::Error::without_url)?
        {
            ensure!(
                received.saturating_add(chunk.len() as u64) <= limit as u64,
                "registry object exceeds size limit"
            );
            context.update(&chunk);
            file.write_all(&chunk).await?;
            received += chunk.len() as u64;
        }
        file.flush().await?;
        ensure!(received == descriptor.size, "blob size mismatch");
        ensure!(
            format!("sha256:{}", hex(context.finish().as_ref())) == descriptor.digest,
            "blob digest mismatch"
        );
        Ok(blob)
    }

    async fn config(&self, repo: &str, manifest: &Manifest) -> Result<ImageConfig> {
        let bytes = self.blob(repo, &manifest.config, MAX_CONFIG).await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Resolve a release channel to one exact image digest and its validated
    /// metadata. Old images missing these labels are observation-only.
    pub async fn discover(&self, component: &str, channel: &str) -> Result<ComponentRelease> {
        ensure!(
            matches!(channel, "latest" | "stable" | "nightly"),
            "invalid release channel"
        );
        self.discover_ref(component, channel, channel).await
    }

    /// Resolve a counterpart at the selected full source revision, never a
    /// second moving channel tag. All normal repository/platform checks apply.
    pub async fn discover_source(
        &self,
        component: &str,
        channel: &str,
        sha: &str,
    ) -> Result<ComponentRelease> {
        ensure!(
            sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid source revision"
        );
        let release = self.discover_ref(component, channel, sha).await?;
        ensure!(
            release.source_sha == sha,
            "counterpart source revision mismatch"
        );
        Ok(release)
    }

    async fn discover_ref(
        &self,
        component: &str,
        channel: &str,
        reference: &str,
    ) -> Result<ComponentRelease> {
        ensure!(
            matches!(channel, "latest" | "stable" | "nightly"),
            "invalid channel"
        );
        let repo = repository(component)?;
        let (manifest, image_digest, manifest_bytes) = self.manifest(repo, reference).await?;
        self.verify_manifest(
            component,
            repo,
            &image_digest,
            &manifest_bytes,
            signature::SignatureExpiry::Enforce,
        )
        .await?;
        let config = self.config(repo, &manifest).await?;
        ensure!(
            config.architecture == "amd64" && config.os == "linux",
            "unsupported image platform"
        );
        let labels = &config.config.labels;
        let label = |key: &str| -> Result<String> {
            labels
                .get(key)
                .cloned()
                .with_context(|| format!("release label {key} is missing"))
        };
        ensure!(
            label("io.jmodguard.component")? == component,
            "component label mismatch"
        );
        let published_channel = label("io.jmodguard.channel")?;
        ensure!(
            if channel == "latest" {
                matches!(published_channel.as_str(), "development" | "stable")
            } else {
                published_channel == channel
            },
            "channel label mismatch"
        );
        let release = ComponentRelease {
            upgrade: if labels.contains_key("io.jmodguard.update-control") {
                Some(jmodguard::db::UpgradeCompatibility {
                    control_protocol: label("io.jmodguard.update-control")?.parse()?,
                    min_core: label("io.jmodguard.upgrade-min-core")?.parse()?,
                    min_guild: label("io.jmodguard.upgrade-min-guild")?.parse()?,
                    guild_schema: label("io.jmodguard.guild-schema")?.parse()?,
                })
            } else {
                None
            },
            component: component.into(),
            channel: channel.into(),
            release_id: label("io.jmodguard.release-id")?,
            source_sha: label("org.opencontainers.image.revision")?,
            published_at: label("io.jmodguard.published-at")?,
            image: format!("ghcr.io/{repo}"),
            digest: image_digest,
            min_schema: label("io.jmodguard.min-schema")?.parse()?,
            max_schema: label("io.jmodguard.max-schema")?.parse()?,
            result_schema: label("io.jmodguard.result-schema")?.parse()?,
            min_api: label("io.jmodguard.min-api")?.parse()?,
            max_api: label("io.jmodguard.max-api")?.parse()?,
            launcher_protocol: label("io.jmodguard.launcher-protocol")?.parse()?,
            launcher: if labels.contains_key("io.jmodguard.launcher-revision")
                || labels.contains_key("io.jmodguard.min-launcher-revision")
            {
                Some(jmodguard::db::LauncherCompatibility {
                    revision: label("io.jmodguard.launcher-revision")?.parse()?,
                    min_revision: label("io.jmodguard.min-launcher-revision")?.parse()?,
                })
            } else {
                None
            },
            base_abi: label("io.jmodguard.base-abi")?,
        };
        release.validate()?;
        ensure!(
            chrono::DateTime::parse_from_rfc3339(&release.published_at).is_ok(),
            "invalid release publication time"
        );
        Ok(release)
    }

    /// Pull just the image's final COPY layer by the approved digest. Reject
    /// any path ambiguity or special tar entry before handing bytes to a
    /// launcher or Storage writer.
    pub async fn files(
        &self,
        release: &ComponentRelease,
        requested: &[&str],
    ) -> Result<HashMap<String, Vec<u8>>> {
        self.files_with(release, requested, signature::SignatureExpiry::Enforce)
            .await
    }

    /// Re-read an already-installed release. The signature must still verify.
    /// `io.jmodguard.not-after` does not block this download. The durable
    /// installation record must match before expiry can be bypassed.
    pub async fn files_for_installed_restore(
        &self,
        control: &jmodguard::db::UpdateControl,
        release: &ComponentRelease,
        requested: &[&str],
    ) -> Result<HashMap<String, Vec<u8>>> {
        let layer = self.installed_layer(control, release, requested).await?;
        self.read_release_layer(layer, requested).await
    }

    /// Launcher staging form of `files`. The verified layer and the
    /// executable stream through `dir` instead of both being held in memory.
    pub async fn stage_executable(
        &self,
        release: &ComponentRelease,
        executable: &str,
        inventory: &str,
        dir: &Path,
    ) -> Result<StagedExecutable> {
        let layer = self
            .verified_release_layer(
                release,
                &[executable, inventory],
                signature::SignatureExpiry::Enforce,
            )
            .await?;
        self.stage_release_layer(layer, executable, inventory, dir)
            .await
    }

    /// Launcher staging form of `files_for_installed_restore`, with the same
    /// installation record and signature requirements.
    pub async fn stage_installed_executable(
        &self,
        control: &jmodguard::db::UpdateControl,
        release: &ComponentRelease,
        executable: &str,
        inventory: &str,
        dir: &Path,
    ) -> Result<StagedExecutable> {
        let layer = self
            .installed_layer(control, release, &[executable, inventory])
            .await?;
        self.stage_release_layer(layer, executable, inventory, dir)
            .await
    }

    pub(crate) async fn installed_layer(
        &self,
        control: &jmodguard::db::UpdateControl,
        release: &ComponentRelease,
        requested: &[&str],
    ) -> Result<VerifiedReleaseLayer> {
        let state = control.component(&release.component).await?;
        ensure!(
            state.installed.as_ref() == Some(release)
                || state.installed_launcher.as_ref() == Some(release),
            "release is not installed"
        );
        self.verified_release_layer(release, requested, signature::SignatureExpiry::AllowExpired)
            .await
    }

    async fn files_with(
        &self,
        release: &ComponentRelease,
        requested: &[&str],
        expiry: signature::SignatureExpiry,
    ) -> Result<HashMap<String, Vec<u8>>> {
        let layer = self
            .verified_release_layer(release, requested, expiry)
            .await?;
        self.read_release_layer(layer, requested).await
    }

    async fn verified_release_layer(
        &self,
        release: &ComponentRelease,
        requested: &[&str],
        expiry: signature::SignatureExpiry,
    ) -> Result<VerifiedReleaseLayer> {
        release.validate()?;
        let repo = repository(&release.component)?;
        ensure!(
            release.image == format!("ghcr.io/{repo}"),
            "image repository mismatch"
        );
        ensure!(
            !requested.is_empty() && requested.len() <= 16,
            "invalid file request"
        );
        let (manifest, digest, manifest_bytes) = self.manifest(repo, &release.digest).await?;
        ensure!(digest == release.digest, "manifest digest mismatch");
        self.verify_manifest(&release.component, repo, &digest, &manifest_bytes, expiry)
            .await?;
        let config = self.config(repo, &manifest).await?;
        ensure!(
            config
                .config
                .labels
                .get("org.opencontainers.image.revision")
                == Some(&release.source_sha),
            "release revision changed"
        );
        ensure!(
            config.config.labels.get("io.jmodguard.release-id") == Some(&release.release_id),
            "release ID changed"
        );
        let descriptor = manifest
            .layers
            .into_iter()
            .last()
            .context("image has no final layer")?;
        Ok(VerifiedReleaseLayer { repo, descriptor })
    }

    pub(crate) async fn read_release_layer(
        &self,
        layer: VerifiedReleaseLayer,
        requested: &[&str],
    ) -> Result<HashMap<String, Vec<u8>>> {
        let bytes = self.blob(layer.repo, &layer.descriptor, MAX_LAYER).await?;
        extract_layer(&bytes, &layer.descriptor.media_type, requested)
    }

    /// Download the layer to a temporary file, verify its size and digest,
    /// then extract the executable to another. Both are removed on failure or
    /// cancellation; the layer file is removed once extraction finishes.
    async fn stage_release_layer(
        &self,
        layer: VerifiedReleaseLayer,
        executable: &str,
        inventory: &str,
        dir: &Path,
    ) -> Result<StagedExecutable> {
        tokio::fs::create_dir_all(dir).await?;
        let blob = self
            .blob_to_file(layer.repo, &layer.descriptor, MAX_LAYER, dir)
            .await?;
        let staged = TempFile::new(dir, "stage");
        let media = layer.descriptor.media_type;
        let (executable, inventory) = (executable.to_owned(), inventory.to_owned());
        tokio::task::spawn_blocking(move || {
            extract_executable(&blob, &media, &executable, &inventory, staged)
        })
        .await?
    }
}

fn extract_executable(
    layer: &TempFile,
    media: &str,
    executable: &str,
    inventory: &str,
    staged: TempFile,
) -> Result<StagedExecutable> {
    let reader =
        std::io::BufReader::with_capacity(STREAM_BUFFER, std::fs::File::open(layer.path())?);
    let mut written = None;
    let mut listed = None;
    walk_layer(
        Box::new(reader),
        media,
        &[executable, inventory],
        |name, entry, size| {
            if name == inventory {
                let mut data = Vec::with_capacity(size as usize);
                entry.read_to_end(&mut data)?;
                ensure!(data.len() as u64 == size, "truncated release file");
                listed = Some(data);
                return Ok(());
            }
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(staged.path())?;
            let mut writer = HashingWriter {
                inner: std::io::BufWriter::with_capacity(STREAM_BUFFER, file),
                context: DigestContext::new(&SHA256),
                size: 0,
                header: Vec::with_capacity(EXECUTABLE_HEADER),
            };
            std::io::copy(entry, &mut writer)?;
            writer.flush()?;
            ensure!(writer.size == size, "truncated release file");
            written = Some((
                hex(writer.context.finish().as_ref()),
                writer.size,
                writer.header,
            ));
            Ok(())
        },
    )?;
    let (sha256, size, header) = written.context("release layer is missing a required file")?;
    Ok(StagedExecutable {
        file: staged,
        sha256,
        size,
        header,
        inventory: listed.context("release layer is missing a required file")?,
    })
}

fn extract_layer(
    bytes: &[u8],
    media: &str,
    requested: &[&str],
) -> Result<HashMap<String, Vec<u8>>> {
    let mut found = HashMap::new();
    walk_layer(Box::new(bytes), media, requested, |name, entry, size| {
        let mut data = Vec::with_capacity(size as usize);
        entry.read_to_end(&mut data)?;
        ensure!(data.len() as u64 == size, "truncated release file");
        found.insert(name.to_owned(), data);
        Ok(())
    })?;
    Ok(found)
}

/// Apply every archive limit and path check, handing each requested regular
/// file to `take` exactly once.
fn walk_layer<'a>(
    layer: Box<dyn Read + 'a>,
    media: &str,
    requested: &[&str],
    mut take: impl FnMut(&str, &mut dyn Read, u64) -> Result<()>,
) -> Result<()> {
    let reader: Box<dyn Read + 'a> = match media {
        "application/vnd.oci.image.layer.v1.tar+gzip"
        | "application/vnd.docker.image.rootfs.diff.tar.gzip" => {
            Box::new(flate2::read::GzDecoder::new(layer))
        }
        "application/vnd.oci.image.layer.v1.tar" => layer,
        _ => bail!("unsupported image layer compression"),
    };
    let mut archive = tar::Archive::new(reader);
    let wanted: HashSet<&str> = requested.iter().copied().collect();
    ensure!(wanted.len() == requested.len(), "duplicate file request");
    let mut found = HashSet::new();
    let mut total = 0u64;
    let mut count = 0usize;
    for entry in archive.entries()? {
        let mut entry = entry?;
        count += 1;
        ensure!(count <= 256, "too many layer entries");
        let path = entry.path()?;
        let raw = path.to_str().context("non-UTF8 layer path")?;
        let normalized = raw.trim_start_matches("./");
        ensure!(
            !normalized.is_empty()
                && std::path::Path::new(normalized)
                    .components()
                    .all(|part| matches!(part, Component::Normal(_))),
            "unsafe layer path"
        );
        let name = normalized.to_owned();
        let kind = entry.header().entry_type();
        ensure!(
            kind.is_file() || kind.is_dir(),
            "link or special file in release layer"
        );
        let size = entry.size();
        total = total.checked_add(size).context("layer size overflow")?;
        ensure!(total <= MAX_UNPACKED, "unpacked layer exceeds limit");
        if wanted.contains(name.as_str()) {
            ensure!(
                kind.is_file() && size <= MAX_FILE && !found.contains(&name),
                "invalid or duplicate release file"
            );
            take(&name, &mut entry, size)?;
            found.insert(name);
        }
    }
    ensure!(
        found.len() == wanted.len(),
        "release layer is missing a required file"
    );
    Ok(())
}

#[cfg(test)]
library_unit_tests!(
    mod tests {
        #[tokio::test]
        async fn registry_transport_errors_omit_urls() {
            // Exercise the same request owner used by API discovery and launcher downloads.
            // Reserve the port without listening so another test cannot claim it
            // and turn the expected connection refusal into an HTTP response.
            let socket = tokio::net::TcpSocket::new_v4().unwrap();
            socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = socket.local_addr().unwrap();
            assert!(
                tokio::net::TcpListener::bind(addr).await.is_err(),
                "the refused endpoint must stay reserved until the request completes"
            );
            let registry = super::Registry::new(&format!("http://{addr}")).unwrap();
            let error = registry
                .get_limited(
                    format!("http://{addr}/PRIVATE_PATH?token=PRIVATE_TOKEN"),
                    None,
                    None,
                    1024,
                )
                .await
                .unwrap_err();
            let request = error.downcast_ref::<reqwest::Error>().unwrap();
            assert!(request.url().is_none());
            let diagnostic = format!("{error:#} {error:?}");
            assert!(!diagnostic.contains("PRIVATE_"));
            assert!(!diagnostic.contains(&format!("http://{addr}")));
            assert!(
                super::registry_unreachable(&error),
                "connection failure must be unreachable: {error:#} {error:?}"
            );
            let unavailable = anyhow::Error::msg("registry returned HTTP 503 Service Unavailable");
            let limited = anyhow::Error::msg("registry returned HTTP 429 Too Many Requests");
            let server = anyhow::Error::msg("registry returned HTTP 500 Internal Server Error");
            let unsigned = anyhow::Error::new(signature::ImageSignatureRejection {
                phrase: signature::UNSIGNED_IMAGE,
            });
            assert!(super::registry_unreachable(&unavailable));
            assert!(super::registry_unreachable(&limited));
            assert!(super::registry_unreachable(&server));
            assert!(!super::registry_unreachable(&unsigned));
        }

        #[test]
        fn registry_status_classification() {
            for status in 100..=599 {
                let expected = matches!(status, 408 | 429 | 500..=599);
                let error = anyhow::anyhow!("registry returned HTTP {status} status");
                assert_eq!(registry_unreachable(&error), expected, "HTTP {status}");
                let response = axum::http::Response::builder()
                    .status(status)
                    .body("")
                    .unwrap();
                if let Err(error) = reqwest::Response::from(response).error_for_status() {
                    let error = anyhow::Error::new(error.without_url());
                    assert_eq!(
                        registry_unreachable(&error),
                        expected,
                        "typed HTTP {status}"
                    );
                    let rejected = anyhow::Error::new(signature::ImageSignatureRejection {
                        phrase: signature::UNSIGNED_IMAGE,
                    })
                    .context(format!("registry returned HTTP {status}"));
                    assert!(!registry_unreachable(&rejected));
                }
            }
        }

        #[tokio::test]
        async fn registry_request_timeout_is_unreachable_but_body_timeout_is_not() {
            for partial_body in [false, true] {
                let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                    .await
                    .unwrap();
                let base = format!("http://{}", listener.local_addr().unwrap());
                let server = tokio::spawn(async move {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = [0; 1024];
                    tokio::io::AsyncReadExt::read(&mut stream, &mut request)
                        .await
                        .unwrap();
                    if partial_body {
                        tokio::io::AsyncWriteExt::write_all(
                            &mut stream,
                            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{",
                        )
                        .await
                        .unwrap();
                    }
                    std::future::pending::<()>().await;
                });
                let mut registry = Registry::new(&base).unwrap();
                registry.http = http_client_builder()
                    .timeout(Duration::from_millis(100))
                    .build()
                    .unwrap();
                let error = registry
                    .get_limited(base, None, None, 1024)
                    .await
                    .unwrap_err();
                server.abort();
                let _ = server.await;
                assert!(error.downcast_ref::<reqwest::Error>().unwrap().is_timeout());
                assert_eq!(registry_unreachable(&error), !partial_body, "{error:#}");
            }
        }

        // Paused time ends the server's sleep only once the client has read
        // everything sent and is waiting for the rest of the body.
        #[tokio::test(start_paused = true)]
        async fn registry_truncated_body_is_not_unreachable() {
            for reset in [false, true] {
                let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                    .await
                    .unwrap();
                let base = format!("http://{}", listener.local_addr().unwrap());
                let server = tokio::spawn(async move {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = [0; 1024];
                    tokio::io::AsyncReadExt::read(&mut stream, &mut request)
                        .await
                        .unwrap();
                    tokio::io::AsyncWriteExt::write_all(
                        &mut stream,
                        b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{",
                    )
                    .await
                    .unwrap();
                    // Let the reader receive the successful headers and partial body.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if reset {
                        stream.set_zero_linger().unwrap();
                    }
                });
                let error = Registry::new(&base)
                    .unwrap()
                    .get_limited(base, None, None, 1024)
                    .await
                    .unwrap_err();
                server.await.unwrap();
                assert!(error.downcast_ref::<reqwest::Error>().unwrap().is_decode());
                if reset {
                    assert!(error.chain().any(|cause| {
                        cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
                            error.kind() == std::io::ErrorKind::ConnectionReset
                        })
                    }));
                }
                assert!(!registry_unreachable(&error), "reset={reset}: {error:#}");
            }
        }

        use super::*;
        use std::io::Write;

        fn layer(entries: &[(&str, &[u8], tar::EntryType)]) -> Vec<u8> {
            let mut out = Vec::new();
            {
                let mut archive = tar::Builder::new(&mut out);
                for (path, bytes, kind) in entries {
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(*kind);
                    header.set_size(bytes.len() as u64);
                    header.set_mode(0o755);
                    header.set_cksum();
                    archive.append_data(&mut header, path, *bytes).unwrap();
                }
                archive.finish().unwrap();
            }
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(&out).unwrap();
            encoder.finish().unwrap()
        }

        #[test]
        fn extracts_exact_regular_file_and_refuses_duplicates_or_links() {
            let good = layer(&[(
                "usr/local/bin/jmodguard-api",
                b"\x7fELFbody",
                tar::EntryType::Regular,
            )]);
            let found = extract_layer(
                &good,
                "application/vnd.oci.image.layer.v1.tar+gzip",
                &["usr/local/bin/jmodguard-api"],
            )
            .unwrap();
            assert_eq!(found["usr/local/bin/jmodguard-api"], b"\x7fELFbody");
            let duplicate = layer(&[
                (
                    "usr/local/bin/jmodguard-api",
                    b"one",
                    tar::EntryType::Regular,
                ),
                (
                    "usr/local/bin/jmodguard-api",
                    b"two",
                    tar::EntryType::Regular,
                ),
            ]);
            assert!(
                extract_layer(
                    &duplicate,
                    "application/vnd.oci.image.layer.v1.tar+gzip",
                    &["usr/local/bin/jmodguard-api"]
                )
                .is_err()
            );
            let link = layer(&[("usr/local/bin/jmodguard-api", b"", tar::EntryType::Symlink)]);
            assert!(
                extract_layer(
                    &link,
                    "application/vnd.oci.image.layer.v1.tar+gzip",
                    &["usr/local/bin/jmodguard-api"]
                )
                .is_err()
            );
        }

        const BOT: &str = "usr/local/bin/jmodguard-bot";
        const INVENTORY: &str = "usr/local/share/jmodguard/files.sha256";
        const GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

        fn verified(bytes: &[u8], media: &str) -> VerifiedReleaseLayer {
            VerifiedReleaseLayer {
                repo: "jatmn/jmodguard-bot",
                descriptor: Descriptor {
                    digest: format!("sha256:{}", sha256_hex(bytes)),
                    size: bytes.len() as u64,
                    media_type: media.into(),
                    annotations: BTreeMap::new(),
                },
            }
        }

        async fn serve_blob(body: Vec<u8>) -> (Registry, tokio::task::JoinHandle<()>) {
            let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .await
                .unwrap();
            let registry =
                Registry::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
            let body = std::sync::Arc::new(body);
            let app = axum::Router::new().fallback(move || {
                let body = body.clone();
                async move { body.as_ref().clone() }
            });
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            (registry, server)
        }

        fn staging_dir() -> PathBuf {
            std::env::temp_dir().join(format!("oci-stage-{:032x}", rand::random::<u128>()))
        }

        fn names(dir: &Path) -> Vec<String> {
            let mut names = std::fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect::<Vec<_>>();
            names.sort();
            names
        }

        fn executable_body() -> Vec<u8> {
            let mut body = b"\x7fELF".to_vec();
            body.extend((0..300_000u32).map(|index| (index % 251) as u8));
            body
        }

        #[tokio::test]
        async fn streamed_staging_matches_in_memory_extraction() {
            let executable = executable_body();
            let inventory = format!("{}  bin/jmodguard-bot\n", sha256_hex(&executable));
            let bytes = layer(&[
                ("usr/local/bin/other", b"ignored", tar::EntryType::Regular),
                (BOT, &executable, tar::EntryType::Regular),
                (INVENTORY, inventory.as_bytes(), tar::EntryType::Regular),
            ]);
            let expected = extract_layer(&bytes, GZIP, &[BOT, INVENTORY]).unwrap();
            let descriptor = verified(&bytes, GZIP);
            let (registry, server) = serve_blob(bytes).await;
            let dir = staging_dir();
            let staged = registry
                .stage_release_layer(descriptor, BOT, INVENTORY, &dir)
                .await
                .unwrap();
            assert_eq!(staged.sha256(), sha256_hex(&expected[BOT]));
            assert_eq!(staged.size(), executable.len() as u64);
            assert_eq!(staged.header(), b"\x7fELF");
            assert_eq!(staged.inventory(), expected[INVENTORY].as_slice());
            assert_eq!(std::fs::read(staged.path()).unwrap(), expected[BOT]);
            let temporary = names(&dir);
            assert_eq!(temporary.len(), 1, "{temporary:?}");
            assert!(
                temporary[0].starts_with(".stage-"),
                "the layer file must be removed after extraction: {temporary:?}"
            );
            let target = dir.join("bot-target");
            staged.persist(&target).await.unwrap();
            assert_eq!(names(&dir), ["bot-target"]);
            assert_eq!(file_sha256(&target).await.unwrap(), sha256_hex(&executable));
            server.abort();
            std::fs::remove_dir_all(dir).unwrap();
        }

        #[tokio::test]
        async fn streamed_staging_failures_remove_temporary_files() {
            let executable = executable_body();
            let inventory = format!("{}  bin/jmodguard-bot\n", sha256_hex(&executable));
            let good = layer(&[
                (BOT, &executable, tar::EntryType::Regular),
                (INVENTORY, inventory.as_bytes(), tar::EntryType::Regular),
            ]);
            let mut corrupt = good.clone();
            *corrupt.last_mut().unwrap() ^= 1;
            // The executable is fully written before the archive is refused.
            let special = layer(&[
                (BOT, &executable, tar::EntryType::Regular),
                ("usr/local/bin/link", b"", tar::EntryType::Symlink),
                (INVENTORY, inventory.as_bytes(), tar::EntryType::Regular),
            ]);
            let missing = layer(&[(BOT, &executable, tar::EntryType::Regular)]);
            let oversized = VerifiedReleaseLayer {
                repo: "jatmn/jmodguard-bot",
                descriptor: Descriptor {
                    size: MAX_LAYER as u64 + 1,
                    ..verified(&good, GZIP).descriptor
                },
            };
            let cases = [
                (verified(&good, GZIP), corrupt, "blob digest mismatch"),
                (
                    verified(&good, GZIP),
                    good[..good.len() - 10].to_vec(),
                    "blob size mismatch",
                ),
                (
                    verified(&good, "application/vnd.oci.image.layer.v1.tar+zstd"),
                    good.clone(),
                    "unsupported image layer compression",
                ),
                (
                    verified(&special, GZIP),
                    special.clone(),
                    "link or special file in release layer",
                ),
                (
                    verified(&missing, GZIP),
                    missing.clone(),
                    "release layer is missing a required file",
                ),
                (oversized, good.clone(), "invalid blob descriptor"),
            ];
            for (descriptor, served, expected) in cases {
                let (registry, server) = serve_blob(served).await;
                let dir = staging_dir();
                let error = match registry
                    .stage_release_layer(descriptor, BOT, INVENTORY, &dir)
                    .await
                {
                    Ok(_) => panic!("{expected}: staging must fail"),
                    Err(error) => error,
                };
                assert_eq!(error.to_string(), expected);
                assert!(!registry_unreachable(&error), "{expected}");
                assert_eq!(names(&dir), Vec::<String>::new(), "{expected}");
                server.abort();
                std::fs::remove_dir_all(dir).unwrap();
            }
        }

        #[tokio::test]
        async fn cancelled_streamed_staging_removes_the_partial_layer() {
            let executable = executable_body();
            let bytes = layer(&[(BOT, &executable, tar::EntryType::Regular)]);
            let descriptor = verified(&bytes, GZIP);
            let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .await
                .unwrap();
            let registry =
                Registry::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
            let (sent, mut partial) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 1024];
                tokio::io::AsyncReadExt::read(&mut stream, &mut request)
                    .await
                    .unwrap();
                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", bytes.len());
                stream.write_all(head.as_bytes()).await.unwrap();
                stream.write_all(&bytes[..bytes.len() / 2]).await.unwrap();
                sent.send(()).unwrap();
                std::future::pending::<()>().await;
            });
            let dir = staging_dir();
            let mut staging =
                Box::pin(registry.stage_release_layer(descriptor, BOT, INVENTORY, &dir));
            tokio::select! {
                biased;
                result = &mut staging => panic!("staging finished early: {:?}", result.err()),
                _ = &mut partial => {}
            }
            let partial = names(&dir);
            assert_eq!(partial.len(), 1, "{partial:?}");
            assert!(partial[0].starts_with(".layer-"), "{partial:?}");
            drop(staging);
            assert_eq!(names(&dir), Vec::<String>::new());
            server.abort();
            let _ = server.await;
            std::fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn staging_cancelled_while_the_layer_file_is_created_leaves_no_file() {
            // One blocking worker lets the test queue the layer file creation behind a
            // held task, cancel staging, and only then let the creation run.
            let runtime = tokio::runtime::Builder::new_current_thread()
                .max_blocking_threads(1)
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let bytes = layer(&[(BOT, &executable_body(), tar::EntryType::Regular)]);
                let descriptor = verified(&bytes, GZIP);
                let registry = Registry::new("http://127.0.0.1:1").unwrap();
                let dir = staging_dir();
                let mut staging =
                    Box::pin(registry.stage_release_layer(descriptor, BOT, INVENTORY, &dir));
                tokio::select! {
                    biased;
                    result = &mut staging => panic!("staging finished early: {:?}", result.err()),
                    _ = std::future::ready(()) => {}
                }
                let (started, running) = std::sync::mpsc::channel();
                let (release, held) = std::sync::mpsc::channel::<()>();
                let blocker = tokio::task::spawn_blocking(move || {
                    started.send(()).unwrap();
                    let _ = held.recv();
                });
                // The worker reaches this task only after it finished the directory creation.
                running.recv().unwrap();
                tokio::select! {
                    biased;
                    result = &mut staging => panic!("staging finished early: {:?}", result.err()),
                    _ = std::future::ready(()) => {}
                }
                drop(staging);
                release.send(()).unwrap();
                blocker.await.unwrap();
                tokio::task::spawn_blocking(|| ()).await.unwrap();
                assert_eq!(names(&dir), Vec::<String>::new());
                std::fs::remove_dir_all(dir).unwrap();
            });
        }

        #[test]
        fn file_digest_is_stable() {
            assert_eq!(
                sha256_hex(b"abc"),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
            );
        }
    }
);
