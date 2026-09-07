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
    /// Remote libSQL URL. Empty = local SQLite file at `path`.
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
            discord_concurrency: 8,
            discord_timeout_secs: 10,
        }
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
    pub discord: DiscordConfig,
    pub db: DbConfig,
    pub bot: BotConfig,
    pub log: LogConfig,
}

/// API process configuration. The bot token remains required for direct REST actions;
/// remove it only when those operations have a complete replacement.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ApiProcessConfig {
    pub discord: DiscordConfig,
    pub db: DbConfig,
    pub web: WebConfig,
    pub bot: BotConfig,
    pub log: LogConfig,
}

impl From<Config> for BotProcessConfig {
    fn from(c: Config) -> Self {
        Self {
            discord: c.discord,
            db: c.db,
            bot: c.bot,
            log: c.log,
        }
    }
}
impl From<Config> for ApiProcessConfig {
    fn from(c: Config) -> Self {
        Self {
            discord: c.discord,
            db: c.db,
            web: c.web,
            bot: c.bot,
            log: c.log,
        }
    }
}
impl From<BotProcessConfig> for Config {
    fn from(c: BotProcessConfig) -> Self {
        Self {
            discord: c.discord,
            db: c.db,
            web: WebConfig::default(),
            bot: c.bot,
            log: c.log,
        }
    }
}
impl From<ApiProcessConfig> for Config {
    fn from(c: ApiProcessConfig) -> Self {
        Self {
            discord: c.discord,
            db: c.db,
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
        Ok(cfg)
    }

    fn parse_file(raw: &str, api: bool) -> Result<Self> {
        if api {
            Ok(toml::from_str::<ApiProcessConfig>(raw)?.into())
        } else {
            Ok(toml::from_str::<BotProcessConfig>(raw)?.into())
        }
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
        let url = reqwest::Url::parse(&self.web.base_url)
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
                .parse::<reqwest::header::HeaderValue>()
                .with_context(|| format!("invalid web.cors_origins entry `{origin}`"))?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn apply_env(&mut self) -> Result<()> {
        self.apply_env_for(false)
    }

    fn apply_env_for(&mut self, api: bool) -> Result<()> {
        if let Ok(v) = env::var("JMODGUARD_DISCORD_TOKEN") {
            self.discord.token = v.trim().to_string();
        }
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
    fn bot_file_ignores_dashboard_fields_but_api_validates_them() {
        let raw = "[discord]\ntoken='bot-token'\n[web]\noauth_client_secret=123\nbind=[]\n";
        let bot = Config::parse_file(raw, false).unwrap();
        assert_eq!(bot.discord.token, "bot-token");
        assert!(bot.web.oauth_client_secret.is_empty());
        assert!(Config::parse_file(raw, true).is_err());
        // TOML syntax still has to be valid, even in an ignored section.
        assert!(Config::parse_file("[web]\nbind=[", false).is_err());
        assert!(Config::parse_file("[discord]\ntoken=123", false).is_err());
        assert!(Config::parse_file("[discord]\ntoken=123", true).is_err());
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
        assert_eq!(api.discord.token, "bot-secret");
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
}
