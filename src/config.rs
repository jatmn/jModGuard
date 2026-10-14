use anyhow::{Context as _, Result};
use serde::Deserialize;
use std::env;
use std::path::PathBuf;

use crate::db::{DbOpenOptions, MAX_RETENTION_DAYS};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[derive(Default)]
// Private loader adapter; runtime constructors accept only a process configuration.
struct Config {
    pub actions: crate::bot_actions::Limits,
    pub action_worker: ActionWorkerConfig,
    pub member_sync: MemberSyncConfig,
    pub enrollment: EnrollmentConfig,
    pub image_spam: crate::image_spam::Limits,
    pub discord: DiscordConfig,
    pub db: DbConfig,
    pub guild_stores: GuildStoresConfig,
    pub evidence: EvidenceConfig,
    pub web: WebConfig,
    pub bot: BotConfig,
    pub log: LogConfig,
}

#[derive(Clone, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct DiscordConfig {
    pub token: String,
}

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct DbConfig {
    pub path: PathBuf,
    /// Remote libSQL URL. Empty = local SQLite file at `path`.
    pub url: String,
    /// Access token for remote DB (never send to the browser).
    pub token: String,
    pub pool_size: u32,
}

#[derive(Clone, Deserialize)]
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

impl std::fmt::Debug for DiscordConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordConfig")
            .field("token", &"<redacted>")
            .finish()
    }
}

impl std::fmt::Debug for DbConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbConfig")
            .field("path", &self.path)
            .field("url", &"<redacted>")
            .field("token", &"<redacted>")
            .field("pool_size", &self.pool_size)
            .finish()
    }
}

impl std::fmt::Debug for WebConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebConfig")
            .field("bind", &self.bind)
            .field("base_url", &self.base_url)
            .field("oauth_client_id", &self.oauth_client_id)
            .field("oauth_client_secret", &"<redacted>")
            .field("cors_origins", &self.cors_origins)
            .finish()
    }
}

impl DbConfig {
    /// Remote diagnostics never include credentials, paths, queries or fragments.
    pub fn log_label(&self) -> String {
        if self.url.is_empty() {
            return self.path.display().to_string();
        }
        match url::Url::parse(&self.url) {
            Ok(url) => match url.host_str() {
                Some(host) => format!("{}://{host}", url.scheme()),
                None => "<remote database>".into(),
            },
            Err(_) => "<invalid remote database URL>".into(),
        }
    }
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
    /// Automatically retain eligible guild messages unless explicitly disabled.
    pub message_capture_enabled: bool,
    /// Bot control listener. Not the public health or web bind.
    pub control_bind: String,
    /// API dial target for the control link. No default. Empty disables writes.
    pub control_target: String,
    /// API loopback bind for container-use readings (`JMODGUARD_READ_BIND`).
    pub read_bind: String,
    /// Bot dial target for private API container-use readings. Empty disables them.
    pub read_target: String,
    /// Shared HMAC secret for the private writer link. Not a database token.
    pub control_secret: ControlSecret,
}

/// Dedicated writer-link secret. Debug output never includes the value.
#[derive(Clone, Default, Deserialize)]
#[serde(transparent)]
pub struct ControlSecret(String);

impl std::fmt::Debug for ControlSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ControlSecret(<redacted>)")
    }
}

impl ControlSecret {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    pub level: String,
    pub directory: PathBuf,
    pub discord_concurrency: usize,
    pub discord_timeout_secs: u64,
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

/// Protected store connection details. Identities here must match the catalog; no URLs belong in SQL.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GuildStoresConfig {
    pub max_handles: u32,
    #[serde(default)]
    pub stores: Vec<GuildStoreConfig>,
}
impl Default for GuildStoresConfig {
    fn default() -> Self {
        Self {
            max_handles: 8,
            stores: Vec::new(),
        }
    }
}
impl GuildStoresConfig {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            (2..=32).contains(&self.max_handles),
            "guild_stores.max_handles must be 2..32"
        );
        let mut dedicated = std::collections::BTreeSet::new();
        let mut ids = std::collections::BTreeSet::new();
        for store in &self.stores {
            store.validate()?;
            if !store.id.is_empty() {
                anyhow::ensure!(ids.insert(store.id.clone()), "duplicate guild store id");
            }
            if store.dedicated_guild_id > 0 {
                anyhow::ensure!(
                    dedicated.insert(store.dedicated_guild_id),
                    "duplicate dedicated guild store"
                );
            }
        }
        Ok(())
    }
}

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct GuildStoreConfig {
    pub id: String,
    pub dedicated_guild_id: i64,
    pub path: PathBuf,
    pub url: String,
    pub token: String,
}
impl Default for GuildStoreConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            dedicated_guild_id: 0,
            path: PathBuf::new(),
            url: String::new(),
            token: String::new(),
        }
    }
}
impl std::fmt::Debug for GuildStoreConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuildStoreConfig")
            .field("id", &self.id)
            .field("dedicated_guild_id", &self.dedicated_guild_id)
            .field("path", &self.path)
            .field("url", &self.url)
            .field("token", &"<redacted>")
            .finish()
    }
}
impl GuildStoreConfig {
    pub fn validate(&self) -> Result<()> {
        if !self.id.is_empty() {
            crate::db::guild_stores::StoreId::parse(&self.id)?;
        }
        anyhow::ensure!(
            self.dedicated_guild_id >= 0,
            "dedicated_guild_id must be 0 or a positive snowflake"
        );
        if !self.url.trim().is_empty() {
            anyhow::ensure!(
                !self.token.trim().is_empty(),
                "guild store token is required when url is set"
            );
        }
        if !self.token.trim().is_empty() {
            anyhow::ensure!(
                !self.url.trim().is_empty(),
                "guild store url is required when token is set"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EvidenceConfig {
    pub backend: String,
    pub root: PathBuf,
    pub buffer_bytes: usize,
    pub storage_id: String,
    pub bunny: BunnyEvidenceConfig,
}
impl Default for EvidenceConfig {
    fn default() -> Self {
        Self {
            backend: "local".into(),
            root: PathBuf::new(),
            buffer_bytes: 65536,
            storage_id: String::new(),
            bunny: BunnyEvidenceConfig::default(),
        }
    }
}
impl EvidenceConfig {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            matches!(self.backend.as_str(), "local" | "bunny" | "none"),
            "evidence.backend must be local, bunny, or none"
        );
        anyhow::ensure!(
            self.buffer_bytes > 0,
            "evidence.buffer_bytes must be positive"
        );
        if !self.storage_id.is_empty() {
            crate::db::guild_data::CaseId::try_from(self.storage_id.clone())?;
        }
        if self.backend == "bunny" {
            anyhow::ensure!(
                !self.bunny.zone.is_empty() && !self.bunny.access_key.is_empty(),
                "bunny evidence zone and access_key are required"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct BunnyEvidenceConfig {
    pub hostname: String,
    pub zone: String,
    pub access_key: String,
    pub timeout_secs: u64,
}
impl Default for BunnyEvidenceConfig {
    fn default() -> Self {
        Self {
            hostname: "storage.bunnycdn.com".into(),
            zone: String::new(),
            access_key: String::new(),
            timeout_secs: 30,
        }
    }
}
impl std::fmt::Debug for BunnyEvidenceConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BunnyEvidenceConfig")
            .field("hostname", &self.hostname)
            .field("zone", &self.zone)
            .field("access_key", &"<redacted>")
            .field("timeout_secs", &self.timeout_secs)
            .finish()
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
            message_capture_enabled: true,
            control_bind: "127.0.0.1:8082".into(),
            control_target: String::new(),
            read_bind: crate::read_link::default_read_bind().to_owned(),
            read_target: String::new(),
            control_secret: ControlSecret::default(),
        }
    }
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            directory: PathBuf::from("logs"),
            discord_concurrency: 8,
            discord_timeout_secs: 10,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EnrollmentConfig {
    pub enabled: bool,
    pub poll_secs: u32,
    pub batch_size: u32,
    pub sweep_pause_secs: u32,
    pub retry_secs: u32,
    pub request_timeout_secs: u32,
}
impl Default for EnrollmentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_secs: 15,
            batch_size: 16,
            sweep_pause_secs: 3600,
            retry_secs: 60,
            request_timeout_secs: 10,
        }
    }
}
impl EnrollmentConfig {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.poll_secs > 0 && self.sweep_pause_secs > 0 && self.retry_secs > 0,
            "enrollment intervals must be positive"
        );
        anyhow::ensure!(
            (1..=200).contains(&self.batch_size),
            "enrollment batch size must be 1..200"
        );
        anyhow::ensure!(
            (1..=300).contains(&self.request_timeout_secs),
            "enrollment request timeout must be 1..300 seconds"
        );
        Ok(())
    }
    fn apply_env(&mut self) -> Result<()> {
        if let Ok(value) = env::var("JMODGUARD_ENROLLMENT_REPAIR_ENABLED") {
            self.enabled = value
                .parse()
                .context("invalid JMODGUARD_ENROLLMENT_REPAIR_ENABLED (expected true/false)")?;
        }
        for (name, target) in [
            ("JMODGUARD_ENROLLMENT_POLL_SECS", &mut self.poll_secs),
            ("JMODGUARD_ENROLLMENT_BATCH_SIZE", &mut self.batch_size),
            (
                "JMODGUARD_ENROLLMENT_SWEEP_PAUSE_SECS",
                &mut self.sweep_pause_secs,
            ),
            ("JMODGUARD_ENROLLMENT_RETRY_SECS", &mut self.retry_secs),
            (
                "JMODGUARD_ENROLLMENT_REQUEST_TIMEOUT_SECS",
                &mut self.request_timeout_secs,
            ),
        ] {
            if let Ok(value) = env::var(name) {
                *target = value.parse().with_context(|| format!("invalid {name}"))?;
            }
        }
        Ok(())
    }
}

/// Configuration retained by the Discord worker. It cannot hold OAuth credentials.
/// ```compile_fail
/// fn oauth_access(cfg: &jmodguard::config::BotProcessConfig) {
///     let _ = &cfg.web;
/// }
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct BotProcessConfig {
    pub actions: crate::bot_actions::Limits,
    pub action_worker: ActionWorkerConfig,
    pub member_sync: MemberSyncConfig,
    pub enrollment: EnrollmentConfig,
    pub image_spam: crate::image_spam::Limits,
    pub discord: DiscordConfig,
    pub db: DbConfig,
    pub guild_stores: GuildStoresConfig,
    pub evidence: EvidenceConfig,
    pub bot: BotConfig,
    pub log: LogConfig,
}

/// API process configuration has no bot credential or worker controls.
/// ```compile_fail
/// fn bot_secret(cfg: &jmodguard::config::ApiProcessConfig) { let _ = &cfg.discord; }
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ApiProcessConfig {
    pub actions: crate::bot_actions::Limits,
    pub db: DbConfig,
    pub guild_stores: GuildStoresConfig,
    pub evidence: EvidenceConfig,
    pub web: WebConfig,
    pub bot: BotConfig,
    pub log: LogConfig,
}

impl From<Config> for BotProcessConfig {
    fn from(c: Config) -> Self {
        Self {
            actions: c.actions,
            action_worker: c.action_worker,
            member_sync: c.member_sync,
            enrollment: c.enrollment,
            image_spam: c.image_spam,
            discord: c.discord,
            db: c.db,
            guild_stores: c.guild_stores,
            evidence: c.evidence,
            bot: c.bot,
            log: c.log,
        }
    }
}
impl From<Config> for ApiProcessConfig {
    fn from(c: Config) -> Self {
        Self {
            actions: c.actions,
            db: c.db,
            guild_stores: c.guild_stores,
            evidence: c.evidence,
            web: c.web,
            bot: c.bot,
            log: c.log,
        }
    }
}
impl From<BotProcessConfig> for Config {
    fn from(c: BotProcessConfig) -> Self {
        Self {
            actions: c.actions,
            action_worker: c.action_worker,
            member_sync: c.member_sync,
            enrollment: c.enrollment,
            image_spam: c.image_spam,
            discord: c.discord,
            db: c.db,
            guild_stores: c.guild_stores,
            evidence: c.evidence,
            web: WebConfig::default(),
            bot: c.bot,
            log: c.log,
        }
    }
}
impl From<ApiProcessConfig> for Config {
    fn from(c: ApiProcessConfig) -> Self {
        Self {
            actions: c.actions,
            action_worker: ActionWorkerConfig::default(),
            member_sync: MemberSyncConfig::default(),
            enrollment: EnrollmentConfig::default(),
            image_spam: crate::image_spam::Limits::default(),
            discord: DiscordConfig::default(),
            db: c.db,
            guild_stores: c.guild_stores,
            evidence: c.evidence,
            web: c.web,
            bot: c.bot,
            log: c.log,
        }
    }
}
impl BotProcessConfig {
    pub fn is_remote_db(&self) -> bool {
        !self.db.url.trim().is_empty()
    }
    pub fn load() -> Result<Self> {
        Config::load_for(false).map(Into::into)
    }
    pub fn db_open_options(&self) -> DbOpenOptions {
        self.db.open_options(&self.bot, true)
    }
    pub fn health_bind(&self) -> Option<String> {
        health_bind_from_env()
    }
}
impl ApiProcessConfig {
    pub fn load() -> Result<Self> {
        let cfg = Config::load_for(true)?;
        cfg.validate_cors()?;
        cfg.validate_public_base_url()?;
        Ok(cfg.into())
    }
    pub fn db_open_options(&self) -> DbOpenOptions {
        self.db.open_options(&self.bot, false)
    }
}
impl DbConfig {
    fn open_options(&self, bot: &BotConfig, migrate: bool) -> DbOpenOptions {
        DbOpenOptions {
            path: self.path.clone(),
            url: self.url.clone(),
            token: self.token.clone(),
            pool_size: self.pool_size,
            migrate,
            read_only: !migrate,
            default_prefix: bot.default_prefix.clone(),
            retention_days: bot.retention_days,
        }
    }
}

impl Config {
    /// Load the requested process shape from JMODGUARD_CONFIG or config.toml.
    /// File sections belonging only to the other process are ignored by serde.
    fn load_for(api: bool) -> Result<Self> {
        let path = env::var("JMODGUARD_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("config.toml"));
        let mut cfg = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read config file {}", path.display()))?;
            Self::parse_file(&raw, api)
                .with_context(|| format!("failed to parse config file {}", path.display()))?
        } else {
            tracing::warn!("config.toml not found; using defaults. See config.example.toml.");
            Self::default()
        };
        cfg.apply_env_for(api)?;
        cfg.actions.validate()?;
        cfg.guild_stores.validate()?;
        cfg.evidence.validate()?;
        if !api {
            cfg.enrollment.validate()?;
            cfg.image_spam.validate()?;
            cfg.action_worker.validate()?;
            cfg.member_sync.validate()?;
        }
        Ok(cfg)
    }

    fn parse_file(raw: &str, api: bool) -> Result<Self> {
        let parsed = if api {
            toml::from_str::<ApiProcessConfig>(raw).map(Self::from)
        } else {
            toml::from_str::<BotProcessConfig>(raw).map(Self::from)
        };
        parsed.map_err(|error| {
            // Do not retain the TOML error as a source: Display and Debug quote
            // the input line and invalid value, which can contain credentials.
            if let Some(span) = error.span() {
                let before = &raw[..raw.floor_char_boundary(span.start.min(raw.len()))];
                let line = before.bytes().filter(|&b| b == b'\n').count() + 1;
                let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
                anyhow::anyhow!("invalid TOML at line {line}, column {column}")
            } else {
                anyhow::anyhow!("invalid TOML configuration")
            }
        })
    }

    fn retention_days_in_range(days: u32, source: &str) -> Result<u32> {
        if days > MAX_RETENTION_DAYS {
            anyhow::bail!("{source} must be between 0 and {MAX_RETENTION_DAYS} (got {days})");
        }
        Ok(days)
    }

    /// Reject a public HTTP bind that still uses a localhost or non-HTTPS
    /// OAuth origin.
    ///
    /// Call this from `jmodguard-api` only. Local `127.0.0.1` binds keep the
    /// SETUP.md default `http://localhost:8080`. Container images bind
    /// `0.0.0.0:8080`, so they must set `JMODGUARD_WEB_BASE_URL` to the public
    /// HTTPS origin browsers open (the CDN hostname on Bunny). Magic Container
    /// hostnames are not a stable, detectable class — this check enforces
    /// HTTPS + non-loopback, not a particular DNS suffix.
    pub fn validate_public_base_url(&self) -> Result<()> {
        if !bind_is_unspecified(&self.web.bind) {
            return Ok(());
        }
        let url = url::Url::parse(&self.web.base_url)
            .with_context(|| format!("invalid web.base_url `{}`", self.web.base_url))?;
        let host = url.host_str().unwrap_or("");
        if host_is_loopback(host) {
            anyhow::bail!(
                "jmodguard-api is bound on `{}` but web.base_url is `{}`. \
                 Set JMODGUARD_WEB_BASE_URL to the public HTTPS origin \
                 (the CDN hostname browsers open, not a Magic Container). \
                 It must match the Discord OAuth redirect.",
                self.web.bind,
                self.web.base_url
            );
        }
        if url.scheme() != "https" {
            anyhow::bail!(
                "jmodguard-api is bound on `{}` so web.base_url must be https \
                 (got `{}`). Discord OAuth and Secure session cookies require it.",
                self.web.bind,
                self.web.base_url
            );
        }
        Ok(())
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
                .parse::<http::header::HeaderValue>()
                .with_context(|| format!("invalid web.cors_origins entry `{origin}`"))?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn apply_env(&mut self) -> Result<()> {
        self.apply_env_for(false)
    }

    fn apply_env_for(&mut self, api: bool) -> Result<()> {
        match (
            nonempty_env("JMODGUARD_GUILD_STORE_URL"),
            nonempty_env("JMODGUARD_GUILD_STORE_TOKEN"),
        ) {
            (Some(url), Some(token)) => {
                let shared = match self
                    .guild_stores
                    .stores
                    .iter()
                    .position(|store| store.dedicated_guild_id == 0)
                {
                    Some(index) => &mut self.guild_stores.stores[index],
                    None => {
                        self.guild_stores.stores.push(GuildStoreConfig::default());
                        self.guild_stores
                            .stores
                            .last_mut()
                            .expect("shared entry was inserted")
                    }
                };
                shared.url = url;
                shared.token = token;
                if let Some(id) = nonempty_env("JMODGUARD_GUILD_STORE_ID") {
                    shared.id = id;
                }
            }
            (None, None) => {
                anyhow::ensure!(
                    nonempty_env("JMODGUARD_GUILD_STORE_ID").is_none(),
                    "JMODGUARD_GUILD_STORE_ID requires the shared store URL/token pair"
                );
            }
            _ => anyhow::bail!(
                "JMODGUARD_GUILD_STORE_URL and JMODGUARD_GUILD_STORE_TOKEN must be set together and non-empty"
            ),
        }
        if !api {
            if let Ok(raw) = env::var("JMODGUARD_IMAGE_SPAM_LIMITS") {
                self.image_spam = serde_json::from_str(&raw)
                    .context("invalid JMODGUARD_IMAGE_SPAM_LIMITS JSON")?;
                self.image_spam.validate()?;
            }
            self.enrollment.apply_env()?;
            self.action_worker.apply_env()?;
            self.member_sync.apply_env()?;
            if let Ok(v) = env::var("JMODGUARD_DISCORD_TOKEN") {
                self.discord.token = v.trim().to_string();
            }
        }
        if let Ok(v) = env::var("JMODGUARD_EVIDENCE_BACKEND") {
            self.evidence.backend = v;
        }
        if let Ok(v) = env::var("JMODGUARD_EVIDENCE_ROOT") {
            self.evidence.root = PathBuf::from(v);
        }
        if let Ok(v) = env::var("JMODGUARD_BUNNY_EVIDENCE_ZONE") {
            self.evidence.bunny.zone = v;
        }
        if let Ok(v) = env::var("JMODGUARD_BUNNY_EVIDENCE_ACCESS_KEY") {
            self.evidence.bunny.access_key = v;
        }
        for (name, target) in [
            (
                "JMODGUARD_ACTION_PENDING_PER_GUILD",
                &mut self.actions.pending_per_guild,
            ),
            (
                "JMODGUARD_ACTION_MAX_AGE_SECS",
                &mut self.actions.max_age_secs,
            ),
            (
                "JMODGUARD_ACTION_HISTORY_RETENTION_SECS",
                &mut self.actions.history_retention_secs,
            ),
            (
                "JMODGUARD_ACTION_CLEANUP_BATCH_SIZE",
                &mut self.actions.cleanup_batch_size,
            ),
        ] {
            if let Ok(value) = env::var(name) {
                *target = value.parse().with_context(|| format!("invalid {name}"))?;
            }
        }
        self.actions.validate()?;
        if let Ok(v) = env::var("JMODGUARD_DB_PATH") {
            self.db.path = PathBuf::from(v.trim());
        }
        let portable = (
            nonempty_env("JMODGUARD_DB_URL"),
            nonempty_env("JMODGUARD_DB_TOKEN"),
        );
        let bunny = (
            nonempty_env("BUNNY_DATABASE_URL"),
            nonempty_env("BUNNY_DATABASE_AUTH_TOKEN"),
        );
        let selected = if portable.0.is_some() || portable.1.is_some() {
            match portable {
                (Some(url), Some(token)) => Some((url, token)),
                _ => anyhow::bail!(
                    "JMODGUARD_DB_URL and JMODGUARD_DB_TOKEN must be set together and non-empty"
                ),
            }
        } else if bunny.0.is_some() || bunny.1.is_some() {
            match bunny {
                (Some(url), Some(token)) => Some((url, token)),
                _ => anyhow::bail!(
                    "BUNNY_DATABASE_URL and BUNNY_DATABASE_AUTH_TOKEN must be set together and non-empty"
                ),
            }
        } else {
            None
        };
        if let Some((url, token)) = selected {
            self.db.url = url;
            self.db.token = token;
        }
        if env_flag("JMODGUARD_REQUIRE_REMOTE_DB")
            && (self.db.url.trim().is_empty() || self.db.token.trim().is_empty())
        {
            anyhow::bail!(
                "JMODGUARD_REQUIRE_REMOTE_DB is set but no complete remote database URL/token pair was found. \
                 Set BUNNY_DATABASE_URL and BUNNY_DATABASE_AUTH_TOKEN \
                 (or JMODGUARD_DB_URL and JMODGUARD_DB_TOKEN). \
                 Unset JMODGUARD_REQUIRE_REMOTE_DB only for a local SQLite run."
            );
        }
        if api {
            if let Ok(v) = env::var("JMODGUARD_WEB_BIND") {
                self.web.bind = v.trim().to_string();
            }
            if let Ok(v) = env::var("JMODGUARD_WEB_BASE_URL") {
                self.web.base_url = v.trim().trim_end_matches('/').to_string();
            }
            if let Ok(v) = env::var("JMODGUARD_OAUTH_CLIENT_ID") {
                self.web.oauth_client_id = v.trim().to_string();
            }
            if let Ok(v) = env::var("JMODGUARD_OAUTH_CLIENT_SECRET") {
                self.web.oauth_client_secret = v.trim().to_string();
            }
            if let Ok(v) = env::var("JMODGUARD_CORS_ORIGINS") {
                self.web.cors_origins = v
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
        }
        if let Ok(value) = env::var("JMODGUARD_CONTROL_SECRET") {
            self.bot.control_secret = ControlSecret(value);
        }
        if let Ok(value) = env::var("JMODGUARD_CONTROL_BIND") {
            self.bot.control_bind = value;
        }
        if let Ok(value) = env::var("JMODGUARD_CONTROL_TARGET") {
            self.bot.control_target = value;
        }
        if let Ok(value) = env::var("JMODGUARD_READ_BIND") {
            self.bot.read_bind = value;
        }
        if let Ok(value) = env::var("JMODGUARD_READ_TARGET") {
            self.bot.read_target = value;
        }
        if let Ok(v) = env::var("JMODGUARD_OWNER_ID") {
            let v = v.trim();
            if !v.is_empty() {
                self.bot.owner_id = v.parse().with_context(|| {
                    format!("JMODGUARD_OWNER_ID must be a Discord snowflake, got `{v}`")
                })?;
            }
        }
        if let Ok(v) = env::var("JMODGUARD_DB_POOL_SIZE") {
            let v = v.trim();
            if !v.is_empty() {
                let parsed: u32 = v.parse().with_context(|| {
                    format!("JMODGUARD_DB_POOL_SIZE must be an integer, got `{v}`")
                })?;
                if parsed < 2 {
                    anyhow::bail!("JMODGUARD_DB_POOL_SIZE must be at least 2, got `{parsed}`");
                }
                self.db.pool_size = parsed;
            }
        }
        if let Ok(v) = env::var("JMODGUARD_DEFAULT_PREFIX") {
            let v = v.trim();
            if !v.is_empty() {
                self.bot.default_prefix = v.to_string();
            }
        }
        if let Ok(v) = env::var("JMODGUARD_RETENTION_DAYS") {
            let v = v.trim();
            if !v.is_empty() {
                let parsed: u32 = v.parse().with_context(|| {
                    format!("JMODGUARD_RETENTION_DAYS must be an integer, got `{v}`")
                })?;
                self.bot.retention_days =
                    Self::retention_days_in_range(parsed, "JMODGUARD_RETENTION_DAYS")?;
            }
        }
        if let Ok(v) = env::var("JMODGUARD_BOT_STATUS") {
            let v = v.trim();
            if !v.is_empty() {
                self.bot.status = v.to_string();
            }
        }
        if let Ok(v) = env::var("JMODGUARD_CONFIG_POLL_SECS") {
            let v = v.trim();
            if !v.is_empty() {
                self.bot.config_poll_secs = v.parse().with_context(|| {
                    format!("JMODGUARD_CONFIG_POLL_SECS must be a positive integer, got `{v}`")
                })?;
            }
        }
        if let Ok(v) = env::var("JMODGUARD_MESSAGE_CAPTURE_ENABLED") {
            self.bot.message_capture_enabled = v
                .trim()
                .parse()
                .context("JMODGUARD_MESSAGE_CAPTURE_ENABLED must be true or false")?;
        }
        if let Ok(v) = env::var("JMODGUARD_LOG_DISCORD_CONCURRENCY") {
            self.log.discord_concurrency = v
                .trim()
                .parse()
                .context("JMODGUARD_LOG_DISCORD_CONCURRENCY must be a positive integer")?;
        }
        if let Ok(v) = env::var("JMODGUARD_LOG_DISCORD_TIMEOUT_SECS") {
            self.log.discord_timeout_secs = v
                .trim()
                .parse()
                .context("JMODGUARD_LOG_DISCORD_TIMEOUT_SECS must be a positive integer")?;
        }
        if let Ok(v) = env::var("JMODGUARD_LOG_LEVEL") {
            self.log.level = v.trim().to_string();
        }
        if let Ok(v) = env::var("JMODGUARD_LOG_DIRECTORY") {
            self.log.directory = PathBuf::from(v.trim());
        }
        // File and env both feed bot migrate-open terminal defaults; WebUI
        // writes already reject above MAX_RETENTION_DAYS or purge skips.
        Self::retention_days_in_range(self.bot.retention_days, "bot.retention_days")?;
        Ok(())
    }
}

fn health_bind_from_env() -> Option<String> {
    match env::var("JMODGUARD_HEALTH_BIND") {
        Ok(value) => {
            let value = value.trim();
            if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            }
        }
        Err(_) => None,
    }
}

fn nonempty_env(name: &str) -> Option<String> {
    env::var(name).ok().and_then(|value| {
        let value = value.trim();
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    })
}

fn env_flag(name: &str) -> bool {
    match env::var(name) {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

fn bind_is_unspecified(bind: &str) -> bool {
    let host = bind_host(bind);
    host == "0.0.0.0" || host == "::" || host == "[::]"
}

fn host_is_loopback(host: &str) -> bool {
    let host = host.trim().trim_matches(['[', ']']).trim_end_matches('.');
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let Ok(address) = host.parse::<std::net::IpAddr>() else {
        return false;
    };
    match address {
        std::net::IpAddr::V4(address) => address.is_loopback() || address.is_unspecified(),
        std::net::IpAddr::V6(address) => {
            address.is_loopback()
                || address.is_unspecified()
                || address
                    .to_ipv4_mapped()
                    .is_some_and(|address| address.is_loopback() || address.is_unspecified())
        }
    }
}

fn bind_host(bind: &str) -> &str {
    let bind = bind.trim();
    if let Some(rest) = bind.strip_prefix('[')
        && let Some((host, _)) = rest.split_once("]:")
    {
        return host;
    }
    bind.rsplit_once(':').map(|(host, _)| host).unwrap_or(bind)
}

#[cfg(test)]
mod tests {
    use super::Config;
    use std::ffi::OsString;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct SavedEnv(Vec<(&'static str, Option<OsString>)>);

    impl SavedEnv {
        fn capture(names: &[&'static str]) -> Self {
            Self(
                names
                    .iter()
                    .map(|name| (*name, std::env::var_os(name)))
                    .collect(),
            )
        }
    }

    impl Drop for SavedEnv {
        fn drop(&mut self) {
            for (name, value) in self.0.drain(..) {
                match value {
                    Some(value) => unsafe { std::env::set_var(name, value) },
                    None => unsafe { std::env::remove_var(name) },
                }
            }
        }
    }

    #[test]
    fn shared_store_environment_is_atomic_and_preserves_other_store_entries() {
        let _lock = ENV_LOCK.lock().unwrap();
        let names = [
            "JMODGUARD_GUILD_STORE_URL",
            "JMODGUARD_GUILD_STORE_TOKEN",
            "JMODGUARD_GUILD_STORE_ID",
        ];
        let _saved = SavedEnv::capture(&names);
        for name in names {
            unsafe {
                std::env::remove_var(name);
            }
        }
        unsafe {
            std::env::set_var(names[0], "libsql://guild.example");
        }
        assert!(Config::default().apply_env_for(false).is_err());
        unsafe {
            std::env::set_var(names[1], "guild-token");
        }
        for api in [false, true] {
            let mut cfg = Config::parse_file("", api).unwrap();
            cfg.apply_env_for(api).unwrap();
            assert_eq!(cfg.guild_stores.stores.len(), 1);
            assert_eq!(cfg.guild_stores.stores[0].url, "libsql://guild.example");
            assert_eq!(cfg.guild_stores.stores[0].token, "guild-token");
            cfg.guild_stores.validate().unwrap();
        }
        let mut cfg = Config::default();
        let shared_id = "a".repeat(32);
        cfg.guild_stores.stores = vec![
            super::GuildStoreConfig {
                dedicated_guild_id: 10,
                path: "dedicated.db".into(),
                ..Default::default()
            },
            super::GuildStoreConfig {
                id: shared_id.clone(),
                path: "shared.db".into(),
                ..Default::default()
            },
            super::GuildStoreConfig {
                id: "b".repeat(32),
                path: "other-shared.db".into(),
                ..Default::default()
            },
        ];
        cfg.apply_env_for(true).unwrap();
        assert_eq!(cfg.guild_stores.stores.len(), 3);
        assert_eq!(
            cfg.guild_stores.stores[0].path,
            std::path::PathBuf::from("dedicated.db")
        );
        assert!(cfg.guild_stores.stores[0].url.is_empty());
        assert_eq!(cfg.guild_stores.stores[1].id, shared_id);
        assert_eq!(cfg.guild_stores.stores[1].url, "libsql://guild.example");
        assert!(cfg.guild_stores.stores[2].url.is_empty());
        unsafe {
            std::env::set_var(names[2], "invalid");
        }
        cfg.apply_env_for(true).unwrap();
        assert!(cfg.guild_stores.validate().is_err());
        unsafe {
            std::env::remove_var(names[0]);
            std::env::remove_var(names[1]);
        }
        assert!(Config::default().apply_env_for(true).is_err());
    }

    #[test]
    fn image_work_controls_are_bot_only_and_validate_env() {
        let bot = Config::parse_file("[image_spam]\nconcurrency=1\nmetadata_bytes=8388608", false)
            .unwrap();
        assert_eq!(bot.image_spam.concurrency, 1);
        assert_eq!(bot.image_spam.metadata_bytes, 8388608);
        assert!(Config::parse_file("[image_spam]\nconcurrency='bad'", true).is_ok());
        assert!(Config::parse_file("[image_spam]\nconcurrency='bad'", false).is_err());
        let _lock = ENV_LOCK.lock().unwrap();
        let _saved = SavedEnv::capture(&["JMODGUARD_IMAGE_SPAM_LIMITS"]);
        unsafe {
            std::env::set_var("JMODGUARD_IMAGE_SPAM_LIMITS", "invalid");
        }
        assert!(Config::default().apply_env_for(false).is_err());
        // API ignores this bot-only environment variable.
        let mut api = Config::default();
        api.image_spam.concurrency = 7;
        let _ = api.apply_env_for(true);
        assert_eq!(api.image_spam.concurrency, 7);
        unsafe {
            std::env::set_var("JMODGUARD_IMAGE_SPAM_LIMITS", r#"{"concurrency":1}"#);
        }
        let mut bot = Config::default();
        bot.apply_env_for(false).unwrap();
        assert_eq!(bot.image_spam.concurrency, 1);
        unsafe {
            std::env::set_var("JMODGUARD_IMAGE_SPAM_LIMITS", r#"{"concurrency":0}"#);
        }
        assert!(Config::default().apply_env_for(false).is_err());
    }

    #[test]
    fn action_configuration_separates_admission_from_bot_scheduling() {
        let raw = "[actions]\npending_per_guild=7\nmax_age_secs=30\n[action_worker]\npoll_secs=9\nbatch_size=2\nrequest_timeout_secs=4\ncleanup_interval_secs=90\n";
        let bot = Config::parse_file(raw, false).unwrap();
        let api = Config::parse_file(raw, true).unwrap();
        assert_eq!(bot.actions.pending_per_guild, 7);
        assert_eq!(api.actions.pending_per_guild, 7);
        assert_eq!(bot.action_worker.poll_secs, 9);
        assert_eq!(bot.action_worker.limits.batch_size, 2);
        assert_eq!(bot.action_worker.limits.request_timeout_secs, 4);
        bot.action_worker.validate().unwrap();
        assert_eq!(api.action_worker.poll_secs, 1);
        assert!(Config::parse_file("[action_worker]\npoll_secs='bad'", true).is_ok());
        assert!(Config::parse_file("[action_worker]\npoll_secs='bad'", false).is_err());
        assert!(Config::parse_file("[actions]\npending_per_guild='bad'", true).is_err());
    }

    #[test]
    fn worker_environment_overrides_are_validated_only_by_bot() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _saved = SavedEnv::capture(&[
            "JMODGUARD_ACTION_POLL_SECS",
            "JMODGUARD_ACTION_BATCH_SIZE",
            "JMODGUARD_ACTION_PENDING_PER_GUILD",
        ]);
        unsafe {
            std::env::set_var("JMODGUARD_ACTION_POLL_SECS", "0");
            std::env::set_var("JMODGUARD_ACTION_PENDING_PER_GUILD", "5");
        }
        let mut api = Config::default();
        api.apply_env_for(true).unwrap();
        assert_eq!(api.actions.pending_per_guild, 5);
        let mut bot = Config::default();
        assert!(bot.apply_env_for(false).is_err());
        unsafe {
            std::env::set_var("JMODGUARD_ACTION_POLL_SECS", "8");
            std::env::set_var("JMODGUARD_ACTION_BATCH_SIZE", "2");
        }
        bot.apply_env_for(false).unwrap();
        assert_eq!(bot.action_worker.poll_secs, 8);
        assert_eq!(bot.action_worker.limits.batch_size, 2);
    }

    #[test]
    fn bot_file_ignores_dashboard_fields_but_api_validates_them() {
        let raw = "[discord]\ntoken='bot-token'\n[web]\noauth_client_secret=123\nbind=[]\n";
        let bot = Config::parse_file(raw, false).unwrap();
        assert_eq!(bot.discord.token, "bot-token");
        assert!(bot.web.oauth_client_secret.is_empty());
        assert!(Config::parse_file(raw, true).is_err());
        // TOML syntax still has to be valid, even in an ignored section.
        assert!(Config::parse_file("[web]\nbind=[", false).is_err());
        assert!(Config::parse_file("[discord]\ntoken=123", false).is_err());
        assert!(Config::parse_file("[discord]\ntoken=123", true).is_ok());
    }

    #[test]
    fn membership_repair_config_is_bot_only_and_bounded() {
        let raw = "[enrollment]\nenabled=false\nbatch_size=3\npoll_secs=7\nsweep_pause_secs=60\nretry_secs=4\nrequest_timeout_secs=2";
        let bot: super::BotProcessConfig = Config::parse_file(raw, false).unwrap().into();
        assert!(!bot.enrollment.enabled);
        assert_eq!(bot.enrollment.batch_size, 3);
        bot.enrollment.validate().unwrap();
        assert!(Config::parse_file("[enrollment]\nbatch_size='invalid'", false).is_err());
        assert!(Config::parse_file("[enrollment]\nbatch_size='invalid'", true).is_ok());
        for raw in [
            "poll_secs=0",
            "sweep_pause_secs=0",
            "retry_secs=0",
            "batch_size=0",
            "batch_size=201",
            "request_timeout_secs=0",
            "request_timeout_secs=301",
        ] {
            assert!(
                toml::from_str::<super::EnrollmentConfig>(raw)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
    }

    #[test]
    fn control_bind_defaults_to_loopback_and_target_defaults_empty() {
        let cfg = Config::default();
        assert_eq!(cfg.bot.control_bind, "127.0.0.1:8082");
        assert!(cfg.bot.control_target.is_empty());
        assert_eq!(cfg.web.bind, "127.0.0.1:8080");
        let _lock = ENV_LOCK.lock().unwrap();
        let _saved = SavedEnv::capture(&["JMODGUARD_CONTROL_BIND", "JMODGUARD_CONTROL_TARGET"]);
        unsafe { std::env::set_var("JMODGUARD_CONTROL_BIND", "127.0.0.1:9") };
        unsafe { std::env::set_var("JMODGUARD_CONTROL_TARGET", "") };
        let mut cfg = Config::default();
        cfg.apply_env_for(false).unwrap();
        assert_eq!(cfg.bot.control_bind, "127.0.0.1:9");
        assert!(cfg.bot.control_target.is_empty());
        unsafe { std::env::set_var("JMODGUARD_CONTROL_TARGET", "127.0.0.1:9") };
        cfg.apply_env_for(true).unwrap();
        assert_eq!(cfg.bot.control_target, "127.0.0.1:9");
    }

    #[test]
    fn listener_defaults_are_distinct() {
        let bot_image = include_str!("../Dockerfile.bot");
        let api_image = include_str!("../Dockerfile.api");
        let bot_health = env_port(bot_image, "JMODGUARD_HEALTH_BIND");
        let bot_expose = expose_port(bot_image);
        let api_bind = env_port(api_image, "JMODGUARD_WEB_BIND");
        let api_expose = expose_port(api_image);
        let cfg = Config::default();
        let control = port_of(&cfg.bot.control_bind);
        let web = port_of(&cfg.web.bind);
        assert_eq!(bot_health, 8081);
        assert_eq!(bot_expose, 8081);
        assert_eq!(api_bind, 8080);
        assert_eq!(api_expose, 8080);
        assert_eq!(web, 8080);
        assert_eq!(control, 8082);
        let read = port_of(&cfg.bot.read_bind);
        assert_eq!(read, 8083);
        let mut ports = [bot_health, api_bind, control, read];
        ports.sort_unstable();
        assert!(
            ports.windows(2).all(|pair| pair[0] != pair[1]),
            "listener defaults must be distinct: {ports:?}"
        );
    }

    fn env_port(dockerfile: &str, key: &str) -> u16 {
        let line = dockerfile
            .lines()
            .find(|line| line.contains(&format!("ENV {key}=")))
            .unwrap_or_else(|| panic!("missing {key}"));
        port_of(line.rsplit('=').next().unwrap())
    }

    fn expose_port(dockerfile: &str) -> u16 {
        let line = dockerfile
            .lines()
            .find(|line| line.starts_with("EXPOSE "))
            .expect("missing EXPOSE");
        line.trim_start_matches("EXPOSE ").trim().parse().unwrap()
    }

    fn port_of(address: &str) -> u16 {
        address
            .rsplit(':')
            .next()
            .unwrap()
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("port in {address}"))
    }

    #[test]
    fn control_secret_loads_for_bot_and_api() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _saved = SavedEnv::capture(&["JMODGUARD_CONTROL_SECRET"]);
        let secret = "x".repeat(32);
        unsafe { std::env::set_var("JMODGUARD_CONTROL_SECRET", &secret) };
        let mut bot = Config::default();
        bot.apply_env_for(false).unwrap();
        let mut api = Config::default();
        api.apply_env_for(true).unwrap();
        assert_eq!(bot.bot.control_secret.as_str(), secret);
        assert_eq!(api.bot.control_secret.as_str(), secret);
        let rendered = format!("{:?}", bot.bot.control_secret);
        assert!(!rendered.contains(&secret));
        assert!(rendered.contains("redacted"));
    }

    #[test]
    fn membership_repair_environment_applies_only_to_bot() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _saved = SavedEnv::capture(&[
            "JMODGUARD_ENROLLMENT_BATCH_SIZE",
            "JMODGUARD_ENROLLMENT_REPAIR_ENABLED",
        ]);
        unsafe {
            std::env::set_var("JMODGUARD_ENROLLMENT_BATCH_SIZE", "4");
            std::env::set_var("JMODGUARD_ENROLLMENT_REPAIR_ENABLED", "false");
        }
        let mut bot = Config::parse_file("[enrollment]\nbatch_size=3", false).unwrap();
        bot.apply_env_for(false).unwrap();
        assert_eq!(bot.enrollment.batch_size, 4);
        assert!(!bot.enrollment.enabled);
        unsafe {
            std::env::set_var("JMODGUARD_ENROLLMENT_BATCH_SIZE", "bad");
        }
        assert!(bot.apply_env_for(false).is_err());
        let mut api = Config::default();
        api.apply_env_for(true).unwrap();
        assert_eq!(
            api.enrollment.batch_size,
            super::EnrollmentConfig::default().batch_size
        );
    }

    #[test]
    fn process_options_fix_migration_role_and_keep_shared_settings() {
        let raw = "[db]\npath='custom.db'\npool_size=3\n[bot]\ndefault_prefix='?'\nowner_id=42\nconfig_poll_secs=2\n[log]\ndiscord_concurrency=4\ndiscord_timeout_secs=6\n";
        let bot: super::BotProcessConfig = Config::parse_file(raw, false).unwrap().into();
        let api: super::ApiProcessConfig = Config::parse_file(raw, true).unwrap().into();
        let bot_db = bot.db_open_options();
        let api_db = api.db_open_options();
        assert!(bot_db.migrate);
        assert!(!api_db.migrate);
        assert_eq!(bot_db.path, api_db.path);
        assert_eq!(api_db.pool_size, 3);
        assert_eq!(bot_db.default_prefix, "?");
        assert_eq!(api.bot.owner_id, 42);
        assert_eq!(api.bot.config_poll_secs, 2);
        assert_eq!(bot.log.discord_concurrency, 4);
        assert_eq!(bot.log.discord_timeout_secs, 6);
    }

    #[test]
    fn process_environment_loads_oauth_only_for_api_and_bot_token_for_both() {
        let _lock = ENV_LOCK.lock().unwrap();
        const NAMES: &[&str] = &[
            "JMODGUARD_OAUTH_CLIENT_ID",
            "JMODGUARD_OAUTH_CLIENT_SECRET",
            "JMODGUARD_WEB_BASE_URL",
            "JMODGUARD_DISCORD_TOKEN",
        ];
        let _saved = SavedEnv::capture(NAMES);
        unsafe {
            std::env::set_var("JMODGUARD_OAUTH_CLIENT_ID", "oauth-id");
            std::env::set_var("JMODGUARD_OAUTH_CLIENT_SECRET", "oauth-secret");
            std::env::set_var("JMODGUARD_WEB_BASE_URL", "https://dashboard.invalid/");
            std::env::set_var("JMODGUARD_DISCORD_TOKEN", " bot-secret ");
        }
        let mut bot = Config::default();
        bot.apply_env_for(false).unwrap();
        let mut api = Config::default();
        api.apply_env_for(true).unwrap();
        assert!(bot.web.oauth_client_id.is_empty());
        assert!(bot.web.oauth_client_secret.is_empty());
        assert_eq!(api.web.oauth_client_id, "oauth-id");
        assert_eq!(api.web.oauth_client_secret, "oauth-secret");
        assert_eq!(api.web.base_url, "https://dashboard.invalid");
        assert_eq!(bot.discord.token, "bot-secret");
        assert!(api.discord.token.is_empty());
    }

    #[test]
    fn rejects_wildcard_credentialed_cors_origin() {
        let mut cfg = Config::default();
        cfg.web.cors_origins = vec!["*".into()];
        let err = cfg.validate_cors().unwrap_err();
        assert!(err.to_string().contains("cannot contain `*`"));
    }

    #[test]
    fn bunny_database_environment_names_populate_remote_config() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        const NAMES: &[&str] = &[
            "JMODGUARD_DB_URL",
            "JMODGUARD_DB_TOKEN",
            "BUNNY_DATABASE_URL",
            "BUNNY_DATABASE_AUTH_TOKEN",
        ];
        let _saved = SavedEnv::capture(NAMES);
        for name in NAMES {
            unsafe { std::env::remove_var(name) };
        }
        unsafe {
            std::env::set_var("BUNNY_DATABASE_URL", "libsql://example.bunnydb.test");
            std::env::set_var("BUNNY_DATABASE_AUTH_TOKEN", "bunny-test-token");
        }

        let mut cfg = Config::default();
        cfg.apply_env().unwrap();

        assert_eq!(cfg.db.url, "libsql://example.bunnydb.test");
        assert_eq!(cfg.db.token, "bunny-test-token");
    }

    #[test]
    fn database_environment_credentials_are_an_atomic_pair() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        const NAMES: &[&str] = &[
            "JMODGUARD_DB_URL",
            "JMODGUARD_DB_TOKEN",
            "BUNNY_DATABASE_URL",
            "BUNNY_DATABASE_AUTH_TOKEN",
        ];
        let _saved = SavedEnv::capture(NAMES);
        for name in NAMES {
            unsafe { std::env::remove_var(name) };
        }
        unsafe {
            std::env::set_var("BUNNY_DATABASE_URL", "libsql://bunny.example");
            std::env::set_var("BUNNY_DATABASE_AUTH_TOKEN", "bunny-secret");
            std::env::set_var("JMODGUARD_DB_URL", "libsql://portable.example");
        }

        let mut cfg = Config::default();
        assert!(cfg.apply_env().is_err());
        assert!(cfg.db.url.is_empty());
        assert!(cfg.db.token.is_empty());

        unsafe { std::env::set_var("JMODGUARD_DB_TOKEN", "portable-secret") };
        cfg.apply_env().unwrap();
        assert_eq!(cfg.db.url, "libsql://portable.example");
        assert_eq!(cfg.db.token, "portable-secret");

        unsafe { std::env::set_var("JMODGUARD_DB_URL", "   ") };
        assert!(cfg.apply_env().is_err());

        unsafe {
            std::env::set_var("JMODGUARD_DB_URL", "libsql://portable.example");
            std::env::set_var("JMODGUARD_DB_TOKEN", "   ");
        }
        assert!(cfg.apply_env().is_err());
    }

    #[test]
    fn remote_tokens_are_trimmed() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        const NAMES: &[&str] = &[
            "JMODGUARD_DB_URL",
            "JMODGUARD_DB_TOKEN",
            "BUNNY_DATABASE_URL",
            "BUNNY_DATABASE_AUTH_TOKEN",
            "JMODGUARD_REQUIRE_REMOTE_DB",
        ];
        let _saved = SavedEnv::capture(NAMES);
        for name in NAMES {
            unsafe { std::env::remove_var(name) };
        }
        unsafe {
            std::env::set_var("BUNNY_DATABASE_URL", "  libsql://example.bunnydb.test\n");
            std::env::set_var("BUNNY_DATABASE_AUTH_TOKEN", " bunny-test-token\n");
        }

        let mut cfg = Config::default();
        cfg.apply_env().unwrap();
        assert_eq!(cfg.db.url, "libsql://example.bunnydb.test");
        assert_eq!(cfg.db.token, "bunny-test-token");
    }

    #[test]
    fn require_remote_db_fails_closed_without_credentials() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        const NAMES: &[&str] = &[
            "JMODGUARD_DB_URL",
            "JMODGUARD_DB_TOKEN",
            "BUNNY_DATABASE_URL",
            "BUNNY_DATABASE_AUTH_TOKEN",
            "JMODGUARD_REQUIRE_REMOTE_DB",
        ];
        let _saved = SavedEnv::capture(NAMES);
        for name in NAMES {
            unsafe { std::env::remove_var(name) };
        }
        unsafe { std::env::set_var("JMODGUARD_REQUIRE_REMOTE_DB", "1") };

        let mut cfg = Config::default();
        let err = cfg.apply_env().unwrap_err();
        assert!(
            err.to_string().contains("JMODGUARD_REQUIRE_REMOTE_DB"),
            "unexpected: {err:#}"
        );
        assert!(cfg.db.url.is_empty());
    }

    #[test]
    fn blank_portable_env_does_not_block_bunny_credentials() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        const NAMES: &[&str] = &[
            "JMODGUARD_DB_URL",
            "JMODGUARD_DB_TOKEN",
            "BUNNY_DATABASE_URL",
            "BUNNY_DATABASE_AUTH_TOKEN",
        ];
        let _saved = SavedEnv::capture(NAMES);
        for name in NAMES {
            unsafe { std::env::remove_var(name) };
        }
        unsafe {
            std::env::set_var("JMODGUARD_DB_URL", "   ");
            std::env::set_var("JMODGUARD_DB_TOKEN", "");
            std::env::set_var("BUNNY_DATABASE_URL", "libsql://bunny.example");
            std::env::set_var("BUNNY_DATABASE_AUTH_TOKEN", "bunny-secret");
        }

        let mut cfg = Config::default();
        cfg.apply_env().unwrap();
        assert_eq!(cfg.db.url, "libsql://bunny.example");
        assert_eq!(cfg.db.token, "bunny-secret");
    }

    #[test]
    fn require_remote_db_fails_closed_with_url_but_no_token() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        const NAMES: &[&str] = &[
            "JMODGUARD_DB_URL",
            "JMODGUARD_DB_TOKEN",
            "BUNNY_DATABASE_URL",
            "BUNNY_DATABASE_AUTH_TOKEN",
            "JMODGUARD_REQUIRE_REMOTE_DB",
        ];
        let _saved = SavedEnv::capture(NAMES);
        for name in NAMES {
            unsafe { std::env::remove_var(name) };
        }
        unsafe { std::env::set_var("JMODGUARD_REQUIRE_REMOTE_DB", "1") };

        let mut cfg = Config::default();
        cfg.db.url = "libsql://example.bunnydb.test".into();
        let err = cfg.apply_env().unwrap_err();
        assert!(
            err.to_string()
                .contains("complete remote database URL/token pair"),
            "unexpected: {err:#}"
        );
    }

    #[test]
    fn public_bind_rejects_localhost_oauth_origin() {
        let mut cfg = Config::default();
        cfg.web.bind = "0.0.0.0:8080".into();
        cfg.web.base_url = "http://localhost:8080".into();
        let err = cfg.validate_public_base_url().unwrap_err();
        assert!(
            err.to_string().contains("JMODGUARD_WEB_BASE_URL"),
            "unexpected: {err:#}"
        );

        cfg.web.base_url = "https://jmodguard.example.b-cdn.net".into();
        cfg.validate_public_base_url().unwrap();

        for origin in ["https://127.0.0.2", "https://[::]", "https://localhost."] {
            cfg.web.base_url = origin.into();
            assert!(
                cfg.validate_public_base_url().is_err(),
                "public bind must reject {origin}"
            );
        }

        cfg.web.bind = "127.0.0.1:8080".into();
        cfg.web.base_url = "http://localhost:8080".into();
        cfg.validate_public_base_url().unwrap();
    }

    #[test]
    fn invalid_owner_id_env_fails_closed() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        const NAMES: &[&str] = &["JMODGUARD_OWNER_ID"];
        let _saved = SavedEnv::capture(NAMES);
        unsafe { std::env::set_var("JMODGUARD_OWNER_ID", "not-a-snowflake") };
        let mut cfg = Config::default();
        let err = cfg.apply_env().unwrap_err();
        assert!(
            err.to_string().contains("JMODGUARD_OWNER_ID"),
            "unexpected: {err:#}"
        );
    }

    #[test]
    fn pool_size_and_bot_env_override_config_file_defaults() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        const NAMES: &[&str] = &[
            "JMODGUARD_DB_POOL_SIZE",
            "JMODGUARD_DEFAULT_PREFIX",
            "JMODGUARD_RETENTION_DAYS",
            "JMODGUARD_BOT_STATUS",
        ];
        let _saved = SavedEnv::capture(NAMES);
        for name in NAMES {
            unsafe { std::env::remove_var(name) };
        }
        unsafe {
            std::env::set_var("JMODGUARD_DB_POOL_SIZE", "4");
            std::env::set_var("JMODGUARD_DEFAULT_PREFIX", "?");
            std::env::set_var("JMODGUARD_RETENTION_DAYS", "14");
            std::env::set_var("JMODGUARD_BOT_STATUS", "watching");
        }
        let mut cfg = Config::default();
        cfg.apply_env().unwrap();
        assert_eq!(cfg.db.pool_size, 4);
        assert_eq!(cfg.bot.default_prefix, "?");
        assert_eq!(cfg.bot.retention_days, 14);
        assert_eq!(cfg.bot.status, "watching");
    }

    #[test]
    fn empty_bot_status_env_does_not_clear_default() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        const NAMES: &[&str] = &["JMODGUARD_BOT_STATUS"];
        let _saved = SavedEnv::capture(NAMES);
        unsafe { std::env::set_var("JMODGUARD_BOT_STATUS", "  ") };
        let mut cfg = Config::default();
        cfg.apply_env().unwrap();
        assert_eq!(cfg.bot.status, "moderating");
    }

    #[test]
    fn pool_size_env_rejects_below_minimum() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        const NAMES: &[&str] = &["JMODGUARD_DB_POOL_SIZE"];
        let _saved = SavedEnv::capture(NAMES);
        unsafe { std::env::set_var("JMODGUARD_DB_POOL_SIZE", "1") };
        let mut cfg = Config::default();
        let err = cfg.apply_env().unwrap_err();
        assert!(
            err.to_string().contains("JMODGUARD_DB_POOL_SIZE"),
            "unexpected: {err:#}"
        );
    }

    #[test]
    fn retention_days_env_rejects_above_purge_maximum() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        const NAMES: &[&str] = &["JMODGUARD_RETENTION_DAYS"];
        let _saved = SavedEnv::capture(NAMES);
        unsafe { std::env::set_var("JMODGUARD_RETENTION_DAYS", "100001") };
        let mut cfg = Config::default();
        let err = cfg.apply_env().unwrap_err();
        assert!(
            err.to_string().contains("JMODGUARD_RETENTION_DAYS"),
            "unexpected: {err:#}"
        );
        assert!(
            err.to_string().contains("100000") || err.to_string().contains("100_000"),
            "unexpected: {err:#}"
        );
    }

    #[test]
    fn retention_days_file_value_rejects_above_purge_maximum() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        const NAMES: &[&str] = &["JMODGUARD_RETENTION_DAYS"];
        let _saved = SavedEnv::capture(NAMES);
        unsafe { std::env::remove_var("JMODGUARD_RETENTION_DAYS") };
        let mut cfg = Config::default();
        cfg.bot.retention_days = crate::db::MAX_RETENTION_DAYS + 1;
        let err = cfg.apply_env().unwrap_err();
        assert!(
            err.to_string().contains("bot.retention_days"),
            "unexpected: {err:#}"
        );
    }

    #[test]
    fn message_capture_defaults_on_and_has_explicit_disable() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let _saved = SavedEnv::capture(&["JMODGUARD_MESSAGE_CAPTURE_ENABLED"]);
        unsafe { std::env::remove_var("JMODGUARD_MESSAGE_CAPTURE_ENABLED") };
        assert!(Config::default().bot.message_capture_enabled);
        unsafe { std::env::set_var("JMODGUARD_MESSAGE_CAPTURE_ENABLED", "false") };
        let mut cfg = Config::default();
        cfg.apply_env_for(false).unwrap();
        assert!(!cfg.bot.message_capture_enabled);
        unsafe { std::env::set_var("JMODGUARD_MESSAGE_CAPTURE_ENABLED", "maybe") };
        assert!(Config::default().apply_env_for(false).is_err());
    }
}

/// Bot-only queue scheduling. API ignores this file section and its environment keys.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ActionWorkerConfig {
    pub poll_secs: u32,
    pub cleanup_interval_secs: u32,
    #[serde(flatten)]
    pub limits: crate::bot_actions::WorkerLimits,
}

/// One paced directory importer per bot process. Discord imposes its own bucket
/// limits in addition to this deliberately slower installation-wide budget.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MemberSyncConfig {
    pub page_size: u16,
    pub min_request_interval_secs: u32,
    pub request_timeout_secs: u32,
    pub retry_base_secs: u32,
    pub retry_max_secs: u32,
}
impl Default for MemberSyncConfig {
    fn default() -> Self {
        Self {
            page_size: 100,
            min_request_interval_secs: 15,
            request_timeout_secs: 30,
            retry_base_secs: 30,
            retry_max_secs: 3600,
        }
    }
}
impl MemberSyncConfig {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            (1..=500).contains(&self.page_size),
            "member sync page size must be 1..=500"
        );
        anyhow::ensure!(
            self.min_request_interval_secs >= 15,
            "member sync interval must be at least 15 seconds"
        );
        anyhow::ensure!(
            self.request_timeout_secs > 0 && self.request_timeout_secs <= 120,
            "member sync request timeout must be 1..=120 seconds"
        );
        anyhow::ensure!(
            self.retry_base_secs > 0 && self.retry_max_secs >= self.retry_base_secs,
            "invalid member sync retry interval"
        );
        Ok(())
    }
    fn apply_env(&mut self) -> Result<()> {
        for (name, target) in [("JMODGUARD_MEMBER_SYNC_PAGE_SIZE", &mut self.page_size)] {
            if let Ok(value) = env::var(name) {
                *target = value.parse().with_context(|| format!("invalid {name}"))?;
            }
        }
        for (name, target) in [
            (
                "JMODGUARD_MEMBER_SYNC_INTERVAL_SECS",
                &mut self.min_request_interval_secs,
            ),
            (
                "JMODGUARD_MEMBER_SYNC_TIMEOUT_SECS",
                &mut self.request_timeout_secs,
            ),
            (
                "JMODGUARD_MEMBER_SYNC_RETRY_BASE_SECS",
                &mut self.retry_base_secs,
            ),
            (
                "JMODGUARD_MEMBER_SYNC_RETRY_MAX_SECS",
                &mut self.retry_max_secs,
            ),
        ] {
            if let Ok(value) = env::var(name) {
                *target = value.parse().with_context(|| format!("invalid {name}"))?;
            }
        }
        self.validate()
    }
}
impl Default for ActionWorkerConfig {
    fn default() -> Self {
        Self {
            poll_secs: 1,
            cleanup_interval_secs: 3600,
            limits: Default::default(),
        }
    }
}
impl ActionWorkerConfig {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.poll_secs > 0 && self.cleanup_interval_secs > 0,
            "action intervals must be positive"
        );
        self.limits.validate()
    }
    fn apply_env(&mut self) -> Result<()> {
        for (name, target) in [
            ("JMODGUARD_ACTION_POLL_SECS", &mut self.poll_secs),
            (
                "JMODGUARD_ACTION_CLEANUP_INTERVAL_SECS",
                &mut self.cleanup_interval_secs,
            ),
            ("JMODGUARD_ACTION_BATCH_SIZE", &mut self.limits.batch_size),
            (
                "JMODGUARD_ACTION_REQUEST_TIMEOUT_SECS",
                &mut self.limits.request_timeout_secs,
            ),
        ] {
            if let Ok(value) = env::var(name) {
                *target = value.parse().with_context(|| format!("invalid {name}"))?;
            }
        }
        self.validate()
    }
}

#[cfg(test)]
mod logging_privacy_tests {
    use super::*;

    #[test]
    fn remote_database_labels_omit_private_url_parts() {
        for url in [
            "libsql://user:PRIVATE_PASSWORD@example.com/PRIVATE_PATH?token=PRIVATE_QUERY#PRIVATE_FRAGMENT",
            "https://user:PRIVATE_PASSWORD@example.com:8443/PRIVATE_PATH?token=PRIVATE_QUERY",
        ] {
            let cfg = DbConfig {
                url: url.into(),
                ..Default::default()
            };
            let label = cfg.log_label();
            assert_eq!(
                label,
                format!("{}://example.com", url.split(':').next().unwrap())
            );
        }
        let cfg = DbConfig {
            url: "PRIVATE_INVALID_URL".into(),
            ..Default::default()
        };
        assert!(!cfg.log_label().contains("PRIVATE_"));
    }

    #[test]
    fn config_debug_redacts_nested_secrets() {
        let secret = "SENTINEL_PRIVATE_CREDENTIAL";
        let mut bot = BotProcessConfig::default();
        bot.discord.token = secret.into();
        bot.db.url = format!("libsql://user:{secret}@example.com/private?token={secret}");
        bot.db.token = secret.into();
        let api = ApiProcessConfig {
            db: bot.db.clone(),
            web: WebConfig {
                oauth_client_secret: secret.into(),
                ..Default::default()
            },
            ..Default::default()
        };
        for output in [
            format!("{bot:?}"),
            format!("{api:?}"),
            format!("{:?}", bot.db_open_options()),
            format!("{:?}", api.db_open_options()),
        ] {
            assert!(!output.contains(secret), "Debug exposed a credential");
            assert!(!output.contains("/private"), "Debug exposed the DB URL");
            assert!(output.contains("<redacted>"));
        }
    }

    #[test]
    fn config_parse_errors_omit_source_and_values() {
        for api in [false, true] {
            for raw in [
                "[db]\ntoken = \"SENTINEL_PRIVATE_CREDENTIAL",
                "[db]\npool_size = \"SENTINEL_PRIVATE_CREDENTIAL\"",
            ] {
                let error = Config::parse_file(raw, api).unwrap_err();
                let output = format!("{error:#} {error:?}");
                assert!(!output.contains("SENTINEL_PRIVATE_CREDENTIAL"));
                assert!(output.contains("line 2"));
                assert!(output.contains("column"));
            }
        }
    }
}
