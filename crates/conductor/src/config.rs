//! Process configuration only. Guild/bot/API credentials are never loaded here.
use anyhow::{Context as _, Result};
use std::{net::SocketAddr, path::PathBuf, time::Duration};

#[derive(Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub log_filter: String,
    pub shutdown_grace: Duration,
    pub db_path: Option<PathBuf>,
    pub public_url: String,
    pub owner_id: u64,
    pub oauth_client_id: String,
    pub oauth_client_secret: String,
    pub bot_health_url: String,
    pub api_health_url: String,
    pub oauth_token_url: String,
    pub oauth_me_url: String,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("bind", &self.bind)
            .field("log_filter", &self.log_filter)
            .field("shutdown_grace", &self.shutdown_grace)
            .field("db_path", &self.db_path)
            .field("public_url", &self.public_url)
            .field("owner_id", &self.owner_id)
            .field("oauth_client_id", &self.oauth_client_id)
            .field("oauth_client_secret", &"<redacted>")
            .field("bot_health_url", &self.bot_health_url)
            .field("api_health_url", &self.api_health_url)
            .field("oauth_token_url", &self.oauth_token_url)
            .field("oauth_me_url", &self.oauth_me_url)
            .finish()
    }
}

impl Config {
    /// Deliberately does not load the bot/API config file or their environment keys.
    pub fn load() -> Result<Self> {
        Self::from_lookup(|key| match std::env::var(key) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(_) => anyhow::bail!("{key} must contain valid Unicode"),
        })
    }

    fn from_lookup(mut lookup: impl FnMut(&str) -> Result<Option<String>>) -> Result<Self> {
        let bind = lookup("JMODGUARD_CONDUCTOR_BIND")?
            .unwrap_or_else(|| "127.0.0.1:8081".into())
            .parse()
            .context("JMODGUARD_CONDUCTOR_BIND must be an IP address and port")?;
        let log_filter = lookup("JMODGUARD_CONDUCTOR_LOG")?.unwrap_or_else(|| "info".into());
        let seconds: u64 = lookup("JMODGUARD_CONDUCTOR_SHUTDOWN_SECS")?
            .unwrap_or_else(|| "5".into())
            .parse()
            .context("JMODGUARD_CONDUCTOR_SHUTDOWN_SECS must be a positive integer")?;
        anyhow::ensure!(seconds > 0, "shutdown grace must be greater than zero");
        let shutdown_grace = Duration::from_secs(seconds);
        anyhow::ensure!(
            std::time::Instant::now()
                .checked_add(shutdown_grace)
                .is_some(),
            "shutdown grace exceeds the platform clock range"
        );
        let db_path = lookup("JMODGUARD_CONDUCTOR_DB")?
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let public_url = lookup("JMODGUARD_CONDUCTOR_PUBLIC_URL")?
            .map(|value| value.trim().trim_end_matches('/').to_string())
            .unwrap_or_default();
        if !public_url.is_empty() {
            parse_http_origin(&public_url).context("JMODGUARD_CONDUCTOR_PUBLIC_URL")?;
        }
        let owner_raw = lookup("JMODGUARD_CONDUCTOR_OWNER_ID")?.unwrap_or_default();
        let owner_id = if owner_raw.trim().is_empty() {
            0
        } else {
            owner_raw
                .trim()
                .parse()
                .context("JMODGUARD_CONDUCTOR_OWNER_ID must be a Discord snowflake")?
        };
        let oauth_client_id = lookup("JMODGUARD_CONDUCTOR_OAUTH_CLIENT_ID")?
            .unwrap_or_default()
            .trim()
            .to_string();
        let oauth_client_secret = lookup("JMODGUARD_CONDUCTOR_OAUTH_CLIENT_SECRET")?
            .unwrap_or_default()
            .trim()
            .to_string();
        let bot_health_url = optional_health_url(
            lookup("JMODGUARD_CONDUCTOR_BOT_HEALTH_URL")?.unwrap_or_default(),
            "JMODGUARD_CONDUCTOR_BOT_HEALTH_URL",
        )?;
        let api_health_url = optional_health_url(
            lookup("JMODGUARD_CONDUCTOR_API_HEALTH_URL")?.unwrap_or_default(),
            "JMODGUARD_CONDUCTOR_API_HEALTH_URL",
        )?;
        Ok(Self {
            bind,
            log_filter,
            shutdown_grace,
            db_path,
            public_url,
            owner_id,
            oauth_client_id,
            oauth_client_secret,
            bot_health_url,
            api_health_url,
            oauth_token_url: String::new(),
            oauth_me_url: String::new(),
        })
    }

    pub fn operations_configured(&self) -> bool {
        self.db_path.is_some()
            && !self.public_url.is_empty()
            && self.owner_id != 0
            && !self.oauth_client_id.is_empty()
            && !self.oauth_client_secret.is_empty()
    }

    pub fn redirect_uri(&self) -> String {
        format!("{}/oauth/callback", self.public_url.trim_end_matches('/'))
    }

    pub fn cookie_secure(&self) -> bool {
        self.public_url.to_ascii_lowercase().starts_with("https://")
    }
}

pub(crate) fn parse_http_origin(raw: &str) -> Result<String> {
    let url = url::Url::parse(raw).context("must be an absolute http(s) origin")?;
    anyhow::ensure!(
        url.scheme() == "http" || url.scheme() == "https",
        "must be http or https"
    );
    anyhow::ensure!(
        url.username().is_empty() && url.password().is_none(),
        "must not contain credentials"
    );
    anyhow::ensure!(
        url.path() == "/" || url.path().is_empty(),
        "must be an origin without a path"
    );
    anyhow::ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "must not contain a query or fragment"
    );
    Ok(raw.trim_end_matches('/').to_string())
}

fn optional_health_url(raw: String, name: &str) -> Result<String> {
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    parse_http_url(&trimmed).with_context(|| format!("{name} must be an http(s) URL"))
}

pub(crate) fn parse_http_url(raw: &str) -> Result<String> {
    let url = url::Url::parse(raw).context("must be an absolute http(s) URL")?;
    anyhow::ensure!(
        url.scheme() == "http" || url.scheme() == "https",
        "must be http or https"
    );
    anyhow::ensure!(
        url.username().is_empty() && url.password().is_none(),
        "must not contain credentials"
    );
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_are_loopback_and_do_not_request_application_credentials() {
        let mut keys = Vec::new();
        let cfg = Config::from_lookup(|key| {
            keys.push(key.to_owned());
            Ok(None)
        })
        .unwrap();
        assert!(cfg.bind.ip().is_loopback());
        assert_eq!(cfg.bind.port(), 8081);
        assert!(keys.iter().all(|k| k.starts_with("JMODGUARD_CONDUCTOR_")));
        assert!(!cfg.operations_configured());
    }
    #[test]
    fn process_overrides_are_validated_without_environment_mutation() {
        let cfg = Config::from_lookup(|key| {
            Ok(Some(
                match key {
                    "JMODGUARD_CONDUCTOR_BIND" => "[::1]:9090",
                    "JMODGUARD_CONDUCTOR_LOG" => "warn",
                    "JMODGUARD_CONDUCTOR_SHUTDOWN_SECS" => "12",
                    _ => return Ok(None),
                }
                .into(),
            ))
        })
        .unwrap();
        assert_eq!(cfg.bind.port(), 9090);
        assert_eq!(cfg.shutdown_grace, Duration::from_secs(12));
        for (key, value) in [
            ("JMODGUARD_CONDUCTOR_BIND", "not-an-address"),
            ("JMODGUARD_CONDUCTOR_SHUTDOWN_SECS", "0"),
            ("JMODGUARD_CONDUCTOR_SHUTDOWN_SECS", "-1"),
            ("JMODGUARD_CONDUCTOR_SHUTDOWN_SECS", "18446744073709551615"),
            ("JMODGUARD_CONDUCTOR_OWNER_ID", "not-a-snowflake"),
            (
                "JMODGUARD_CONDUCTOR_PUBLIC_URL",
                "https://user:secret@example",
            ),
        ] {
            assert!(
                Config::from_lookup(|k| Ok((k == key).then(|| value.into()))).is_err(),
                "{key}"
            );
        }
    }
}
