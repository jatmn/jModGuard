//! Durable queue protocol: typed API-to-bot requests. No opaque SQL, URLs, executable code or credentials.
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Unban {
        #[serde(with = "snowflake")]
        user_id: i64,
        reason: String,
    },
    SetLogChannel {
        event: String,
        #[serde(with = "optional_snowflake")]
        channel_id: Option<i64>,
    },
}
mod snowflake {
    pub use crate::ids::de_i64 as deserialize;
    pub use crate::ids::ser_i64_str as serialize;
}
mod optional_snowflake {
    pub use crate::ids::de_opt_i64 as deserialize;
    pub use crate::ids::ser_opt_i64_str as serialize;
}
impl Action {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Unban { user_id, reason } => {
                anyhow::ensure!(*user_id > 0, "user_id must be positive");
                anyhow::ensure!(
                    reason.chars().count() <= 512 && !reason.chars().any(char::is_control),
                    "invalid audit reason"
                );
            }
            Self::SetLogChannel { event, channel_id } => {
                anyhow::ensure!(
                    crate::types::EVENT_TYPES.contains(&event.as_str()),
                    "unknown log event"
                );
                anyhow::ensure!(
                    channel_id.is_none_or(|id| id > 0),
                    "channel_id must be positive"
                );
            }
        }
        Ok(())
    }
    pub fn resource(&self) -> String {
        match self {
            Self::Unban { user_id, .. } => format!("unban:{user_id}"),
            Self::SetLogChannel { event, .. } => format!("log:{event}"),
        }
    }
}

/// Storage limits loaded by bot/API; keep matching admission/retention values.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Limits {
    pub pending_per_guild: u32,
    pub max_age_secs: u32,
    pub history_retention_secs: u32,
    pub cleanup_batch_size: u32,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            pending_per_guild: 32,
            max_age_secs: 300,
            history_retention_secs: 7 * 24 * 3600,
            cleanup_batch_size: 128,
        }
    }
}
impl Limits {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            (1..=4096).contains(&self.pending_per_guild),
            "invalid action queue capacity"
        );
        anyhow::ensure!(
            (1..=86400).contains(&self.max_age_secs),
            "invalid action maximum age"
        );
        anyhow::ensure!(
            self.history_retention_secs > 0,
            "action history retention must be positive"
        );
        anyhow::ensure!(
            (1..=4096).contains(&self.cleanup_batch_size),
            "invalid action cleanup batch size"
        );
        Ok(())
    }
}

/// Only these admission failures are safe to expose without database details.
#[derive(Debug, thiserror::Error)]
pub enum RequestKeyError {
    #[error(
        "The action belongs to an earlier bot membership. Verify its outcome before deliberately starting a new request."
    )]
    EnrollmentChanged,
    #[error(
        "The saved action request is invalid or your device clock is ahead. Check your clock and the previous outcome before starting a new request."
    )]
    Invalid,
    #[error(
        "This action request is too old to admit safely. Check the previous outcome before deliberately submitting a new action; its earlier result may no longer be available."
    )]
    Expired,
}

pub(crate) fn request_key_time(request: &str, now: i64) -> Result<i64> {
    let valid = || -> Option<i64> {
        let (time, nonce) = request.split_once('_')?;
        if request.len() > 64
            || time.is_empty()
            || time.starts_with('0')
            || !time.bytes().all(|b| b.is_ascii_digit())
            || nonce.is_empty()
            || !nonce
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return None;
        }
        let time: i64 = time.parse().ok()?;
        // Five minutes of forward clock skew is a protocol tolerance, not retention.
        (time > 0 && time <= now.checked_add(300)?).then_some(time)
    };
    valid().ok_or_else(|| RequestKeyError::Invalid.into())
}

/// Validated bot dispatch parameters, loaded through action_worker configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WorkerLimits {
    pub batch_size: u32,
    pub request_timeout_secs: u32,
}
impl Default for WorkerLimits {
    fn default() -> Self {
        Self {
            batch_size: 4,
            request_timeout_secs: 15,
        }
    }
}
impl WorkerLimits {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            (1..=64).contains(&self.batch_size),
            "invalid action worker batch size"
        );
        anyhow::ensure!(
            (1..=120).contains(&self.request_timeout_secs),
            "invalid action request timeout"
        );
        Ok(())
    }
}
