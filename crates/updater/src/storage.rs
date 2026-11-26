//! Scoped WebUI publication using a Bunny Storage Zone password. This never
//! calls the account API or recursively deletes a Storage directory.
use crate::http_client_builder;
use crate::oci::{Registry, VerifiedReleaseLayer, sha256_hex};
use anyhow::{Context, Result, ensure};
use jmodguard::db::{ComponentRelease, UpdateControl};
use reqwest::StatusCode;
use std::time::Duration;

const ROOT_HTML: &str = "index.html";
const ADMIN_HTML: &str = "administration/index.html";
const SHARED_ASSETS: [&str; 2] = ["style.css", "administration.js"];
const LEGACY_DASHBOARD: &str = "app.js";
const SPLIT_DASHBOARD: &str = "workspace.js";
const RUNTIME: &str = "webui-runtime.js";
const SELECTED_ASSETS: [&str; 3] = [LEGACY_DASHBOARD, SPLIT_DASHBOARD, RUNTIME];

pub enum ReleaseVerification {
    Mismatch,
    Verified {
        root_hash: String,
        admin_hash: String,
    },
}

/// Expected package metadata for one installed WebUI release.
#[derive(Default)]
pub struct InstalledWebuiInventory(Option<(ComponentRelease, Vec<ExpectedFile>)>);

struct ExpectedFile {
    path: String,
    len: usize,
    hash: String,
}

pub struct Storage {
    http: reqwest::Client,
    root: String,
    password: String,
}

impl Storage {
    pub fn new(hostname: &str, zone: &str, password: &str) -> Result<Self> {
        ensure!(
            hostname == "storage.bunnycdn.com" || hostname.ends_with(".storage.bunnycdn.com"),
            "invalid Bunny Storage hostname"
        );
        ensure!(
            !zone.is_empty() && zone.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'),
            "invalid Bunny Storage zone"
        );
        ensure!(!password.is_empty(), "Bunny Storage password is missing");
        Ok(Self {
            http: http_client_builder()
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            root: format!("https://{hostname}/{zone}"),
            password: password.to_owned(),
        })
    }

    fn url(&self, path: &str) -> Result<String> {
        ensure!(
            is_managed(path),
            "Storage path is outside managed WebUI files"
        );
        Ok(format!("{}/{}", self.root, path))
    }

    pub async fn get(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let response = self
            .http
            .get(self.url(path)?)
            .header("AccessKey", &self.password)
            .send()
            .await
            .map_err(reqwest::Error::without_url)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let mut response = response
            .error_for_status()
            .map_err(reqwest::Error::without_url)?;
        let mut out = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(reqwest::Error::without_url)?
        {
            ensure!(
                out.len().saturating_add(chunk.len()) <= 32 * 1024 * 1024,
                "Storage object too large"
            );
            out.extend_from_slice(&chunk);
        }
        Ok(Some(out))
    }

    pub async fn put_verified(&self, path: &str, data: &[u8], content_type: &str) -> Result<()> {
        ensure!(data.len() <= 32 * 1024 * 1024, "WebUI object too large");
        let hash = sha256_hex(data);
        self.http
            .put(self.url(path)?)
            .header("AccessKey", &self.password)
            .header("Checksum", hash.to_ascii_uppercase())
            .header("Content-Type", content_type)
            .body(data.to_vec())
            .send()
            .await
            .map_err(reqwest::Error::without_url)?
            .error_for_status()
            .map_err(reqwest::Error::without_url)?;
        let stored = self
            .get(path)
            .await?
            .context("uploaded WebUI object is missing")?;
        ensure!(
            sha256_hex(&stored) == hash,
            "uploaded WebUI object did not verify"
        );
        Ok(())
    }

    pub async fn delete(&self, path: &str) -> Result<()> {
        ensure!(
            path.starts_with("assets/releases/"),
            "only immutable managed assets may be pruned"
        );
        let response = self
            .http
            .delete(self.url(path)?)
            .header("AccessKey", &self.password)
            .send()
            .await
            .map_err(reqwest::Error::without_url)?;
        ensure!(
            response.status().is_success() || response.status() == StatusCode::NOT_FOUND,
            "Storage DELETE failed: {}",
            response.status()
        );
        Ok(())
    }

    /// Upload immutable files first, then administration HTML, then root HTML
    /// last. On a switching error, restore only the exact HTML files that we
    /// read before publication and leave old managed assets intact.
    pub async fn publish(
        &self,
        registry: &Registry,
        release: &ComponentRelease,
    ) -> Result<Vec<String>> {
        let paths = ReleasePaths::new(release)?;
        let layer = registry.release_layer(release, &paths.requested()).await?;
        let files = paths.read(layer).await?;
        let old_root = self.get(ROOT_HTML).await?;
        let old_admin = self.get(ADMIN_HTML).await?;
        let assets = &files[2..];
        for (asset, body) in assets {
            let mime = if asset.ends_with(".css") {
                "text/css; charset=utf-8"
            } else {
                "text/javascript; charset=utf-8"
            };
            self.put_verified(asset, body, mime).await?;
        }
        let switched = async {
            self.put_verified(ADMIN_HTML, &files[1].1, "text/html; charset=utf-8")
                .await?;
            self.put_verified(ROOT_HTML, &files[0].1, "text/html; charset=utf-8")
                .await?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(error) = switched {
            let recovery = self.restore_html(ROOT_HTML, old_root).await;
            let admin_recovery = self.restore_html(ADMIN_HTML, old_admin).await;
            if let Err(recovery_error) = recovery.and(admin_recovery) {
                return Err(
                    error.context(format!("HTML restoration also failed: {recovery_error:#}"))
                );
            }
            return Err(error);
        }
        Ok(assets.iter().map(|(path, _)| path.clone()).collect())
    }

    /// Reconcile a response lost after the HTML switch. Adoption requires all
    /// stored files to match the digest-pinned public release package.
    pub async fn verify_release(
        &self,
        registry: &Registry,
        release: &ComponentRelease,
    ) -> Result<ReleaseVerification> {
        let paths = ReleasePaths::new(release)?;
        let layer = registry.release_layer(release, &paths.requested()).await?;
        self.compare_release(&paths.read(layer).await?).await
    }

    /// Re-check the Storage files of the durably installed release. As with
    /// launcher boot repair, the signature must still verify, but
    /// `io.jmodguard.not-after` does not block this re-read.
    pub async fn verify_installed_release(
        &self,
        registry: &Registry,
        control: &UpdateControl,
        release: &ComponentRelease,
    ) -> Result<ReleaseVerification> {
        let paths = ReleasePaths::new(release)?;
        let layer = registry
            .installed_layer(control, release, &paths.requested())
            .await?;
        self.compare_release(&paths.read(layer).await?).await
    }

    /// Reconfirm installed identity and current registry trust, then compare
    /// freshly read Storage bytes with the expected package inventory.
    pub async fn verify_installed_release_cached(
        &self,
        registry: &Registry,
        control: &UpdateControl,
        release: &ComponentRelease,
        inventory: &mut InstalledWebuiInventory,
    ) -> Result<ReleaseVerification> {
        // Leave no warm entry if trust confirmation fails or is cancelled.
        let cached = inventory.0.take().filter(|(cached, _)| cached == release);
        let paths = ReleasePaths::new(release)?;
        let layer = registry
            .installed_layer(control, release, &paths.requested())
            .await?;
        let expected = match cached {
            Some(entry) => entry,
            None => {
                let files = paths
                    .read(layer)
                    .await?
                    .into_iter()
                    .map(|(path, body)| ExpectedFile {
                        path,
                        len: body.len(),
                        hash: sha256_hex(&body),
                    })
                    .collect();
                (release.clone(), files)
            }
        };
        let (_, files) = inventory.0.insert(expected);
        for file in files.iter() {
            let Some(actual) = self.get(&file.path).await? else {
                return Ok(ReleaseVerification::Mismatch);
            };
            if actual.len() != file.len || sha256_hex(&actual) != file.hash {
                return Ok(ReleaseVerification::Mismatch);
            }
        }
        Ok(ReleaseVerification::Verified {
            root_hash: files[0].hash.clone(),
            admin_hash: files[1].hash.clone(),
        })
    }

    async fn compare_release(&self, expected: &[(String, Vec<u8>)]) -> Result<ReleaseVerification> {
        for (path, expected_bytes) in expected {
            if self.get(path).await?.as_ref() != Some(expected_bytes) {
                return Ok(ReleaseVerification::Mismatch);
            }
        }
        Ok(ReleaseVerification::Verified {
            root_hash: sha256_hex(&expected[0].1),
            admin_hash: sha256_hex(&expected[1].1),
        })
    }

    async fn restore_html(&self, path: &str, previous: Option<Vec<u8>>) -> Result<()> {
        if let Some(bytes) = previous {
            self.put_verified(path, &bytes, "text/html; charset=utf-8")
                .await
        } else {
            // Only a file known absent before the switch may be removed.
            let response = self
                .http
                .delete(self.url(path)?)
                .header("AccessKey", &self.password)
                .send()
                .await
                .map_err(reqwest::Error::without_url)?;
            ensure!(
                response.status().is_success() || response.status() == StatusCode::NOT_FOUND,
                "Storage HTML restore failed"
            );
            Ok(())
        }
    }

    pub async fn prune_release(&self, source_sha: &str) -> Result<()> {
        ensure!(
            source_sha.len() == 40 && source_sha.bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid managed WebUI SHA"
        );
        let prefix = format!("assets/releases/{source_sha}/");
        for file in SHARED_ASSETS.into_iter().chain(SELECTED_ASSETS) {
            self.delete(&format!("{prefix}{file}")).await?;
        }
        Ok(())
    }
}

/// The first four files are common to both package layouts. The final three
/// are selected by the signed archive's inventory.
struct ReleasePaths {
    prefix: String,
    destinations: Vec<String>,
    archive: Vec<String>,
}

impl ReleasePaths {
    fn new(release: &ComponentRelease) -> Result<Self> {
        ensure!(release.component == "webui", "not a WebUI release");
        let prefix = format!("assets/releases/{}/", release.source_sha);
        let destinations: Vec<_> = [ROOT_HTML.to_owned(), ADMIN_HTML.to_owned()]
            .into_iter()
            .chain(
                SHARED_ASSETS
                    .into_iter()
                    .chain(SELECTED_ASSETS)
                    .map(|name| format!("{prefix}{name}")),
            )
            .collect();
        let archive = destinations
            .iter()
            .map(|path| format!("webui/{path}"))
            .collect();
        Ok(Self {
            prefix,
            destinations,
            archive,
        })
    }

    fn requested(&self) -> Vec<&str> {
        self.archive.iter().map(String::as_str).collect()
    }

    async fn read(&self, layer: VerifiedReleaseLayer<'_>) -> Result<Vec<(String, Vec<u8>)>> {
        let requested = self.requested();
        let (required, optional) = requested.split_at(2 + SHARED_ASSETS.len());
        let mut files = layer.read_with_optional(required, optional).await?;
        let dashboard = match (
            files.contains_key(optional[0]),
            files.contains_key(optional[1]),
            files.contains_key(optional[2]),
        ) {
            (true, false, false) => LEGACY_DASHBOARD,
            (false, true, true) => SPLIT_DASHBOARD,
            _ => anyhow::bail!("invalid WebUI package layout"),
        };
        for (html, script) in [(required[0], dashboard), (required[1], "administration.js")] {
            let content = std::str::from_utf8(&files[html])?;
            let contains = |name| content.contains(&format!("/{}{name}", self.prefix));
            ensure!(
                contains(script)
                    && contains("style.css")
                    && (contains(RUNTIME) == (dashboard == SPLIT_DASHBOARD)),
                "{html} does not reference this release's assets"
            );
        }
        Ok(self
            .destinations
            .iter()
            .zip(&self.archive)
            .filter_map(|(path, source)| files.remove(source).map(|bytes| (path.clone(), bytes)))
            .collect())
    }
}

fn is_managed(path: &str) -> bool {
    if matches!(path, ROOT_HTML | ADMIN_HTML) {
        return true;
    }
    let Some(rest) = path.strip_prefix("assets/releases/") else {
        return false;
    };
    let Some((sha, file)) = rest.split_once('/') else {
        return false;
    };
    sha.len() == 40
        && sha.bytes().all(|c| c.is_ascii_hexdigit())
        && (SHARED_ASSETS.contains(&file) || SELECTED_ASSETS.contains(&file))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signing_fixture::{SigningKeyChoice, signature_not_after_far_future, signing};
    use axum::{
        Router,
        body::Bytes,
        extract::State,
        http::{HeaderMap, Method, StatusCode, Uri},
        routing::any,
    };
    use std::{collections::HashMap, sync::Arc};
    use tokio::sync::Mutex;
    #[tokio::test]
    async fn storage_status_errors_omit_urls() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let app = Router::new().fallback(any(|| async { StatusCode::FORBIDDEN }));
            axum::serve(listener, app).await.unwrap();
        });
        let storage = Storage {
            http: http_client_builder().build().unwrap(),
            root: format!("http://{address}/PRIVATE_ZONE"),
            password: "PRIVATE_PASSWORD".into(),
        };
        for result in [
            storage.get("index.html").await.map(|_| ()),
            storage
                .put_verified("index.html", b"html", "text/html")
                .await,
        ] {
            let error = result.unwrap_err();
            let request = error.downcast_ref::<reqwest::Error>().unwrap();
            assert!(request.url().is_none());
            assert_eq!(request.status(), Some(StatusCode::FORBIDDEN));
            assert!(!format!("{error:#} {error:?}").contains("PRIVATE_"));
        }
        server.abort();
        let _ = server.await;
    }

    #[test]
    fn storage_path_allowlist_excludes_directories_and_legacy_files() {
        assert!(is_managed("index.html"));
        assert!(is_managed(&format!(
            "assets/releases/{}/app.js",
            "a".repeat(40)
        )));
        assert!(!is_managed("app.js"));
        assert!(!is_managed("assets/releases/../app.js"));
        assert!(!is_managed(&format!(
            "assets/releases/{}/other.js",
            "a".repeat(40)
        )));
    }

    #[tokio::test]
    async fn uploads_use_uppercase_checksum_and_prune_only_exact_files() {
        #[derive(Default)]
        struct Fixture {
            files: HashMap<String, Vec<u8>>,
            deleted: Vec<String>,
            checksum: String,
        }
        async fn endpoint(
            State(state): State<Arc<Mutex<Fixture>>>,
            method: Method,
            uri: Uri,
            headers: HeaderMap,
            body: Bytes,
        ) -> (StatusCode, Vec<u8>) {
            let path = uri.path().trim_start_matches("/zone/").to_owned();
            let mut fixture = state.lock().await;
            match method {
                Method::PUT => {
                    fixture.checksum = headers
                        .get("Checksum")
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_owned();
                    fixture.files.insert(path, body.to_vec());
                    (StatusCode::CREATED, vec![])
                }
                Method::GET => match fixture.files.get(&path) {
                    Some(bytes) => (StatusCode::OK, bytes.clone()),
                    None => (StatusCode::NOT_FOUND, vec![]),
                },
                Method::DELETE => {
                    fixture.deleted.push(path.clone());
                    fixture.files.remove(&path);
                    (StatusCode::OK, vec![])
                }
                _ => (StatusCode::METHOD_NOT_ALLOWED, vec![]),
            }
        }
        let state = Arc::new(Mutex::new(Fixture::default()));
        let router = Router::new()
            .fallback(any(endpoint))
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let storage = Storage {
            http: http_client_builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            root: format!("http://{address}/zone"),
            password: "zone-only-secret".into(),
        };
        let sha = "a".repeat(40);
        let asset = format!("assets/releases/{sha}/app.js");
        storage
            .put_verified(&asset, b"asset bytes", "text/javascript")
            .await
            .unwrap();
        assert_eq!(
            state.lock().await.checksum,
            sha256_hex(b"asset bytes").to_ascii_uppercase()
        );
        assert!(storage.delete("assets/releases/").await.is_err());
        storage.prune_release(&sha).await.unwrap();
        let deleted = state.lock().await.deleted.clone();
        assert_eq!(
            deleted,
            [
                "style.css",
                "administration.js",
                "app.js",
                "workspace.js",
                "webui-runtime.js"
            ]
            .map(|name| format!("assets/releases/{sha}/{name}"))
        );
        server.abort();
    }

    async fn serve(router: Router) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (address, server)
    }

    struct RegistryFixture {
        objects: HashMap<String, Vec<u8>>,
        layer_path: String,
        reads: Vec<String>,
    }

    async fn object(
        State(state): State<Arc<Mutex<RegistryFixture>>>,
        uri: Uri,
    ) -> (StatusCode, Vec<u8>) {
        let mut state = state.lock().await;
        state.reads.push(uri.path().to_owned());
        match state
            .objects
            .get(uri.path().rsplit('/').next().unwrap_or(""))
        {
            Some(bytes) => (StatusCode::OK, bytes.clone()),
            None => (StatusCode::NOT_FOUND, vec![]),
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum PackageLayout {
        Legacy,
        Split,
        MissingRuntime,
    }

    async fn webui_registry(
        not_after: &str,
        layout: PackageLayout,
    ) -> (
        Registry,
        ComponentRelease,
        HashMap<String, Vec<u8>>,
        Arc<Mutex<RegistryFixture>>,
        tokio::task::JoinHandle<()>,
    ) {
        let signing = signing();
        let mut release = ComponentRelease {
            upgrade: None,
            component: "webui".into(),
            channel: "latest".into(),
            release_id: "webui-fixture".into(),
            source_sha: if layout == PackageLayout::Legacy {
                "c"
            } else {
                "d"
            }
            .repeat(40),
            published_at: "2025-01-01T00:00:00Z".into(),
            image: "ghcr.io/jatmn/jmodguard-webui".into(),
            digest: String::new(),
            min_schema: 15,
            max_schema: 15,
            result_schema: 15,
            min_api: 1,
            max_api: 1,
            launcher_protocol: 1,
            launcher: None,
            base_abi: "static-webui-v1".into(),
        };
        let assets = format!("assets/releases/{}", release.source_sha);
        let (dashboard, preload) = if layout == PackageLayout::Legacy {
            ("app.js", String::new())
        } else {
            (
                "workspace.js",
                format!(r#"<link rel="modulepreload" href="/{assets}/webui-runtime.js">"#),
            )
        };
        let mut files = HashMap::from([
            (
                ROOT_HTML.to_owned(),
                format!(r#"{preload}<script src="/{assets}/{dashboard}"></script><link href="/{assets}/style.css">"#)
                    .into_bytes(),
            ),
            (
                ADMIN_HTML.to_owned(),
                format!(r#"{preload}<script src="/{assets}/administration.js"></script><link href="/{assets}/style.css">"#)
                    .into_bytes(),
            ),
            (format!("{assets}/{dashboard}"), b"dashboard JavaScript".to_vec()),
            (format!("{assets}/style.css"), b"shared stylesheet".to_vec()),
            (
                format!("{assets}/administration.js"),
                b"administration JavaScript".to_vec(),
            ),
        ]);
        if layout == PackageLayout::Split {
            files.insert(
                format!("{assets}/webui-runtime.js"),
                b"shared JavaScript".to_vec(),
            );
        }
        let mut layer = Vec::new();
        {
            let mut tar = tar::Builder::new(&mut layer);
            for (path, bytes) in &files {
                let mut header = tar::Header::new_gnu();
                header.set_size(bytes.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                tar.append_data(&mut header, format!("webui/{path}"), bytes.as_slice())
                    .unwrap();
            }
            tar.finish().unwrap();
        }
        let config = serde_json::to_vec(&serde_json::json!({
            "architecture": "amd64",
            "os": "linux",
            "config": {"Labels": {
                "org.opencontainers.image.revision": release.source_sha,
                "io.jmodguard.release-id": release.release_id,
            }},
        }))
        .unwrap();
        let digest = |bytes: &[u8]| format!("sha256:{}", sha256_hex(bytes));
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": digest(&config),
                "size": config.len(),
            },
            "layers": [{
                "mediaType": "application/vnd.oci.image.layer.v1.tar",
                "digest": digest(&layer),
                "size": layer.len(),
            }],
        }))
        .unwrap();
        release.digest = digest(&manifest);
        let mut objects = HashMap::new();
        objects.insert(digest(&config), config);
        let layer_digest = digest(&layer);
        objects.insert(layer_digest.clone(), layer);
        objects.insert(release.digest.clone(), manifest);
        signing.sign(
            &mut objects,
            "webui",
            &release.digest,
            not_after,
            SigningKeyChoice::Current,
        );
        let state = Arc::new(Mutex::new(RegistryFixture {
            objects,
            layer_path: format!("/v2/jatmn/jmodguard-webui/blobs/{layer_digest}"),
            reads: Vec::new(),
        }));
        let (registry_address, registry_server) = serve(
            Router::new()
                .fallback(any(object))
                .with_state(Arc::clone(&state)),
        )
        .await;
        let registry = Registry::new(&format!("http://{registry_address}")).unwrap();
        (registry, release, files, state, registry_server)
    }

    #[derive(Default)]
    struct StorageFixture {
        files: HashMap<String, Vec<u8>>,
        reads: Vec<String>,
        uploads: Vec<String>,
        fail_root_upload: bool,
    }

    async fn stored(
        State(state): State<Arc<Mutex<StorageFixture>>>,
        method: Method,
        uri: Uri,
        body: Bytes,
    ) -> (StatusCode, Vec<u8>) {
        let path = uri.path().trim_start_matches("/zone/");
        let mut state = state.lock().await;
        match method {
            Method::GET => {
                state.reads.push(path.to_owned());
                match state.files.get(path) {
                    Some(bytes) => (StatusCode::OK, bytes.clone()),
                    None => (StatusCode::NOT_FOUND, vec![]),
                }
            }
            Method::PUT => {
                state.uploads.push(path.to_owned());
                state.files.insert(path.to_owned(), body.to_vec());
                // The server may have stored the new HTML before reporting failure.
                if path == ROOT_HTML && std::mem::take(&mut state.fail_root_upload) {
                    return (StatusCode::INTERNAL_SERVER_ERROR, vec![]);
                }
                (StatusCode::CREATED, vec![])
            }
            Method::DELETE => {
                state.files.remove(path);
                (StatusCode::OK, vec![])
            }
            _ => (StatusCode::METHOD_NOT_ALLOWED, vec![]),
        }
    }

    async fn webui_storage(
        fixture: StorageFixture,
    ) -> (
        Storage,
        Arc<Mutex<StorageFixture>>,
        tokio::task::JoinHandle<()>,
    ) {
        let state = Arc::new(Mutex::new(fixture));
        let (storage_address, storage_server) = serve(
            Router::new()
                .fallback(any(stored))
                .with_state(Arc::clone(&state)),
        )
        .await;
        let storage = Storage {
            http: http_client_builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            root: format!("http://{storage_address}/zone"),
            password: "zone-only-secret".into(),
        };
        (storage, state, storage_server)
    }

    #[tokio::test]
    async fn publishes_split_release_and_rolls_back_to_legacy_package() {
        let not_after = signature_not_after_far_future();
        let (legacy, previous, previous_files, _, legacy_server) =
            webui_registry(&not_after, PackageLayout::Legacy).await;
        let (split, release, files, _, split_server) =
            webui_registry(&not_after, PackageLayout::Split).await;
        let (incomplete, broken, _, _, incomplete_server) =
            webui_registry(&not_after, PackageLayout::MissingRuntime).await;
        let (storage, state, storage_server) = webui_storage(StorageFixture {
            files: previous_files.clone(),
            ..StorageFixture::default()
        })
        .await;

        // Exercise the exact strict archive read older Storage::publish performs
        // before its first Storage request. A split package omits the legacy entry.
        let old_paths = previous_files
            .keys()
            .map(|path| {
                format!(
                    "webui/{}",
                    path.replace(&previous.source_sha, &release.source_sha)
                )
            })
            .collect::<Vec<_>>();
        let requested = old_paths.iter().map(String::as_str).collect::<Vec<_>>();
        assert_eq!(
            split
                .files(&release, &requested)
                .await
                .unwrap_err()
                .to_string(),
            "release layer is missing a required file"
        );
        assert_eq!(
            storage
                .publish(&incomplete, &broken)
                .await
                .unwrap_err()
                .to_string(),
            "invalid WebUI package layout"
        );
        {
            let state = state.lock().await;
            assert!(state.reads.is_empty());
            assert!(state.uploads.is_empty());
            assert_eq!(state.files, previous_files);
        }

        let uploaded = storage.publish(&split, &release).await.unwrap();
        let runtime = format!("assets/releases/{}/webui-runtime.js", release.source_sha);
        assert!(uploaded.contains(&runtime));
        assert_verified(
            storage.verify_release(&split, &release).await.unwrap(),
            &files,
        );
        state.lock().await.files.remove(&runtime).unwrap();
        assert!(matches!(
            storage.verify_release(&split, &release).await.unwrap(),
            ReleaseVerification::Mismatch
        ));
        storage.publish(&split, &release).await.unwrap();
        storage.publish(&legacy, &previous).await.unwrap();
        assert_verified(
            storage.verify_release(&legacy, &previous).await.unwrap(),
            &previous_files,
        );
        {
            let state = state.lock().await;
            assert_eq!(
                state.files[&runtime], files[&runtime],
                "rollback retains split assets"
            );
        }
        storage.prune_release(&release.source_sha).await.unwrap();
        assert_eq!(state.lock().await.files, previous_files);

        for server in [
            legacy_server,
            split_server,
            incomplete_server,
            storage_server,
        ] {
            server.abort();
            let _ = server.await;
        }
    }

    #[tokio::test]
    async fn fresh_release_verification_rejects_changed_or_missing_assets() {
        let (registry, release, files, _, registry_server) =
            webui_registry(&signature_not_after_far_future(), PackageLayout::Legacy).await;
        let (storage, state, storage_server) = webui_storage(StorageFixture {
            files: files.clone(),
            ..StorageFixture::default()
        })
        .await;
        assert_verified(
            storage.verify_release(&registry, &release).await.unwrap(),
            &files,
        );
        for name in ["app.js", "style.css", "administration.js"] {
            let asset = format!("assets/releases/{}/{name}", release.source_sha);
            state
                .lock()
                .await
                .files
                .insert(asset.clone(), b"changed asset".to_vec());
            assert!(
                matches!(
                    storage.verify_release(&registry, &release).await.unwrap(),
                    ReleaseVerification::Mismatch
                ),
                "changed {name} must prevent adoption"
            );
            state.lock().await.files.remove(&asset).unwrap();
            assert!(
                matches!(
                    storage.verify_release(&registry, &release).await.unwrap(),
                    ReleaseVerification::Mismatch
                ),
                "missing {name} must prevent adoption"
            );
            let original = files[&asset].clone();
            state.lock().await.files.insert(asset, original);
        }
        registry_server.abort();
        storage_server.abort();
    }

    #[tokio::test]
    async fn publish_restores_html_when_root_upload_fails() {
        let (registry, release, files, _, registry_server) =
            webui_registry(&signature_not_after_far_future(), PackageLayout::Split).await;
        let old_asset = format!("assets/releases/{}/app.js", "b".repeat(40));
        let assets = format!("assets/releases/{}", release.source_sha);
        for old_admin in [Some(b"previous administration HTML".as_slice()), None] {
            let mut previous = HashMap::from([
                (ROOT_HTML.to_owned(), b"previous root HTML".to_vec()),
                (old_asset.clone(), b"previous JavaScript".to_vec()),
            ]);
            if let Some(admin) = old_admin {
                previous.insert(ADMIN_HTML.to_owned(), admin.to_vec());
            }
            let (storage, state, storage_server) = webui_storage(StorageFixture {
                files: previous.clone(),
                fail_root_upload: true,
                ..StorageFixture::default()
            })
            .await;

            let error = storage.publish(&registry, &release).await.unwrap_err();
            assert_eq!(
                error.downcast_ref::<reqwest::Error>().unwrap().status(),
                Some(StatusCode::INTERNAL_SERVER_ERROR)
            );
            assert!(!format!("{error:#}").contains("HTML restoration also failed"));

            let state = state.lock().await;
            let mut asset_uploads = state.uploads[..4].to_vec();
            asset_uploads.sort_unstable();
            assert_eq!(
                asset_uploads,
                [
                    format!("{assets}/administration.js"),
                    format!("{assets}/style.css"),
                    format!("{assets}/webui-runtime.js"),
                    format!("{assets}/workspace.js"),
                ]
            );
            assert_eq!(&state.uploads[4..6], [ADMIN_HTML, ROOT_HTML]);
            previous.extend(
                files
                    .iter()
                    .filter(|(path, _)| path.starts_with(&assets))
                    .map(|(path, bytes)| (path.clone(), bytes.clone())),
            );
            assert_eq!(
                state.files, previous,
                "failed publication must restore the prior HTML"
            );
            storage_server.abort();
        }
        registry_server.abort();
    }

    fn assert_verified(result: ReleaseVerification, files: &HashMap<String, Vec<u8>>) {
        match result {
            ReleaseVerification::Verified {
                root_hash,
                admin_hash,
            } => {
                assert_eq!(root_hash, sha256_hex(&files[ROOT_HTML]));
                assert_eq!(admin_hash, sha256_hex(&files[ADMIN_HTML]));
            }
            ReleaseVerification::Mismatch => panic!("matching files must verify"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn installed_release_reverification_reuses_only_trusted_expected_files() {
        use jmodguard::db::{Db, SqlExecutor, params};
        let (registry, release, files, registry_state, registry_server) =
            webui_registry("2020-01-01T00:00:00Z", PackageLayout::Split).await;
        let (storage, storage_state, storage_server) = webui_storage(StorageFixture {
            files: files.clone(),
            ..StorageFixture::default()
        })
        .await;
        let path = std::env::temp_dir().join(format!(
            "installed-webui-{:032x}.db",
            rand::random::<u128>()
        ));
        let db = Db::open(&path, 2).unwrap();
        let control = UpdateControl::from_application(&db);

        // A new install still refuses the expired signature.
        let fresh = storage.verify_release(&registry, &release).await;
        assert_eq!(
            crate::signature::signature_rejection_phrase(&fresh.err().unwrap()),
            Some(crate::signature::EXPIRED_IMAGE_SIGNATURE)
        );
        // Expiry can be bypassed only for the durably installed release.
        let uninstalled = storage
            .verify_installed_release(&registry, &control, &release)
            .await;
        assert_eq!(
            uninstalled.err().unwrap().to_string(),
            "release is not installed"
        );
        let install = async |release: &ComponentRelease| {
            let installed = serde_json::to_string(release).unwrap();
            db.with_conn(move |conn| {
                conn.execute_bounded(
                    "UPDATE component_updates SET installed=?1,installed_at=1 WHERE component='webui'",
                    params![installed],
                    128 * 1024,
                )?;
                Ok(())
            })
            .await
            .unwrap();
        };
        install(&release).await;
        let signature_tag = crate::signature::signature_tag_for_digest(&release.digest).unwrap();
        let layer_reads = async || {
            let state = registry_state.lock().await;
            state
                .reads
                .iter()
                .filter(|path| *path == &state.layer_path)
                .count()
        };
        let assert_storage_rounds = async |rounds: usize| {
            let mut reads = std::mem::take(&mut storage_state.lock().await.reads);
            reads.sort_unstable();
            let mut expected = (0..rounds)
                .flat_map(|_| files.keys().cloned())
                .collect::<Vec<_>>();
            expected.sort_unstable();
            assert_eq!(reads, expected);
        };

        // The manual verifier still exercises the original uncached path.
        for _ in 0..2 {
            assert_verified(
                storage
                    .verify_installed_release(&registry, &control, &release)
                    .await
                    .unwrap(),
                &files,
            );
        }
        assert_eq!(layer_reads().await, 2);
        assert_storage_rounds(2).await;
        registry_state.lock().await.reads.clear();

        let mut inventory = InstalledWebuiInventory::default();
        let verify = async |release: &ComponentRelease, inventory: &mut InstalledWebuiInventory| {
            storage
                .verify_installed_release_cached(&registry, &control, release, inventory)
                .await
        };
        for _ in 0..2 {
            assert_verified(verify(&release, &mut inventory).await.unwrap(), &files);
        }
        assert_eq!(layer_reads().await, 1);
        assert_storage_rounds(2).await;
        {
            let state = registry_state.lock().await;
            for reference in [&release.digest, &signature_tag] {
                let path = format!("/v2/jatmn/jmodguard-webui/manifests/{reference}");
                assert_eq!(state.reads.iter().filter(|get| *get == &path).count(), 2);
            }
            let manifest: serde_json::Value =
                serde_json::from_slice(&state.objects[&release.digest]).unwrap();
            let config = manifest["config"]["digest"].as_str().unwrap();
            let path = format!("/v2/jatmn/jmodguard-webui/blobs/{config}");
            assert_eq!(state.reads.iter().filter(|get| *get == &path).count(), 2);
        }

        let asset = format!("assets/releases/{}/webui-runtime.js", release.source_sha);
        storage_state.lock().await.files.get_mut(&asset).unwrap()[0] ^= 1;
        assert!(matches!(
            verify(&release, &mut inventory).await.unwrap(),
            ReleaseVerification::Mismatch
        ));
        assert_storage_rounds(1).await;
        assert_eq!(layer_reads().await, 1);
        storage_state
            .lock()
            .await
            .files
            .insert(asset.clone(), files[&asset].clone());

        let signature = registry_state
            .lock()
            .await
            .objects
            .remove(&signature_tag)
            .unwrap();
        let rejected = verify(&release, &mut inventory).await.err().unwrap();
        assert_eq!(
            crate::signature::signature_rejection_phrase(&rejected),
            Some(crate::signature::UNSIGNED_IMAGE)
        );
        assert_storage_rounds(0).await;
        registry_state
            .lock()
            .await
            .objects
            .insert(signature_tag, signature);
        assert_verified(verify(&release, &mut inventory).await.unwrap(), &files);
        assert_eq!(layer_reads().await, 2, "trust loss clears the inventory");
        assert_storage_rounds(1).await;

        let selected = ComponentRelease {
            channel: "stable".into(),
            ..release.clone()
        };
        install(&selected).await;
        let stale = verify(&release, &mut inventory).await.err().unwrap();
        assert_eq!(stale.to_string(), "release is not installed");
        assert_storage_rounds(0).await;
        assert_verified(verify(&selected, &mut inventory).await.unwrap(), &files);
        assert_eq!(layer_reads().await, 3);
        assert_storage_rounds(1).await;
        install(&release).await;
        assert_verified(verify(&release, &mut inventory).await.unwrap(), &files);
        assert_eq!(
            layer_reads().await,
            4,
            "a changed release replaces the warm entry"
        );
        assert_storage_rounds(1).await;
        registry_server.abort();
        storage_server.abort();
        drop(control);
        drop(db);
        let _ = std::fs::remove_file(&path);
    }
}
