//! In-memory marks tying automod REST deletes to later gateway delete logs.

use crate::db::guild_data::MessageSnapshot;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AutomodSettlement {
    pub rule_id: String,
    pub rule_name: String,
    pub retained: Option<MessageSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnconfirmedRemoval {
    pub action: UnconfirmedMarkRemoval,
    pub retained: Option<MessageSnapshot>,
}

#[derive(Clone)]
struct AutomodDeleteMark {
    rule_id: String,
    rule_name: String,
    confirmed: bool,
    gateway_pending: bool,
    expires: Instant,
    retained: Option<MessageSnapshot>,
}

#[derive(Default)]
pub(crate) struct AutomodDeleteMarks {
    order: VecDeque<(u64, u64, u64)>,
    entries: HashMap<(u64, u64, u64), AutomodDeleteMark>,
}

fn snapshot_still_valid(row: &MessageSnapshot, now_ms: i64) -> bool {
    row.expires_at_ms > now_ms
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Drop retained snapshot text when the fixed expiry has passed at settlement time.
pub(crate) fn settlement_retained(row: Option<MessageSnapshot>) -> Option<MessageSnapshot> {
    let wall = now_ms();
    row.filter(|snapshot| snapshot_still_valid(snapshot, wall))
}

impl AutomodDeleteMarks {
    fn prune(&mut self, now: Instant) {
        let wall = now_ms();
        while let Some(key) = self.order.front().copied() {
            let Some(entry) = self.entries.get_mut(&key) else {
                self.order.pop_front();
                continue;
            };
            if entry
                .retained
                .as_ref()
                .is_some_and(|row| !snapshot_still_valid(row, wall))
            {
                entry.retained = None;
            }
            if entry.expires > now {
                break;
            }
            if entry.gateway_pending && !entry.confirmed {
                entry.expires = now + AUTOMOD_DELETE_MARK_TTL;
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
                retained: None,
            },
        );
    }

    fn store_retained(&mut self, key: (u64, u64, u64), row: MessageSnapshot) {
        let wall = now_ms();
        if !snapshot_still_valid(&row, wall) {
            return;
        }
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.retained = Some(row);
        }
    }

    /// Records a gateway delete and classifies it under one lock so REST
    /// confirmation cannot race between observation and retained snapshot storage.
    pub(crate) fn observe_gateway_delete(
        &mut self,
        guild: u64,
        channel: u64,
        message: u64,
        retained: Option<MessageSnapshot>,
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
        entry.expires = now + AUTOMOD_DELETE_MARK_TTL;
        if let Some(row) = retained {
            self.store_retained(key, row);
        }
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
    ) -> Option<AutomodSettlement> {
        self.prune(now);
        let key = (guild, channel, message);
        let entry = self.entries.get_mut(&key)?;
        entry.confirmed = true;
        if !entry.gateway_pending {
            return None;
        }
        let entry = self.entries.remove(&key)?;
        self.order.retain(|candidate| *candidate != key);
        Some(AutomodSettlement {
            rule_id: entry.rule_id,
            rule_name: entry.rule_name,
            retained: settlement_retained(entry.retained),
        })
    }

    pub(crate) fn remove_unconfirmed(
        &mut self,
        guild: u64,
        channel: u64,
        message: u64,
        now: Instant,
    ) -> UnconfirmedRemoval {
        self.prune(now);
        let key = (guild, channel, message);
        let Some(entry) = self.entries.get(&key) else {
            return UnconfirmedRemoval {
                action: UnconfirmedMarkRemoval::Removed,
                retained: None,
            };
        };
        if entry.confirmed {
            return UnconfirmedRemoval {
                action: UnconfirmedMarkRemoval::Removed,
                retained: None,
            };
        }
        let replay = entry.gateway_pending;
        let retained = settlement_retained(entry.retained.clone());
        self.order.retain(|candidate| *candidate != key);
        self.entries.remove(&key);
        UnconfirmedRemoval {
            action: if replay {
                UnconfirmedMarkRemoval::ReplayPlainDelete
            } else {
                UnconfirmedMarkRemoval::Removed
            },
            retained,
        }
    }

    /// REST remained uncertain after a gateway observation; emit without retrying Discord.
    pub(crate) fn take_uncertain_gateway_settlement(
        &mut self,
        guild: u64,
        channel: u64,
        message: u64,
        now: Instant,
    ) -> Option<AutomodSettlement> {
        self.prune(now);
        let key = (guild, channel, message);
        let entry = self.entries.get(&key)?;
        if entry.confirmed || !entry.gateway_pending {
            return None;
        }
        let entry = self.entries.remove(&key)?;
        self.order.retain(|candidate| *candidate != key);
        Some(AutomodSettlement {
            rule_id: entry.rule_id,
            rule_name: entry.rule_name,
            retained: settlement_retained(entry.retained),
        })
    }

    pub(crate) fn take_stored_retained(
        &mut self,
        guild: u64,
        channel: u64,
        message: u64,
        now: Instant,
    ) -> Option<MessageSnapshot> {
        self.prune(now);
        let key = (guild, channel, message);
        let entry = self.entries.get_mut(&key)?;
        settlement_retained(entry.retained.take())
    }

    #[cfg(test)]
    pub(crate) fn retained_count(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| entry.retained.is_some())
            .count()
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
        retained: Option<MessageSnapshot>,
    ) -> GatewayDeleteObservation {
        self.inner
            .lock()
            .observe_gateway_delete(guild, channel, message, retained, Instant::now())
    }

    pub(crate) fn confirm(
        &self,
        guild: u64,
        channel: u64,
        message: u64,
    ) -> Option<AutomodSettlement> {
        self.inner
            .lock()
            .confirm(guild, channel, message, Instant::now())
    }

    pub(crate) fn remove_unconfirmed(
        &self,
        guild: u64,
        channel: u64,
        message: u64,
    ) -> UnconfirmedRemoval {
        self.inner
            .lock()
            .remove_unconfirmed(guild, channel, message, Instant::now())
    }

    pub(crate) fn take_uncertain_gateway_settlement(
        &self,
        guild: u64,
        channel: u64,
        message: u64,
    ) -> Option<AutomodSettlement> {
        self.inner
            .lock()
            .take_uncertain_gateway_settlement(guild, channel, message, Instant::now())
    }

    pub(crate) fn take_stored_retained(
        &self,
        guild: u64,
        channel: u64,
        message: u64,
    ) -> Option<MessageSnapshot> {
        self.inner
            .lock()
            .take_stored_retained(guild, channel, message, Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::guild_data::{MessageSnapshot, SnapshotFields};

    fn sample_retained(message: u64, content: &str) -> MessageSnapshot {
        MessageSnapshot {
            guild_id: 1,
            channel_id: 2,
            message_id: message as i64,
            fields: SnapshotFields {
                content: Some(content.into()),
                author_id: Some(9),
                ..Default::default()
            },
            created_at_ms: 1,
            first_observed_at_ms: 1,
            last_observed_at_ms: 1,
            edit_at_ms: None,
            expires_at_ms: now_ms() + 3_600_000,
            generation: 1,
            coverage_epoch: 1,
            continuous: true,
            content_supplied: true,
            attachments_supplied: false,
        }
    }

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
    fn retained_payload_count_tracks_mark_capacity() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        for message in 0..MAX_AUTOMOD_DELETE_MARKS as u64 + 8 {
            marks.insert(1, 2, message, "badword", "Bad word", now);
            marks.observe_gateway_delete(
                1,
                2,
                message,
                Some(sample_retained(message, "body")),
                now,
            );
        }
        assert_eq!(marks.len(), MAX_AUTOMOD_DELETE_MARKS);
        assert!(marks.retained_count() <= MAX_AUTOMOD_DELETE_MARKS);
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
    fn gateway_pending_mark_survives_prune_until_settled() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        marks.insert(1, 2, 3, "badword", "Bad word", now);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, 3, None, now),
            GatewayDeleteObservation::PendingAutomod
        );
        marks.prune(now + AUTOMOD_DELETE_MARK_TTL + Duration::from_secs(1));
        assert!(marks.contains(1, 2, 3));
    }

    #[test]
    fn gateway_first_confirm_returns_rule_for_reconcile_emit() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        marks.insert(1, 2, 3, "badword", "Bad word", now);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, 3, None, now),
            GatewayDeleteObservation::PendingAutomod
        );
        let settlement = marks.confirm(1, 2, 3, now).unwrap();
        assert_eq!(settlement.rule_id, "badword");
        assert!(!marks.contains(1, 2, 3));
    }

    #[test]
    fn observe_gateway_delete_is_atomic_with_confirm() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        marks.insert(1, 2, 3, "badword", "Bad word", now);
        marks.confirm(1, 2, 3, now);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, 3, None, now),
            GatewayDeleteObservation::Automod {
                rule_id: "badword".into(),
                rule_name: "Bad word".into(),
            }
        );
        assert!(!marks.contains(1, 2, 3));
    }

    #[test]
    fn retained_snapshot_is_stored_with_gateway_pending_observation() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        let row = sample_retained(3, "atomic body");
        marks.insert(1, 2, 3, "badword", "Bad word", now);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, 3, Some(row.clone()), now),
            GatewayDeleteObservation::PendingAutomod
        );
        let settlement = marks.confirm(1, 2, 3, now).unwrap();
        assert_eq!(
            settlement
                .retained
                .as_ref()
                .map(|r| r.fields.content.clone()),
            Some(Some("atomic body".into()))
        );
    }

    #[test]
    fn settlement_drops_expired_retained_snapshot() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        marks.insert(1, 2, 3, "badword", "Bad word", now);
        marks.observe_gateway_delete(1, 2, 3, Some(sample_retained(3, "stale body")), now);
        marks
            .entries
            .get_mut(&(1, 2, 3))
            .unwrap()
            .retained
            .as_mut()
            .unwrap()
            .expires_at_ms = 1;
        let settlement = marks.confirm(1, 2, 3, now).unwrap();
        assert!(settlement.retained.is_none());
        let mut marks = AutomodDeleteMarks::default();
        marks.insert(1, 2, 4, "badword", "Bad word", now);
        marks.observe_gateway_delete(1, 2, 4, Some(sample_retained(4, "stale uncertain")), now);
        marks
            .entries
            .get_mut(&(1, 2, 4))
            .unwrap()
            .retained
            .as_mut()
            .unwrap()
            .expires_at_ms = 1;
        let settlement = marks
            .take_uncertain_gateway_settlement(1, 2, 4, now)
            .unwrap();
        assert!(settlement.retained.is_none());
    }

    #[test]
    fn cancel_without_gateway_observation_clears_unconfirmed_mark() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        marks.insert(1, 2, 3, "badword", "Bad word", now);
        assert!(marks.contains(1, 2, 3));
        assert!(
            marks
                .take_uncertain_gateway_settlement(1, 2, 3, now)
                .is_none()
        );
        marks.remove_unconfirmed(1, 2, 3, now);
        assert!(!marks.contains(1, 2, 3));
    }

    #[test]
    fn uncertain_gateway_settlement_returns_retained_context() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        marks.insert(1, 2, 3, "badword", "Bad word", now);
        marks.observe_gateway_delete(1, 2, 3, Some(sample_retained(3, "uncertain")), now);
        let settlement = marks
            .take_uncertain_gateway_settlement(1, 2, 3, now)
            .unwrap();
        assert_eq!(settlement.rule_id, "badword");
        assert!(
            settlement
                .retained
                .as_ref()
                .and_then(|r| r.fields.content.as_deref())
                .is_some_and(|c| c.contains("uncertain"))
        );
        assert!(!marks.contains(1, 2, 3));
    }
}
