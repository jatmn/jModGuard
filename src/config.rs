use anyhow::{Context as _, Result};
use serde::Deserialize;
use std::env;
use std::path::PathBuf;

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
    pub pool_size: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    pub bind: String,
    pub base_url: String,
    pub oauth_client_id: String,
    pub oauth_client_secret: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BotConfig {
    pub default_prefix: String,
    pub owner_id: u64,
    pub retention_days: u32,
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    pub level: String,
    pub directory: PathBuf,
}



impl Default for DbConfig {
    fn default() -> Self {
        Self { path: PathBuf::from("jmodguard.db"), pool_size: 8 }
    }
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080".into(),
            base_url: "http://localhost:8080".into(),
            oauth_client_id: String::new(),
            oauth_client_secret: String::new(),
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
        }
    }
}

impl Default for LogConfig {
    fn default() -> Self {
        Self { level: "info".into(), directory: PathBuf::from("logs") }
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

    fn apply_env(&mut self) {
        if let Ok(v) = env::var("JMODGUARD_DISCORD_TOKEN") {
            self.discord.token = v;
        }
        if let Ok(v) = env::var("JMODGUARD_DB_PATH") {
            self.db.path = PathBuf::from(v);
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
        if let Ok(v) = env::var("JMODGUARD_OWNER_ID")
            && let Ok(id) = v.parse() {
                self.bot.owner_id = id;
            }
        if let Ok(v) = env::var("JMODGUARD_LOG_LEVEL") {
            self.log.level = v;
        }
    }
}
