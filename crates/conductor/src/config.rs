//! Process configuration only. Installation settings and credentials are separate work.
use anyhow::{Context as _, Result};
use std::{net::SocketAddr, time::Duration};

#[derive(Debug)]
pub struct Config {
    pub bind: SocketAddr,
    pub log_filter: String,
    pub shutdown_grace: Duration,
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
        Ok(Self {
            bind,
            log_filter,
            shutdown_grace,
        })
    }
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
    }
    #[test]
    fn process_overrides_are_validated_without_environment_mutation() {
        let cfg = Config::from_lookup(|key| {
            Ok(Some(
                match key {
                    "JMODGUARD_CONDUCTOR_BIND" => "[::1]:9090",
                    "JMODGUARD_CONDUCTOR_LOG" => "warn",
                    "JMODGUARD_CONDUCTOR_SHUTDOWN_SECS" => "12",
                    _ => unreachable!(),
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
        ] {
            assert!(Config::from_lookup(|k| Ok((k == key).then(|| value.into()))).is_err());
        }
    }
}
