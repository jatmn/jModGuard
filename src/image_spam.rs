//! Shared exact-image test policy and bounded Discord attachment hashing.
//! No image decoding, arbitrary URL fetch, archive, or punishment API.
use anyhow::{Result, ensure};
use hyper::{Body, Client, body::HttpBody};
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;

pub fn parse_hash(raw: &str) -> Option<[u8; 32]> {
    if raw.len() != 64 || !raw.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut hash = [0; 32];
    for (i, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&raw[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(hash)
}

pub const TEST_MODE: &str = "exact_image_test";
pub const ENFORCE_MODE: &str = "exact_image";
pub const MODE: &str = TEST_MODE;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub mode: String,
    pub window_secs: u64,
    pub min_channels: usize,
    pub min_attachments: usize,
    #[serde(default, skip_deserializing)]
    pub enforce: bool,
    #[serde(default, skip_deserializing)]
    pub known_bad_hashes: std::collections::BTreeSet<[u8; 32]>,
}
impl Policy {
    pub fn parse(rule: &crate::types::RuleDef) -> Result<Self> {
        let mut value: Self = serde_json::from_value(rule.params.clone())?;
        ensure!(
            (1..=600).contains(&value.window_secs),
            "image window must be 1–600 seconds"
        );
        ensure!(
            (2..=10).contains(&value.min_channels),
            "image channel threshold must be 2–10"
        );
        ensure!(
            (1..=10).contains(&value.min_attachments),
            "common distinct images must be 1–10"
        );
        match value.mode.as_str() {
            TEST_MODE => {
                ensure!(
                    rule.dry_run && rule.actions.iter().all(|a| a.action == "log"),
                    "exact-image test mode requires log-only actions"
                );
                value.enforce = false;
            }
            ENFORCE_MODE if rule.dry_run => {
                ensure!(
                    rule.actions.iter().all(|a| a.action == "log"),
                    "exact-image test mode cannot configure punitive actions"
                );
                value.enforce = false;
            }
            ENFORCE_MODE => {
                ensure!(!rule.dry_run, "image enforcement cannot be dry-run");
                let actions: std::collections::BTreeSet<_> =
                    rule.actions.iter().map(|a| a.action.as_str()).collect();
                ensure!(
                    actions.contains("delete") && actions.contains("ban"),
                    "image enforcement requires delete and ban"
                );
                ensure!(
                    actions
                        .iter()
                        .all(|a| matches!(*a, "delete" | "ban" | "log")),
                    "image enforcement cannot use the generic punishment table"
                );
                value.enforce = true;
            }
            _ => anyhow::bail!("unsupported exact-image mode"),
        }
        Ok(value)
    }
}

/// Legacy attachment-count configurations stay inert until explicitly converted.
pub fn validate_rule(id: &str, rule: &crate::types::RuleDef) -> Result<()> {
    match id {
        "attachment_flood" if rule.params.get("mode").is_some() => {
            Policy::parse(rule)?;
        }
        "repeat_text" => {
            crate::text_spam::Policy::parse(rule)?;
        }
        _ => {}
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub max_authors: usize,
    pub max_authors_per_guild: usize,
    pub max_posts_per_author: usize,
    pub max_files_per_post: usize,
    pub max_file_bytes: u64,
    pub concurrency: usize,
    pub timeout_secs: u64,
    pub requests_per_minute: u64,
    pub requests_per_guild_minute: u64,
    pub requests_per_author_minute: u64,
    pub bytes_per_minute: u64,
    pub comparison_steps: usize,
    pub metadata_bytes: usize,
    pub metadata_bytes_per_guild: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_authors: 512,
            max_authors_per_guild: 64,
            max_posts_per_author: 16,
            max_files_per_post: 10,
            max_file_bytes: 8 * 1024 * 1024,
            concurrency: 2,
            timeout_secs: 10,
            requests_per_minute: 120,
            requests_per_guild_minute: 40,
            requests_per_author_minute: 20,
            bytes_per_minute: 64 * 1024 * 1024,
            comparison_steps: 100_000,
            metadata_bytes: 4 * 1024 * 1024,
            metadata_bytes_per_guild: 1024 * 1024,
        }
    }
}
impl Limits {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=100_000).contains(&self.max_authors),
            "image max_authors must be 1–100000"
        );
        ensure!(
            (1..=self.max_authors).contains(&self.max_authors_per_guild),
            "invalid image guild author limit"
        );
        ensure!(
            (2..=64).contains(&self.max_posts_per_author),
            "image posts per author must be 2–64"
        );
        ensure!(
            (1..=10).contains(&self.max_files_per_post),
            "image files per post must be 1–10"
        );
        ensure!(
            (1..=1024 * 1024 * 1024).contains(&self.max_file_bytes),
            "image file byte limit must be 1–1GiB"
        );
        ensure!(
            (1..=32).contains(&self.concurrency) && (1..=60).contains(&self.timeout_secs),
            "invalid image concurrency/timeout"
        );
        ensure!(
            self.requests_per_minute > 0
                && self.requests_per_guild_minute > 0
                && self.requests_per_author_minute > 0
                && self.bytes_per_minute > 0,
            "image rate budgets must be positive"
        );
        ensure!(
            (4096..=1024 * 1024 * 1024).contains(&self.metadata_bytes)
                && (4096..=self.metadata_bytes).contains(&self.metadata_bytes_per_guild),
            "invalid image metadata byte budgets"
        );
        ensure!(
            (1..=10_000_000).contains(&self.comparison_steps),
            "image comparison steps must be 1–10000000"
        );
        Ok(())
    }
}

#[derive(Clone)]
pub struct Attachment {
    pub id: u64,
    pub size: u64,
    uri: hyper::Uri,
}
impl Attachment {
    pub fn retained_bytes(&self) -> usize {
        self.uri.to_string().len() + 160
    }

    pub fn new(channel: u64, id: u64, size: u64, url: &str, limit: u64) -> Result<Self> {
        ensure!(
            url.len() <= 2048 && size > 0 && size <= limit,
            "unsupported image attachment size or URL"
        );
        let parsed = url::Url::parse(url).map_err(|_| anyhow::anyhow!("invalid attachment URL"))?;
        ensure!(
            parsed.scheme() == "https"
                && parsed.host_str() == Some("cdn.discordapp.com")
                && parsed.username().is_empty()
                && parsed.password().is_none()
                && parsed.port().is_none()
                && parsed.fragment().is_none(),
            "unsupported attachment origin"
        );
        let prefix = format!("/attachments/{channel}/{id}/");
        let tail = parsed.path().strip_prefix(&prefix).unwrap_or_default();
        ensure!(
            !tail.is_empty()
                && !tail.contains('/')
                && !["%2f", "%5c", "%2e"]
                    .iter()
                    .any(|part| tail.to_ascii_lowercase().contains(part))
                && !tail.contains('\\'),
            "attachment path does not match Discord identifiers"
        );
        let uri = parsed
            .as_str()
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid attachment URI"))?;
        Ok(Self { id, size, uri })
    }

    /// Test-only URI substitution for the loopback download fixture.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_request_uri_for_test(&mut self, uri: hyper::Uri) {
        self.uri = uri;
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_request_uri_str_for_test(&mut self, uri: &str) {
        self.uri = uri.parse().expect("test uri");
    }
}

pub type Hash = [u8; 32];

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DownloadError {
    #[error("transport failure")]
    Transport,
    #[error("http status {0}")]
    HttpStatus(u16),
    #[error("content-encoded response")]
    ContentEncoding,
    #[error("download timed out")]
    Timeout,
    #[error("body size mismatch")]
    SizeMismatch,
    #[error("body read failure")]
    BodyRead,
    #[error("capture failure")]
    Capture,
}

pub struct Hasher {
    client: Client<crate::remote_https::HttpsConnector<hyper::client::HttpConnector>>,
}
impl Default for Hasher {
    fn default() -> Self {
        Self {
            client: Client::builder()
                .http1_max_buf_size(8192)
                .retry_canceled_requests(false)
                .pool_max_idle_per_host(2)
                .build(crate::remote_https::https_connector()),
        }
    }
}
impl Hasher {
    async fn get_body(&self, attachment: &Attachment) -> Result<Body, DownloadError> {
        let response = self
            .client
            .get(attachment.uri.clone())
            .await
            .map_err(|_| DownloadError::Transport)?;
        if response.status() != hyper::StatusCode::OK {
            return Err(DownloadError::HttpStatus(response.status().as_u16()));
        }
        if response
            .headers()
            .contains_key(hyper::header::CONTENT_ENCODING)
        {
            return Err(DownloadError::ContentEncoding);
        }
        Ok(response.into_body())
    }

    pub async fn hash(
        &self,
        attachment: &Attachment,
        timeout: Duration,
    ) -> Result<Hash, DownloadError> {
        match tokio::time::timeout(timeout, async {
            hash_body(self.get_body(attachment).await?, attachment.size).await
        })
        .await
        {
            Ok(Ok(hash)) => Ok(hash),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(DownloadError::Timeout),
        }
    }

    /// Stream the attachment to a new local file using the same CDN contract as `hash`.
    pub async fn fetch_file(
        &self,
        attachment: &Attachment,
        timeout: Duration,
        dest: &Path,
    ) -> Result<Hash> {
        let dest = dest.to_path_buf();
        match tokio::time::timeout(timeout, async {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&dest)
                .map_err(|_| anyhow::anyhow!("image capture file could not be created"))?;
            match stream_body(
                self.get_body(attachment).await?,
                attachment.size,
                Some(&mut file),
            )
            .await
            {
                Ok(hash) => Ok(hash),
                Err(error) => {
                    let _ = std::fs::remove_file(&dest);
                    Err(download_anyhow(error))
                }
            }
        })
        .await
        {
            Ok(Ok(hash)) => Ok(hash),
            Ok(Err(error)) => {
                let _ = std::fs::remove_file(&dest);
                Err(error)
            }
            Err(_) => {
                let _ = std::fs::remove_file(&dest);
                Err(anyhow::anyhow!("image download timed out"))
            }
        }
    }
}

fn download_anyhow(error: DownloadError) -> anyhow::Error {
    match error {
        DownloadError::Transport => anyhow::anyhow!("image download failed"),
        DownloadError::HttpStatus(_) => anyhow::anyhow!("image download did not return 200"),
        DownloadError::ContentEncoding => anyhow::anyhow!("encoded image response is unsupported"),
        DownloadError::Timeout => anyhow::anyhow!("image download timed out"),
        DownloadError::SizeMismatch => {
            anyhow::anyhow!("image body length differs from declared size")
        }
        DownloadError::BodyRead => anyhow::anyhow!("image body failed"),
        DownloadError::Capture => anyhow::anyhow!("image capture failed"),
    }
}

async fn hash_body(body: Body, expected: u64) -> Result<Hash, DownloadError> {
    stream_body(body, expected, None).await
}
async fn stream_body(
    mut body: Body,
    expected: u64,
    mut dest: Option<&mut std::fs::File>,
) -> Result<Hash, DownloadError> {
    use std::io::Write;
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let mut bytes = 0u64;
    while let Some(chunk) = body.data().await {
        let chunk = chunk.map_err(|_| DownloadError::BodyRead)?;
        bytes = bytes
            .checked_add(chunk.len() as u64)
            .ok_or(DownloadError::SizeMismatch)?;
        if bytes > expected {
            return Err(DownloadError::SizeMismatch);
        }
        if let Some(file) = dest.as_mut() {
            file.write_all(&chunk).map_err(|_| DownloadError::Capture)?;
        }
        hash.update(&chunk);
    }
    if bytes != expected {
        return Err(DownloadError::SizeMismatch);
    }
    if let Some(file) = dest.as_mut() {
        file.sync_all().map_err(|_| DownloadError::Capture)?;
    }
    Ok(hash.finish().as_ref().try_into().expect("SHA256 length"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_rejects_arbitrary_endpoints_and_mismatched_ids() {
        for url in [
            "http://cdn.discordapp.com/attachments/1/2/a.png",
            "https://example.org/attachments/1/2/a.png",
            "https://cdn.discordapp.com.evil.org/attachments/1/2/a.png",
            "https://x@cdn.discordapp.com/attachments/1/2/a.png",
            "https://cdn.discordapp.com:123/attachments/1/2/a.png",
            "https://cdn.discordapp.com/attachments/2/2/a.png",
            "https://cdn.discordapp.com/attachments/1/2/../a.png",
            "https://cdn.discordapp.com/attachments/1/2/%2f.png",
            "https://cdn.discordapp.com/attachments/1/2/a.png#fragment",
        ] {
            assert!(Attachment::new(1, 2, 3, url, 10).is_err(), "{url}");
        }
        assert!(
            Attachment::new(
                1,
                2,
                3,
                "https://cdn.discordapp.com/attachments/1/2/a.png?ex=abc&is=abc&hm=abc",
                10
            )
            .is_ok()
        );
        assert!(
            Attachment::new(
                1,
                2,
                11,
                "https://cdn.discordapp.com/attachments/1/2/a.png",
                10
            )
            .is_err()
        );
    }
    #[tokio::test]
    async fn hashes_bytes_and_rejects_truncation_or_overrun() {
        let h = hash_body(Body::from("abc"), 3).await.unwrap();
        assert_eq!(
            h.as_slice(),
            ring::digest::digest(&ring::digest::SHA256, b"abc").as_ref()
        );
        assert_eq!(
            hash_body(Body::from("ab"), 3).await,
            Err(DownloadError::SizeMismatch)
        );
        assert_eq!(
            hash_body(Body::from("abcd"), 3).await,
            Err(DownloadError::SizeMismatch)
        );
        assert_ne!(h, hash_body(Body::from("abd"), 3).await.unwrap());
    }
    #[tokio::test]
    async fn transport_streams_without_redirects_and_has_a_deadline() {
        use hyper::{
            Response, Server,
            service::{make_service_fn, service_fn},
        };
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&hits);
        let server =
            Server::bind(&"127.0.0.1:0".parse().unwrap()).serve(make_service_fn(move |_| {
                let hits = Arc::clone(&seen);
                async move {
                    Ok::<_, std::convert::Infallible>(service_fn(
                        move |request: hyper::Request<Body>| {
                            hits.fetch_add(1, Ordering::SeqCst);
                            async move {
                                let response = match request.uri().path() {
                                    "/too_many" => {
                                        Response::builder().status(429).body(Body::empty()).unwrap()
                                    }
                                    "/redirect" => Response::builder()
                                        .status(302)
                                        .header("location", "/ok")
                                        .body(Body::empty())
                                        .unwrap(),
                                    "/encoding" => Response::builder()
                                        .header("content-encoding", "gzip")
                                        .body(Body::from("abc"))
                                        .unwrap(),
                                    "/slow" => {
                                        tokio::time::sleep(Duration::from_secs(2)).await;
                                        Response::new(Body::from("abc"))
                                    }
                                    _ => Response::new(Body::from("abc")),
                                };
                                Ok::<_, std::convert::Infallible>(response)
                            }
                        },
                    ))
                }
            }));
        let address = server.local_addr();
        let task = tokio::spawn(server);
        let client = Hasher::default();
        let mut source = Attachment::new(
            1,
            2,
            3,
            "https://cdn.discordapp.com/attachments/1/2/a.png",
            10,
        )
        .unwrap();
        // Private test fixture substitution, unavailable to production callers.
        source.uri = format!("http://{address}/ok").parse().unwrap();
        assert_eq!(
            client.hash(&source, Duration::from_secs(1)).await.unwrap(),
            hash_body(Body::from("abc"), 3).await.unwrap()
        );
        source.uri = format!("http://{address}/too_many").parse().unwrap();
        let started = std::time::Instant::now();
        assert_eq!(
            client.hash(&source, Duration::from_secs(1)).await,
            Err(DownloadError::HttpStatus(429))
        );
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "429 must not backoff"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2, "429 is a single request");
        source.uri = format!("http://{address}/redirect").parse().unwrap();
        assert_eq!(
            client.hash(&source, Duration::from_secs(1)).await,
            Err(DownloadError::HttpStatus(302))
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            3,
            "redirect must not issue a second request"
        );
        source.uri = format!("http://{address}/encoding").parse().unwrap();
        assert_eq!(
            client.hash(&source, Duration::from_secs(1)).await,
            Err(DownloadError::ContentEncoding)
        );
        source.uri = format!("http://{address}/slow").parse().unwrap();
        assert_eq!(
            client.hash(&source, Duration::from_millis(30)).await,
            Err(DownloadError::Timeout)
        );
        source.uri = format!("http://{address}/ok").parse().unwrap();
        let dest = std::env::temp_dir().join(format!(
            "jmodguard-attach-fetch-{:032x}.part",
            rand::random::<u128>()
        ));
        assert_eq!(
            client
                .fetch_file(&source, Duration::from_secs(1), &dest)
                .await
                .unwrap(),
            hash_body(Body::from("abc"), 3).await.unwrap()
        );
        assert_eq!(std::fs::read(&dest).unwrap(), b"abc");
        assert!(
            client
                .fetch_file(&source, Duration::from_secs(1), &dest)
                .await
                .is_err(),
            "capture must not overwrite an existing file"
        );
        let timeout_dest = std::env::temp_dir().join(format!(
            "jmodguard-attach-fetch-timeout-{:032x}.part",
            rand::random::<u128>()
        ));
        source.uri = format!("http://{address}/slow").parse().unwrap();
        assert!(
            client
                .fetch_file(&source, Duration::from_millis(30), &timeout_dest)
                .await
                .is_err()
        );
        assert!(
            !timeout_dest.exists(),
            "timed-out fetch must not leave a temporary capture file"
        );
        let _ = std::fs::remove_file(&dest);
        task.abort();
    }

    #[tokio::test]
    async fn capture_write_is_not_a_download_transport_error() {
        let path = std::env::temp_dir().join(format!(
            "jmodguard-capture-write-{:032x}.part",
            rand::random::<u128>()
        ));
        std::fs::write(&path, b"x").unwrap();
        let mut file = std::fs::OpenOptions::new().read(true).open(&path).unwrap();
        let error = stream_body(Body::from("abc"), 3, Some(&mut file))
            .await
            .unwrap_err();
        assert_eq!(error, DownloadError::Capture);
        let message = format!("{:#}", download_anyhow(error));
        assert_eq!(message, "image capture failed");
        assert!(!message.contains("image download failed"));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn capture_sync_is_not_a_download_transport_error() {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .unwrap();
        let error = stream_body(Body::from("abc"), 3, Some(&mut file))
            .await
            .unwrap_err();
        assert_eq!(error, DownloadError::Capture);
    }

    #[test]
    fn download_transport_anyhow_is_unchanged() {
        let message = format!("{:#}", download_anyhow(DownloadError::Transport));
        assert_eq!(message, "image download failed");
    }

    #[test]
    fn policy_requires_explicit_mode_and_valid_thresholds() {
        let mut r = crate::defaults::default_rules()
            .into_iter()
            .find(|rule| rule.name.contains("image"))
            .unwrap();
        assert!(Policy::parse(&r).is_ok());
        r.dry_run = false;
        assert!(Policy::parse(&r).is_err());
        r.dry_run = true;
        r.params["min_channels"] = 1.into();
        assert!(Policy::parse(&r).is_err());
        r.params["min_channels"] = 3.into();
        r.actions[0].action = "ban".into();
        assert!(Policy::parse(&r).is_err());
        r.params = serde_json::json!({"min_attachments":2});
        assert!(Policy::parse(&r).is_err());
        assert!(validate_rule("attachment_flood", &r).is_ok()); // inert legacy row
        r.params = serde_json::json!({
            "mode":"exact_image","window_secs":60,"min_channels":3,"min_attachments":2
        });
        r.dry_run = false;
        r.actions = vec![
            crate::types::ActionDef {
                at: 1,
                action: "delete".into(),
                duration_minutes: 0,
                delete_days: 0,
                reason: "image spam".into(),
            },
            crate::types::ActionDef {
                at: 1,
                action: "ban".into(),
                duration_minutes: 0,
                delete_days: 0,
                reason: "image spam".into(),
            },
        ];
        let parsed = Policy::parse(&r).unwrap();
        assert!(parsed.enforce);
        r.actions[0].action = "timeout".into();
        assert!(Policy::parse(&r).is_err());
    }
}
