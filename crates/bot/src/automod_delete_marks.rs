//! Short-lived record that this bot's automod deleted a message.
//!
//! The gateway delete that follows consumes the mark and logs `automod_delete`
//! instead of a plain delete. A missing or expired mark stays a plain delete.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// How long a successful delete can still be told apart from a later plain delete.
pub(crate) const TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AutomodDeleteMark {
    pub(crate) rule_id: String,
    pub(crate) rule_name: String,
    pub(crate) reason: String,
}

type MessageKey = (u64, u64, u64);

#[derive(Default)]
pub(crate) struct AutomodDeleteMarks {
    entries: HashMap<MessageKey, (AutomodDeleteMark, Instant)>,
    /// Oldest deadline at the front. A record is stale when the stored instant
    /// no longer matches, which happens after a consume or a later remember.
    expiry: VecDeque<(MessageKey, Instant)>,
}

fn within_ttl(at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(at) < TTL
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
        self.reclaim_expired(now);
        let key = (guild, channel, message);
        self.entries.insert(key, (mark, now));
        self.expiry.push_back((key, now));
    }

    /// Removes a mark that is still inside [`TTL`]. An expired mark is dropped.
    pub(crate) fn take_live(
        &mut self,
        guild: u64,
        channel: u64,
        message: u64,
        now: Instant,
    ) -> Option<AutomodDeleteMark> {
        self.reclaim_expired(now);
        match self.entries.remove(&(guild, channel, message)) {
            Some((mark, at)) if within_ttl(at, now) => Some(mark),
            _ => None,
        }
    }

    /// Pops due deadlines only. Live records stay queued, so insert and lookup
    /// never walk the mark map.
    fn reclaim_expired(&mut self, now: Instant) {
        while self
            .expiry
            .front()
            .is_some_and(|(_, at)| !within_ttl(*at, now))
        {
            let Some((key, at)) = self.expiry.pop_front() else {
                break;
            };
            if self
                .entries
                .get(&key)
                .is_some_and(|(_, stored)| *stored == at)
            {
                self.entries.remove(&key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(rule_id: &str) -> AutomodDeleteMark {
        AutomodDeleteMark {
            rule_id: rule_id.into(),
            rule_name: "Rule".into(),
            reason: "because".into(),
        }
    }

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

    #[test]
    fn expired_entries_reclaim_incrementally_and_live_entries_stay() {
        let start = Instant::now();
        let live_at = start + Duration::from_secs(30);
        let now = start + TTL + Duration::from_secs(1);

        let mut marks = AutomodDeleteMarks::default();
        for message in 1..=3 {
            marks.remember(9, 8, message, mark("expired"), start);
        }
        for message in 4..=6 {
            marks.remember(9, 8, message, mark("live"), live_at);
        }
        assert_eq!(marks.expiry.len(), 6);
        assert_eq!(marks.entries.len(), 6);

        assert!(marks.take_live(1, 2, 99, now).is_none());
        assert_eq!(marks.expiry.len(), 3, "only the due prefix is popped");
        assert_eq!(marks.entries.len(), 3, "live marks stay stored");
        assert!(marks.entries.contains_key(&(9, 8, 4)));
        assert!(!marks.entries.contains_key(&(9, 8, 1)));

        assert!(marks.take_live(1, 2, 98, now).is_none());
        assert_eq!(
            marks.expiry.len(),
            3,
            "a lookup with nothing newly due pops nothing"
        );
        assert_eq!(marks.entries.len(), 3);

        let kept = marks.take_live(9, 8, 4, now).expect("live mark");
        assert_eq!(kept.rule_id, "live");
        assert_eq!(marks.entries.len(), 2);
        assert_eq!(
            marks.expiry.len(),
            3,
            "consuming a live mark does not scan the queue"
        );

        let mut inserted = AutomodDeleteMarks::default();
        for message in 1..=3 {
            inserted.remember(9, 8, message, mark("expired"), start);
        }
        for message in 4..=6 {
            inserted.remember(9, 8, message, mark("live"), live_at);
        }
        inserted.remember(9, 8, 7, mark("fresh"), now);
        assert_eq!(inserted.expiry.len(), 4);
        assert_eq!(inserted.entries.len(), 4);
        assert!(inserted.entries.contains_key(&(9, 8, 7)));
        assert!(!inserted.entries.contains_key(&(9, 8, 2)));

        let mut replaced = AutomodDeleteMarks::default();
        replaced.remember(1, 1, 7, mark("old"), start);
        replaced.remember(1, 1, 7, mark("new"), live_at);
        assert!(replaced.take_live(0, 0, 0, now).is_none());
        let newer = replaced.take_live(1, 1, 7, now).expect("replaced mark");
        assert_eq!(newer.rule_id, "new");

        let mut unordered = AutomodDeleteMarks::default();
        unordered.remember(1, 1, 10, mark("live"), live_at);
        unordered.remember(1, 1, 11, mark("expired"), start);
        assert_eq!(unordered.expiry.len(), 2);
        assert!(unordered.take_live(1, 1, 11, now).is_none());
        assert_eq!(
            unordered.expiry.len(),
            2,
            "a blocked deadline is not a map scan"
        );
        let still_live = unordered.take_live(1, 1, 10, now).expect("live mark");
        assert_eq!(still_live.rule_id, "live");
    }
}
