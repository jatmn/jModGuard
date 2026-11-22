//! Stagger ordinary pollers at startup while preserving their configured periods.

use std::time::Duration;
use tokio::time::{Instant, Interval, MissedTickBehavior};

/// Each poller has its own phase so random jitter cannot put two simultaneous
/// starts in the same 100 ms window. Lease renewals schedule their own retries.
#[derive(Clone, Copy)]
pub enum IdlePoll {
    Enrollment,
    Actions,
    Erasure,
    BanRecovery,
    Nicknames,
    BotVersions,
    ApiVersions,
    Updater,
    BotResources,
    ApiResources,
}

impl IdlePoll {
    pub fn first_tick(self, start: Instant) -> Instant {
        let phase_ms = (self as u64 + 1) * 120;
        let jitter_ms = rand::random_range(0..20);
        start + Duration::from_millis(phase_ms + jitter_ms)
    }

    pub fn interval(self, start: Instant, period: Duration) -> Interval {
        let mut interval = tokio::time::interval_at(self.first_tick(start), period);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        interval
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn idle_pollers_start_more_than_a_hundred_milliseconds_apart() {
        let start = Instant::now();
        let period = Duration::from_secs(5);
        let phases = [
            IdlePoll::Enrollment,
            IdlePoll::Actions,
            IdlePoll::Erasure,
            IdlePoll::BanRecovery,
            IdlePoll::Nicknames,
            IdlePoll::BotVersions,
            IdlePoll::ApiVersions,
            IdlePoll::Updater,
            IdlePoll::BotResources,
            IdlePoll::ApiResources,
        ];
        let mut timers = phases.map(|phase| phase.interval(start, period));
        let mut prior = start;
        for timer in &mut timers {
            let tick = timer.tick().await;
            assert!(tick - prior >= Duration::from_millis(100));
            assert!(tick - start < period);
            prior = tick;
        }
    }
}
