//! Conductor runtime state: control store, HTTP client and worker handles.
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::settings::ControlSettings;
use crate::store::{ControlStore, SessionRow, now_ms};

pub struct Conductor {
    pub cfg: Config,
    pub store: Option<ControlStore>,
    pub http: reqwest::Client,
    pub started: Instant,
}

#[derive(Clone)]
pub struct Operator {
    pub session_id: String,
    pub operator_id: String,
}

#[derive(Serialize, Deserialize)]
struct DiscordUser {
    id: String,
    username: String,
    global_name: Option<String>,
}

impl Conductor {
    pub async fn open(cfg: Config) -> Result<Arc<Self>> {
        Self::start(cfg, true).await
    }

    pub async fn start(cfg: Config, spawn_workers: bool) -> Result<Arc<Self>> {
        let http = reqwest::Client::builder()
            .user_agent("jModGuard-conductor")
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("failed to build conductor HTTP client")?;
        let store = if let Some(path) = &cfg.db_path {
            Some(ControlStore::open(path).await?)
        } else {
            None
        };
        let conductor = Arc::new(Self {
            cfg,
            store,
            http,
            started: Instant::now(),
        });
        if spawn_workers && conductor.store.is_some() {
            let worker = Arc::clone(&conductor);
            tokio::spawn(async move { worker.job_loop().await });
            let sampler = Arc::clone(&conductor);
            tokio::spawn(async move { sampler.telemetry_loop().await });
        }
        Ok(conductor)
    }

    pub fn operations_available(&self) -> bool {
        self.cfg.operations_configured() && self.store.is_some()
    }

    pub fn store(&self) -> Result<&ControlStore> {
        self.store
            .as_ref()
            .context("control store is not configured")
    }

    pub async fn settings(&self) -> Result<ControlSettings> {
        let (mut settings, _, _) = self.store()?.load_settings().await?;
        if settings.bot_health_url.is_empty() {
            settings.bot_health_url = self.cfg.bot_health_url.clone();
        }
        if settings.api_health_url.is_empty() {
            settings.api_health_url = self.cfg.api_health_url.clone();
        }
        settings.validate()?;
        Ok(settings)
    }

    pub fn authorize_url(&self, state: &str) -> String {
        let redirect_uri = self.cfg.redirect_uri();
        let redirect = percent_encoding::utf8_percent_encode(
            &redirect_uri,
            percent_encoding::NON_ALPHANUMERIC,
        );
        format!(
            "https://discord.com/oauth2/authorize?response_type=code&client_id={}&redirect_uri={}&scope=identify&state={}",
            self.cfg.oauth_client_id, redirect, state
        )
    }

    pub async fn exchange_code(&self, code: &str) -> Result<String> {
        let token_url = if self.cfg.oauth_token_url.is_empty() {
            "https://discord.com/api/oauth2/token".to_string()
        } else {
            self.cfg.oauth_token_url.clone()
        };
        let redirect_uri = self.cfg.redirect_uri();
        let response = self
            .http
            .post(&token_url)
            .form(&[
                ("client_id", self.cfg.oauth_client_id.as_str()),
                ("client_secret", self.cfg.oauth_client_secret.as_str()),
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect_uri.as_str()),
            ])
            .send()
            .await
            .context("conductor oauth token request failed")?;
        let status = response.status();
        let bytes = response.bytes().await?;
        anyhow::ensure!(
            status.is_success(),
            "discord oauth error {status}: {}",
            String::from_utf8_lossy(&bytes)
        );
        let token: serde_json::Value = serde_json::from_slice(&bytes)?;
        token
            .get("access_token")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .context("oauth token response missing access_token")
    }

    pub async fn fetch_user(&self, access_token: &str) -> Result<(String, String)> {
        let me_url = if self.cfg.oauth_me_url.is_empty() {
            "https://discord.com/api/users/@me".to_string()
        } else {
            self.cfg.oauth_me_url.clone()
        };
        let user: DiscordUser = self
            .http
            .get(&me_url)
            .bearer_auth(access_token)
            .send()
            .await
            .context("conductor discord identity request failed")?
            .error_for_status()
            .context("conductor discord identity request failed")?
            .json()
            .await?;
        Ok((user.id, user.username))
    }

    pub async fn complete_login(&self, code: &str) -> Result<String> {
        let token = self.exchange_code(code).await?;
        let (user_id, username) = self.fetch_user(&token).await?;
        let expected = self.cfg.owner_id.to_string();
        anyhow::ensure!(user_id == expected, "not the configured conductor operator");
        let store = self.store()?;
        if let Some(bound) = store.setup_operator_id().await? {
            anyhow::ensure!(
                bound == user_id,
                "conductor setup identity does not match this Discord user"
            );
        } else {
            store.stamp_setup_identity(&user_id).await?;
        }
        let session_id = store.create_session(&user_id).await?;
        store
            .audit(&user_id, "login", &format!("username={username}"))
            .await?;
        Ok(session_id)
    }

    pub async fn require_operator(&self, cookie_header: Option<&str>) -> Result<Operator> {
        let store = self.store()?;
        let session_id = parse_session_cookie(cookie_header).context("not authenticated")?;
        let session = store
            .load_session(&session_id)
            .await?
            .context("not authenticated")?;
        self.enforce_session(&session).await?;
        store.touch_session(&session.id).await?;
        Ok(Operator {
            session_id: session.id,
            operator_id: session.operator_id,
        })
    }

    async fn enforce_session(&self, session: &SessionRow) -> Result<()> {
        let settings = self.settings().await?;
        let now = now_ms();
        let idle_ms =
            i64::try_from(settings.session_idle_secs.saturating_mul(1000)).unwrap_or(i64::MAX);
        let absolute_ms =
            i64::try_from(settings.session_absolute_secs.saturating_mul(1000)).unwrap_or(i64::MAX);
        anyhow::ensure!(
            now.saturating_sub(session.last_seen_ms) <= idle_ms,
            "session expired"
        );
        anyhow::ensure!(
            now.saturating_sub(session.created_ms) <= absolute_ms,
            "session expired"
        );
        let bound = self
            .store()?
            .setup_operator_id()
            .await?
            .unwrap_or_else(|| self.cfg.owner_id.to_string());
        anyhow::ensure!(
            session.operator_id == bound && session.operator_id == self.cfg.owner_id.to_string(),
            "not the configured conductor operator"
        );
        Ok(())
    }

    pub async fn probe(&self, url: &str) -> serde_json::Value {
        if url.is_empty() {
            return serde_json::json!({ "configured": false });
        }
        match self.http.get(url).send().await {
            Ok(response) => serde_json::json!({
                "configured": true,
                "url": url,
                "status": response.status().as_u16(),
                "ok": response.status().is_success(),
            }),
            Err(error) => serde_json::json!({
                "configured": true,
                "url": url,
                "ok": false,
                "error": error.to_string(),
            }),
        }
    }

    pub fn resource_sample(&self, kind: &str) -> serde_json::Value {
        serde_json::json!({
            "kind": kind,
            "uptime_secs": self.started.elapsed().as_secs(),
            "rss_bytes": process_rss_bytes(),
            "note": "RSS is observed process memory, not a configured limit.",
        })
    }

    async fn job_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            if let Err(error) = self.tick_job().await {
                tracing::warn!("conductor job worker: {error:#}");
            }
        }
    }

    pub async fn tick_job(&self) -> Result<()> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let Some(job) = store.claim_job().await? else {
            return Ok(());
        };
        let result = self.run_job(&job.kind, &job.payload_json).await;
        match result {
            Ok(()) => store.finish_job(&job.id, true, None).await?,
            Err(error) => {
                let message = format!("{error:#}");
                store.finish_job(&job.id, false, Some(&message)).await?;
            }
        }
        Ok(())
    }

    async fn run_job(&self, kind: &str, payload_json: &str) -> Result<()> {
        match kind {
            "notify_test" | "notify_event" => {
                crate::notify::deliver(self, kind, payload_json).await
            }
            "compact_telemetry" => {
                let settings = self.settings().await?;
                let oldest = now_ms()
                    - i64::try_from(settings.telemetry_history_hours.saturating_mul(3_600_000))
                        .unwrap_or(i64::MAX);
                self.store()?
                    .prune_telemetry(
                        oldest,
                        i64::try_from(settings.telemetry_max_rows).unwrap_or(4096),
                    )
                    .await
            }
            other => anyhow::bail!("unsupported job kind {other}"),
        }
    }

    async fn telemetry_loop(self: Arc<Self>) {
        loop {
            let settings = self.settings().await.unwrap_or_default();
            let live = match &self.store {
                Some(store) => store.has_live_lease().await.unwrap_or(false),
                None => false,
            };
            let wait = if live {
                settings.telemetry_live_secs
            } else {
                settings.telemetry_background_secs
            };
            tokio::time::sleep(Duration::from_secs(wait.max(2))).await;
            if let Err(error) = self.capture_sample(live).await {
                tracing::warn!("conductor telemetry: {error:#}");
            }
        }
    }

    async fn capture_sample(&self, live: bool) -> Result<()> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let settings = self.settings().await?;
        let kind = if live { "live" } else { "background" };
        let mut sample = self.resource_sample(kind);
        if let Some(object) = sample.as_object_mut() {
            object.insert("bot".into(), self.probe(&settings.bot_health_url).await);
            object.insert("api".into(), self.probe(&settings.api_health_url).await);
            object.insert(
                "jobs".into(),
                serde_json::json!({ "recent": store.list_jobs(8).await?.len() }),
            );
        }
        store.insert_telemetry(kind, &sample).await?;
        let oldest = now_ms()
            - i64::try_from(settings.telemetry_history_hours.saturating_mul(3_600_000))
                .unwrap_or(i64::MAX);
        store
            .prune_telemetry(
                oldest,
                i64::try_from(settings.telemetry_max_rows).unwrap_or(4096),
            )
            .await?;
        Ok(())
    }
}

pub fn parse_session_cookie(header: Option<&str>) -> Option<String> {
    const NAME: &str = "jmodguard_conductor_session=";
    for part in header.unwrap_or("").split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix(NAME)
            && !value.is_empty()
        {
            return Some(value.to_string());
        }
    }
    None
}

fn process_rss_bytes() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb.saturating_mul(1024));
        }
    }
    None
}
