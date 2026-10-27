//! Cross-channel attachment burst settings.
//!
//! Guild exact-image thresholds stay on `attachment_flood`. These knobs are
//! process settings for the separate short-window trigger.

use anyhow::{Result, ensure};
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
    /// Distinct channels that carry the same attachment.
    pub min_channels: usize,
    /// How close those posts must be, in seconds.
    pub window_secs: u64,
    /// How long the automatic timeout lasts, in seconds.
    pub timeout_secs: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            min_channels: 2,
            window_secs: 10,
            timeout_secs: 600,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (2..=10).contains(&self.min_channels),
            "attachment burst channel count must be 2–10"
        );
        ensure!(
            (1..=120).contains(&self.window_secs),
            "attachment burst window must be 1–120 seconds"
        );
        ensure!(
            (60..=86_400).contains(&self.timeout_secs),
            "attachment burst timeout must be 60–86400 seconds"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_enabled_and_valid() {
        let settings = Settings::default();
        assert!(settings.enabled);
        assert_eq!(settings.min_channels, 2);
        assert_eq!(settings.window_secs, 10);
        assert_eq!(settings.timeout_secs, 600);
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn rejects_a_single_channel_or_a_zero_timeout() {
        assert!(
            Settings {
                min_channels: 1,
                ..Settings::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            Settings {
                timeout_secs: 0,
                ..Settings::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            Settings {
                window_secs: 0,
                ..Settings::default()
            }
            .validate()
            .is_err()
        );
    }
}
