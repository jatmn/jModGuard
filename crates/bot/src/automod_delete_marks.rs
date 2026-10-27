//! In-memory marks tying automod REST deletes to later gateway delete logs.

use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

const AUTOMOD_DELETE_MARK_TTL: Duration = Duration::from_secs(10 * 60);
pub(crate) const MAX_AUTOMOD_DELETE_MARKS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnconfirmedMarkRemoval {
    /// Mark removed without a gateway observation to replay.
    Removed,
    /// Gateway suppressed a plain delete while REST was pending; emit `message_delete`.
    ReplayPlainDelete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GatewayDeleteObservation {
    Automod {
        rule_id: String,
        rule_name: String,
    },
    /// Gateway arrived before REST confirmation; suppress interim plain logging.
    PendingAutomod,
    Plain,
}

#[derive(Clone)]
struct AutomodDeleteMark {
    rule_id: String,
    rule_name: String,
    confirmed: bool,
    gateway_pending: bool,
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
                gateway_pending: false,
                expires: now + AUTOMOD_DELETE_MARK_TTL,
            },
        );
    }

    /// Records a gateway delete and classifies it under one lock so REST
    /// confirmation cannot race between `note_gateway_delete` and `take`.
    pub(crate) fn observe_gateway_delete(
        &mut self,
        guild: u64,
        channel: u64,
        message: u64,
        now: Instant,
    ) -> GatewayDeleteObservation {
        self.prune(now);
        let key = (guild, channel, message);
        let Some(entry) = self.entries.get_mut(&key) else {
            return GatewayDeleteObservation::Plain;
        };
        if entry.confirmed {
            let entry = self.entries.remove(&key).expect("entry present");
            self.order.retain(|candidate| *candidate != key);
            return GatewayDeleteObservation::Automod {
                rule_id: entry.rule_id,
                rule_name: entry.rule_name,
            };
        }
        entry.gateway_pending = true;
        GatewayDeleteObservation::PendingAutomod
    }

    /// Marks REST success. When the gateway delete already logged as plain
    /// `message_delete`, returns rule identity so the caller can emit
    /// `automod_delete` after settlement.
    pub(crate) fn confirm(
        &mut self,
        guild: u64,
        channel: u64,
        message: u64,
        now: Instant,
    ) -> Option<(String, String)> {
        self.prune(now);
        let key = (guild, channel, message);
        let entry = self.entries.get_mut(&key)?;
        entry.confirmed = true;
        if !entry.gateway_pending {
            return None;
        }
        let entry = self.entries.remove(&key)?;
        self.order.retain(|candidate| *candidate != key);
        Some((entry.rule_id, entry.rule_name))
    }

    pub(crate) fn remove_unconfirmed(
        &mut self,
        guild: u64,
        channel: u64,
        message: u64,
        now: Instant,
    ) -> UnconfirmedMarkRemoval {
        self.prune(now);
        let key = (guild, channel, message);
        let Some(entry) = self.entries.get(&key) else {
            return UnconfirmedMarkRemoval::Removed;
        };
        if entry.confirmed {
            return UnconfirmedMarkRemoval::Removed;
        }
        let replay = entry.gateway_pending;
        self.order.retain(|candidate| *candidate != key);
        self.entries.remove(&key);
        if replay {
            UnconfirmedMarkRemoval::ReplayPlainDelete
        } else {
            UnconfirmedMarkRemoval::Removed
        }
    }

    /// Consumes a confirmed mark for one gateway delete. Unconfirmed marks are
    /// left in place until REST settles or `remove_unconfirmed` runs.
    #[cfg(test)]
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

    pub(crate) fn observe_gateway_delete(
        &self,
        guild: u64,
        channel: u64,
        message: u64,
    ) -> GatewayDeleteObservation {
        self.inner
            .lock()
            .observe_gateway_delete(guild, channel, message, Instant::now())
    }

    pub(crate) fn confirm(
        &self,
        guild: u64,
        channel: u64,
        message: u64,
    ) -> Option<(String, String)> {
        self.inner
            .lock()
            .confirm(guild, channel, message, Instant::now())
    }

    pub(crate) fn remove_unconfirmed(
        &self,
        guild: u64,
        channel: u64,
        message: u64,
    ) -> UnconfirmedMarkRemoval {
        self.inner
            .lock()
            .remove_unconfirmed(guild, channel, message, Instant::now())
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
        assert!(marks.confirm(1, 2, 3, now).is_none());
        let expired = now + AUTOMOD_DELETE_MARK_TTL + Duration::from_secs(1);
        marks.prune(expired);
        assert!(marks.take(1, 2, 3, expired).is_none());
        assert!(!marks.contains(1, 2, 3));
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
        assert!(marks.confirm(1, 2, 3, now).is_none());
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
        assert!(marks.confirm(1, 2, 3, now).is_none());
        assert_eq!(
            marks.take(1, 2, 3, now),
            Some(("badword".into(), "Bad word".into()))
        );
    }

    #[test]
    fn gateway_first_confirm_returns_rule_for_reconcile_emit() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        marks.insert(1, 2, 3, "badword", "Bad word", now);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, 3, now),
            GatewayDeleteObservation::PendingAutomod
        );
        assert_eq!(
            marks.confirm(1, 2, 3, now),
            Some(("badword".into(), "Bad word".into()))
        );
        assert!(!marks.contains(1, 2, 3));
    }

    #[test]
    fn observe_gateway_delete_is_atomic_with_confirm() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        marks.insert(1, 2, 3, "badword", "Bad word", now);
        marks.confirm(1, 2, 3, now);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, 3, now),
            GatewayDeleteObservation::Automod {
                rule_id: "badword".into(),
                rule_name: "Bad word".into(),
            }
        );
        assert!(!marks.contains(1, 2, 3));
    }
}
