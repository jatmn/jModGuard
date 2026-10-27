//! In-memory marks tying automod REST deletes to later gateway delete logs.

use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

const AUTOMOD_DELETE_MARK_TTL: Duration = Duration::from_secs(10 * 60);
pub(crate) const MAX_AUTOMOD_DELETE_MARKS: usize = 4096;

#[derive(Clone)]
struct AutomodDeleteMark {
    rule_id: String,
    rule_name: String,
    confirmed: bool,
    expires: Instant,
}

#[derive(Default)]
pub(crate) struct AutomodDeleteMarks {
    order: VecDeque<(u64, u64, u64)>,
    entries: HashMap<(u64, u64, u64), AutomodDeleteMark>,
}

impl AutomodDeleteMarks {
    fn prune(&mut self, now: Instant) {
        while let Some(key) = self.order.front().copied() {
            let expired = self
                .entries
                .get(&key)
                .is_none_or(|entry| entry.expires <= now);
            if !expired {
                break;
            }
            self.order.pop_front();
            self.entries.remove(&key);
        }
        self.entries.retain(|_, entry| entry.expires > now);
        self.order.retain(|key| self.entries.contains_key(key));
    }

    fn evict_one(&mut self) {
        if let Some(key) = self.order.front().copied().or_else(|| {
            self.entries
                .iter()
                .min_by_key(|(_, entry)| entry.expires)
                .map(|(key, _)| *key)
        }) {
            self.order.retain(|candidate| *candidate != key);
            self.entries.remove(&key);
        }
    }

    pub(crate) fn insert(
        &mut self,
        guild: u64,
        channel: u64,
        message: u64,
        rule_id: &str,
        rule_name: &str,
        now: Instant,
    ) {
        self.prune(now);
        let key = (guild, channel, message);
        if self.entries.len() >= MAX_AUTOMOD_DELETE_MARKS && !self.entries.contains_key(&key) {
            self.evict_one();
        }
        if !self.entries.contains_key(&key) {
            self.order.push_back(key);
        }
        self.entries.insert(
            key,
            AutomodDeleteMark {
                rule_id: rule_id.to_owned(),
                rule_name: rule_name.to_owned(),
                confirmed: false,
                expires: now + AUTOMOD_DELETE_MARK_TTL,
            },
        );
    }

    pub(crate) fn confirm(&mut self, guild: u64, channel: u64, message: u64, now: Instant) {
        self.prune(now);
        let key = (guild, channel, message);
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.confirmed = true;
        }
    }

    pub(crate) fn remove_unconfirmed(
        &mut self,
        guild: u64,
        channel: u64,
        message: u64,
        now: Instant,
    ) {
        self.prune(now);
        let key = (guild, channel, message);
        if self.entries.get(&key).is_some_and(|entry| !entry.confirmed) {
            self.order.retain(|candidate| *candidate != key);
            self.entries.remove(&key);
        }
    }

    /// Consumes a confirmed mark for one gateway delete. Unconfirmed marks are
    /// left in place until REST settles or `remove_unconfirmed` runs.
    pub(crate) fn take(
        &mut self,
        guild: u64,
        channel: u64,
        message: u64,
        now: Instant,
    ) -> Option<(String, String)> {
        self.prune(now);
        let key = (guild, channel, message);
        let entry = self.entries.get(&key)?;
        if !entry.confirmed {
            return None;
        }
        let entry = self.entries.remove(&key)?;
        self.order.retain(|candidate| *candidate != key);
        Some((entry.rule_id, entry.rule_name))
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, guild: u64, channel: u64, message: u64) -> bool {
        self.entries.contains_key(&(guild, channel, message))
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

pub(crate) struct AutomodDeleteMarkAccess {
    pub(crate) inner: Mutex<AutomodDeleteMarks>,
}

impl AutomodDeleteMarkAccess {
    pub(crate) fn insert(
        &self,
        guild: u64,
        channel: u64,
        message: u64,
        rule_id: &str,
        rule_name: &str,
    ) {
        self.inner
            .lock()
            .insert(guild, channel, message, rule_id, rule_name, Instant::now());
    }

    pub(crate) fn confirm(&self, guild: u64, channel: u64, message: u64) {
        self.inner
            .lock()
            .confirm(guild, channel, message, Instant::now());
    }

    pub(crate) fn remove_unconfirmed(&self, guild: u64, channel: u64, message: u64) {
        self.inner
            .lock()
            .remove_unconfirmed(guild, channel, message, Instant::now());
    }

    pub(crate) fn take(&self, guild: u64, channel: u64, message: u64) -> Option<(String, String)> {
        self.inner
            .lock()
            .take(guild, channel, message, Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_expired_marks_do_not_relabel() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        marks.insert(1, 2, 3, "badword", "Bad word", now);
        assert!(
            marks
                .take(
                    1,
                    2,
                    3,
                    now + AUTOMOD_DELETE_MARK_TTL + Duration::from_secs(1)
                )
                .is_none()
        );
    }

    #[test]
    fn cap_evicts_oldest_without_unbounded_growth() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        for message in 0..MAX_AUTOMOD_DELETE_MARKS as u64 + 8 {
            marks.insert(1, 2, message, "badword", "Bad word", now);
        }
        assert_eq!(marks.len(), MAX_AUTOMOD_DELETE_MARKS);
        assert!(!marks.contains(1, 2, 0));
        assert!(marks.contains(1, 2, MAX_AUTOMOD_DELETE_MARKS as u64 + 7));
    }

    #[test]
    fn take_is_single_consumer() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        marks.insert(1, 2, 3, "invite", "Invites", now);
        marks.confirm(1, 2, 3, now);
        assert_eq!(
            marks.take(1, 2, 3, now),
            Some(("invite".into(), "Invites".into()))
        );
        assert!(marks.take(1, 2, 3, now).is_none());
    }

    #[test]
    fn failed_delete_removes_unconfirmed_mark() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        marks.insert(1, 2, 3, "badword", "Bad word", now);
        marks.remove_unconfirmed(1, 2, 3, now);
        assert!(!marks.contains(1, 2, 3));
    }

    #[test]
    fn race_take_before_confirm_then_failure_leaves_no_mark() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        marks.insert(1, 2, 3, "badword", "Bad word", now);
        assert!(marks.take(1, 2, 3, now).is_none());
        marks.remove_unconfirmed(1, 2, 3, now);
        assert!(!marks.contains(1, 2, 3));
    }

    #[test]
    fn unconfirmed_mark_stays_until_confirm_or_remove() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        marks.insert(1, 2, 3, "badword", "Bad word", now);
        assert!(marks.contains(1, 2, 3));
        assert!(marks.take(1, 2, 3, now).is_none());
        marks.confirm(1, 2, 3, now);
        assert_eq!(
            marks.take(1, 2, 3, now),
            Some(("badword".into(), "Bad word".into()))
        );
    }
}
