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

        cfg.apply_env()?;
        Ok(cfg)
    }

    /// True when this process is configured for a remote libSQL database.
    pub fn is_remote_db(&self) -> bool {
        !self.db.url.trim().is_empty()
    }

    /// Optional process-local health bind (`JMODGUARD_HEALTH_BIND`).
    /// Empty / unset disables the bot health listener. Container images set
    /// `0.0.0.0:8080` so Magic Containers HTTP GET `/health` has a port.
    pub fn health_bind(&self) -> Option<String> {
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

    /// Reject a public HTTP bind that still uses a localhost OAuth origin.
    ///
    /// Call this from `jmodguard-api` only. Local `127.0.0.1` binds keep the
    /// SETUP.md default `http://localhost:8080`. Container images bind
    /// `0.0.0.0:8080`, so they must set `JMODGUARD_WEB_BASE_URL` to the public
    /// HTTPS hostname.
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
                 (the Magic Containers endpoint) that matches the Discord OAuth redirect.",
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

    fn apply_env(&mut self) -> Result<()> {
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
        if let Ok(v) = env::var("JMODGUARD_OWNER_ID") {
            let v = v.trim();
            if !v.is_empty() {
                self.bot.owner_id = v.parse().with_context(|| {
                    format!("JMODGUARD_OWNER_ID must be a Discord snowflake, got `{v}`")
                })?;
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
        if let Ok(v) = env::var("JMODGUARD_LOG_LEVEL") {
            self.log.level = v.trim().to_string();
        }
        if let Ok(v) = env::var("JMODGUARD_LOG_DIRECTORY") {
            self.log.directory = PathBuf::from(v.trim());
        }
        Ok(())
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
    let host = host.trim().trim_matches(['[', ']']);
    host.eq_ignore_ascii_case("localhost")
        || host == "127.0.0.1"
        || host == "::1"
        || host == "0.0.0.0"
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
            err.to_string().contains("complete remote database URL/token pair"),
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
}
