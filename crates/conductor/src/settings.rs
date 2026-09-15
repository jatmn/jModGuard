//! Typed conductor-owned settings. Bot/API process knobs stay pending here.
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::config::parse_http_url;

pub const SETTINGS_KEY: &str = "control";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ControlSettings {
    pub session_idle_secs: u64,
    pub session_absolute_secs: u64,
    pub telemetry_background_secs: u64,
    pub telemetry_live_secs: u64,
    pub telemetry_history_hours: u64,
    pub telemetry_max_rows: u64,
    pub lease_secs: u64,
    pub notify_enabled: bool,
    #[serde(default)]
    pub notify_webhook_url: String,
    #[serde(default)]
    pub notify_webhook_configured: bool,
    pub notify_categories: Vec<String>,
    pub notify_mentions: bool,
    pub notify_verbosity: String,
    pub notify_dedup_secs: u64,
    pub bot_health_url: String,
    pub api_health_url: String,
    #[serde(default)]
    pub backup_enabled: bool,
    #[serde(default = "default_backup_interval")]
    pub backup_interval_hours: u64,
    #[serde(default = "default_backup_retain_scheduled")]
    pub backup_retain_scheduled: u64,
    #[serde(default = "default_backup_retain_daily")]
    pub backup_retain_daily: u64,
    #[serde(default = "default_backup_retain_checkpoints")]
    pub backup_retain_checkpoints: u64,
    #[serde(default = "default_backup_max_bytes")]
    pub backup_max_archive_bytes: u64,
    #[serde(default = "default_discovery_channels")]
    pub discovery_channels: Vec<String>,
    #[serde(default = "default_discovery_interval")]
    pub discovery_interval_secs: u64,
}

impl Default for ControlSettings {
    fn default() -> Self {
        Self {
            session_idle_secs: 3600,
            session_absolute_secs: 86400,
            telemetry_background_secs: 60,
            telemetry_live_secs: 5,
            telemetry_history_hours: 48,
            telemetry_max_rows: 4096,
            lease_secs: 30,
            notify_enabled: false,
            notify_webhook_url: String::new(),
            notify_webhook_configured: false,
            notify_categories: vec![
                "health".into(),
                "jobs".into(),
                "login".into(),
                "backup".into(),
            ],
            notify_mentions: false,
            notify_verbosity: "errors".into(),
            notify_dedup_secs: 300,
            bot_health_url: String::new(),
            api_health_url: String::new(),
            backup_enabled: false,
            backup_interval_hours: 6,
            backup_retain_scheduled: 24,
            backup_retain_daily: 7,
            backup_retain_checkpoints: 3,
            backup_max_archive_bytes: 256 * 1024 * 1024,
            discovery_channels: default_discovery_channels(),
            discovery_interval_secs: default_discovery_interval(),
        }
    }
}

impl ControlSettings {
    pub fn validate(&mut self) -> Result<()> {
        ensure_range("session_idle_secs", self.session_idle_secs, 300, 86_400)?;
        ensure_range(
            "session_absolute_secs",
            self.session_absolute_secs,
            3_600,
            604_800,
        )?;
        anyhow::ensure!(
            self.session_absolute_secs >= self.session_idle_secs,
            "session_absolute_secs must be at least session_idle_secs"
        );
        ensure_range(
            "telemetry_background_secs",
            self.telemetry_background_secs,
            30,
            600,
        )?;
        ensure_range("telemetry_live_secs", self.telemetry_live_secs, 2, 30)?;
        ensure_range(
            "telemetry_history_hours",
            self.telemetry_history_hours,
            1,
            168,
        )?;
        ensure_range("telemetry_max_rows", self.telemetry_max_rows, 64, 8_192)?;
        ensure_range("lease_secs", self.lease_secs, 10, 120)?;
        ensure_range("notify_dedup_secs", self.notify_dedup_secs, 30, 86_400)?;
        anyhow::ensure!(
            self.notify_verbosity == "errors" || self.notify_verbosity == "all",
            "notify_verbosity must be errors or all"
        );
        self.notify_categories = unique_categories(&self.notify_categories)?;
        if !self.notify_webhook_url.is_empty() {
            self.notify_webhook_url =
                parse_http_url(&self.notify_webhook_url).context("notify_webhook_url")?;
            self.notify_webhook_configured = true;
        }
        if !self.bot_health_url.is_empty() {
            self.bot_health_url = parse_http_url(&self.bot_health_url).context("bot_health_url")?;
        }
        if !self.api_health_url.is_empty() {
            self.api_health_url = parse_http_url(&self.api_health_url).context("api_health_url")?;
        }
        ensure_range("backup_interval_hours", self.backup_interval_hours, 1, 24)?;
        ensure_range(
            "backup_retain_scheduled",
            self.backup_retain_scheduled,
            1,
            168,
        )?;
        ensure_range("backup_retain_daily", self.backup_retain_daily, 1, 30)?;
        ensure_range(
            "backup_retain_checkpoints",
            self.backup_retain_checkpoints,
            1,
            10,
        )?;
        ensure_range(
            "backup_max_archive_bytes",
            self.backup_max_archive_bytes,
            1_048_576,
            2 * 1024 * 1024 * 1024,
        )?;
        self.discovery_channels = unique_channels(&self.discovery_channels)?;
        ensure_range(
            "discovery_interval_secs",
            self.discovery_interval_secs,
            300,
            86_400,
        )?;
        Ok(())
    }

    pub fn public_view(&self) -> serde_json::Value {
        serde_json::json!({
            "session_idle_secs": { "value": self.session_idle_secs, "applied": true, "unit": "seconds" },
            "session_absolute_secs": { "value": self.session_absolute_secs, "applied": true, "unit": "seconds" },
            "telemetry_background_secs": { "value": self.telemetry_background_secs, "applied": true, "unit": "seconds" },
            "telemetry_live_secs": { "value": self.telemetry_live_secs, "applied": true, "unit": "seconds" },
            "telemetry_history_hours": { "value": self.telemetry_history_hours, "applied": true, "unit": "hours" },
            "telemetry_max_rows": { "value": self.telemetry_max_rows, "applied": true, "unit": "rows" },
            "lease_secs": { "value": self.lease_secs, "applied": true, "unit": "seconds" },
            "notify_enabled": { "value": self.notify_enabled, "applied": true },
            "notify_webhook_configured": { "value": self.notify_webhook_configured, "applied": true },
            "notify_categories": { "value": self.notify_categories, "applied": true },
            "notify_mentions": { "value": self.notify_mentions, "applied": true },
            "notify_verbosity": { "value": self.notify_verbosity, "applied": true },
            "notify_dedup_secs": { "value": self.notify_dedup_secs, "applied": true, "unit": "seconds" },
            "bot_health_url": { "value": self.bot_health_url, "applied": true },
            "api_health_url": { "value": self.api_health_url, "applied": true },
            "backup_enabled": { "value": self.backup_enabled, "applied": true },
            "backup_interval_hours": { "value": self.backup_interval_hours, "applied": true, "unit": "hours" },
            "backup_retain_scheduled": { "value": self.backup_retain_scheduled, "applied": true, "unit": "points" },
            "backup_retain_daily": { "value": self.backup_retain_daily, "applied": true, "unit": "points" },
            "backup_retain_checkpoints": { "value": self.backup_retain_checkpoints, "applied": true, "unit": "releases" },
            "backup_max_archive_bytes": { "value": self.backup_max_archive_bytes, "applied": true, "unit": "bytes" },
            "discovery_channels": { "value": self.discovery_channels, "applied": true },
            "discovery_interval_secs": { "value": self.discovery_interval_secs, "applied": true, "unit": "seconds" },
            "process_owned": {
                "image_spam": "Bot process configuration. Not applied by the conductor.",
                "log_discord_concurrency": "Bot process configuration. Not applied by the conductor.",
                "enrollment_repair": "Bot process configuration. Not applied by the conductor.",
            },
            "unavailable": [],
        })
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct SettingsPatch {
    pub session_idle_secs: Option<u64>,
    pub session_absolute_secs: Option<u64>,
    pub telemetry_background_secs: Option<u64>,
    pub telemetry_live_secs: Option<u64>,
    pub telemetry_history_hours: Option<u64>,
    pub telemetry_max_rows: Option<u64>,
    pub lease_secs: Option<u64>,
    pub notify_enabled: Option<bool>,
    pub notify_webhook_url: Option<String>,
    pub notify_categories: Option<Vec<String>>,
    pub notify_mentions: Option<bool>,
    pub notify_verbosity: Option<String>,
    pub notify_dedup_secs: Option<u64>,
    pub bot_health_url: Option<String>,
    pub api_health_url: Option<String>,
    pub backup_enabled: Option<bool>,
    pub backup_interval_hours: Option<u64>,
    pub backup_retain_scheduled: Option<u64>,
    pub backup_retain_daily: Option<u64>,
    pub backup_retain_checkpoints: Option<u64>,
    pub backup_max_archive_bytes: Option<u64>,
    pub discovery_channels: Option<Vec<String>>,
    pub discovery_interval_secs: Option<u64>,
    pub revision: Option<i64>,
}

impl ControlSettings {
    pub fn apply_patch(&mut self, patch: SettingsPatch) -> Result<i64> {
        if let Some(v) = patch.session_idle_secs {
            self.session_idle_secs = v;
        }
        if let Some(v) = patch.session_absolute_secs {
            self.session_absolute_secs = v;
        }
        if let Some(v) = patch.telemetry_background_secs {
            self.telemetry_background_secs = v;
        }
        if let Some(v) = patch.telemetry_live_secs {
            self.telemetry_live_secs = v;
        }
        if let Some(v) = patch.telemetry_history_hours {
            self.telemetry_history_hours = v;
        }
        if let Some(v) = patch.telemetry_max_rows {
            self.telemetry_max_rows = v;
        }
        if let Some(v) = patch.lease_secs {
            self.lease_secs = v;
        }
        if let Some(v) = patch.notify_enabled {
            self.notify_enabled = v;
        }
        if let Some(v) = patch.notify_webhook_url {
            let trimmed = v.trim().to_string();
            if trimmed.is_empty() {
                self.notify_webhook_url.clear();
                self.notify_webhook_configured = false;
            } else {
                self.notify_webhook_url = trimmed;
            }
        }
        if let Some(v) = patch.notify_categories {
            self.notify_categories = v;
        }
        if let Some(v) = patch.notify_mentions {
            self.notify_mentions = v;
        }
        if let Some(v) = patch.notify_verbosity {
            self.notify_verbosity = v;
        }
        if let Some(v) = patch.notify_dedup_secs {
            self.notify_dedup_secs = v;
        }
        if let Some(v) = patch.bot_health_url {
            self.bot_health_url = v.trim().to_string();
        }
        if let Some(v) = patch.api_health_url {
            self.api_health_url = v.trim().to_string();
        }
        if let Some(v) = patch.backup_enabled {
            self.backup_enabled = v;
        }
        if let Some(v) = patch.backup_interval_hours {
            self.backup_interval_hours = v;
        }
        if let Some(v) = patch.backup_retain_scheduled {
            self.backup_retain_scheduled = v;
        }
        if let Some(v) = patch.backup_retain_daily {
            self.backup_retain_daily = v;
        }
        if let Some(v) = patch.backup_retain_checkpoints {
            self.backup_retain_checkpoints = v;
        }
        if let Some(v) = patch.backup_max_archive_bytes {
            self.backup_max_archive_bytes = v;
        }
        if let Some(v) = patch.discovery_channels {
            self.discovery_channels = v;
        }
        if let Some(v) = patch.discovery_interval_secs {
            self.discovery_interval_secs = v;
        }
        self.validate()?;
        Ok(patch.revision.unwrap_or(0))
    }
}

fn ensure_range(name: &str, value: u64, min: u64, max: u64) -> Result<()> {
    anyhow::ensure!((min..=max).contains(&value), "{name} must be {min}..={max}");
    Ok(())
}

fn default_backup_interval() -> u64 {
    6
}
fn default_backup_retain_scheduled() -> u64 {
    24
}
fn default_backup_retain_daily() -> u64 {
    7
}
fn default_backup_retain_checkpoints() -> u64 {
    3
}
fn default_backup_max_bytes() -> u64 {
    256 * 1024 * 1024
}

fn default_discovery_channels() -> Vec<String> {
    vec!["stable".into()]
}

fn default_discovery_interval() -> u64 {
    3600
}

fn unique_categories(values: &[String]) -> Result<Vec<String>> {
    const ALLOWED: &[&str] = &["health", "jobs", "login", "settings", "backup", "updates"];
    let mut out = Vec::new();
    for value in values {
        let trimmed = value.trim();
        anyhow::ensure!(
            ALLOWED.contains(&trimmed),
            "unsupported notification category"
        );
        if !out.iter().any(|existing: &String| existing == trimmed) {
            out.push(trimmed.to_string());
        }
    }
    Ok(out)
}

fn unique_channels(values: &[String]) -> Result<Vec<String>> {
    const ALLOWED: &[&str] = &["stable", "nightly"];
    let mut out = Vec::new();
    for value in values {
        let trimmed = value.trim();
        anyhow::ensure!(ALLOWED.contains(&trimmed), "unsupported discovery channel");
        if !out.iter().any(|existing: &String| existing == trimmed) {
            out.push(trimmed.to_string());
        }
    }
    Ok(out)
}
