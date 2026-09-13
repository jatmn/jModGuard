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
            notify_categories: vec!["health".into(), "jobs".into(), "login".into()],
            notify_mentions: false,
            notify_verbosity: "errors".into(),
            notify_dedup_secs: 300,
            bot_health_url: String::new(),
            api_health_url: String::new(),
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
            "process_owned": {
                "image_spam": "Bot process configuration. Not applied by the conductor.",
                "log_discord_concurrency": "Bot process configuration. Not applied by the conductor.",
                "enrollment_repair": "Bot process configuration. Not applied by the conductor.",
            },
            "unavailable": ["backups", "updates", "guild_moves"],
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
        self.validate()?;
        Ok(patch.revision.unwrap_or(0))
    }
}

fn ensure_range(name: &str, value: u64, min: u64, max: u64) -> Result<()> {
    anyhow::ensure!((min..=max).contains(&value), "{name} must be {min}..={max}");
    Ok(())
}

fn unique_categories(values: &[String]) -> Result<Vec<String>> {
    const ALLOWED: &[&str] = &["health", "jobs", "login", "settings"];
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
