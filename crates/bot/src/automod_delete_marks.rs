//! Short-lived record that this bot's automod deleted a message.
//!
//! The gateway delete that follows consumes the mark and logs `automod_delete`
//! instead of a plain delete. A missing or expired mark stays a plain delete.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How long a successful delete can still be told apart from a later plain delete.
pub(crate) const TTL: Duration = Duration::from_secs(10 * 60);

const NIL: u32 = u32::MAX;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AutomodDeleteMark {
    pub(crate) rule_id: String,
    pub(crate) rule_name: String,
    pub(crate) reason: String,
}

type MessageKey = (u64, u64, u64);

#[derive(Clone, Copy)]
struct Deadline {
    key: MessageKey,
    at: Instant,
    prev: u32,
    next: u32,
}

/// Live marks plus one insertion-ordered deadline cell per mark.
///
/// The cell index sits on the map entry, so taking or replacing a mark unlinks
/// that cell without walking the map or the list. Reclaim still pops only a due
/// front, which leaves a later deadline blocked behind a live one.
pub(crate) struct AutomodDeleteMarks {
    entries: HashMap<MessageKey, (AutomodDeleteMark, Instant, u32)>,
    deadlines: Vec<Deadline>,
    free: Vec<u32>,
    head: u32,
    tail: u32,
    live_deadlines: usize,
}

impl Default for AutomodDeleteMarks {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            deadlines: Vec::new(),
            free: Vec::new(),
            head: NIL,
            tail: NIL,
            live_deadlines: 0,
        }
    }
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
        if let Some((_, _, index)) = self.entries.remove(&key) {
            self.release(index);
        }
        let index = self.alloc(key, now);
        self.attach_back(index);
        self.entries.insert(key, (mark, now, index));
        debug_assert_eq!(self.live_deadlines, self.entries.len());
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
        let key = (guild, channel, message);
        let Some((mark, at, index)) = self.entries.remove(&key) else {
            debug_assert_eq!(self.live_deadlines, self.entries.len());
            return None;
        };
        self.release(index);
        debug_assert_eq!(self.live_deadlines, self.entries.len());
        if within_ttl(at, now) {
            Some(mark)
        } else {
            None
        }
    }

    #[cfg(test)]
    fn expiry_len(&self) -> usize {
        self.live_deadlines
    }

    /// Pops due fronts only. A live front stays, so later cells are not scanned.
    fn reclaim_expired(&mut self, now: Instant) {
        while self.head != NIL {
            let index = self.head;
            let key = self.deadlines[index as usize].key;
            let at = self.deadlines[index as usize].at;
            if within_ttl(at, now) {
                break;
            }
            self.release(index);
            if self
                .entries
                .get(&key)
                .is_some_and(|(_, stored, stored_index)| *stored == at && *stored_index == index)
            {
                self.entries.remove(&key);
            }
        }
    }

    fn alloc(&mut self, key: MessageKey, at: Instant) -> u32 {
        let deadline = Deadline {
            key,
            at,
            prev: NIL,
            next: NIL,
        };
        if let Some(index) = self.free.pop() {
            self.deadlines[index as usize] = deadline;
            index
        } else {
            let index = u32::try_from(self.deadlines.len()).expect("deadline index fits in u32");
            self.deadlines.push(deadline);
            index
        }
    }

    fn attach_back(&mut self, index: u32) {
        self.deadlines[index as usize].prev = self.tail;
        self.deadlines[index as usize].next = NIL;
        if self.tail == NIL {
            self.head = index;
        } else {
            self.deadlines[self.tail as usize].next = index;
        }
        self.tail = index;
        self.live_deadlines += 1;
    }

    fn release(&mut self, index: u32) {
        let prev = self.deadlines[index as usize].prev;
        let next = self.deadlines[index as usize].next;
        if prev == NIL {
            self.head = next;
        } else {
            self.deadlines[prev as usize].next = next;
        }
        if next == NIL {
            self.tail = prev;
        } else {
            self.deadlines[next as usize].prev = prev;
        }
        self.deadlines[index as usize].prev = NIL;
        self.deadlines[index as usize].next = NIL;
        self.live_deadlines -= 1;
        self.free.push(index);
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
        assert_eq!(marks.expiry_len(), 0);
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
        assert_eq!(marks.entries.len(), 0);
        assert_eq!(marks.expiry_len(), 0);
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
        assert_eq!(marks.expiry_len(), 6);
        assert_eq!(marks.entries.len(), 6);

        assert!(marks.take_live(1, 2, 99, now).is_none());
        assert_eq!(marks.expiry_len(), 3, "only the due prefix is popped");
        assert_eq!(marks.entries.len(), 3, "live marks stay stored");
        assert!(marks.entries.contains_key(&(9, 8, 4)));
        assert!(!marks.entries.contains_key(&(9, 8, 1)));

        assert!(marks.take_live(1, 2, 98, now).is_none());
        assert_eq!(
            marks.expiry_len(),
            3,
            "a lookup with nothing newly due pops nothing"
        );
        assert_eq!(marks.entries.len(), 3);

        let kept = marks.take_live(9, 8, 4, now).expect("live mark");
        assert_eq!(kept.rule_id, "live");
        assert_eq!(marks.entries.len(), 2);
        assert_eq!(
            marks.expiry_len(),
            marks.entries.len(),
            "consuming a live mark drops only its deadline"
        );

        let mut inserted = AutomodDeleteMarks::default();
        for message in 1..=3 {
            inserted.remember(9, 8, message, mark("expired"), start);
        }
        for message in 4..=6 {
            inserted.remember(9, 8, message, mark("live"), live_at);
        }
        inserted.remember(9, 8, 7, mark("fresh"), now);
        assert_eq!(inserted.expiry_len(), 4);
        assert_eq!(inserted.entries.len(), 4);
        assert!(inserted.entries.contains_key(&(9, 8, 7)));
        assert!(!inserted.entries.contains_key(&(9, 8, 2)));

        let mut replaced = AutomodDeleteMarks::default();
        replaced.remember(1, 1, 7, mark("old"), start);
        replaced.remember(1, 1, 7, mark("new"), live_at);
        assert_eq!(replaced.entries.len(), 1);
        assert_eq!(
            replaced.expiry_len(),
            1,
            "remembering the same key again replaces its deadline"
        );
        assert!(replaced.take_live(0, 0, 0, now).is_none());
        let newer = replaced.take_live(1, 1, 7, now).expect("replaced mark");
        assert_eq!(newer.rule_id, "new");
        assert_eq!(replaced.expiry_len(), 0);

        let mut unordered = AutomodDeleteMarks::default();
        unordered.remember(1, 1, 10, mark("live"), live_at);
        unordered.remember(1, 1, 11, mark("expired"), start);
        assert_eq!(unordered.expiry_len(), 2);
        assert!(unordered.take_live(0, 0, 0, now).is_none());
        assert_eq!(
            unordered.expiry_len(),
            2,
            "a live front still blocks a later deadline"
        );
        assert_eq!(unordered.entries.len(), 2);
        assert!(unordered.take_live(1, 1, 11, now).is_none());
        assert_eq!(unordered.entries.len(), 1);
        assert_eq!(
            unordered.expiry_len(),
            1,
            "taking the blocked mark drops only that deadline"
        );
        let still_live = unordered.take_live(1, 1, 10, now).expect("live mark");
        assert_eq!(still_live.rule_id, "live");
        assert_eq!(unordered.expiry_len(), 0);
    }

    #[test]
    fn consumed_burst_behind_live_front_keeps_expiry_bounded() {
        let front_at = Instant::now();
        let burst_at = front_at + Duration::from_secs(1);
        let mut marks = AutomodDeleteMarks::default();
        marks.remember(1, 1, 0, mark("front"), front_at);
        const BURST: u64 = 4_096;
        for message in 1..=BURST {
            marks.remember(1, 2, message, mark("burst"), burst_at);
            let taken = marks
                .take_live(1, 2, message, burst_at)
                .expect("burst mark is live");
            assert_eq!(taken.rule_id, "burst");
        }
        assert_eq!(marks.entries.len(), 1, "the front mark stays live");
        assert!(
            marks.expiry_len() <= marks.entries.len(),
            "expiry {} exceeds live entries {}",
            marks.expiry_len(),
            marks.entries.len()
        );
        marks.remember(3, 3, 1, mark("left"), burst_at);
        marks.remember(3, 3, 2, mark("middle"), burst_at);
        marks.remember(3, 3, 3, mark("right"), burst_at);
        marks.remember(3, 3, 2, mark("middle-again"), burst_at);
        assert_eq!(marks.expiry_len(), 4);
        assert_eq!(marks.entries.len(), 4);
        let middle = marks
            .take_live(3, 3, 2, burst_at)
            .expect("replaced middle mark");
        assert_eq!(middle.rule_id, "middle-again");
        assert!(marks.take_live(3, 3, 1, burst_at).is_some());
        assert!(marks.take_live(3, 3, 3, burst_at).is_some());
        assert_eq!(marks.expiry_len(), 1);
        marks.remember(1, 1, 0, mark("front-again"), burst_at);
        assert_eq!(marks.entries.len(), 1);
        assert_eq!(
            marks.expiry_len(),
            1,
            "remembering the same key again must not stack a deadline"
        );
        let front = marks
            .take_live(1, 1, 0, burst_at)
            .expect("front mark still live");
        assert_eq!(front.rule_id, "front-again");
        assert_eq!(marks.expiry_len(), 0);
        assert_eq!(marks.entries.len(), 0);
    }
}
