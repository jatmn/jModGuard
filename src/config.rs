use anyhow::{Context as _, Result};
use serde::Deserialize;
use std::env;
use std::path::PathBuf;

use crate::db::DbOpenOptions;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct Config {
    pub discord: DiscordConfig,
    pub db: DbConfig,
    pub web: WebConfig,
    pub bot: BotConfig,
    pub log: LogConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct DiscordConfig {
    pub token: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DbConfig {
    pub path: PathBuf,
    /// Remote libSQL / Bunny Database URL. Empty = local SQLite file at `path`.
    pub url: String,
    /// Access token for remote DB (never send to the browser).
    pub token: String,
    pub pool_size: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    pub bind: String,
    pub base_url: String,
    pub oauth_client_id: String,
    pub oauth_client_secret: String,
    /// Extra browser origins allowed to call the API with credentials (CDN SPA).
    /// Empty = same-origin only (no CORS layer). Env: `JMODGUARD_CORS_ORIGINS`
    /// (comma-separated).
    pub cors_origins: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BotConfig {
    pub default_prefix: String,
    pub owner_id: u64,
    pub retention_days: u32,
    pub status: String,
    /// How often the bot polls durable `config_version` for cross-process cache invalidation.
    pub config_poll_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    pub level: String,
    pub directory: PathBuf,
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("jmodguard.db"),
            url: String::new(),
            token: String::new(),
            pool_size: 8,
        }
    }
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080".into(),
            base_url: "http://localhost:8080".into(),
            oauth_client_id: String::new(),
            oauth_client_secret: String::new(),
            cors_origins: Vec::new(),
        }
    }
}

impl Default for BotConfig {
    fn default() -> Self {
        Self {
            default_prefix: "!".into(),
            owner_id: 0,
            retention_days: 7,
            status: "moderating".into(),
            config_poll_secs: 5,
        }
    }
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            directory: PathBuf::from("logs"),
        }
    }
}

impl Config {
    /// Loads config from `config.toml` (path from `--config <path>` or env
    /// `JMODGUARD_CONFIG`, else default in CWD). Environment variables
    /// (JMODGUARD_DISCORD_TOKEN, JMODGUARD_WEB_BASE_URL, ...) override values.
    pub fn load() -> Result<Self> {
        let path = env::var("JMODGUARD_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("config.toml"));

        let mut cfg: Config = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read config file {}", path.display()))?;
            toml::from_str(&raw)
                .with_context(|| format!("failed to parse config file {}", path.display()))?
        } else {
            tracing::warn!("config.toml not found; using defaults. See config.example.toml.");
            Config::default()
        };

        cfg.apply_env();
        Ok(cfg)
    }

    /// Reject CORS values which would otherwise make tower-http panic while
    /// building a credentialed `AllowOrigin::list` layer.
    ///
    /// Call this from `jmodguard-api` only. The bot never serves HTTP, so a
    /// typo in `web.cors_origins` must not take down the Discord gateway.
    pub fn validate_cors(&self) -> Result<()> {
        for origin in &self.web.cors_origins {
            let origin = origin.trim();
            if origin.is_empty() {
                anyhow::bail!("web.cors_origins cannot contain an empty origin");
            }
            if origin == "*" {
                anyhow::bail!(
                    "web.cors_origins cannot contain `*`; credentialed CORS requires explicit origins"
                );
            }
            origin
                .parse::<reqwest::header::HeaderValue>()
                .with_context(|| format!("invalid web.cors_origins entry `{origin}`"))?;
        }
        Ok(())
    }

    /// Builds DB open options for this process (`migrate=true` for bot, `false` for API).
    pub fn db_open_options(&self, migrate: bool) -> DbOpenOptions {
        DbOpenOptions {
            path: self.db.path.clone(),
            url: self.db.url.clone(),
            token: self.db.token.clone(),
            pool_size: self.db.pool_size,
            migrate,
            default_prefix: self.bot.default_prefix.clone(),
            retention_days: self.bot.retention_days,
        }
    }

    fn apply_env(&mut self) {
        if let Ok(v) = env::var("JMODGUARD_DISCORD_TOKEN") {
            self.discord.token = v;
        }
        if let Ok(v) = env::var("JMODGUARD_DB_PATH") {
            self.db.path = PathBuf::from(v);
        }
        if let Ok(v) = env::var("JMODGUARD_DB_URL") {
            self.db.url = v;
        }
        if let Ok(v) = env::var("JMODGUARD_DB_TOKEN") {
            self.db.token = v;
        }
        if let Ok(v) = env::var("JMODGUARD_WEB_BIND") {
            self.web.bind = v;
        }
        if let Ok(v) = env::var("JMODGUARD_WEB_BASE_URL") {
            self.web.base_url = v;
        }
        if let Ok(v) = env::var("JMODGUARD_OAUTH_CLIENT_ID") {
            self.web.oauth_client_id = v;
        }
        if let Ok(v) = env::var("JMODGUARD_OAUTH_CLIENT_SECRET") {
            self.web.oauth_client_secret = v;
        }
        if let Ok(v) = env::var("JMODGUARD_CORS_ORIGINS") {
            self.web.cors_origins = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
        if let Ok(v) = env::var("JMODGUARD_OWNER_ID")
            && let Ok(id) = v.parse()
        {
            self.bot.owner_id = id;
        }
        if let Ok(v) = env::var("JMODGUARD_CONFIG_POLL_SECS")
            && let Ok(secs) = v.parse()
        {
            self.bot.config_poll_secs = secs;
        }
        if let Ok(v) = env::var("JMODGUARD_LOG_LEVEL") {
            self.log.level = v;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn rejects_wildcard_credentialed_cors_origin() {
        let mut cfg = Config::default();
        cfg.web.cors_origins = vec!["*".into()];
        let err = cfg.validate_cors().unwrap_err();
        assert!(err.to_string().contains("cannot contain `*`"));
    }
}
