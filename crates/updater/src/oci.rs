//! Anonymous, digest-pinned reads of the project's public GHCR OCI images.
//! Registry tags are discovery pointers; a queued install keeps its digest.
use crate::signature;
use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use jmodguard::db::ComponentRelease;
use reqwest::StatusCode;
use reqwest::header::{ACCEPT, CONTENT_LENGTH};
use ring::digest::{SHA256, digest};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::Component;
use std::time::Duration;

const MAX_MANIFEST: usize = 2 * 1024 * 1024;
const MAX_CONFIG: usize = 256 * 1024;
const MAX_LAYER: usize = 128 * 1024 * 1024;
const MAX_UNPACKED: u64 = 256 * 1024 * 1024;
const MAX_FILE: u64 = 64 * 1024 * 1024;
const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";

pub fn sha256_hex(bytes: &[u8]) -> String {
    digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn valid_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|b| b.is_ascii_hexdigit())
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
            http: reqwest::Client::builder()
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
        let (status, body, headers) = self.get_limited_status(url, token, accept, limit).await?;
        ensure!(status.is_success(), "registry returned HTTP {}", status);
        Ok((body, headers))
    }

    async fn get_limited_status(
        &self,
        url: String,
        token: Option<&str>,
        accept: Option<&str>,
        limit: usize,
    ) -> Result<(StatusCode, Vec<u8>, reqwest::header::HeaderMap)> {
        let mut request = self.http.get(url);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(accept) = accept {
            request = request.header(ACCEPT, accept);
        }
        let mut response = request.send().await.map_err(reqwest::Error::without_url)?;
        let status = response.status();
        if let Some(raw) = response.headers().get(CONTENT_LENGTH)
            && let Ok(length) = raw.to_str()?.parse::<u64>()
            && status.is_success()
        {
            ensure!(length <= limit as u64, "registry object exceeds size limit");
        }
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
        if status.is_success()
            && let Some(raw) = headers.get(CONTENT_LENGTH)
            && let Ok(length) = raw.to_str()?.parse::<usize>()
            && body.len() != length
        {
            bail!("incomplete registry body");
        }
        Ok((status, body, headers))
    }

    async fn signature_body(&self, repo: &str, manifest_digest: &str) -> Result<Vec<u8>> {
        let tag = signature::signature_tag_for_digest(manifest_digest)?;
        let token = self.token(repo).await?;
        let url = format!("{}/v2/{repo}/manifests/{tag}", self.base);
        let (status, body, _) = self
            .get_limited_status(url, token.as_deref(), None, MAX_MANIFEST)
            .await?;
        if status == StatusCode::NOT_FOUND {
            bail!(signature::ImageSignatureRejection {
                phrase: signature::UNSIGNED_IMAGE,
            });
        }
        ensure!(status.is_success(), "registry returned HTTP {}", status);
        Ok(body)
    }

    async fn signature_envelope_bytes(&self, repo: &str, body: &[u8]) -> Result<Vec<u8>> {
        if serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .is_some_and(|value| {
                value.get("payloadType").is_some() && value.get("payload").is_some()
            })
        {
            return Ok(body.to_vec());
        }
        let manifest: Manifest =
            serde_json::from_slice(body).map_err(|_| signature::ImageSignatureRejection {
                phrase: signature::BAD_IMAGE_SIGNATURE,
            })?;
        ensure!(
            manifest.schema_version == 2 && !manifest.layers.is_empty(),
            signature::ImageSignatureRejection {
                phrase: signature::BAD_IMAGE_SIGNATURE,
            }
        );
        for layer in manifest.layers.iter().rev() {
            let signature_b64 = layer
                .annotations
                .get("dev.cosignproject.cosign/signature")
                .map(String::as_str);
            if let Some(signature_b64) = signature_b64 {
                let payload = self.blob(repo, layer, MAX_MANIFEST).await?;
                if let Ok(envelope) =
                    signature::envelope_from_cosign_payload(&payload, signature_b64)
                {
                    return Ok(envelope);
                }
            }
        }
        let layer = &manifest.layers[0];
        let payload = self.blob(repo, layer, MAX_MANIFEST).await?;
        let signature_b64 = layer
            .annotations
            .get("dev.cosignproject.cosign/signature")
            .map(String::as_str)
            .ok_or(signature::ImageSignatureRejection {
                phrase: signature::BAD_IMAGE_SIGNATURE,
            })?;
        signature::envelope_from_cosign_payload(&payload, signature_b64)
    }

    async fn verify_manifest(
        &self,
        component: &str,
        repo: &str,
        manifest_digest: &str,
        manifest_bytes: &[u8],
    ) -> Result<()> {
        let signature = self.signature_body(repo, manifest_digest).await?;
        let envelope = self.signature_envelope_bytes(repo, &signature).await?;
        signature::verify_manifest_signature(
            component,
            manifest_digest,
            manifest_bytes,
            &envelope,
            Utc::now(),
        )
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
        self.verify_manifest(component, repo, &image_digest, &manifest_bytes)
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
        self.verify_manifest(&release.component, repo, &digest, &manifest_bytes)
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
        let mut found = HashMap::new();
        let mut remaining: Vec<&str> = requested.to_vec();
        for layer in manifest.layers.iter().rev() {
            if remaining.is_empty() {
                break;
            }
            let bytes = self.blob(repo, layer, MAX_LAYER).await?;
            let partial = extract_layer_files(&bytes, &layer.media_type, &remaining)?;
            for path in partial.keys() {
                remaining.retain(|wanted| wanted != path);
            }
            found.extend(partial);
        }
        ensure!(
            found.len() == requested.len(),
            "release image is missing a required file"
        );
        Ok(found)
    }
}

fn extract_layer_files(
    bytes: &[u8],
    media: &str,
    requested: &[&str],
) -> Result<HashMap<String, Vec<u8>>> {
    let wanted: HashSet<&str> = requested.iter().copied().collect();
    ensure!(wanted.len() == requested.len(), "duplicate file request");
    let reader: Box<dyn Read + '_> = match media {
        "application/vnd.oci.image.layer.v1.tar+gzip"
        | "application/vnd.docker.image.rootfs.diff.tar.gzip" => {
            Box::new(flate2::read::GzDecoder::new(bytes))
        }
        "application/vnd.oci.image.layer.v1.tar" => Box::new(bytes),
        _ => bail!("unsupported image layer compression"),
    };
    let mut archive = tar::Archive::new(reader);
    let mut found = HashMap::new();
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
        if !wanted.contains(name.as_str()) {
            continue;
        }
        let kind = entry.header().entry_type();
        ensure!(
            kind.is_file() || kind.is_dir(),
            "link or special file in release layer"
        );
        let size = entry.size();
        total = total.checked_add(size).context("layer size overflow")?;
        ensure!(total <= MAX_UNPACKED, "unpacked layer exceeds limit");
        ensure!(
            kind.is_file() && size <= MAX_FILE && !found.contains_key(&name),
            "invalid or duplicate release file"
        );
        let mut data = Vec::with_capacity(size as usize);
        entry.read_to_end(&mut data)?;
        ensure!(data.len() as u64 == size, "truncated release file");
        found.insert(name, data);
    }
    Ok(found)
}

fn extract_layer(
    bytes: &[u8],
    media: &str,
    requested: &[&str],
) -> Result<HashMap<String, Vec<u8>>> {
    let found = extract_layer_files(bytes, media, requested)?;
    ensure!(
        found.len() == requested.len(),
        "release layer is missing a required file"
    );
    Ok(found)
}

#[cfg(test)]
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
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
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

    #[test]
    fn collects_requested_files_across_multiple_layers() {
        let bin_layer = layer(&[(
            "usr/local/bin/jmodguard-api",
            b"\x7fELFbody",
            tar::EntryType::Regular,
        )]);
        let manifest_layer = layer(&[(
            "usr/local/share/jmodguard/files.sha256",
            b"deadbeef  usr/local/bin/jmodguard-api\n",
            tar::EntryType::Regular,
        )]);
        let bin = extract_layer_files(
            &bin_layer,
            "application/vnd.oci.image.layer.v1.tar+gzip",
            &["usr/local/bin/jmodguard-api"],
        )
        .unwrap();
        let manifest = extract_layer_files(
            &manifest_layer,
            "application/vnd.oci.image.layer.v1.tar+gzip",
            &["usr/local/share/jmodguard/files.sha256"],
        )
        .unwrap();
        let mut merged = bin;
        merged.extend(manifest);
        assert_eq!(merged.len(), 2);
        assert!(merged.contains_key("usr/local/bin/jmodguard-api"));
        assert!(merged.contains_key("usr/local/share/jmodguard/files.sha256"));
    }

    #[test]
    fn file_digest_is_stable() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
