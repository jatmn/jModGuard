//! In-memory marks tying automod REST deletes to later gateway delete logs.

use crate::db::guild_data::MessageSnapshot;
use parking_lot::Mutex;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
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

/// Per-REST-delete attempt identity for one message target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct AutomodDeleteAttempt(pub(crate) u64);

type MarkKey = (u64, u64, u64, u64);
type MessageTarget = (u64, u64, u64);

#[derive(Clone)]
struct AutomodDeleteMark {
    rule_id: String,
    rule_name: String,
    confirmed: bool,
    expires: Instant,
    retained: Option<MessageSnapshot>,
}

#[derive(Clone, Default)]
struct MessageGatewayState {
    observed: bool,
    retained: Option<MessageSnapshot>,
    /// Gateway delete already produced or reserved a report class for this message.
    settled: bool,
}

#[derive(Default)]
pub(crate) struct AutomodDeleteMarks {
    next_attempt: u64,
    order: VecDeque<MarkKey>,
    entries: HashMap<MarkKey, AutomodDeleteMark>,
    by_message: HashMap<MessageTarget, Vec<MarkKey>>,
    gateway: HashMap<MessageTarget, MessageGatewayState>,
    mark_expiry_heap: BinaryHeap<Reverse<(Instant, MarkKey)>>,
    gateway_retained_expiry_heap: BinaryHeap<Reverse<(i64, MessageTarget)>>,
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
    fn mark_key(guild: u64, channel: u64, message: u64, attempt: AutomodDeleteAttempt) -> MarkKey {
        (guild, channel, message, attempt.0)
    }

    fn message_target(guild: u64, channel: u64, message: u64) -> MessageTarget {
        (guild, channel, message)
    }

    fn mark_keys_for_message(&self, guild: u64, channel: u64, message: u64) -> Vec<MarkKey> {
        self.by_message
            .get(&Self::message_target(guild, channel, message))
            .cloned()
            .unwrap_or_default()
    }

    fn push_mark_expiry(&mut self, key: MarkKey, expires: Instant) {
        self.mark_expiry_heap.push(Reverse((expires, key)));
    }

    fn store_gateway_retained(&mut self, target: MessageTarget, row: MessageSnapshot, wall: i64) {
        if !snapshot_still_valid(&row, wall) {
            return;
        }
        let expires_at_ms = row.expires_at_ms;
        if let Some(state) = self.gateway.get_mut(&target) {
            state.retained = Some(row);
        }
        self.gateway_retained_expiry_heap
            .push(Reverse((expires_at_ms, target)));
    }

    fn settlement_retained_for(
        gateway: &MessageGatewayState,
        entry_retained: Option<MessageSnapshot>,
    ) -> Option<MessageSnapshot> {
        settlement_retained(gateway.retained.clone().or(entry_retained))
    }

    fn remove_key(&mut self, key: MarkKey) {
        self.entries.remove(&key);
        self.order.retain(|candidate| *candidate != key);
        let target = Self::message_target(key.0, key.1, key.2);
        if let Some(keys) = self.by_message.get_mut(&target) {
            keys.retain(|candidate| *candidate != key);
            if keys.is_empty() {
                self.by_message.remove(&target);
                self.gateway.remove(&target);
            }
        }
    }

    fn message_has_open_attempts(&self, target: MessageTarget) -> bool {
        self.by_message
            .get(&target)
            .is_some_and(|keys| keys.iter().any(|key| self.entries.contains_key(key)))
    }

    fn has_older_unconfirmed(&self, target: MessageTarget, key: MarkKey) -> bool {
        let attempt_id = key.3;
        self.by_message.get(&target).is_some_and(|keys| {
            keys.iter().any(|candidate| {
                candidate.3 < attempt_id
                    && self
                        .entries
                        .get(candidate)
                        .is_some_and(|entry| !entry.confirmed)
            })
        })
    }

    fn clear_expired_retained(&mut self, wall: i64) {
        while let Some(Reverse((heap_expires, target))) =
            self.gateway_retained_expiry_heap.peek().copied()
        {
            if heap_expires > wall {
                break;
            }
            self.gateway_retained_expiry_heap.pop();
            let Some(state) = self.gateway.get_mut(&target) else {
                continue;
            };
            if state
                .retained
                .as_ref()
                .is_some_and(|row| row.expires_at_ms <= wall)
            {
                state.retained = None;
            }
        }
    }

    fn can_evict_for_capacity(
        entry: &AutomodDeleteMark,
        gateway: &MessageGatewayState,
        now: Instant,
    ) -> bool {
        if entry.expires > now {
            return false;
        }
        !(gateway.observed && !gateway.settled && !entry.confirmed)
    }

    fn prune(&mut self, now: Instant) {
        let wall = now_ms();
        self.clear_expired_retained(wall);
        while let Some(Reverse((heap_expires, key))) = self.mark_expiry_heap.peek().copied() {
            if heap_expires > now {
                break;
            }
            self.mark_expiry_heap.pop();
            let target = Self::message_target(key.0, key.1, key.2);
            let gateway = self.gateway.get(&target).cloned().unwrap_or_default();
            let action = self.entries.get(&key).map(|entry| {
                if entry.expires > now {
                    return None;
                }
                if gateway.observed && !gateway.settled && !entry.confirmed {
                    Some(true)
                } else {
                    Some(false)
                }
            });
            match action {
                None => {}
                Some(None) => {}
                Some(Some(true)) => {
                    let new_expires = now + AUTOMOD_DELETE_MARK_TTL;
                    if let Some(entry) = self.entries.get_mut(&key) {
                        entry.expires = new_expires;
                    }
                    self.push_mark_expiry(key, new_expires);
                }
                Some(Some(false)) => self.remove_key(key),
            }
        }
    }

    fn evict_one(&mut self, now: Instant) -> bool {
        for key in &self.order {
            let target = Self::message_target(key.0, key.1, key.2);
            let gateway = self.gateway.get(&target).cloned().unwrap_or_default();
            if self
                .entries
                .get(key)
                .is_some_and(|entry| Self::can_evict_for_capacity(entry, &gateway, now))
            {
                self.remove_key(*key);
                return true;
            }
        }
        false
    }

    pub(crate) fn insert(
        &mut self,
        guild: u64,
        channel: u64,
        message: u64,
        rule_id: &str,
        rule_name: &str,
        now: Instant,
    ) -> AutomodDeleteAttempt {
        self.prune(now);
        let attempt = AutomodDeleteAttempt(self.next_attempt);
        self.next_attempt += 1;
        let key = Self::mark_key(guild, channel, message, attempt);
        while self.entries.len() >= MAX_AUTOMOD_DELETE_MARKS && !self.entries.contains_key(&key) {
            if !self.evict_one(now) {
                return attempt;
            }
        }
        if self.entries.len() >= MAX_AUTOMOD_DELETE_MARKS && !self.entries.contains_key(&key) {
            return attempt;
        }
        self.order.push_back(key);
        let expires = now + AUTOMOD_DELETE_MARK_TTL;
        self.by_message
            .entry(Self::message_target(guild, channel, message))
            .or_default()
            .push(key);
        self.push_mark_expiry(key, expires);
        self.entries.insert(
            key,
            AutomodDeleteMark {
                rule_id: rule_id.to_owned(),
                rule_name: rule_name.to_owned(),
                confirmed: false,
                expires,
                retained: None,
            },
        );
        attempt
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
        let target = Self::message_target(guild, channel, message);
        let keys = self.mark_keys_for_message(guild, channel, message);
        if keys.is_empty() {
            return GatewayDeleteObservation::Plain;
        }
        let already_settled = self.gateway.get(&target).is_some_and(|state| state.settled);
        if !already_settled {
            for key in &keys {
                let Some(entry) = self.entries.get(key) else {
                    continue;
                };
                if entry.confirmed {
                    let rule_id = entry.rule_id.clone();
                    let rule_name = entry.rule_name.clone();
                    self.remove_key(*key);
                    return GatewayDeleteObservation::Automod { rule_id, rule_name };
                }
            }
        }
        let gateway = self.gateway.entry(target).or_default();
        if gateway.settled {
            return GatewayDeleteObservation::PendingAutomod;
        }
        if !gateway.observed {
            gateway.observed = true;
        }
        if let Some(row) = retained {
            self.store_gateway_retained(target, row, now_ms());
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
        attempt: AutomodDeleteAttempt,
        now: Instant,
    ) -> Option<AutomodSettlement> {
        self.prune(now);
        let key = Self::mark_key(guild, channel, message, attempt);
        let target = Self::message_target(guild, channel, message);
        let gateway = self.gateway.get(&target).cloned().unwrap_or_default();
        let defer_settlement =
            gateway.observed && !gateway.settled && self.has_older_unconfirmed(target, key);
        let entry = self.entries.get_mut(&key)?;
        entry.confirmed = true;
        if !gateway.observed || gateway.settled || defer_settlement {
            return None;
        }
        let rule_id = entry.rule_id.clone();
        let rule_name = entry.rule_name.clone();
        let retained = Self::settlement_retained_for(&gateway, entry.retained.clone());
        self.remove_key(key);
        if let Some(state) = self.gateway.get_mut(&target) {
            state.settled = true;
        }
        Some(AutomodSettlement {
            rule_id,
            rule_name,
            retained,
        })
    }

    pub(crate) fn remove_unconfirmed(
        &mut self,
        guild: u64,
        channel: u64,
        message: u64,
        attempt: AutomodDeleteAttempt,
        now: Instant,
    ) -> UnconfirmedRemoval {
        self.prune(now);
        let key = Self::mark_key(guild, channel, message, attempt);
        let target = Self::message_target(guild, channel, message);
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
        let gateway = self.gateway.get(&target).cloned().unwrap_or_default();
        let retained = Self::settlement_retained_for(&gateway, entry.retained.clone());
        self.remove_key(key);
        let replay =
            gateway.observed && !gateway.settled && !self.message_has_open_attempts(target);
        if replay && let Some(state) = self.gateway.get_mut(&target) {
            state.settled = true;
        }
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
        attempt: AutomodDeleteAttempt,
        now: Instant,
    ) -> Option<AutomodSettlement> {
        self.prune(now);
        let key = Self::mark_key(guild, channel, message, attempt);
        let target = Self::message_target(guild, channel, message);
        let gateway = self.gateway.get(&target).cloned().unwrap_or_default();
        let entry = self.entries.get(&key)?;
        if entry.confirmed || !gateway.observed || gateway.settled {
            return None;
        }
        let rule_id = entry.rule_id.clone();
        let rule_name = entry.rule_name.clone();
        let retained = Self::settlement_retained_for(&gateway, entry.retained.clone());
        self.remove_key(key);
        if let Some(state) = self.gateway.get_mut(&target) {
            state.settled = true;
        }
        Some(AutomodSettlement {
            rule_id,
            rule_name,
            retained,
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
        let target = Self::message_target(guild, channel, message);
        if let Some(row) = self
            .gateway
            .get_mut(&target)
            .and_then(|state| settlement_retained(state.retained.take()))
        {
            return Some(row);
        }
        for key in self.mark_keys_for_message(guild, channel, message) {
            if let Some(entry) = self.entries.get_mut(&key)
                && let Some(row) = settlement_retained(entry.retained.take())
            {
                return Some(row);
            }
        }
        None
    }

    #[cfg(test)]
    pub(crate) fn retained_count(&self) -> usize {
        let gateway_retained = self
            .gateway
            .values()
            .filter(|state| state.retained.is_some())
            .count();
        let mark_retained = self
            .entries
            .values()
            .filter(|entry| entry.retained.is_some())
            .count();
        gateway_retained + mark_retained
    }

    #[cfg(test)]
    fn assert_indexes_consistent(&self) {
        assert_eq!(
            self.order
                .iter()
                .filter(|key| self.entries.contains_key(key))
                .count(),
            self.entries.len()
        );
        let indexed = self
            .by_message
            .values()
            .map(|keys| keys.len())
            .sum::<usize>();
        assert_eq!(indexed, self.entries.len());
        for (target, keys) in &self.by_message {
            for key in keys {
                assert!(self.entries.contains_key(key));
                assert_eq!(key.0, target.0);
                assert_eq!(key.1, target.1);
                assert_eq!(key.2, target.2);
            }
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
        for key in self.mark_keys_for_message(guild, channel, message) {
            let Some(entry) = self.entries.get(&key) else {
                continue;
            };
            if !entry.confirmed {
                continue;
            }
            let rule_id = entry.rule_id.clone();
            let rule_name = entry.rule_name.clone();
            self.remove_key(key);
            self.assert_indexes_consistent();
            return Some((rule_id, rule_name));
        }
        None
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, guild: u64, channel: u64, message: u64) -> bool {
        self.mark_keys_for_message(guild, channel, message)
            .iter()
            .any(|key| self.entries.contains_key(key))
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(crate) fn by_message_index_len(&self) -> usize {
        self.by_message.values().map(|keys| keys.len()).sum()
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
    ) -> AutomodDeleteAttempt {
        self.inner
            .lock()
            .insert(guild, channel, message, rule_id, rule_name, Instant::now())
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
        attempt: AutomodDeleteAttempt,
    ) -> Option<AutomodSettlement> {
        self.inner
            .lock()
            .confirm(guild, channel, message, attempt, Instant::now())
    }

    pub(crate) fn remove_unconfirmed(
        &self,
        guild: u64,
        channel: u64,
        message: u64,
        attempt: AutomodDeleteAttempt,
    ) -> UnconfirmedRemoval {
        self.inner
            .lock()
            .remove_unconfirmed(guild, channel, message, attempt, Instant::now())
    }

    pub(crate) fn take_uncertain_gateway_settlement(
        &self,
        guild: u64,
        channel: u64,
        message: u64,
        attempt: AutomodDeleteAttempt,
    ) -> Option<AutomodSettlement> {
        self.inner.lock().take_uncertain_gateway_settlement(
            guild,
            channel,
            message,
            attempt,
            Instant::now(),
        )
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

    fn attempt_for_message(marks: &AutomodDeleteMarks, message: u64) -> AutomodDeleteAttempt {
        AutomodDeleteAttempt(
            marks
                .entries
                .keys()
                .find(|key| key.2 == message)
                .expect("mark")
                .3,
        )
    }

    fn set_mark_expires(marks: &mut AutomodDeleteMarks, message: u64, expires: Instant) {
        let keys: Vec<MarkKey> = marks
            .entries
            .keys()
            .filter(|key| key.2 == message)
            .copied()
            .collect();
        for key in keys {
            if let Some(entry) = marks.entries.get_mut(&key) {
                entry.expires = expires;
            }
            marks.push_mark_expiry(key, expires);
        }
    }

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
        assert!(
            marks
                .confirm(1, 2, 3, attempt_for_message(&marks, 3), now)
                .is_none()
        );
        let expired = now + AUTOMOD_DELETE_MARK_TTL + Duration::from_secs(1);
        marks.prune(expired);
        assert!(marks.take(1, 2, 3, expired).is_none());
        assert!(!marks.contains(1, 2, 3));
        marks.assert_indexes_consistent();
    }

    #[test]
    fn cap_evicts_oldest_without_unbounded_growth() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        for message in 0..MAX_AUTOMOD_DELETE_MARKS as u64 {
            marks.insert(1, 2, message, "badword", "Bad word", now);
        }
        let later = now + Duration::from_secs(30);
        for message in 0..8 {
            set_mark_expires(&mut marks, message, later - Duration::from_millis(1));
        }
        for message in MAX_AUTOMOD_DELETE_MARKS as u64..MAX_AUTOMOD_DELETE_MARKS as u64 + 8 {
            marks.insert(1, 2, message, "badword", "Bad word", later);
        }
        assert_eq!(marks.len(), MAX_AUTOMOD_DELETE_MARKS);
        assert!(!marks.contains(1, 2, 0));
        assert!(marks.contains(1, 2, MAX_AUTOMOD_DELETE_MARKS as u64 + 7));
        marks.assert_indexes_consistent();
    }

    #[test]
    fn cap_does_not_evict_unsettled_in_flight_marks() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        for message in 0..MAX_AUTOMOD_DELETE_MARKS as u64 {
            marks.insert(1, 2, message, "badword", "Bad word", now);
        }
        marks.insert(1, 2, 9_999, "badword", "Bad word", now);
        assert_eq!(marks.len(), MAX_AUTOMOD_DELETE_MARKS);
        assert!(marks.contains(1, 2, 0));
        assert!(!marks.contains(1, 2, 9_999));
    }

    #[test]
    fn expired_confirmed_mark_pruned_behind_refreshed_pending_does_not_relabel() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        marks.insert(1, 2, 1, "stale", "Stale rule", now);
        assert!(
            marks
                .confirm(1, 2, 1, attempt_for_message(&marks, 1), now)
                .is_none()
        );
        marks.insert(1, 2, 2, "front", "Front rule", now);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, 2, None, now),
            GatewayDeleteObservation::PendingAutomod
        );
        let expired = now + AUTOMOD_DELETE_MARK_TTL + Duration::from_secs(1);
        marks.prune(expired);
        assert!(!marks.contains(1, 2, 1));
        assert!(marks.contains(1, 2, 2));
        assert_eq!(
            marks.observe_gateway_delete(1, 2, 1, None, expired),
            GatewayDeleteObservation::Plain
        );
    }

    #[test]
    fn cap_does_not_evict_gateway_pending_mark() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        for message in 0..MAX_AUTOMOD_DELETE_MARKS as u64 {
            marks.insert(1, 2, message, "badword", "Bad word", now);
        }
        assert_eq!(
            marks.observe_gateway_delete(1, 2, 0, None, now),
            GatewayDeleteObservation::PendingAutomod
        );
        marks.insert(1, 2, 9_999, "badword", "Bad word", now);
        assert!(marks.contains(1, 2, 0));
        assert!(!marks.contains(1, 2, 9_999));
    }

    #[test]
    fn prune_drops_expired_mark_behind_extended_front_pending() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        marks.insert(1, 2, 1, "front", "Front", now);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, 1, None, now),
            GatewayDeleteObservation::PendingAutomod
        );
        marks.insert(1, 2, 2, "stale", "Stale", now);
        let expired = now + AUTOMOD_DELETE_MARK_TTL + Duration::from_secs(1);
        marks.prune(expired);
        assert!(marks.contains(1, 2, 1));
        assert!(!marks.contains(1, 2, 2));
    }

    #[test]
    fn prune_clears_expired_retained_behind_extended_front_mark() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        marks.insert(1, 2, 1, "front", "Front", now);
        marks.observe_gateway_delete(1, 2, 1, None, now);
        marks.insert(1, 2, 2, "later", "Later", now);
        marks.observe_gateway_delete(1, 2, 2, Some(sample_retained(2, "stale")), now);
        marks
            .gateway
            .get_mut(&(1, 2, 2))
            .unwrap()
            .retained
            .as_mut()
            .unwrap()
            .expires_at_ms = 1;
        marks.prune(now);
        let settlement = marks
            .confirm(1, 2, 2, attempt_for_message(&marks, 2), now)
            .unwrap();
        assert!(settlement.retained.is_none());
    }

    #[test]
    fn retained_payload_count_tracks_mark_capacity() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        for message in 0..MAX_AUTOMOD_DELETE_MARKS as u64 {
            marks.insert(1, 2, message, "badword", "Bad word", now);
        }
        let later = now + Duration::from_secs(30);
        for message in 0..8 {
            set_mark_expires(&mut marks, message, later - Duration::from_millis(1));
        }
        for message in MAX_AUTOMOD_DELETE_MARKS as u64..MAX_AUTOMOD_DELETE_MARKS as u64 + 8 {
            marks.insert(1, 2, message, "badword", "Bad word", later);
            marks.observe_gateway_delete(
                1,
                2,
                message,
                Some(sample_retained(message, "body")),
                later,
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
        assert!(
            marks
                .confirm(1, 2, 3, attempt_for_message(&marks, 3), now)
                .is_none()
        );
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
        marks.remove_unconfirmed(1, 2, 3, attempt_for_message(&marks, 3), now);
        assert!(!marks.contains(1, 2, 3));
    }

    #[test]
    fn race_take_before_confirm_then_failure_leaves_no_mark() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        marks.insert(1, 2, 3, "badword", "Bad word", now);
        assert!(marks.take(1, 2, 3, now).is_none());
        marks.remove_unconfirmed(1, 2, 3, attempt_for_message(&marks, 3), now);
        assert!(!marks.contains(1, 2, 3));
    }

    #[test]
    fn unconfirmed_mark_stays_until_confirm_or_remove() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        marks.insert(1, 2, 3, "badword", "Bad word", now);
        assert!(marks.contains(1, 2, 3));
        assert!(marks.take(1, 2, 3, now).is_none());
        assert!(
            marks
                .confirm(1, 2, 3, attempt_for_message(&marks, 3), now)
                .is_none()
        );
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
        let settlement = marks
            .confirm(1, 2, 3, attempt_for_message(&marks, 3), now)
            .unwrap();
        assert_eq!(settlement.rule_id, "badword");
        assert!(!marks.contains(1, 2, 3));
    }

    #[test]
    fn observe_gateway_delete_is_atomic_with_confirm() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        marks.insert(1, 2, 3, "badword", "Bad word", now);
        marks.confirm(1, 2, 3, attempt_for_message(&marks, 3), now);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, 3, None, now),
            GatewayDeleteObservation::Automod {
                rule_id: "badword".into(),
                rule_name: "Bad word".into(),
            }
        );
        assert!(!marks.contains(1, 2, 3));
        assert_eq!(marks.by_message_index_len(), 0);
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
        let settlement = marks
            .confirm(1, 2, 3, attempt_for_message(&marks, 3), now)
            .unwrap();
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
            .gateway
            .get_mut(&(1, 2, 3))
            .unwrap()
            .retained
            .as_mut()
            .unwrap()
            .expires_at_ms = 1;
        let settlement = marks
            .confirm(1, 2, 3, attempt_for_message(&marks, 3), now)
            .unwrap();
        assert!(settlement.retained.is_none());
        let mut marks = AutomodDeleteMarks::default();
        marks.insert(1, 2, 4, "badword", "Bad word", now);
        marks.observe_gateway_delete(1, 2, 4, Some(sample_retained(4, "stale uncertain")), now);
        marks
            .gateway
            .get_mut(&(1, 2, 4))
            .unwrap()
            .retained
            .as_mut()
            .unwrap()
            .expires_at_ms = 1;
        marks
            .gateway_retained_expiry_heap
            .push(Reverse((1, (1, 2, 4))));
        marks.prune(now);
        let settlement = marks
            .take_uncertain_gateway_settlement(1, 2, 4, attempt_for_message(&marks, 4), now)
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
                .take_uncertain_gateway_settlement(1, 2, 3, attempt_for_message(&marks, 3), now)
                .is_none()
        );
        marks.remove_unconfirmed(1, 2, 3, attempt_for_message(&marks, 3), now);
        assert!(!marks.contains(1, 2, 3));
    }

    #[test]
    fn uncertain_gateway_settlement_returns_retained_context() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        marks.insert(1, 2, 3, "badword", "Bad word", now);
        marks.observe_gateway_delete(1, 2, 3, Some(sample_retained(3, "uncertain")), now);
        let settlement = marks
            .take_uncertain_gateway_settlement(1, 2, 3, attempt_for_message(&marks, 3), now)
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

    #[test]
    fn overlapping_delete_attempts_settle_with_own_rule_identity() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        let first = marks.insert(1, 2, 3, "badword", "Bad word", now);
        let second = marks.insert(1, 2, 3, "repeat_text", "Repeated text", now);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, 3, None, now),
            GatewayDeleteObservation::PendingAutomod
        );
        let settlement = marks.confirm(1, 2, 3, first, now).unwrap();
        assert_eq!(settlement.rule_id, "badword");
        assert!(marks.contains(1, 2, 3));
        let removal = marks.remove_unconfirmed(1, 2, 3, second, now);
        assert_eq!(removal.action, UnconfirmedMarkRemoval::Removed);
        marks.assert_indexes_consistent();
    }

    #[test]
    fn overlapping_gateway_success_then_sibling_failure_emits_one_report_class() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        let first = marks.insert(1, 2, 3, "badword", "Bad word", now);
        let second = marks.insert(1, 2, 3, "repeat_text", "Repeated text", now);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, 3, None, now),
            GatewayDeleteObservation::PendingAutomod
        );
        marks.confirm(1, 2, 3, first, now).unwrap();
        let removal = marks.remove_unconfirmed(1, 2, 3, second, now);
        assert_eq!(removal.action, UnconfirmedMarkRemoval::Removed);
    }

    #[test]
    fn overlapping_gateway_owner_failure_transfers_to_sibling_before_plain_replay() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        let first = marks.insert(1, 2, 3, "badword", "Bad word", now);
        let second = marks.insert(1, 2, 3, "repeat_text", "Repeated text", now);
        marks.observe_gateway_delete(1, 2, 3, None, now);
        let first_removal = marks.remove_unconfirmed(1, 2, 3, first, now);
        assert_eq!(first_removal.action, UnconfirmedMarkRemoval::Removed);
        let settlement = marks.confirm(1, 2, 3, second, now).unwrap();
        assert_eq!(settlement.rule_id, "repeat_text");
        assert!(!marks.contains(1, 2, 3));
    }

    #[test]
    fn overlapping_gateway_all_fail_replays_plain_once() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        let first = marks.insert(1, 2, 3, "badword", "Bad word", now);
        let second = marks.insert(1, 2, 3, "repeat_text", "Repeated text", now);
        marks.observe_gateway_delete(1, 2, 3, None, now);
        assert_eq!(
            marks.remove_unconfirmed(1, 2, 3, first, now).action,
            UnconfirmedMarkRemoval::Removed
        );
        assert_eq!(
            marks.remove_unconfirmed(1, 2, 3, second, now).action,
            UnconfirmedMarkRemoval::ReplayPlainDelete
        );
    }

    #[test]
    fn later_insert_does_not_erase_gateway_pending_on_earlier_attempt() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        let first = marks.insert(1, 2, 3, "badword", "Bad word", now);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, 3, None, now),
            GatewayDeleteObservation::PendingAutomod
        );
        let _second = marks.insert(1, 2, 3, "repeat_text", "Repeated text", now);
        assert!(marks.confirm(1, 2, 3, first, now).is_some());
    }

    #[test]
    fn confirmed_sibling_blocks_plain_replay_when_owner_cancelled() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        let first = marks.insert(1, 2, 3, "badword", "Bad word", now);
        let second = marks.insert(1, 2, 3, "repeat_text", "Repeated text", now);
        assert!(marks.confirm(1, 2, 3, second, now).is_none());
        assert_eq!(
            marks.observe_gateway_delete(1, 2, 3, None, now),
            GatewayDeleteObservation::Automod {
                rule_id: "repeat_text".into(),
                rule_name: "Repeated text".into(),
            }
        );
        let removal = marks.remove_unconfirmed(1, 2, 3, first, now);
        assert_eq!(removal.action, UnconfirmedMarkRemoval::Removed);
        marks.assert_indexes_consistent();
        assert_eq!(marks.by_message_index_len(), 0);
    }

    #[test]
    fn gateway_retained_survives_owner_transfer_to_sibling() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        let first = marks.insert(1, 2, 3, "badword", "Bad word", now);
        let second = marks.insert(1, 2, 3, "repeat_text", "Repeated text", now);
        marks.observe_gateway_delete(1, 2, 3, Some(sample_retained(3, "transferred body")), now);
        marks.remove_unconfirmed(1, 2, 3, first, now);
        let settlement = marks.confirm(1, 2, 3, second, now).unwrap();
        assert_eq!(
            settlement
                .retained
                .as_ref()
                .and_then(|r| r.fields.content.as_deref()),
            Some("transferred body")
        );
    }

    #[test]
    fn review_successful_consumption_must_not_grow_forever() {
        use std::time::Duration;

        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        for batch in 0..2u64 {
            let at = now + Duration::from_secs(batch * 601);
            for message in batch * 256..(batch + 1) * 256 {
                let attempt = marks.insert(1, 2, message, "badword", "Bad words", at);
                assert!(marks.confirm(1, 2, message, attempt, at).is_none());
                assert!(matches!(
                    marks.observe_gateway_delete(1, 2, message, None, at),
                    GatewayDeleteObservation::Automod { .. }
                ));
            }
            let expired = at + Duration::from_secs(601);
            assert_eq!(
                marks.observe_gateway_delete(1, 2, u64::MAX, None, expired),
                GatewayDeleteObservation::Plain
            );
            assert_eq!(marks.len(), 0);
            marks.assert_indexes_consistent();
            assert_eq!(marks.by_message_index_len(), 0);
            assert!(marks.gateway.is_empty());
        }
    }

    #[test]
    fn review_confirmed_sibling_must_not_replay_plain() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        let first = marks.insert(1, 2, 3, "badword", "Bad words", now);
        let second = marks.insert(1, 2, 3, "repeat_text", "Repeated text", now);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, 3, None, now),
            GatewayDeleteObservation::PendingAutomod
        );
        assert!(marks.confirm(1, 2, 3, second, now).is_none());
        let removal = marks.remove_unconfirmed(1, 2, 3, first, now);
        assert_ne!(removal.action, UnconfirmedMarkRemoval::ReplayPlainDelete);
        let settlement = marks.confirm(1, 2, 3, second, now).unwrap();
        assert_eq!(settlement.rule_id, "repeat_text");
    }

    #[test]
    fn review_owner_transfer_must_keep_retained_snapshot() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        let first = marks.insert(1, 2, 3, "badword", "Bad words", now);
        let second = marks.insert(1, 2, 3, "repeat_text", "Repeated text", now);
        let snapshot = sample_retained(3, "retained observation");
        marks.observe_gateway_delete(1, 2, 3, Some(snapshot.clone()), now);
        let removal = marks.remove_unconfirmed(1, 2, 3, first, now);
        assert_eq!(removal.action, UnconfirmedMarkRemoval::Removed);
        let settlement = marks.confirm(1, 2, 3, second, now).unwrap();
        assert_eq!(settlement.rule_id, "repeat_text");
        assert_eq!(settlement.retained, Some(snapshot));
    }

    #[test]
    fn review_capacity_eviction_does_not_scan_entries_map() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        for message in 0..MAX_AUTOMOD_DELETE_MARKS as u64 {
            marks.insert(1, 2, message, "badword", "Bad word", now);
        }
        let later = now + Duration::from_secs(30);
        for message in 0..8 {
            set_mark_expires(&mut marks, message, later - Duration::from_millis(1));
        }
        for message in MAX_AUTOMOD_DELETE_MARKS as u64..MAX_AUTOMOD_DELETE_MARKS as u64 + 8 {
            marks.insert(1, 2, message, "badword", "Bad word", later);
        }
        assert_eq!(marks.len(), MAX_AUTOMOD_DELETE_MARKS);
        assert!(!marks.contains(1, 2, 0));
        assert!(marks.contains(1, 2, MAX_AUTOMOD_DELETE_MARKS as u64 + 7));
        marks.assert_indexes_consistent();
    }

    #[test]
    fn prune_uses_retained_expiry_heap_without_full_map_scan() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        marks.insert(1, 2, 3, "badword", "Bad word", now);
        marks.observe_gateway_delete(1, 2, 3, Some(sample_retained(3, "heap body")), now);
        let target = AutomodDeleteMarks::message_target(1, 2, 3);
        marks
            .gateway
            .get_mut(&target)
            .unwrap()
            .retained
            .as_mut()
            .unwrap()
            .expires_at_ms = 1;
        marks
            .gateway_retained_expiry_heap
            .push(Reverse((1, target)));
        marks.prune(now);
        let settlement = marks
            .confirm(1, 2, 3, attempt_for_message(&marks, 3), now)
            .unwrap();
        assert!(settlement.retained.is_none());
    }
}
