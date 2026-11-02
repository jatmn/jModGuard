//! Short-lived record that this bot's automod deleted a message.
//!
//! The gateway delete that follows consumes the mark and logs `automod_delete`
//! instead of a plain delete. A missing or expired mark stays a plain delete.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How long a successful delete can still be told apart from a later plain delete.
pub(crate) const TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AutomodDeleteMark {
    pub(crate) rule_id: String,
    pub(crate) rule_name: String,
    pub(crate) reason: String,
}

#[derive(Default)]
pub(crate) struct AutomodDeleteMarks {
    entries: HashMap<(u64, u64, u64), (AutomodDeleteMark, Instant)>,
}

impl AutomodDeleteMarks {
    pub(crate) fn remember(
        &mut self,
        guild: u64,
        channel: u64,
        message: u64,
        mark: AutomodDeleteMark,
        now: Instant,
    ) {
        self.prune(now);
        self.entries.insert((guild, channel, message), (mark, now));
    }

    /// Removes a mark that is still inside [`TTL`]. An expired mark is dropped.
    pub(crate) fn take_live(
        &mut self,
        guild: u64,
        channel: u64,
        message: u64,
        now: Instant,
    ) -> Option<AutomodDeleteMark> {
        self.prune(now);
        self.entries
            .remove(&(guild, channel, message))
            .map(|(mark, _)| mark)
    }

    fn prune(&mut self, now: Instant) {
        self.entries
            .retain(|_, (_, at)| now.saturating_duration_since(*at) < TTL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_consumes_a_live_mark() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        marks.remember(
            1,
            2,
            3,
            AutomodDeleteMark {
                rule_id: "badword".into(),
                rule_name: "Bad words".into(),
                reason: "blocked term".into(),
            },
            now,
        );
        let mark = marks.take_live(1, 2, 3, now).expect("live mark");
        assert_eq!(mark.rule_id, "badword");
        assert_eq!(mark.rule_name, "Bad words");
        assert_eq!(mark.reason, "blocked term");
        assert!(marks.take_live(1, 2, 3, now).is_none());
    }

    #[test]
    fn expired_mark_is_absent() {
        let mut marks = AutomodDeleteMarks::default();
        let then = Instant::now();
        marks.remember(
            1,
            2,
            3,
            AutomodDeleteMark {
                rule_id: "spam".into(),
                rule_name: "Spam".into(),
                reason: "repeated text".into(),
            },
            then,
        );
        assert!(
            marks
                .take_live(1, 2, 3, then + TTL + Duration::from_secs(1))
                .is_none()
        );
    }
}
