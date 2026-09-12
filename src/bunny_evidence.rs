//! Opt-in Bunny Storage evidence I/O. No runtime worker, source fetcher or serving route.
use crate::db::guild_data::*;
use crate::evidence_transfer::{EvidenceTarget, EvidenceUpload};
use crate::remote_https::{HttpsConnector, https_connector};
use anyhow::{Context, Result, ensure};
use hyper::body::{Bytes, HttpBody};
use hyper::client::HttpConnector;
use hyper::header::{CONTENT_LENGTH, CONTENT_TYPE, HeaderValue};
use hyper::{Body, Client, Method, Request, Response, StatusCode};
use ring::digest::{Context as Digest, SHA256};
use std::fmt::Write as _;
use std::io::SeekFrom;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt};
use tokio::sync::Semaphore;

/// Explicit installation controls, not guild-facing request parameters.
#[derive(Clone, Copy)]
pub struct BunnyTransferLimits {
    pub buffer_bytes: usize,
    pub request_timeout: Duration,
}

/// Trusted dedicated private evidence zone. Deliberately no Debug/Serialize: contains a key.
#[derive(Clone)]
pub struct BunnyEvidenceStore {
    storage_id: CaseId,
    base: String,
    key: HeaderValue,
    limits: BunnyTransferLimits,
    client: Client<HttpsConnector<HttpConnector>>,
    admission: Arc<Semaphore>,
}
fn digest_hex(digest: ring::digest::Digest) -> String {
    let mut out = String::with_capacity(64);
    for byte in digest.as_ref() {
        write!(&mut out, "{byte:02x}").expect("writing to string");
    }
    out
}
impl BunnyEvidenceStore {
    /// The installer must verify a dedicated zone without public delivery and provision
    /// `jmodguard-evidence-identity` with `jmodguard-bunny-evidence-1\n{storage_id}\n`.
    /// This client verifies that marker but cannot audit Pull Zones using a storage password.
    pub fn new(
        storage_id: CaseId,
        hostname: &str,
        zone: &str,
        access_key: &str,
        limits: BunnyTransferLimits,
    ) -> Result<Self> {
        ensure!(
            matches!(
                hostname,
                "storage.bunnycdn.com"
                    | "uk.storage.bunnycdn.com"
                    | "ny.storage.bunnycdn.com"
                    | "la.storage.bunnycdn.com"
                    | "sg.storage.bunnycdn.com"
                    | "se.storage.bunnycdn.com"
                    | "br.storage.bunnycdn.com"
                    | "jh.storage.bunnycdn.com"
                    | "syd.storage.bunnycdn.com"
            ),
            "unsupported Bunny storage endpoint"
        );
        ensure!(
            !zone.is_empty()
                && zone.len() <= 100
                && zone.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "invalid storage zone name"
        );
        ensure!(!access_key.is_empty(), "missing storage credential");
        ensure!(
            limits.buffer_bytes > 0 && !limits.request_timeout.is_zero(),
            "positive transfer limits required"
        );
        let mut key = HeaderValue::from_str(access_key)
            .map_err(|_| anyhow::anyhow!("invalid storage credential"))?;
        key.set_sensitive(true);
        let client = Client::builder()
            .http1_max_buf_size(8192)
            .pool_max_idle_per_host(1)
            .retry_canceled_requests(false)
            .build(https_connector());
        Ok(Self {
            storage_id,
            base: format!("https://{hostname}/{zone}/"),
            key,
            limits,
            client,
            admission: Arc::new(Semaphore::new(1)),
        })
    }
    fn request(&self, method: Method, path: &str, body: Body) -> Result<Request<Body>> {
        Ok(Request::builder()
            .method(method)
            .uri(format!("{}{path}", self.base))
            .header("AccessKey", self.key.clone())
            .body(body)?)
    }
    async fn send(&self, request: Request<Body>) -> Result<Response<Body>> {
        // Do not surface URLs, credential headers or arbitrary provider error bodies.
        self.client.request(request).await.map_err(|_| {
            anyhow::anyhow!("Bunny storage request failed; write outcome may be unknown")
        })
    }
    async fn timed<T>(&self, future: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        tokio::time::timeout(self.limits.request_timeout, future)
            .await
            .context("Bunny storage deadline exceeded; write outcome may be unknown")?
    }
    async fn verify_resource(&self) -> Result<()> {
        self.timed(async {
            let mut response = self
                .send(self.request(Method::GET, "jmodguard-evidence-identity", Body::empty())?)
                .await?;
            ensure!(
                response.status() == StatusCode::OK,
                "evidence resource marker unavailable"
            );
            let expected = format!("jmodguard-bunny-evidence-1\n{}\n", self.storage_id.as_str());
            let mut received = Vec::with_capacity(expected.len());
            while let Some(chunk) = response.body_mut().data().await {
                let chunk =
                    chunk.map_err(|_| anyhow::anyhow!("invalid evidence resource marker"))?;
                ensure!(
                    chunk.len() <= expected.len() - received.len(),
                    "invalid evidence resource marker"
                );
                received.extend_from_slice(&chunk);
            }
            ensure!(
                received == expected.as_bytes(),
                "evidence resource identity mismatch"
            );
            Ok(())
        })
        .await
    }
    fn buffer(&self) -> Result<Vec<u8>> {
        let mut buffer = Vec::new();
        buffer.try_reserve_exact(self.limits.buffer_bytes)?;
        buffer.resize(self.limits.buffer_bytes, 0);
        Ok(buffer)
    }
    async fn source_hash<R: AsyncRead + Unpin>(
        &self,
        source: &mut R,
        bytes: u64,
        mut sender: Option<hyper::body::Sender>,
    ) -> Result<String> {
        let mut buffer = self.buffer()?;
        let mut total = 0;
        let mut digest = Digest::new(&SHA256);
        loop {
            let wanted = (bytes - total).saturating_add(1).min(buffer.len() as u64) as usize;
            let n = source
                .read(&mut buffer[..wanted])
                .await
                .map_err(|_| anyhow::anyhow!("evidence source read failed"))?;
            if n == 0 {
                break;
            }
            ensure!(n as u64 <= bytes - total, "evidence exceeds reserved size");
            digest.update(&buffer[..n]);
            total += n as u64;
            if let Some(sender) = sender.as_mut() {
                sender
                    .send_data(Bytes::copy_from_slice(&buffer[..n]))
                    .await
                    .map_err(|_| anyhow::anyhow!("Bunny upload body interrupted"))?;
            }
        }
        ensure!(total == bytes, "evidence shorter than reserved size");
        Ok(digest_hex(digest.finish()))
    }
    async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(
            Self,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T>> + Send>>
        + Send
        + 'static,
    ) -> Result<T> {
        let permit = self
            .admission
            .clone()
            .try_acquire_owned()
            .context("Bunny evidence operation already running")?;
        let this = self.clone();
        // Keep the operation alive if its caller disappears. SQL fences PUT attempts
        // across independent handles/processes; this permit only bounds local admission.
        tokio::spawn(async move {
            let _permit = permit;
            operation(this).await
        })
        .await?
    }
    async fn job(&self, store: &GuildDataStore, t: &EvidenceTarget) -> Result<EvidenceJob> {
        let job = store
            .evidence_transfer_job(t.scope.clone(), t.case_id.clone(), t.evidence_id.clone())
            .await?;
        ensure!(
            job.storage_id == self.storage_id,
            "evidence storage alias mismatch"
        );
        Ok(job)
    }
    async fn progress(&self, store: &GuildDataStore, t: &EvidenceTarget) -> Result<BunnyTransfer> {
        let progress = store
            .bunny_transfer(t.scope.clone(), t.case_id.clone(), t.evidence_id.clone())
            .await?;
        ensure!(
            progress.binding == self.base,
            "evidence storage binding changed"
        );
        ensure!(
            progress.accepted,
            "Bunny PUT outcome unresolved; provider confirmation required"
        );
        Ok(progress)
    }
    async fn verify_object(&self, job: &EvidenceJob, sha256: &str) -> Result<()> {
        self.timed(async {
            let mut response = self
                .send(self.request(Method::GET, &job.object_key, Body::empty())?)
                .await?;
            ensure!(
                response.status() == StatusCode::OK,
                "stored evidence unavailable for verification"
            );
            let mut total = 0u64;
            let mut digest = Digest::new(&SHA256);
            while let Some(chunk) = response.body_mut().data().await {
                let chunk =
                    chunk.map_err(|_| anyhow::anyhow!("evidence verification interrupted"))?;
                ensure!(
                    chunk.len() as u64 <= job.reserved_bytes - total,
                    "stored evidence exceeds reservation"
                );
                total += chunk.len() as u64;
                digest.update(&chunk);
            }
            ensure!(
                total == job.reserved_bytes && digest_hex(digest.finish()) == sha256,
                "stored evidence checksum or size mismatch"
            );
            Ok(())
        })
        .await
    }

    pub async fn read_available(&self, job: &EvidenceJob, expected_sha: &str) -> Result<Vec<u8>> {
        ensure!(
            job.storage_id == self.storage_id,
            "evidence storage alias mismatch"
        );
        self.timed(async {
            let mut response = self
                .send(self.request(Method::GET, &job.object_key, Body::empty())?)
                .await?;
            ensure!(
                response.status() == StatusCode::OK,
                "stored evidence unavailable"
            );
            let mut received = Vec::new();
            let mut digest = Digest::new(&SHA256);
            while let Some(chunk) = response.body_mut().data().await {
                let chunk = chunk.map_err(|_| anyhow::anyhow!("evidence read interrupted"))?;
                ensure!(
                    received.len() as u64 + chunk.len() as u64 <= job.reserved_bytes,
                    "stored evidence exceeds reservation"
                );
                digest.update(&chunk);
                received.extend_from_slice(&chunk);
            }
            ensure!(
                received.len() as u64 == job.reserved_bytes
                    && digest_hex(digest.finish()) == expected_sha,
                "stored evidence checksum or size mismatch"
            );
            Ok(received)
        })
        .await
    }

    pub async fn capture<R: AsyncRead + AsyncSeek + Unpin + Send + 'static>(
        &self,
        store: GuildDataStore,
        upload: EvidenceUpload,
        mut source: R,
    ) -> Result<EvidenceState> {
        ensure!(
            upload.request.storage_id == self.storage_id,
            "evidence storage alias mismatch"
        );
        // Enforce trusted admission bounds before scanning a caller's source.
        upload.limits.validate()?;
        ensure!(
            upload.request.bytes > 0
                && upload.request.bytes <= upload.limits.max_file_bytes
                && upload.request.bytes <= upload.limits.max_case_bytes,
            "evidence exceeds admission limits"
        );
        self.run(move |this| {
            Box::pin(async move {
                ensure!(
                    store
                        .find_evidence_transfer_job(
                            upload.target.scope.clone(),
                            upload.target.case_id.clone(),
                            upload.target.evidence_id.clone()
                        )
                        .await?
                        .is_none(),
                    "existing reservation requires reconciliation, never repeated PUT"
                );
                this.verify_resource().await?;
                let hash = this
                    .timed(async {
                        source
                            .seek(SeekFrom::Start(0))
                            .await
                            .map_err(|_| anyhow::anyhow!("evidence source seek failed"))?;
                        let hash = this
                            .source_hash(&mut source, upload.request.bytes, None)
                            .await?;
                        source
                            .seek(SeekFrom::Start(0))
                            .await
                            .map_err(|_| anyhow::anyhow!("evidence source seek failed"))?;
                        Ok(hash)
                    })
                    .await?;
                let t = upload.target;
                let reserved = store
                    .reserve_case_evidence(
                        t.scope.clone(),
                        t.case_id.clone(),
                        t.evidence_id.clone(),
                        upload.request,
                        upload.limits,
                    )
                    .await?;
                ensure!(
                    reserved.created,
                    "existing reservation requires reconciliation, never repeated PUT"
                );
                store
                    .start_bunny_transfer(
                        t.scope.clone(),
                        t.case_id.clone(),
                        t.evidence_id.clone(),
                        this.base.clone(),
                        hash.clone(),
                    )
                    .await?;
                let (sender, body) = Body::channel();
                let mut request = this.request(Method::PUT, &reserved.job.object_key, body)?;
                request.headers_mut().insert(
                    CONTENT_LENGTH,
                    HeaderValue::from(reserved.job.reserved_bytes),
                );
                request.headers_mut().insert(
                    CONTENT_TYPE,
                    HeaderValue::from_static("application/octet-stream"),
                );
                request.headers_mut().insert(
                    "Checksum",
                    HeaderValue::from_str(&hash.to_ascii_uppercase())?,
                );
                this.timed(async {
                    let (sent, response) = tokio::join!(
                        this.source_hash(&mut source, reserved.job.reserved_bytes, Some(sender)),
                        this.send(request)
                    );
                    ensure!(
                        sent? == hash,
                        "evidence changed during upload; outcome unresolved"
                    );
                    ensure!(
                        response?.status() == StatusCode::CREATED,
                        "Bunny PUT was not acknowledged; outcome unresolved"
                    );
                    Ok(())
                })
                .await?;
                store
                    .accept_bunny_transfer(
                        t.scope.clone(),
                        t.case_id.clone(),
                        t.evidence_id.clone(),
                    )
                    .await?;
                this.finish(&store, &t).await
            })
        })
        .await
    }
    async fn finish(&self, store: &GuildDataStore, t: &EvidenceTarget) -> Result<EvidenceState> {
        let job = self.job(store, t).await?;
        let progress = self.progress(store, t).await?;
        if job.state != EvidenceState::Capturing {
            return Ok(job.state);
        }
        let capture = if job.cancelled {
            EvidenceCapture::Discarded
        } else {
            let hash = progress
                .sha256
                .context("missing expected evidence checksum")?;
            self.verify_object(&job, &hash).await?;
            EvidenceCapture::Stored {
                bytes: job.reserved_bytes,
                sha256: hash,
            }
        };
        store
            .settle_evidence_capture(
                t.scope.clone(),
                t.case_id.clone(),
                t.evidence_id.clone(),
                capture,
            )
            .await
    }
    /// Only a durable 201 receipt allows recovery. Missing/uncertain PUT receipts remain
    /// blocked even if a GET returns 404 or happens to return matching bytes.
    pub async fn reconcile(
        &self,
        store: GuildDataStore,
        t: EvidenceTarget,
    ) -> Result<EvidenceState> {
        self.run(move |this| {
            Box::pin(async move {
                this.job(&store, &t).await?;
                this.progress(&store, &t).await?;
                this.verify_resource().await?;
                this.finish(&store, &t).await
            })
        })
        .await
    }
    pub async fn cleanup(&self, store: GuildDataStore, t: EvidenceTarget) -> Result<bool> {
        self.run(move |this| {
            Box::pin(async move {
                let mut job = this.job(&store, &t).await?;
                this.progress(&store, &t).await?;
                this.verify_resource().await?;
                if job.state == EvidenceState::Capturing && job.cancelled {
                    job.state = store
                        .settle_evidence_capture(
                            t.scope.clone(),
                            t.case_id.clone(),
                            t.evidence_id.clone(),
                            EvidenceCapture::Discarded,
                        )
                        .await?;
                }
                ensure!(
                    matches!(
                        job.state,
                        EvidenceState::CleanupPending | EvidenceState::Erased
                    ),
                    "evidence is not ready for cleanup"
                );
                this.timed(async {
                    let response = this
                        .send(this.request(Method::DELETE, &job.object_key, Body::empty())?)
                        .await?;
                    ensure!(
                        matches!(response.status(), StatusCode::OK | StatusCode::NOT_FOUND),
                        "Bunny deletion not acknowledged"
                    );
                    let response = this
                        .send(this.request(Method::GET, &job.object_key, Body::empty())?)
                        .await?;
                    ensure!(
                        response.status() == StatusCode::NOT_FOUND,
                        "Bunny evidence absence not verified"
                    );
                    Ok(())
                })
                .await?;
                store
                    .confirm_evidence_deleted(t.scope, t.case_id, t.evidence_id)
                    .await
            })
        })
        .await
    }
}
#[cfg(test)]
mod tests;
