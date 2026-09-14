//! Conductor runtime state: control store, HTTP client and worker handles.
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::backup::BackupZone;
use crate::config::Config;
use crate::settings::ControlSettings;
use crate::store::{ControlStore, SessionRow, now_ms};

const BACKUP_CAPTURE_MAX_ATTEMPTS: i64 = 3;

pub struct Conductor {
    pub cfg: Config,
    pub store: Option<ControlStore>,
    pub backup: Option<BackupZone>,
    pub http: reqwest::Client,
    pub started: Instant,
    pub(crate) restore_passwords: Mutex<HashMap<String, String>>,
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
        let backup = if let Some(path) = &cfg.backup_root {
            let zone = BackupZone::open(path)?;
            if let Some(store) = &store {
                if let Err(error) = crate::backup::recover_catalog_archive_bytes(&zone, store).await
                {
                    tracing::warn!("could not recover staged backups: {error:#}");
                }
                zone.prune_untracked_complete(&store.complete_archive_ids().await?)?;
                if let Ok((settings, _, _)) = store.load_settings().await {
                    let keep =
                        i64::try_from(settings.backup_retain_scheduled).unwrap_or(24);
                    if let Ok(removed) = store.prune_quarantined_archives(keep).await {
                        for id in removed {
                            zone.remove_quarantine(&id);
                        }
                    }
                }
            }
            zone.prune_incomplete()?;
            Some(zone)
        } else {
            None
        };
        if let Some(core) = cfg.capture_stores.iter().find(|s| s.role == "core")
            && core.path.exists()
        {
            // Interrupted restore jobs fail closed; a leftover fence must not
            // keep the live database write-blocked after restart.
            if let Err(error) = crate::backup::set_restore_fence(&core.path, false).await {
                tracing::warn!("could not clear restore fence: {error:#}");
            }
        }
        let conductor = Arc::new(Self {
            cfg,
            store,
            backup,
            http,
            started: Instant::now(),
            restore_passwords: Mutex::new(HashMap::new()),
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

    pub fn backup_available(&self) -> bool {
        self.operations_available() && self.backup.is_some()
    }

    pub fn capture_available(&self) -> bool {
        self.backup_available() && self.cfg.capture_core_ready()
    }

    pub fn backup_zone(&self) -> Result<&BackupZone> {
        self.backup
            .as_ref()
            .context("backup zone is not configured")
    }

    pub fn store(&self) -> Result<&ControlStore> {
        self.store
            .as_ref()
            .context("control store is not configured")
    }

    pub fn remember_restore_password(&self, job_id: &str, password: Option<String>) -> Result<()> {
        if let Some(password) = password {
            self.restore_passwords
                .lock()
                .map_err(|_| anyhow::anyhow!("restore password store is unavailable"))?
                .insert(job_id.to_string(), password);
        }
        Ok(())
    }

    pub fn forget_restore_password(&self, job_id: &str) {
        if let Ok(mut passwords) = self.restore_passwords.lock() {
            passwords.remove(job_id);
        }
    }

    fn restore_password(&self, job_id: &str, required: bool) -> Result<Option<String>> {
        let password = self
            .restore_passwords
            .lock()
            .map_err(|_| anyhow::anyhow!("restore password store is unavailable"))?
            .get(job_id)
            .cloned();
        anyhow::ensure!(
            !required || password.is_some(),
            "restore password is no longer available; submit the restore again"
        );
        Ok(password)
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

    pub async fn complete_login(&self, code: &str) -> Result<(String, u64)> {
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
        let absolute_secs = self.settings().await?.session_absolute_secs;
        let session_id = store.create_session(&user_id).await?;
        store
            .audit(&user_id, "login", &format!("username={username}"))
            .await?;
        Ok((session_id, absolute_secs))
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
        if let Err(error) = self.maybe_reconcile_restore_fence().await {
            tracing::warn!("restore fence reconciliation: {error:#}");
        }
        self.maybe_schedule_backup().await?;
        self.maybe_sanitize_backups().await?;
        let Some(job) = store.claim_job().await? else {
            return Ok(());
        };
        let result = self.run_job(&job.id, &job.kind, &job.payload_json).await;
        match result {
            Ok(()) => {
                store.finish_job(&job.id, true, None).await?;
                if job.kind == "backup_restore" {
                    self.forget_restore_password(&job.id);
                }
            }
            Err(error) => {
                let message = format!("{error:#}");
                store.finish_job(&job.id, false, Some(&message)).await?;
                if job.kind == "backup_restore" && job.attempts >= job.max_attempts {
                    self.forget_restore_password(&job.id);
                }
                if job.kind.starts_with("backup_") {
                    let notifications_enabled = self.settings().await.is_ok_and(|settings| {
                        settings.notify_enabled
                            && settings
                                .notify_categories
                                .iter()
                                .any(|category| category == "backup")
                    });
                    if notifications_enabled {
                        let _ = store
                            .insert_job(
                                "notify_event",
                                &serde_json::json!({
                                    "category": "backup",
                                    "message": format!("Backup job failed: {message}"),
                                    "severity": "error",
                                }),
                                3,
                            )
                            .await;
                    }
                }
            }
        }
        Ok(())
    }

    async fn maybe_reconcile_restore_fence(&self) -> Result<()> {
        if !self.backup_available() {
            return Ok(());
        }
        let store = self.store()?;
        if store.has_active_backup_job().await? {
            return Ok(());
        }
        let core = self
            .cfg
            .capture_stores
            .iter()
            .find(|store| store.role == "core" && store.path.exists());
        let Some(core) = core else {
            return Ok(());
        };
        if !crate::backup::restore_fence_active(&core.path).await? {
            return Ok(());
        }
        crate::backup::clear_restore_fence(&core.path).await?;
        Ok(())
    }

    async fn maybe_schedule_backup(&self) -> Result<()> {
        if !self.capture_available() {
            return Ok(());
        }
        let store = self.store()?;
        let settings = self.settings().await?;
        if !settings.backup_enabled {
            return Ok(());
        }
        if store.has_active_backup_job().await? {
            return Ok(());
        }
        let last_attempt_ms = store.last_backup_schedule_activity_ms().await?;
        if !crate::backup::overdue(&settings, last_attempt_ms) {
            return Ok(());
        }
        let _ = store
            .insert_backup_job_if_idle(
                "backup_capture",
                &serde_json::json!({ "kind": "scheduled" }),
                BACKUP_CAPTURE_MAX_ATTEMPTS,
            )
            .await?;
        Ok(())
    }

    async fn maybe_sanitize_backups(&self) -> Result<()> {
        if !self.backup_available() {
            return Ok(());
        }
        let store = self.store()?;
        if store.has_active_backup_job().await? || store.has_failed_backup_sanitize_job().await? {
            return Ok(());
        }
        let intents = crate::backup::load_cleanup_intents(&self.cfg).await?;
        if intents.ids.is_empty() {
            return Ok(());
        }
        let _ = store
            .insert_backup_job_if_idle("backup_sanitize", &serde_json::json!({}), 3)
            .await?;
        Ok(())
    }

    async fn run_job(&self, job_id: &str, kind: &str, payload_json: &str) -> Result<()> {
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
            "backup_capture" => self.run_backup_capture(payload_json).await,
            "backup_restore" => self.run_backup_restore(job_id, payload_json).await,
            "backup_sanitize" => self.run_backup_sanitize().await,
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

    async fn run_backup_capture(&self, payload_json: &str) -> Result<()> {
        let payload: serde_json::Value = serde_json::from_str(payload_json).unwrap_or_default();
        let kind = payload
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("scheduled");
        let settings = self.settings().await?;
        let row = crate::backup::capture(
            &self.cfg,
            self.store()?,
            self.backup_zone()?,
            kind,
            &settings,
        )
        .await?;
        let scheduled_keep = i64::try_from(settings.backup_retain_scheduled).unwrap_or(24);
        let removed = self
            .store()?
            .prune_old_archives(
                scheduled_keep,
                i64::try_from(settings.backup_retain_daily).unwrap_or(7),
                i64::try_from(settings.backup_retain_checkpoints).unwrap_or(3),
            )
            .await?;
        let removed_quarantine = self
            .store()?
            .prune_quarantined_archives(scheduled_keep)
            .await?;
        if let Ok(zone) = self.backup_zone() {
            for id in removed {
                let _ = std::fs::remove_file(zone.complete_path(&id));
            }
            for id in removed_quarantine {
                zone.remove_quarantine(&id);
            }
        }
        let _ = row;
        Ok(())
    }

    async fn run_backup_restore(&self, job_id: &str, payload_json: &str) -> Result<()> {
        let payload: serde_json::Value = serde_json::from_str(payload_json)?;
        let id = payload
            .get("id")
            .and_then(|v| v.as_str())
            .context("restore job missing archive id")?;
        let password_required = payload
            .get("password_required")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let password = self.restore_password(job_id, password_required)?;
        let acknowledge = payload
            .get("acknowledge_historical")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let current_state_available =
            crate::backup::current_policy_state_available(&self.cfg).await?;
        let core = if current_state_available {
            self.cfg.capture_stores.iter().find(|s| s.role == "core")
        } else {
            None
        };
        if let Some(core) = core {
            crate::backup::set_restore_fence(&core.path, true)
                .await
                .context("failed to raise restore fence")?;
        }
        // Load removal policy only after the fence commits. Its write takes the
        // SQLite primary writer lock, so all older application mutations have
        // completed and newer ones will observe the fence before this snapshot.
        let result: Result<()> = async {
            let intents = crate::backup::load_cleanup_intents(&self.cfg).await?;
            let request = crate::backup::RestoreRequest {
                acknowledge_historical: acknowledge,
                current_state_available,
                removed_guilds: intents.guilds,
                removed_cases: intents.cases,
                removed_evidence: intents.evidence,
                removed_object_keys: intents.object_keys,
            };
            crate::backup::restore_archive(
                &self.cfg,
                self.store()?,
                self.backup_zone()?,
                id,
                password.as_deref(),
                request,
                &self.settings().await?,
            )
            .await?;
            Ok(())
        }
        .await;
        let restore_ok = result.is_ok();
        let clear = match core {
            Some(core) => crate::backup::clear_restore_fence(&core.path).await,
            None => Ok(()),
        };
        if restore_ok && let Err(error) = &clear {
            let message = format!("Restore completed; restore fence was not cleared: {error:#}");
            let notifications_enabled = self.settings().await.is_ok_and(|settings| {
                settings.notify_enabled
                    && settings
                        .notify_categories
                        .iter()
                        .any(|category| category == "backup")
            });
            if notifications_enabled && let Ok(store) = self.store() {
                let _ = store
                    .insert_job(
                        "notify_event",
                        &serde_json::json!({
                            "category": "backup",
                            "message": message,
                            "severity": "error",
                        }),
                        3,
                    )
                    .await;
            }
        }
        crate::backup::restore_job_result(result, clear)
    }

    async fn run_backup_sanitize(&self) -> Result<()> {
        let intents = crate::backup::load_cleanup_intents(&self.cfg).await?;
        let request = crate::backup::RestoreRequest {
            acknowledge_historical: true,
            current_state_available: true,
            removed_guilds: intents.guilds,
            removed_cases: intents.cases,
            removed_evidence: intents.evidence,
            removed_object_keys: intents.object_keys,
        };
        crate::backup::sanitize_complete_archives(
            &self.cfg,
            self.store()?,
            self.backup_zone()?,
            &self.settings().await?,
            &request,
        )
        .await?;
        crate::backup::clear_cleanup_intents(&self.cfg, &intents.ids).await
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
