//! Process-start gateway IDENTIFY budget.
//!
//! Serenity resumes inside this process while the shard still has a session id.
//! A new process has no session, so `Client::start` identifies. This gate reads
//! `GET /gateway/bot` and waits when `session_start_limit` or the identify
//! spacing says another identify would burn the daily budget.

use anyhow::{Context as _, Result};
use serenity::model::gateway::SessionStartLimit;
use std::time::Duration;

use crate::db;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionStartSnapshot {
    pub remaining: u64,
    pub reset_after_ms: u64,
    pub total: u64,
    pub max_concurrency: u64,
}

impl SessionStartSnapshot {
    pub fn from_discord(limit: &SessionStartLimit) -> Self {
        Self {
            remaining: limit.remaining,
            reset_after_ms: limit.reset_after,
            total: limit.total,
            max_concurrency: limit.max_concurrency,
        }
    }
}

/// How long to wait before the next process-start IDENTIFY.
///
/// `since_last` is the age of the previous identify recorded by this bot.
/// `None` means this process has no record, so a positive remaining budget
/// may identify now. A crash loop has a record and must wait.
pub fn identify_delay(limit: &SessionStartSnapshot, since_last: Option<Duration>) -> Duration {
    if limit.remaining == 0 {
        return Duration::from_millis(limit.reset_after_ms.max(1_000));
    }
    let concurrency = limit.max_concurrency.max(1);
    // Discord allows `max_concurrency` identifies per 5 seconds.
    let gap = Duration::from_millis((5_000 / concurrency).max(1));
    let scarce = limit.remaining < 8
        || (limit.total > 0 && limit.remaining.saturating_mul(10) < limit.total);
    let needed = if scarce {
        let share = Duration::from_millis(limit.reset_after_ms / limit.remaining.max(1));
        share.clamp(gap, Duration::from_secs(15 * 60))
    } else {
        gap
    };
    match since_last {
        Some(elapsed) if elapsed < needed => needed.saturating_sub(elapsed),
        _ => Duration::ZERO,
    }
}

pub fn identify_age(stored_ms: Option<i64>, now_ms: i64) -> Option<Duration> {
    let stored = stored_ms?;
    if stored > now_ms {
        return Some(Duration::ZERO);
    }
    let delta = now_ms.saturating_sub(stored);
    Some(Duration::from_millis(
        u64::try_from(delta).unwrap_or(u64::MAX),
    ))
}

async fn sleep_or_shutdown(wait: Duration) -> bool {
    tokio::select! {
        _ = crate::shutdown_signal() => false,
        _ = tokio::time::sleep(wait) => true,
    }
}

/// Wait until an IDENTIFY is allowed, record it, and return whether startup
/// should continue. `false` means the process was asked to shut down.
pub async fn wait_for_gateway_identify(http: &serenity::http::Http, db: &db::Db) -> Result<bool> {
    let mut failure_backoff = Duration::from_secs(2);
    loop {
        match http.get_bot_gateway().await {
            Ok(info) => {
                let stored = db::gateway_last_identify_ms(db).await?;
                let now = chrono::Utc::now().timestamp_millis();
                let since = identify_age(stored, now);
                let snapshot = SessionStartSnapshot::from_discord(&info.session_start_limit);
                let wait = identify_delay(&snapshot, since);
                if wait.is_zero() {
                    db::record_gateway_identify_ms(db, now)
                        .await
                        .context("record gateway identify")?;
                    return Ok(true);
                }
                tracing::warn!(
                    remaining = snapshot.remaining,
                    wait_ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
                    "delaying gateway identify to stay inside the session start limit"
                );
                if !sleep_or_shutdown(wait).await {
                    return Ok(false);
                }
            }
            Err(error) => {
                if crate::discord::classify_retry(&error)
                    == crate::discord::RetryClass::Unauthorized
                {
                    tracing::error!("gateway discovery stopped: Discord rejected the bot token");
                    return Err(error.into());
                }
                tracing::warn!(
                    "gateway session start limit unavailable; not identifying yet: {error}"
                );
                if !sleep_or_shutdown(failure_backoff).await {
                    return Ok(false);
                }
                failure_backoff = (failure_backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit(
        remaining: u64,
        reset_after_ms: u64,
        total: u64,
        max_concurrency: u64,
    ) -> SessionStartSnapshot {
        SessionStartSnapshot {
            remaining,
            reset_after_ms,
            total,
            max_concurrency,
        }
    }

    #[test]
    fn exhausted_budget_waits_for_reset() {
        let wait = identify_delay(&limit(0, 247_000, 1000, 1), None);
        assert_eq!(wait, Duration::from_millis(247_000));
    }

    #[test]
    fn exhausted_budget_does_not_hot_loop_when_reset_is_zero() {
        let wait = identify_delay(&limit(0, 0, 1000, 1), None);
        assert!(wait >= Duration::from_secs(1));
    }

    #[test]
    fn healthy_budget_identifies_immediately_without_a_prior_attempt() {
        let wait = identify_delay(&limit(900, 86_400_000, 1000, 1), None);
        assert_eq!(wait, Duration::ZERO);
    }

    #[test]
    fn identify_spacing_follows_max_concurrency() {
        let recent = Some(Duration::from_secs(1));
        let single = identify_delay(&limit(900, 86_400_000, 1000, 1), recent);
        assert_eq!(single, Duration::from_secs(4));
        let wide = identify_delay(&limit(900, 86_400_000, 1000, 5), recent);
        assert!(wide < single);
    }

    #[test]
    fn scarce_budget_slows_a_restart_loop_but_still_allows_a_later_identify() {
        let scarce = limit(2, 86_400_000, 1000, 1);
        let immediate = identify_delay(&scarce, Some(Duration::from_secs(1)));
        assert_eq!(immediate, Duration::from_secs(15 * 60 - 1));
        let later = identify_delay(&scarce, Some(Duration::from_secs(15 * 60)));
        assert_eq!(later, Duration::ZERO);
    }

    #[test]
    fn future_or_equal_timestamp_is_a_zero_age() {
        assert_eq!(identify_age(Some(50), 40), Some(Duration::ZERO));
        assert_eq!(
            identify_age(Some(1_000), 4_000),
            Some(Duration::from_secs(3))
        );
        assert_eq!(identify_age(None, 4_000), None);
    }
}
