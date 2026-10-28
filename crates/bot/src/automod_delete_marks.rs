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
    /// This delete was already reported, or a bulk transcript still owns it.
    Suppressed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AutomodSettlement {
    pub rule_id: String,
    pub rule_name: String,
    pub retained: Option<MessageSnapshot>,
    /// No individual channel is required. If `automod_delete` and
    /// `message_delete` are both absent at send time and `message_bulk_delete`
    /// remains, send one bulk report. Delivered and ambiguous bulk sends do
    /// not use this path.
    pub fallback_to_bulk: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnconfirmedRemoval {
    pub action: UnconfirmedMarkRemoval,
    pub retained: Option<MessageSnapshot>,
    /// Automod settlement for a younger confirmed sibling deferred until this removal.
    pub released_settlement: Option<AutomodSettlement>,
    /// The bulk transcript that included this delete was delivered.
    pub covered_by_bulk: bool,
    /// This delete is in a bulk transcript whose delivery has not settled.
    pub bulk_inflight: bool,
}

pub(crate) struct DeferredBulkReplay {
    pub message: u64,
    pub retained: Option<MessageSnapshot>,
    /// REST succeeded while the bulk send was still in flight. Emit this instead
    /// of a plain replay when that send fails.
    pub automod: Option<AutomodSettlement>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BulkHold {
    /// The transcript was delivered or its send was ambiguous.
    Retained,
    /// The transcript send has not finished.
    Inflight,
}

/// Where bulk placement assigned a pending automod delete.
/// Later config reloads must not move that assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReportOwner {
    /// The id stayed in the bulk transcript.
    BulkTranscript,
    /// The id was peeled because an individual route existed.
    Individual,
}

/// How [`AutomodDeleteMarkAccess::prepare_delete`] recorded one REST attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeleteDisposition {
    /// A live mark holds the rule until REST settles.
    Stored,
    /// The live-mark cap rejected this attempt and a bounded suppression row
    /// remembers it so the gateway path cannot relabel the delete.
    Unmarked,
    /// The cap rejected another attempt for a message that already has a mark.
    /// That mark still owns correlation.
    Shadowed,
    /// Both the mark cache and the suppression map were full. The delete still
    /// runs. A later gateway echo may also log as `message_delete`.
    Untracked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PreparedAutomodDelete {
    pub attempt: AutomodDeleteAttempt,
    pub disposition: DeleteDisposition,
}

/// What a failed REST delete should do with a pending id that stayed in a bulk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BulkPlainReplay {
    /// The transcript that kept this delete was delivered, or its delivery is ambiguous.
    Covered,
    /// Delivery has not settled; emit only if it fails.
    Deferred,
    /// No delivered or in-flight bulk covers this delete.
    Emit,
}

/// Result of one bulk-transcript attempt.
/// Ambiguous means Discord may already have the body, so a deferred replay stays unsent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BulkDeliveryOutcome {
    Delivered,
    Failed,
    Ambiguous,
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
    /// First bulk-placement decision for this message. Not replaced by a reload.
    report_owner: Option<ReportOwner>,
}

/// Delivery of one message that stayed in a bulk transcript.
/// `covered` is set only after that transcript is actually delivered.
/// `ambiguous` means a timeout or partial send may already have reached Discord.
/// `terminal_at` starts the suppression window once every REST attempt for the
/// message has settled. Until then the row is not eligible for expiry.
struct BulkLogState {
    inflight: bool,
    covered: bool,
    ambiguous: bool,
    deferred: bool,
    deferred_retained: Option<MessageSnapshot>,
    /// Success report held back while this transcript send is still in flight.
    deferred_automod: Option<AutomodSettlement>,
    started: Instant,
    terminal_at: Option<Instant>,
}

/// Outstanding attempt ids remembered on one overflow row. Further overlapping
/// deletes share that row and only increment `extra`, so one message cannot
/// grow the overflow map.
const MAX_OVERFLOW_ATTEMPTS: usize = 64;

/// One automod delete that could not take a live mark. Keyed by message, not
/// walked at prune time: expiry only pops the due prefix of `unmarked_expiry`.
/// Every overlapping attempt for that message shares this row.
struct UnmarkedDelete {
    attempts: Vec<u64>,
    /// Attempts that arrived after `attempts` was full. They share this row
    /// without another id slot.
    extra: u32,
    rule_id: String,
    rule_name: String,
    expires: Instant,
    gateway_observed: bool,
    emitted: bool,
    /// REST already succeeded and no individual route existed yet. The next
    /// gateway observation classifies as `automod_delete` instead of a plain delete.
    rest_succeeded: bool,
}

impl UnmarkedDelete {
    fn note_attempt(&mut self, attempt: u64) {
        if self.attempts.contains(&attempt) {
            return;
        }
        if self.attempts.len() < MAX_OVERFLOW_ATTEMPTS {
            self.attempts.push(attempt);
        } else {
            self.extra = self.extra.saturating_add(1);
        }
    }

    fn owns_attempt(&self, attempt: u64) -> bool {
        self.attempts.contains(&attempt) || self.extra > 0
    }

    /// Drop one outstanding attempt. An id that was stored is removed first.
    /// A full-row extra only counts down when the id was not stored.
    fn claim_attempt(&mut self, attempt: u64) -> bool {
        if let Some(index) = self.attempts.iter().position(|id| *id == attempt) {
            self.attempts.swap_remove(index);
            true
        } else if self.extra > 0 {
            self.extra -= 1;
            true
        } else {
            false
        }
    }

    fn pending(&self) -> bool {
        !self.attempts.is_empty() || self.extra > 0
    }
}

#[derive(Default)]
pub(crate) struct AutomodDeleteMarks {
    next_attempt: u64,
    order: VecDeque<MarkKey>,
    entries: HashMap<MarkKey, AutomodDeleteMark>,
    by_message: HashMap<MessageTarget, Vec<MarkKey>>,
    gateway: HashMap<MessageTarget, MessageGatewayState>,
    /// Pending ids kept in a bulk transcript, keyed until delivery settles.
    bulk_logs: HashMap<MessageTarget, BulkLogState>,
    /// Deadlines for delivered rows and for ambiguous rows whose REST attempts
    /// have settled, earliest at the front. In-flight, deferred, and unsettled
    /// ambiguous rows are not indexed. Prune only pops entries that are already due.
    bulk_expiry: VecDeque<(Instant, MessageTarget)>,
    /// Cap misses that still need an `automod_delete` report. Own cap, own
    /// expiry index. Not scanned when the live mark map is full.
    unmarked: HashMap<MessageTarget, UnmarkedDelete>,
    unmarked_expiry: VecDeque<(Instant, MessageTarget)>,
    unmarked_overflow_warned: bool,
    mark_expiry_heap: BinaryHeap<Reverse<(Instant, MarkKey)>>,
    gateway_retained_expiry_heap: BinaryHeap<Reverse<(i64, MessageTarget)>>,
    #[cfg(test)]
    bulk_expiry_pops: u64,
    /// Front peeks inside `expire_bulk_logs`, including the peek that stops
    /// because the next deadline is still in the future.
    #[cfg(test)]
    bulk_expiry_visits: u64,
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
        self.compact_heaps_if_needed();
    }

    fn compact_mark_expiry_heap(&mut self) {
        let mut fresh = BinaryHeap::new();
        for (key, entry) in &self.entries {
            fresh.push(Reverse((entry.expires, *key)));
        }
        self.mark_expiry_heap = fresh;
    }

    fn compact_gateway_retained_expiry_heap(&mut self) {
        let mut fresh = BinaryHeap::new();
        for (target, state) in &self.gateway {
            if let Some(row) = &state.retained {
                fresh.push(Reverse((row.expires_at_ms, *target)));
            }
        }
        self.gateway_retained_expiry_heap = fresh;
    }

    fn compact_heaps_if_needed(&mut self) {
        let entry_bound = self.entries.len().saturating_mul(2).max(1);
        if self.mark_expiry_heap.len() > entry_bound {
            self.compact_mark_expiry_heap();
        }
        let gateway_bound = self.gateway.len().saturating_mul(2).max(1);
        if self.gateway_retained_expiry_heap.len() > gateway_bound {
            self.compact_gateway_retained_expiry_heap();
        }
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
        self.compact_heaps_if_needed();
    }

    fn settlement_retained_for(
        gateway: &MessageGatewayState,
        entry_retained: Option<MessageSnapshot>,
    ) -> Option<MessageSnapshot> {
        settlement_retained(gateway.retained.clone().or(entry_retained))
    }

    fn compact_order_if_needed(&mut self) {
        if self.order.len() <= self.entries.len().saturating_mul(2).max(1) {
            return;
        }
        let live: VecDeque<MarkKey> = self
            .order
            .iter()
            .copied()
            .filter(|key| self.entries.contains_key(key))
            .collect();
        self.order = live;
    }

    fn remove_key(&mut self, key: MarkKey, now: Instant) {
        self.entries.remove(&key);
        self.compact_order_if_needed();
        let target = Self::message_target(key.0, key.1, key.2);
        if let Some(keys) = self.by_message.get_mut(&target) {
            keys.retain(|candidate| *candidate != key);
            if keys.is_empty() {
                self.by_message.remove(&target);
                self.gateway.remove(&target);
            }
        }
        self.compact_heaps_if_needed();
        self.note_ambiguous_settled(target, now);
    }

    fn finalize_automod_settlement(
        &mut self,
        target: MessageTarget,
        key: MarkKey,
        gateway: &MessageGatewayState,
        now: Instant,
    ) -> AutomodSettlement {
        let entry = self.entries.get(&key).expect("settling mark");
        let rule_id = entry.rule_id.clone();
        let rule_name = entry.rule_name.clone();
        let retained = Self::settlement_retained_for(gateway, entry.retained.clone());
        let fallback_to_bulk = self
            .gateway
            .get(&target)
            .and_then(|state| state.report_owner)
            == Some(ReportOwner::Individual);
        self.remove_key(key, now);
        if let Some(state) = self.gateway.get_mut(&target) {
            state.settled = true;
        }
        AutomodSettlement {
            rule_id,
            rule_name,
            retained,
            fallback_to_bulk,
        }
    }

    fn bulk_hold(&self, target: MessageTarget) -> Option<BulkHold> {
        if self
            .gateway
            .get(&target)
            .and_then(|state| state.report_owner)
            != Some(ReportOwner::BulkTranscript)
        {
            return None;
        }
        let state = self.bulk_logs.get(&target)?;
        if state.covered || state.ambiguous {
            Some(BulkHold::Retained)
        } else if state.inflight {
            Some(BulkHold::Inflight)
        } else {
            None
        }
    }

    /// The bulk transcript already owns this delete. Consume the mark so the
    /// caller does not emit a second success report. An in-flight send keeps
    /// the settlement and releases it only if that send fails.
    fn consume_if_bulk_owns(
        &mut self,
        target: MessageTarget,
        key: MarkKey,
        gateway: &MessageGatewayState,
        now: Instant,
    ) -> bool {
        let Some(hold) = self.bulk_hold(target) else {
            return false;
        };
        let settlement = self.finalize_automod_settlement(target, key, gateway, now);
        if matches!(hold, BulkHold::Inflight)
            && let Some(state) = self.bulk_logs.get_mut(&target)
        {
            // The in-flight send has not reached Discord yet. A later definite
            // failure must still have a destination when the guild only
            // configured message_bulk_delete. Delivered and ambiguous finishes
            // clear this settlement instead of sending it.
            state.deferred_automod = Some(AutomodSettlement {
                fallback_to_bulk: true,
                ..settlement
            });
        }
        true
    }

    fn note_report_owner(&mut self, guild: u64, channel: u64, message: u64, owner: ReportOwner) {
        let state = self
            .gateway
            .entry(Self::message_target(guild, channel, message))
            .or_default();
        if state.report_owner.is_none() {
            state.report_owner = Some(owner);
        }
    }

    fn remember_unmarked(
        &mut self,
        target: MessageTarget,
        attempt: u64,
        rule_id: &str,
        rule_name: &str,
        now: Instant,
    ) {
        self.expire_unmarked(now);
        if let Some(row) = self.unmarked.get_mut(&target) {
            // The first overflow row owns the report. Later attempts share it.
            row.note_attempt(attempt);
            return;
        }
        if self.unmarked.len() >= MAX_AUTOMOD_DELETE_MARKS {
            if !self.unmarked_overflow_warned {
                self.unmarked_overflow_warned = true;
                tracing::warn!(
                    cap = MAX_AUTOMOD_DELETE_MARKS,
                    "automod delete marks and overflow suppression are full; the delete still runs, and a gateway echo may also log as message_delete"
                );
            }
            return;
        }
        let expires = now + AUTOMOD_DELETE_MARK_TTL;
        self.unmarked.insert(
            target,
            UnmarkedDelete {
                attempts: vec![attempt],
                extra: 0,
                rule_id: rule_id.to_owned(),
                rule_name: rule_name.to_owned(),
                expires,
                gateway_observed: false,
                emitted: false,
                rest_succeeded: false,
            },
        );
        if self
            .unmarked_expiry
            .back()
            .is_none_or(|(at, _)| *at <= expires)
        {
            self.unmarked_expiry.push_back((expires, target));
        } else {
            let index = self
                .unmarked_expiry
                .partition_point(|(at, _)| *at <= expires);
            self.unmarked_expiry.insert(index, (expires, target));
        }
    }

    fn expire_unmarked(&mut self, now: Instant) {
        while let Some((deadline, target)) = self.unmarked_expiry.front().copied() {
            if deadline > now {
                break;
            }
            self.unmarked_expiry.pop_front();
            let Some(row) = self.unmarked.get(&target) else {
                continue;
            };
            if row.expires > now {
                continue;
            }
            self.unmarked.remove(&target);
            self.reclaim_overflow_gateway(target);
        }
        if self.unmarked.len() < MAX_AUTOMOD_DELETE_MARKS {
            self.unmarked_overflow_warned = false;
        }
    }

    fn bulk_row_owns(&self, target: MessageTarget) -> bool {
        self.bulk_logs
            .get(&target)
            .is_some_and(|state| state.inflight || state.covered || state.ambiguous)
    }

    fn observe_unmarked(
        &mut self,
        target: MessageTarget,
        retained: Option<MessageSnapshot>,
    ) -> Option<GatewayDeleteObservation> {
        let row = self.unmarked.get(&target)?;
        if row.emitted {
            return Some(GatewayDeleteObservation::Suppressed);
        }
        if row.rest_succeeded {
            let rule_id = row.rule_id.clone();
            let rule_name = row.rule_name.clone();
            if let Some(row) = self.unmarked.get_mut(&target) {
                row.emitted = true;
            }
            if let Some(state) = self.gateway.get_mut(&target) {
                state.settled = true;
            }
            return Some(GatewayDeleteObservation::Automod { rule_id, rule_name });
        }
        if let Some(row) = self.unmarked.get_mut(&target) {
            row.gateway_observed = true;
        }
        let gateway = self.gateway.entry(target).or_default();
        if !gateway.observed {
            gateway.observed = true;
        }
        if let Some(row) = retained {
            self.store_gateway_retained(target, row, now_ms());
        }
        Some(GatewayDeleteObservation::PendingAutomod)
    }

    #[allow(clippy::too_many_arguments)]
    fn settle_unmarked(
        &mut self,
        guild: u64,
        channel: u64,
        message: u64,
        attempt: AutomodDeleteAttempt,
        individual_route: bool,
        bulk_route: bool,
        now: Instant,
    ) -> Option<AutomodSettlement> {
        self.prune(now);
        let target = Self::message_target(guild, channel, message);
        let row = self.unmarked.get(&target)?;
        if !row.owns_attempt(attempt.0) {
            return None;
        }
        if row.emitted {
            if let Some(row) = self.unmarked.get_mut(&target) {
                row.claim_attempt(attempt.0);
            }
            return None;
        }
        let rule_id = row.rule_id.clone();
        let rule_name = row.rule_name.clone();
        let retained = self
            .gateway
            .get(&target)
            .and_then(|state| settlement_retained(state.retained.clone()));
        let owner = self
            .gateway
            .get(&target)
            .and_then(|state| state.report_owner);
        let hold = self.bulk_hold(target);
        let settlement = AutomodSettlement {
            rule_id,
            rule_name,
            retained,
            fallback_to_bulk: bulk_route
                && !matches!(hold, Some(BulkHold::Retained | BulkHold::Inflight)),
        };
        match hold {
            Some(BulkHold::Retained) => {
                self.mark_unmarked_emitted(target, attempt.0);
                None
            }
            Some(BulkHold::Inflight) => {
                if let Some(state) = self.bulk_logs.get_mut(&target) {
                    state.deferred_automod = Some(AutomodSettlement {
                        fallback_to_bulk: true,
                        ..settlement
                    });
                }
                self.mark_unmarked_emitted(target, attempt.0);
                None
            }
            None => match owner {
                Some(ReportOwner::BulkTranscript) => {
                    self.mark_unmarked_emitted(target, attempt.0);
                    if individual_route || bulk_route {
                        Some(settlement)
                    } else {
                        None
                    }
                }
                Some(ReportOwner::Individual) | None => {
                    if individual_route || (owner == Some(ReportOwner::Individual) && bulk_route) {
                        self.mark_unmarked_emitted(target, attempt.0);
                        Some(settlement)
                    } else if owner.is_none() {
                        if let Some(row) = self.unmarked.get_mut(&target) {
                            row.rest_succeeded = true;
                            row.claim_attempt(attempt.0);
                        }
                        None
                    } else {
                        self.mark_unmarked_emitted(target, attempt.0);
                        None
                    }
                }
            },
        }
    }

    fn mark_unmarked_emitted(&mut self, target: MessageTarget, attempt: u64) {
        if let Some(row) = self.unmarked.get_mut(&target) {
            row.emitted = true;
            row.claim_attempt(attempt);
        }
        if let Some(state) = self.gateway.get_mut(&target) {
            state.settled = true;
        }
    }

    /// Drop Gateway state that existed only to correlate an overflow delete,
    /// once that row and any bulk owner are gone. A live mark still needs it.
    fn reclaim_overflow_gateway(&mut self, target: MessageTarget) {
        if self.live_marks(target)
            || self.unmarked.contains_key(&target)
            || self.bulk_logs.contains_key(&target)
        {
            return;
        }
        self.gateway.remove(&target);
    }

    fn remove_unmarked(
        &mut self,
        target: MessageTarget,
        attempt: AutomodDeleteAttempt,
    ) -> Option<UnconfirmedRemoval> {
        let row = self.unmarked.get(&target)?;
        if !row.owns_attempt(attempt.0) {
            return None;
        }
        if row.emitted || row.rest_succeeded {
            if let Some(row) = self.unmarked.get_mut(&target) {
                row.claim_attempt(attempt.0);
            }
            return Some(UnconfirmedRemoval {
                action: UnconfirmedMarkRemoval::Removed,
                retained: None,
                released_settlement: None,
                covered_by_bulk: false,
                bulk_inflight: false,
            });
        }
        let observed = row.gateway_observed;
        let retained = self
            .gateway
            .get(&target)
            .and_then(|state| settlement_retained(state.retained.clone()));
        let still_pending = self.unmarked.get_mut(&target).is_some_and(|row| {
            row.claim_attempt(attempt.0);
            row.pending()
        });
        if still_pending {
            return Some(UnconfirmedRemoval {
                action: UnconfirmedMarkRemoval::Removed,
                retained: None,
                released_settlement: None,
                covered_by_bulk: false,
                bulk_inflight: false,
            });
        }
        self.unmarked.remove(&target);
        if !observed {
            self.reclaim_overflow_gateway(target);
            return Some(UnconfirmedRemoval {
                action: UnconfirmedMarkRemoval::Removed,
                retained: None,
                released_settlement: None,
                covered_by_bulk: false,
                bulk_inflight: false,
            });
        }
        let (covered_by_bulk, bulk_inflight) = self.bulk_coverage(target);
        if covered_by_bulk {
            self.reclaim_overflow_gateway(target);
            return Some(UnconfirmedRemoval {
                action: UnconfirmedMarkRemoval::Removed,
                retained,
                released_settlement: None,
                covered_by_bulk: true,
                bulk_inflight,
            });
        }
        if let Some(state) = self.gateway.get_mut(&target) {
            state.settled = true;
        }
        self.reclaim_overflow_gateway(target);
        Some(UnconfirmedRemoval {
            action: UnconfirmedMarkRemoval::ReplayPlainDelete,
            retained,
            released_settlement: None,
            covered_by_bulk: false,
            bulk_inflight,
        })
    }

    fn take_unmarked_uncertain(
        &mut self,
        target: MessageTarget,
        attempt: AutomodDeleteAttempt,
    ) -> Option<AutomodSettlement> {
        let row = self.unmarked.get(&target)?;
        if !row.owns_attempt(attempt.0)
            || row.emitted
            || row.rest_succeeded
            || !row.gateway_observed
        {
            if row.owns_attempt(attempt.0)
                && (row.emitted || row.rest_succeeded)
                && let Some(row) = self.unmarked.get_mut(&target)
            {
                row.claim_attempt(attempt.0);
            }
            return None;
        }
        let rule_id = row.rule_id.clone();
        let rule_name = row.rule_name.clone();
        let retained = self
            .gateway
            .get(&target)
            .and_then(|state| settlement_retained(state.retained.clone()));
        let owner = self
            .gateway
            .get(&target)
            .and_then(|state| state.report_owner);
        let hold = self.bulk_hold(target);
        if hold.is_some() {
            if matches!(hold, Some(BulkHold::Inflight))
                && let Some(state) = self.bulk_logs.get_mut(&target)
            {
                state.deferred_automod = Some(AutomodSettlement {
                    rule_id,
                    rule_name,
                    retained,
                    fallback_to_bulk: true,
                });
            }
            self.mark_unmarked_emitted(target, attempt.0);
            return None;
        }
        self.mark_unmarked_emitted(target, attempt.0);
        Some(AutomodSettlement {
            rule_id,
            rule_name,
            retained,
            fallback_to_bulk: owner == Some(ReportOwner::Individual),
        })
    }

    fn try_release_deferred_settlement(
        &mut self,
        target: MessageTarget,
        now: Instant,
    ) -> Option<AutomodSettlement> {
        let gateway = self.gateway.get(&target)?.clone();
        if !gateway.observed || gateway.settled {
            return None;
        }
        let keys = self.by_message.get(&target)?.clone();
        for key in keys {
            if !self.entries.get(&key).is_some_and(|entry| entry.confirmed) {
                continue;
            }
            if self.has_older_unconfirmed(target, key) {
                continue;
            }
            if self.consume_if_bulk_owns(target, key, &gateway, now) {
                return None;
            }
            return Some(self.finalize_automod_settlement(target, key, &gateway, now));
        }
        None
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

    fn prune(&mut self, now: Instant) {
        self.expire_bulk_logs(now);
        self.expire_unmarked(now);
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
                Some(Some(false)) => self.remove_key(key, now),
            }
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
    ) -> AutomodDeleteAttempt {
        self.prune(now);
        let attempt = AutomodDeleteAttempt(self.next_attempt);
        self.next_attempt += 1;
        let key = Self::mark_key(guild, channel, message, attempt);
        let target = Self::message_target(guild, channel, message);
        // Prune already removed every expired mark that capacity eviction could
        // take, and renewed protected pending marks. A full cache means reject.
        // The delete still runs. A suppression row keeps the gateway path from
        // relabeling it as a plain message_delete when this message has no mark.
        if self.entries.len() >= MAX_AUTOMOD_DELETE_MARKS && !self.entries.contains_key(&key) {
            if !self.live_marks(target) {
                self.remember_unmarked(target, attempt.0, rule_id, rule_name, now);
            }
            return attempt;
        }
        let expires = now + AUTOMOD_DELETE_MARK_TTL;
        // Insert before indexing expiry. Compaction rebuilds the heap from
        // `entries`, so a push beforehand drops this mark's deadline.
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
        self.order.push_back(key);
        self.by_message
            .entry(Self::message_target(guild, channel, message))
            .or_default()
            .push(key);
        self.push_mark_expiry(key, expires);
        self.unmarked.remove(&target);
        if let Some(state) = self.bulk_logs.get_mut(&target) {
            // A new attempt can still fail and needs the ambiguous row.
            if state.ambiguous {
                state.terminal_at = None;
            }
        }
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
            if let Some(observation) = self.observe_unmarked(target, retained) {
                return observation;
            }
            if self.bulk_row_owns(target)
                || self.gateway.get(&target).is_some_and(|state| state.settled)
            {
                return GatewayDeleteObservation::Suppressed;
            }
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
                    let siblings_remain = self
                        .by_message
                        .get(&target)
                        .is_some_and(|keys| keys.iter().any(|candidate| candidate != key));
                    self.remove_key(*key, now);
                    if siblings_remain {
                        let state = self.gateway.entry(target).or_default();
                        state.observed = true;
                        state.settled = true;
                    }
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
        if self.consume_if_bulk_owns(target, key, &gateway, now) {
            return None;
        }
        Some(self.finalize_automod_settlement(target, key, &gateway, now))
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
            if let Some(removal) = self.remove_unmarked(target, attempt) {
                return removal;
            }
            return UnconfirmedRemoval {
                action: UnconfirmedMarkRemoval::Removed,
                retained: None,
                released_settlement: None,
                covered_by_bulk: false,
                bulk_inflight: false,
            };
        };
        if entry.confirmed {
            return UnconfirmedRemoval {
                action: UnconfirmedMarkRemoval::Removed,
                retained: None,
                released_settlement: None,
                covered_by_bulk: false,
                bulk_inflight: false,
            };
        }
        let (covered_by_bulk, bulk_inflight) = self.bulk_coverage(target);
        let gateway = self.gateway.get(&target).cloned().unwrap_or_default();
        let retained = Self::settlement_retained_for(&gateway, entry.retained.clone());
        let remaining_attempts = self
            .mark_keys_for_message(guild, channel, message)
            .into_iter()
            .filter(|candidate| *candidate != key && self.entries.contains_key(candidate))
            .count();
        self.remove_key(key, now);
        let released_settlement = self.try_release_deferred_settlement(target, now);
        let replay = gateway.observed
            && !gateway.settled
            && remaining_attempts == 0
            && released_settlement.is_none();
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
            released_settlement,
            covered_by_bulk,
            bulk_inflight,
        }
    }

    fn bulk_log_deadline(state: &BulkLogState) -> Option<Instant> {
        if state.inflight || state.deferred {
            return None;
        }
        if state.ambiguous {
            return state
                .terminal_at
                .and_then(|at| at.checked_add(AUTOMOD_DELETE_MARK_TTL));
        }
        state.started.checked_add(AUTOMOD_DELETE_MARK_TTL)
    }

    fn live_marks(&self, target: MessageTarget) -> bool {
        self.by_message
            .get(&target)
            .is_some_and(|keys| keys.iter().any(|key| self.entries.contains_key(key)))
    }

    /// Start the bounded suppression window once an ambiguous row can no longer
    /// be needed by an in-flight send or an unsettled REST attempt.
    fn note_ambiguous_settled(&mut self, target: MessageTarget, now: Instant) {
        if self.live_marks(target) {
            return;
        }
        let ready = self.bulk_logs.get(&target).is_some_and(|state| {
            state.ambiguous && !state.inflight && !state.deferred && state.terminal_at.is_none()
        });
        if !ready {
            return;
        }
        if let Some(state) = self.bulk_logs.get_mut(&target) {
            state.terminal_at = Some(now);
        }
        self.schedule_bulk_expiry(target);
    }

    fn schedule_bulk_expiry(&mut self, target: MessageTarget) {
        let Some(deadline) = self
            .bulk_logs
            .get(&target)
            .and_then(Self::bulk_log_deadline)
        else {
            return;
        };
        if self
            .bulk_expiry
            .back()
            .is_none_or(|(at, _)| *at <= deadline)
        {
            self.bulk_expiry.push_back((deadline, target));
            return;
        }
        let index = self.bulk_expiry.partition_point(|(at, _)| *at <= deadline);
        self.bulk_expiry.insert(index, (deadline, target));
    }

    fn expire_bulk_logs(&mut self, now: Instant) {
        // Pop only the due prefix. Fresh rows and unsettled ambiguous rows sit
        // behind that prefix or are absent from the index.
        while let Some((deadline, target)) = self.bulk_expiry.front().copied() {
            #[cfg(test)]
            {
                self.bulk_expiry_visits = self.bulk_expiry_visits.saturating_add(1);
            }
            if deadline > now {
                break;
            }
            self.bulk_expiry.pop_front();
            #[cfg(test)]
            {
                self.bulk_expiry_pops = self.bulk_expiry_pops.saturating_add(1);
            }
            let Some(state) = self.bulk_logs.get(&target) else {
                continue;
            };
            let Some(current) = Self::bulk_log_deadline(state) else {
                continue;
            };
            if current > now {
                continue;
            }
            self.bulk_logs.remove(&target);
            self.reclaim_overflow_gateway(target);
        }
    }

    fn bulk_coverage(&self, target: MessageTarget) -> (bool, bool) {
        match self.bulk_logs.get(&target) {
            Some(state) => (state.covered || state.ambiguous, state.inflight),
            None => (false, false),
        }
    }

    /// Mark one pending gateway delete as in a bulk transcript.
    /// Returns false when the gateway state is already gone, covered, ambiguous, or at cap.
    fn reserve_bulk_delivery(
        &mut self,
        guild: u64,
        channel: u64,
        message: u64,
        now: Instant,
    ) -> bool {
        let target = Self::message_target(guild, channel, message);
        let pending = self
            .gateway
            .get(&target)
            .is_some_and(|state| state.observed && !state.settled);
        if !pending {
            return false;
        }
        if self.bulk_logs.len() >= MAX_AUTOMOD_DELETE_MARKS && !self.bulk_logs.contains_key(&target)
        {
            return false;
        }
        let state = self.bulk_logs.entry(target).or_insert(BulkLogState {
            inflight: false,
            covered: false,
            ambiguous: false,
            deferred: false,
            deferred_retained: None,
            deferred_automod: None,
            started: now,
            terminal_at: None,
        });
        if state.covered || state.ambiguous {
            return false;
        }
        state.inflight = true;
        state.started = now;
        true
    }

    pub(crate) fn begin_bulk_delivery(
        &mut self,
        guild: u64,
        channel: u64,
        ids: &[u64],
        now: Instant,
    ) {
        self.expire_bulk_logs(now);
        for message in ids {
            let _ = self.reserve_bulk_delivery(guild, channel, *message, now);
        }
    }

    pub(crate) fn finish_bulk_delivery(
        &mut self,
        guild: u64,
        channel: u64,
        ids: &[u64],
        outcome: BulkDeliveryOutcome,
    ) -> Vec<DeferredBulkReplay> {
        let mut replay = Vec::new();
        for message in ids {
            let target = Self::message_target(guild, channel, *message);
            match outcome {
                BulkDeliveryOutcome::Delivered => {
                    if let Some(state) = self.bulk_logs.get_mut(&target) {
                        state.inflight = false;
                        state.covered = true;
                        state.ambiguous = false;
                        state.deferred = false;
                        state.deferred_retained = None;
                        state.deferred_automod = None;
                    }
                    self.schedule_bulk_expiry(target);
                }
                BulkDeliveryOutcome::Ambiguous => {
                    // Keep the entry so a later REST failure cannot emit a second report.
                    // Drop any deferred replay: Discord may already have the transcript.
                    if let Some(state) = self.bulk_logs.get_mut(&target) {
                        if state.covered {
                            continue;
                        }
                        state.inflight = false;
                        state.ambiguous = true;
                        state.deferred = false;
                        state.deferred_retained = None;
                        state.deferred_automod = None;
                    }
                    self.note_ambiguous_settled(target, Instant::now());
                }
                BulkDeliveryOutcome::Failed => {
                    if self
                        .bulk_logs
                        .get(&target)
                        .is_some_and(|state| state.covered || state.ambiguous)
                    {
                        continue;
                    }
                    let Some(state) = self.bulk_logs.remove(&target) else {
                        continue;
                    };
                    if let Some(automod) = state.deferred_automod {
                        replay.push(DeferredBulkReplay {
                            message: *message,
                            retained: automod.retained.clone(),
                            automod: Some(automod),
                        });
                    } else if state.deferred {
                        replay.push(DeferredBulkReplay {
                            message: *message,
                            retained: settlement_retained(state.deferred_retained),
                            automod: None,
                        });
                    }
                    self.reclaim_overflow_gateway(target);
                }
            }
        }
        replay
    }

    /// Decide under one lock so a delivery settlement cannot land between the
    /// coverage read and storing the fallback.
    pub(crate) fn claim_or_defer_plain_replay(
        &mut self,
        guild: u64,
        channel: u64,
        message: u64,
        retained: Option<MessageSnapshot>,
    ) -> BulkPlainReplay {
        let Some(state) = self
            .bulk_logs
            .get_mut(&Self::message_target(guild, channel, message))
        else {
            return BulkPlainReplay::Emit;
        };
        if state.covered || state.ambiguous {
            return BulkPlainReplay::Covered;
        }
        if state.inflight {
            state.deferred = true;
            state.deferred_retained = settlement_retained(retained);
            return BulkPlainReplay::Deferred;
        }
        BulkPlainReplay::Emit
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
        let Some(entry) = self.entries.get(&key) else {
            return self.take_unmarked_uncertain(target, attempt);
        };
        if entry.confirmed || !gateway.observed || gateway.settled {
            return None;
        }
        if self.by_message.get(&target).is_some_and(|keys| {
            keys.iter().any(|candidate| {
                *candidate != key
                    && self
                        .entries
                        .get(candidate)
                        .is_some_and(|sibling| sibling.confirmed)
            })
        }) {
            self.remove_key(key, now);
            return self.try_release_deferred_settlement(target, now);
        }
        if self.consume_if_bulk_owns(target, key, &gateway, now) {
            return None;
        }
        Some(self.finalize_automod_settlement(target, key, &gateway, now))
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
    pub(crate) fn mark_expiry_heap_len(&self) -> usize {
        self.mark_expiry_heap.len()
    }

    #[cfg(test)]
    pub(crate) fn gateway_retained_expiry_heap_len(&self) -> usize {
        self.gateway_retained_expiry_heap.len()
    }

    #[cfg(test)]
    fn assert_heaps_bounded(&self) {
        let entry_bound = self.entries.len().saturating_mul(2).max(1);
        assert!(
            self.mark_expiry_heap_len() <= entry_bound,
            "mark expiry heap {} exceeds bound {}",
            self.mark_expiry_heap_len(),
            entry_bound
        );
        let gateway_bound = self.gateway.len().saturating_mul(2).max(1);
        assert!(
            self.gateway_retained_expiry_heap_len() <= gateway_bound,
            "gateway retained heap {} exceeds bound {}",
            self.gateway_retained_expiry_heap_len(),
            gateway_bound
        );
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
            self.remove_key(key, now);
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
    /// Test and direct callers. Enforcement uses [`Self::prepare_delete`].
    #[cfg_attr(not(test), allow(dead_code))]
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

    /// A pending id stayed in a bulk transcript.
    /// Coverage stays false until [`Self::finish_bulk_delivery`] reports success.
    pub(crate) fn begin_bulk_delivery(&self, guild: u64, channel: u64, ids: &[u64]) {
        self.inner
            .lock()
            .begin_bulk_delivery(guild, channel, ids, Instant::now());
    }

    /// Observe a gateway delete and, when `reserve` is set, mark a pending automod
    /// id in-flight before releasing the lock. A concurrent REST failure then defers
    /// instead of emitting while the caller also keeps the id in the transcript.
    pub(crate) fn observe_and_reserve_bulk_delete(
        &self,
        guild: u64,
        channel: u64,
        message: u64,
        retained: Option<MessageSnapshot>,
        reserve: bool,
    ) -> (GatewayDeleteObservation, bool) {
        let mut marks = self.inner.lock();
        let now = Instant::now();
        let observation = marks.observe_gateway_delete(guild, channel, message, retained, now);
        let mut reserved = false;
        if reserve && matches!(observation, GatewayDeleteObservation::PendingAutomod) {
            // The regression test injects a REST failure here. It must run
            // before reserve and while this lock is still held.
            #[cfg(test)]
            run_bulk_reserve_gap_hook();
            reserved = marks.reserve_bulk_delivery(guild, channel, message, now);
        }
        if matches!(observation, GatewayDeleteObservation::PendingAutomod) {
            if reserved {
                marks.note_report_owner(guild, channel, message, ReportOwner::BulkTranscript);
            } else if !reserve {
                marks.note_report_owner(guild, channel, message, ReportOwner::Individual);
            }
        }
        (observation, reserved)
    }

    /// Record the attempt before the Discord delete. A full mark cache still
    /// returns an attempt so the caller deletes the message.
    pub(crate) fn prepare_delete(
        &self,
        guild: u64,
        channel: u64,
        message: u64,
        rule_id: &str,
        rule_name: &str,
    ) -> PreparedAutomodDelete {
        let mut marks = self.inner.lock();
        let attempt = marks.insert(guild, channel, message, rule_id, rule_name, Instant::now());
        let key = AutomodDeleteMarks::mark_key(guild, channel, message, attempt);
        let target = AutomodDeleteMarks::message_target(guild, channel, message);
        let disposition = if marks.entries.contains_key(&key) {
            DeleteDisposition::Stored
        } else if marks.live_marks(target) {
            DeleteDisposition::Shadowed
        } else if marks.unmarked.contains_key(&target) {
            DeleteDisposition::Unmarked
        } else {
            DeleteDisposition::Untracked
        };
        PreparedAutomodDelete {
            attempt,
            disposition,
        }
    }

    /// Settle a prepared delete after REST success.
    #[allow(clippy::too_many_arguments)]
    /// `individual_route` is `automod_delete` or its `message_delete` fallback.
    /// `bulk_route` is `message_bulk_delete`. Stored marks ignore those flags
    /// and use the placement owner recorded earlier.
    pub(crate) fn confirm_prepared(
        &self,
        guild: u64,
        channel: u64,
        message: u64,
        prepared: &PreparedAutomodDelete,
        individual_route: bool,
        bulk_route: bool,
        rule_id: &str,
        rule_name: &str,
    ) -> Option<AutomodSettlement> {
        let mut marks = self.inner.lock();
        let now = Instant::now();
        match prepared.disposition {
            DeleteDisposition::Stored | DeleteDisposition::Shadowed => {
                marks.confirm(guild, channel, message, prepared.attempt, now)
            }
            DeleteDisposition::Unmarked => marks.settle_unmarked(
                guild,
                channel,
                message,
                prepared.attempt,
                individual_route,
                bulk_route,
                now,
            ),
            DeleteDisposition::Untracked => {
                if !individual_route {
                    return None;
                }
                Some(AutomodSettlement {
                    rule_id: rule_id.to_owned(),
                    rule_name: rule_name.to_owned(),
                    retained: None,
                    fallback_to_bulk: bulk_route,
                })
            }
        }
    }

    pub(crate) fn finish_bulk_delivery(
        &self,
        guild: u64,
        channel: u64,
        ids: &[u64],
        outcome: BulkDeliveryOutcome,
    ) -> Vec<DeferredBulkReplay> {
        self.inner
            .lock()
            .finish_bulk_delivery(guild, channel, ids, outcome)
    }

    pub(crate) fn claim_or_defer_plain_replay(
        &self,
        guild: u64,
        channel: u64,
        message: u64,
        retained: Option<MessageSnapshot>,
    ) -> BulkPlainReplay {
        self.inner
            .lock()
            .claim_or_defer_plain_replay(guild, channel, message, retained)
    }

    /// Test and direct callers. Enforcement success uses [`Self::confirm_prepared`].
    #[cfg_attr(not(test), allow(dead_code))]
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

    #[cfg(test)]
    pub(crate) fn gateway_observed_unsettled(
        &self,
        guild: u64,
        channel: u64,
        message: u64,
    ) -> bool {
        self.inner
            .lock()
            .gateway
            .get(&(guild, channel, message))
            .is_some_and(|state| state.observed && !state.settled)
    }

    #[cfg(test)]
    pub(crate) fn bulk_log_contains(&self, guild: u64, channel: u64, message: u64) -> bool {
        self.inner
            .lock()
            .bulk_logs
            .contains_key(&(guild, channel, message))
    }

    #[cfg(test)]
    pub(crate) fn prune_at(&self, now: Instant) {
        self.inner.lock().prune(now);
    }
}

#[cfg(test)]
type BulkReserveGapHook = Box<dyn Fn() + Send>;

#[cfg(test)]
fn bulk_reserve_gap_slot() -> &'static parking_lot::Mutex<Option<BulkReserveGapHook>> {
    use std::sync::OnceLock;
    static HOOK: OnceLock<parking_lot::Mutex<Option<BulkReserveGapHook>>> = OnceLock::new();
    HOOK.get_or_init(|| parking_lot::Mutex::new(None))
}

/// Test hook invoked after observation and before bulk reservation, while the
/// marks mutex is still held. The closure runs outside this hook mutex.
#[cfg(test)]
pub(crate) fn set_bulk_reserve_gap_hook(hook: Option<BulkReserveGapHook>) {
    *bulk_reserve_gap_slot().lock() = hook;
}

#[cfg(test)]
fn run_bulk_reserve_gap_hook() {
    let hook = bulk_reserve_gap_slot().lock().take();
    if let Some(hook) = hook {
        hook();
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
    fn insert_after_consume_prunes_at_ttl() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        let mut attempts = Vec::new();
        for message in 0..4u64 {
            attempts.push(marks.insert(1, 2, message, "badword", "Bad word", now));
        }
        marks.remove_unconfirmed(1, 2, 0, attempts[0], now);
        marks.remove_unconfirmed(1, 2, 1, attempts[1], now);
        let fresh = marks.insert(1, 2, 99, "badword", "Bad word", now);
        assert!(marks.confirm(1, 2, 99, fresh, now).is_none());
        let expired = now + AUTOMOD_DELETE_MARK_TTL + Duration::from_secs(1);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, 99, None, expired),
            GatewayDeleteObservation::Plain
        );
        assert!(!marks.contains(1, 2, 99));
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
            marks.assert_heaps_bounded();
        }
    }

    #[test]
    fn expiry_heaps_stay_bounded_across_many_completed_marks() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        for message in 0..5_000u64 {
            let attempt = marks.insert(1, 2, message, "badword", "Bad words", now);
            assert!(marks.confirm(1, 2, message, attempt, now).is_none());
            assert!(matches!(
                marks.observe_gateway_delete(1, 2, message, None, now),
                GatewayDeleteObservation::Automod { .. }
            ));
            marks.assert_heaps_bounded();
        }
        assert_eq!(marks.len(), 0);
        marks.assert_heaps_bounded();
        assert!(
            marks.order.len() <= 1,
            "insertion order retained {} stale keys",
            marks.order.len()
        );
    }

    #[test]
    fn gateway_after_younger_confirm_does_not_replay_plain_on_older_failure() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        let first = marks.insert(1, 2, 3, "badword", "Bad words", now);
        let second = marks.insert(1, 2, 3, "repeat_text", "Repeated text", now);
        assert!(marks.confirm(1, 2, 3, second, now).is_none());
        assert!(matches!(
            marks.observe_gateway_delete(1, 2, 3, None, now),
            GatewayDeleteObservation::Automod { rule_id, .. } if rule_id == "repeat_text"
        ));
        let removal = marks.remove_unconfirmed(1, 2, 3, first, now);
        assert_eq!(removal.action, UnconfirmedMarkRemoval::Removed);
        assert!(removal.released_settlement.is_none());
    }

    #[test]
    fn uncertain_older_attempt_releases_deferred_younger_success() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        let first = marks.insert(1, 2, 3, "badword", "Bad words", now);
        let second = marks.insert(1, 2, 3, "repeat_text", "Repeated text", now);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, 3, None, now),
            GatewayDeleteObservation::PendingAutomod
        );
        assert!(marks.confirm(1, 2, 3, second, now).is_none());
        let settlement = marks
            .take_uncertain_gateway_settlement(1, 2, 3, first, now)
            .expect("older uncertainty releases younger automod settlement");
        assert_eq!(settlement.rule_id, "repeat_text");
        assert!(marks.confirm(1, 2, 3, second, now).is_none());
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
        let settlement = removal
            .released_settlement
            .expect("deferred younger success released on older failure");
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

    #[test]
    fn bulk_log_ttl_keeps_ambiguous_inflight_and_deferred_rows() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        for message in 1..=4 {
            marks.insert(1, 2, message, "badword", "Bad words", now);
            assert_eq!(
                marks.observe_gateway_delete(1, 2, message, None, now),
                GatewayDeleteObservation::PendingAutomod
            );
            assert!(marks.reserve_bulk_delivery(1, 2, message, now));
        }
        marks.finish_bulk_delivery(1, 2, &[1], BulkDeliveryOutcome::Ambiguous);
        assert_eq!(
            marks.claim_or_defer_plain_replay(1, 2, 3, None),
            BulkPlainReplay::Deferred
        );
        marks.finish_bulk_delivery(1, 2, &[4], BulkDeliveryOutcome::Delivered);

        let expired = now + AUTOMOD_DELETE_MARK_TTL + Duration::from_secs(1);
        marks.prune(expired);

        assert!(
            marks.bulk_logs.contains_key(&(1, 2, 1)),
            "ambiguous suppression expired"
        );
        assert_eq!(
            marks.claim_or_defer_plain_replay(1, 2, 1, None),
            BulkPlainReplay::Covered
        );
        assert!(
            marks.bulk_logs.contains_key(&(1, 2, 2)),
            "in-flight bulk row expired"
        );
        assert_eq!(
            marks.claim_or_defer_plain_replay(1, 2, 2, None),
            BulkPlainReplay::Deferred
        );
        assert!(
            marks.bulk_logs.contains_key(&(1, 2, 3)),
            "deferred bulk row expired"
        );
        assert_eq!(
            marks.claim_or_defer_plain_replay(1, 2, 3, None),
            BulkPlainReplay::Deferred
        );
        assert!(
            !marks.bulk_logs.contains_key(&(1, 2, 4)),
            "a delivered row should expire so the cap stays usable"
        );
        let replay = marks.finish_bulk_delivery(1, 2, &[2, 3], BulkDeliveryOutcome::Failed);
        assert_eq!(replay.len(), 2, "TTL dropped a deferred replay");
        assert_eq!(
            marks.claim_or_defer_plain_replay(1, 2, 1, None),
            BulkPlainReplay::Covered
        );
    }

    fn deliver_covered_bulk(marks: &mut AutomodDeleteMarks, message: u64, now: Instant) {
        marks.insert(1, 2, message, "badword", "Bad word", now);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, message, None, now),
            GatewayDeleteObservation::PendingAutomod
        );
        assert!(marks.reserve_bulk_delivery(1, 2, message, now));
        marks.finish_bulk_delivery(1, 2, &[message], BulkDeliveryOutcome::Delivered);
    }

    fn hold_bulk_row(
        marks: &mut AutomodDeleteMarks,
        message: u64,
        now: Instant,
        outcome: Option<BulkDeliveryOutcome>,
    ) {
        marks.insert(1, 2, message, "badword", "Bad word", now);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, message, None, now),
            GatewayDeleteObservation::PendingAutomod
        );
        assert!(marks.reserve_bulk_delivery(1, 2, message, now));
        if let Some(outcome) = outcome {
            marks.finish_bulk_delivery(1, 2, &[message], outcome);
        }
    }

    #[test]
    fn bulk_expiry_pops_only_due_entries() {
        let mut marks = AutomodDeleteMarks::default();
        let early = Instant::now();
        let later = early + Duration::from_secs(30);
        // A fresher deadline is indexed first. The older delivered row has to
        // land at the front without a scan of the live map.
        deliver_covered_bulk(&mut marks, 1, later);
        hold_bulk_row(&mut marks, 2, early, None);
        hold_bulk_row(&mut marks, 3, early, None);
        assert_eq!(
            marks.claim_or_defer_plain_replay(1, 2, 3, None),
            BulkPlainReplay::Deferred
        );
        hold_bulk_row(&mut marks, 4, early, Some(BulkDeliveryOutcome::Ambiguous));
        deliver_covered_bulk(&mut marks, 0, early);
        for message in 5..37 {
            deliver_covered_bulk(&mut marks, message, later);
        }
        let pops = marks.bulk_expiry_pops;
        let visits = marks.bulk_expiry_visits;
        let due = early + AUTOMOD_DELETE_MARK_TTL + Duration::from_secs(1);
        assert!(due < later + AUTOMOD_DELETE_MARK_TTL);
        marks.prune(due);
        assert_eq!(marks.bulk_expiry_pops, pops + 1);
        assert_eq!(
            marks.bulk_expiry_visits,
            visits + 2,
            "prune walked past the first deadline that was not due"
        );
        assert!(!marks.bulk_logs.contains_key(&(1, 2, 0)));
        assert!(marks.bulk_logs.contains_key(&(1, 2, 1)));
        assert!(
            marks.bulk_logs.contains_key(&(1, 2, 2)),
            "in-flight bulk row expired"
        );
        assert!(
            marks.bulk_logs.contains_key(&(1, 2, 3)),
            "deferred bulk row expired"
        );
        assert!(
            marks.bulk_logs.contains_key(&(1, 2, 4)),
            "ambiguous suppression expired"
        );
        assert_eq!(marks.bulk_logs.len(), 1 + 3 + 32);
        assert_eq!(
            marks.claim_or_defer_plain_replay(1, 2, 2, None),
            BulkPlainReplay::Deferred
        );
        assert_eq!(
            marks.claim_or_defer_plain_replay(1, 2, 4, None),
            BulkPlainReplay::Covered
        );
    }

    #[test]
    fn fresh_bulk_coverage_does_not_slow_unrelated_observations() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        for message in 0..MAX_AUTOMOD_DELETE_MARKS as u64 {
            deliver_covered_bulk(&mut marks, message, now);
        }
        assert_eq!(marks.bulk_logs.len(), MAX_AUTOMOD_DELETE_MARKS);
        assert_eq!(marks.bulk_expiry.len(), MAX_AUTOMOD_DELETE_MARKS);
        let pops = marks.bulk_expiry_pops;
        let visits = marks.bulk_expiry_visits;
        let started = Instant::now();
        for message in 0..10_000u64 {
            assert_eq!(
                marks.observe_gateway_delete(9, 9, message, None, now),
                GatewayDeleteObservation::Plain
            );
        }
        let elapsed = started.elapsed();
        assert_eq!(
            marks.bulk_expiry_pops, pops,
            "prune popped bulk rows when nothing was due"
        );
        assert_eq!(
            marks.bulk_expiry_visits,
            visits + 10_000,
            "each observation should peek one undued deadline, not the live rows"
        );
        assert_eq!(marks.bulk_logs.len(), MAX_AUTOMOD_DELETE_MARKS);
        assert_eq!(marks.bulk_expiry.len(), MAX_AUTOMOD_DELETE_MARKS);
        assert!(
            elapsed < Duration::from_millis(150),
            "10k observations took {elapsed:?}; scanning every delivered bulk row was about 421ms"
        );
    }

    #[test]
    fn capacity_rejection_does_not_walk_live_marks() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        for message in 0..MAX_AUTOMOD_DELETE_MARKS as u64 {
            marks.insert(1, 2, message, "badword", "Bad word", now);
        }
        let started = Instant::now();
        for offset in 0..10_000u64 {
            marks.insert(
                1,
                2,
                MAX_AUTOMOD_DELETE_MARKS as u64 + offset,
                "badword",
                "Bad word",
                now,
            );
        }
        let elapsed = started.elapsed();
        assert_eq!(marks.len(), MAX_AUTOMOD_DELETE_MARKS);
        assert_eq!(marks.order.len(), MAX_AUTOMOD_DELETE_MARKS);
        assert!(marks.contains(1, 2, 0));
        assert!(!marks.contains(1, 2, MAX_AUTOMOD_DELETE_MARKS as u64));
        assert!(!marks.contains(1, 2, MAX_AUTOMOD_DELETE_MARKS as u64 + 9_999));
        assert_eq!(
            marks.bulk_expiry_visits, 0,
            "capacity rejection walked bulk expiry"
        );
        assert!(
            elapsed < Duration::from_millis(400),
            "10k rejected inserts took {elapsed:?}; walking every live mark was about 2.81s"
        );
    }

    #[test]
    fn settled_ambiguous_coverage_expires_after_its_suppression_window() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        hold_bulk_row(&mut marks, 1, now, Some(BulkDeliveryOutcome::Ambiguous));
        hold_bulk_row(&mut marks, 2, now, None);
        let past_ttl = now + AUTOMOD_DELETE_MARK_TTL + Duration::from_secs(1);
        marks.prune(past_ttl);
        assert!(
            marks.bulk_logs.contains_key(&(1, 2, 1)),
            "unsettled ambiguous coverage expired"
        );
        assert!(
            marks.bulk_logs.contains_key(&(1, 2, 2)),
            "in-flight bulk row expired"
        );
        let attempt = attempt_for_message(&marks, 1);
        assert!(marks.confirm(1, 2, 1, attempt, now).is_some());
        marks.prune(now + AUTOMOD_DELETE_MARK_TTL - Duration::from_secs(1));
        assert!(marks.bulk_logs.contains_key(&(1, 2, 1)));
        assert_eq!(
            marks.claim_or_defer_plain_replay(1, 2, 1, None),
            BulkPlainReplay::Covered
        );
        let pops = marks.bulk_expiry_pops;
        marks.prune(past_ttl);
        assert_eq!(marks.bulk_expiry_pops, pops + 1);
        assert!(
            !marks.bulk_logs.contains_key(&(1, 2, 1)),
            "settled ambiguous coverage stayed after its window"
        );
        assert!(marks.bulk_logs.contains_key(&(1, 2, 2)));
        marks.insert(1, 2, 9, "badword", "Bad word", past_ttl);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, 9, None, past_ttl),
            GatewayDeleteObservation::PendingAutomod
        );
        assert!(marks.reserve_bulk_delivery(1, 2, 9, past_ttl));
    }

    #[test]
    fn failed_rest_settlement_also_starts_the_ambiguous_window() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        hold_bulk_row(&mut marks, 4, now, Some(BulkDeliveryOutcome::Ambiguous));
        let past_ttl = now + AUTOMOD_DELETE_MARK_TTL + Duration::from_secs(1);
        marks.prune(past_ttl);
        assert!(marks.bulk_logs.contains_key(&(1, 2, 4)));
        let attempt = attempt_for_message(&marks, 4);
        let removal = marks.remove_unconfirmed(1, 2, 4, attempt, now);
        assert!(removal.covered_by_bulk);
        assert_eq!(
            marks.claim_or_defer_plain_replay(1, 2, 4, None),
            BulkPlainReplay::Covered
        );
        assert!(marks.bulk_logs.contains_key(&(1, 2, 4)));
        marks.prune(past_ttl);
        assert!(!marks.bulk_logs.contains_key(&(1, 2, 4)));
    }

    #[test]
    fn a_new_attempt_keeps_ambiguous_coverage_until_it_settles() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        hold_bulk_row(&mut marks, 5, now, Some(BulkDeliveryOutcome::Ambiguous));
        let first = attempt_for_message(&marks, 5);
        assert!(marks.confirm(1, 2, 5, first, now).is_some());
        let later = now + Duration::from_secs(30);
        marks.insert(1, 2, 5, "badword", "Bad word", later);
        marks.prune(now + AUTOMOD_DELETE_MARK_TTL + Duration::from_secs(1));
        assert!(
            marks.bulk_logs.contains_key(&(1, 2, 5)),
            "a new attempt lost ambiguous suppression"
        );
        assert!(marks.contains(1, 2, 5));
        let second = attempt_for_message(&marks, 5);
        let removal = marks.remove_unconfirmed(1, 2, 5, second, later);
        assert!(removal.covered_by_bulk);
        marks.prune(later + AUTOMOD_DELETE_MARK_TTL + Duration::from_secs(1));
        assert!(!marks.bulk_logs.contains_key(&(1, 2, 5)));
    }

    #[test]
    fn bulk_placement_owner_suppresses_a_second_success_report() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        let attempt = marks.insert(1, 2, 3, "badword", "Bad words", now);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, 3, None, now),
            GatewayDeleteObservation::PendingAutomod
        );
        assert!(marks.reserve_bulk_delivery(1, 2, 3, now));
        marks.note_report_owner(1, 2, 3, ReportOwner::BulkTranscript);
        assert!(marks.confirm(1, 2, 3, attempt, now).is_none());
        assert_eq!(
            marks.observe_gateway_delete(1, 2, 3, None, now),
            GatewayDeleteObservation::Suppressed
        );
        let replay = marks.finish_bulk_delivery(1, 2, &[3], BulkDeliveryOutcome::Failed);
        assert_eq!(replay.len(), 1);
        let automod = replay[0]
            .automod
            .clone()
            .expect("failed bulk releases the success");
        assert!(
            automod.fallback_to_bulk,
            "a deferred success keeps a bulk destination"
        );
    }

    #[test]
    fn delivered_and_ambiguous_bulk_drop_deferred_success() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        for (message, outcome) in [
            (3, BulkDeliveryOutcome::Delivered),
            (4, BulkDeliveryOutcome::Ambiguous),
        ] {
            let attempt = marks.insert(1, 2, message, "badword", "Bad words", now);
            assert_eq!(
                marks.observe_gateway_delete(1, 2, message, None, now),
                GatewayDeleteObservation::PendingAutomod
            );
            assert!(marks.reserve_bulk_delivery(1, 2, message, now));
            marks.note_report_owner(1, 2, message, ReportOwner::BulkTranscript);
            assert!(marks.confirm(1, 2, message, attempt, now).is_none());
            let replay = marks.finish_bulk_delivery(1, 2, &[message], outcome);
            assert!(
                replay.is_empty(),
                "settled bulk send must not release a second report: {outcome:?}"
            );
        }
    }

    #[test]
    fn peeled_placement_keeps_bulk_fallback_on_the_settlement() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        let attempt = marks.insert(1, 2, 3, "badword", "Bad words", now);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, 3, None, now),
            GatewayDeleteObservation::PendingAutomod
        );
        marks.note_report_owner(1, 2, 3, ReportOwner::Individual);
        let settlement = marks.confirm(1, 2, 3, attempt, now).unwrap();
        assert!(settlement.fallback_to_bulk);
        assert_eq!(settlement.rule_id, "badword");
    }

    #[test]
    fn saturated_insert_stays_automod_until_the_action_path_emits() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        for message in 0..MAX_AUTOMOD_DELETE_MARKS as u64 {
            marks.insert(1, 2, message, "filler", "Filler", now);
        }
        let message = MAX_AUTOMOD_DELETE_MARKS as u64;
        let attempt = marks.insert(1, 2, message, "badword", "Bad words", now);
        assert!(!marks.contains(1, 2, message));
        assert_eq!(marks.len(), MAX_AUTOMOD_DELETE_MARKS);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, message, None, now),
            GatewayDeleteObservation::PendingAutomod
        );
        let settlement = marks
            .settle_unmarked(1, 2, message, attempt, true, true, now)
            .expect("saturated success still carries the rule");
        assert_eq!(settlement.rule_id, "badword");
        assert_eq!(settlement.rule_name, "Bad words");
        assert_eq!(
            marks.observe_gateway_delete(1, 2, message, None, now),
            GatewayDeleteObservation::Suppressed
        );
    }

    fn fill_live_marks(marks: &mut AutomodDeleteMarks, now: Instant) {
        for message in 0..MAX_AUTOMOD_DELETE_MARKS as u64 {
            marks.insert(1, 2, message, "filler", "Filler", now);
        }
    }

    #[test]
    fn overlapping_overflow_attempts_share_one_owner() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        fill_live_marks(&mut marks, now);
        let message = 50_000;
        let first = marks.insert(1, 2, message, "badword", "Bad words", now);
        let second = marks.insert(1, 2, message, "badword", "Bad words", now);
        assert_eq!(marks.unmarked.len(), 1);
        assert!(marks.unmarked[&(1, 2, message)].owns_attempt(first.0));
        assert!(marks.unmarked[&(1, 2, message)].owns_attempt(second.0));
        assert_eq!(
            marks.observe_gateway_delete(1, 2, message, None, now),
            GatewayDeleteObservation::PendingAutomod
        );

        let settlement = marks
            .settle_unmarked(1, 2, message, second, true, true, now)
            .expect("the later success owns the only report");
        assert_eq!(settlement.rule_id, "badword");
        let removal = marks.remove_unconfirmed(1, 2, message, first, now);
        assert_eq!(removal.action, UnconfirmedMarkRemoval::Removed);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, message, None, now),
            GatewayDeleteObservation::Suppressed
        );

        let mut marks = AutomodDeleteMarks::default();
        fill_live_marks(&mut marks, now);
        let first = marks.insert(1, 2, message, "badword", "Bad words", now);
        let second = marks.insert(1, 2, message, "badword", "Bad words", now);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, message, None, now),
            GatewayDeleteObservation::PendingAutomod
        );
        let early = marks.remove_unconfirmed(1, 2, message, first, now);
        assert_eq!(early.action, UnconfirmedMarkRemoval::Removed);
        let settlement = marks
            .settle_unmarked(1, 2, message, second, true, true, now)
            .expect("the surviving attempt still reports once");
        assert_eq!(settlement.rule_name, "Bad words");
        assert_eq!(
            marks.observe_gateway_delete(1, 2, message, None, now),
            GatewayDeleteObservation::Suppressed
        );

        let mut marks = AutomodDeleteMarks::default();
        fill_live_marks(&mut marks, now);
        let first = marks.insert(1, 2, message, "badword", "Bad words", now);
        let second = marks.insert(1, 2, message, "badword", "Bad words", now);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, message, None, now),
            GatewayDeleteObservation::PendingAutomod
        );
        assert_eq!(
            marks.remove_unconfirmed(1, 2, message, first, now).action,
            UnconfirmedMarkRemoval::Removed
        );
        assert_eq!(
            marks.remove_unconfirmed(1, 2, message, second, now).action,
            UnconfirmedMarkRemoval::ReplayPlainDelete
        );
        assert!(!marks.unmarked.contains_key(&(1, 2, message)));
        assert!(!marks.gateway.contains_key(&(1, 2, message)));
    }

    #[test]
    fn overflow_gateway_state_is_reclaimed_with_its_owner() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        // Fillers outlive both overflow batches so the cap stays full while
        // those rows expire.
        let fillers_at = now + AUTOMOD_DELETE_MARK_TTL * 4;
        fill_live_marks(&mut marks, fillers_at);
        let fillers = marks.gateway.len();
        assert_eq!(marks.entries.len(), MAX_AUTOMOD_DELETE_MARKS);

        for batch in 0..2u64 {
            let batch_at =
                now + AUTOMOD_DELETE_MARK_TTL * (batch as u32) + Duration::from_secs(batch);
            if batch > 0 {
                marks.prune(batch_at);
                assert!(marks.unmarked.is_empty());
                assert_eq!(
                    marks.gateway.len(),
                    fillers,
                    "the previous overflow batch left gateway state behind"
                );
            }
            for offset in 0..MAX_AUTOMOD_DELETE_MARKS as u64 {
                let message = 100_000 + batch * MAX_AUTOMOD_DELETE_MARKS as u64 + offset;
                let attempt = marks.insert(1, 2, message, "badword", "Bad words", batch_at);
                assert!(
                    marks
                        .settle_unmarked(1, 2, message, attempt, true, false, batch_at)
                        .is_some()
                );
            }
            assert_eq!(marks.unmarked.len(), MAX_AUTOMOD_DELETE_MARKS);
            assert_eq!(marks.gateway.len(), fillers);
        }

        let past_both = now + AUTOMOD_DELETE_MARK_TTL * 3;
        marks.prune(past_both);
        assert!(marks.unmarked.is_empty());
        assert_eq!(marks.entries.len(), MAX_AUTOMOD_DELETE_MARKS);
        assert_eq!(marks.gateway.len(), fillers);

        let observed_at = past_both;
        let message = 300_000;
        let attempt = marks.insert(1, 2, message, "badword", "Bad words", observed_at);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, message, None, observed_at),
            GatewayDeleteObservation::PendingAutomod
        );
        assert!(marks.gateway.contains_key(&(1, 2, message)));
        let removal = marks.remove_unconfirmed(1, 2, message, attempt, observed_at);
        assert_eq!(removal.action, UnconfirmedMarkRemoval::ReplayPlainDelete);
        assert!(!marks.gateway.contains_key(&(1, 2, message)));
        assert!(!marks.unmarked.contains_key(&(1, 2, message)));

        let message = 300_001;
        let attempt = marks.insert(1, 2, message, "badword", "Bad words", observed_at);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, message, None, observed_at),
            GatewayDeleteObservation::PendingAutomod
        );
        assert!(
            marks
                .take_uncertain_gateway_settlement(1, 2, message, attempt, observed_at)
                .is_some()
        );
        assert!(marks.unmarked.contains_key(&(1, 2, message)));
        marks.prune(observed_at + AUTOMOD_DELETE_MARK_TTL + Duration::from_secs(1));
        assert!(!marks.unmarked.contains_key(&(1, 2, message)));
        assert!(!marks.gateway.contains_key(&(1, 2, message)));
        assert_eq!(marks.entries.len(), MAX_AUTOMOD_DELETE_MARKS);
        assert_eq!(marks.gateway.len(), fillers);
    }

    #[test]
    fn inflight_overflow_success_keeps_a_bulk_destination() {
        let mut marks = AutomodDeleteMarks::default();
        let now = Instant::now();
        fill_live_marks(&mut marks, now);
        let message = 50_010;
        let attempt = marks.insert(1, 2, message, "badword", "Bad words", now);
        assert_eq!(
            marks.observe_gateway_delete(1, 2, message, None, now),
            GatewayDeleteObservation::PendingAutomod
        );
        assert!(marks.reserve_bulk_delivery(1, 2, message, now));
        marks.note_report_owner(1, 2, message, ReportOwner::BulkTranscript);
        assert!(
            marks
                .settle_unmarked(1, 2, message, attempt, false, true, now)
                .is_none()
        );
        let replay = marks.finish_bulk_delivery(1, 2, &[message], BulkDeliveryOutcome::Failed);
        let automod = replay[0]
            .automod
            .clone()
            .expect("bulk-only failure still reports");
        assert!(automod.fallback_to_bulk);
        assert_eq!(automod.rule_id, "badword");
        assert!(marks.unmarked.contains_key(&(1, 2, message)));
        marks.prune(now + AUTOMOD_DELETE_MARK_TTL + Duration::from_secs(1));
        assert!(!marks.unmarked.contains_key(&(1, 2, message)));
        assert!(!marks.gateway.contains_key(&(1, 2, message)));
        assert_eq!(marks.gateway.len(), 0);
    }
}
