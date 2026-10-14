//! Scoped WebUI publication using a Bunny Storage Zone password. This never
//! calls the account API or recursively deletes a Storage directory.
use crate::oci::{Registry, sha256_hex};
use anyhow::{Context, Result, ensure};
use jmodguard::db::ComponentRelease;
use reqwest::StatusCode;
use std::time::Duration;

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
            http: reqwest::Client::builder()
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
        ensure!(release.component == "webui", "not a WebUI release");
        let suffix = format!("assets/releases/{}/", release.source_sha);
        let assets = [
            format!("{suffix}app.js"),
            format!("{suffix}style.css"),
            format!("{suffix}administration.js"),
        ];
        let package = [
            "webui/index.html".to_owned(),
            "webui/administration/index.html".to_owned(),
            format!("webui/{}", assets[0]),
            format!("webui/{}", assets[1]),
            format!("webui/{}", assets[2]),
        ];
        let request = package.iter().map(String::as_str).collect::<Vec<_>>();
        let files = registry.files(release, &request).await?;
        let prefix = format!("/{suffix}");
        let root_html =
            std::str::from_utf8(files.get(&package[0]).context("root HTML is missing")?)?;
        let admin_html = std::str::from_utf8(
            files
                .get(&package[1])
                .context("Administration HTML is missing")?,
        )?;
        ensure!(
            root_html.contains(&format!("{prefix}app.js"))
                && root_html.contains(&format!("{prefix}style.css")),
            "root HTML does not reference this release's assets"
        );
        ensure!(
            admin_html.contains(&format!("{prefix}administration.js"))
                && admin_html.contains(&format!("{prefix}style.css")),
            "Administration HTML does not reference this release's assets"
        );
        let old_root = self.get("index.html").await?;
        let old_admin = self.get("administration/index.html").await?;
        for (asset, package_path) in assets.iter().zip(package[2..].iter()) {
            let body = files
                .get(package_path)
                .context("WebUI package is incomplete")?;
            let mime = if asset.ends_with(".css") {
                "text/css; charset=utf-8"
            } else {
                "text/javascript; charset=utf-8"
            };
            self.put_verified(asset, body, mime).await?;
        }
        let switched = async {
            self.put_verified(
                "administration/index.html",
                files
                    .get(&package[1])
                    .context("Administration HTML is missing")?,
                "text/html; charset=utf-8",
            )
            .await?;
            self.put_verified(
                "index.html",
                files.get(&package[0]).context("root HTML is missing")?,
                "text/html; charset=utf-8",
            )
            .await?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(error) = switched {
            let recovery = self.restore_html("index.html", old_root).await;
            let admin_recovery = self
                .restore_html("administration/index.html", old_admin)
                .await;
            if let Err(recovery_error) = recovery.and(admin_recovery) {
                return Err(
                    error.context(format!("HTML restoration also failed: {recovery_error:#}"))
                );
            }
            return Err(error);
        }
        Ok(assets.to_vec())
    }

    /// Reconcile a response lost after the HTML switch. Adoption requires all
    /// five Storage bytes to match the digest-pinned public release package.
    pub async fn verify_release(
        &self,
        registry: &Registry,
        release: &ComponentRelease,
    ) -> Result<Option<(String, String)>> {
        ensure!(release.component == "webui", "not a WebUI release");
        let prefix = format!("assets/releases/{}/", release.source_sha);
        let destinations = [
            "index.html".to_owned(),
            "administration/index.html".to_owned(),
            format!("{prefix}app.js"),
            format!("{prefix}style.css"),
            format!("{prefix}administration.js"),
        ];
        let package = destinations
            .iter()
            .map(|path| format!("webui/{path}"))
            .collect::<Vec<_>>();
        let request = package.iter().map(String::as_str).collect::<Vec<_>>();
        let expected = registry.files(release, &request).await?;
        for (destination, source) in destinations.iter().zip(package.iter()) {
            let Some(actual) = self.get(destination).await? else {
                return Ok(None);
            };
            if expected.get(source).is_none_or(|bytes| bytes != &actual) {
                return Ok(None);
            }
        }
        Ok(Some((
            sha256_hex(expected.get(&package[0]).unwrap()),
            sha256_hex(expected.get(&package[1]).unwrap()),
        )))
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
        for file in ["app.js", "style.css", "administration.js"] {
            self.delete(&format!("{prefix}{file}")).await?;
        }
        Ok(())
    }
}

fn is_managed(path: &str) -> bool {
    if matches!(path, "index.html" | "administration/index.html") {
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
        && matches!(file, "app.js" | "style.css" | "administration.js")
}

#[cfg(test)]
mod tests {
    use super::*;
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
            http: reqwest::Client::new(),
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
            http: reqwest::Client::builder()
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
        assert_eq!(deleted.len(), 3);
        assert!(
            deleted
                .iter()
                .all(|path| path.starts_with(&format!("assets/releases/{sha}/")))
        );
        server.abort();
    }
}
