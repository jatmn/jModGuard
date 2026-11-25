//! Bounded aggregation of automod deletes. Enforcement reports effects here;
//! the delete marks still decide which path owns each message's report.

use super::*;
use crate::automod_delete_marks::{CleanupDeleteClaim, GatewayDeleteOrigin, PreparedAutomodDelete};
use crate::db::guild_data::{CaseId, CaseOutcome};
use crate::db::{CleanupDeleteRecord, CleanupDeleteSave};
use crate::discord::{LogAttempt, LogDispatch};
use std::collections::BTreeSet;
use tokio::time::Instant as Clock;

const MAX_INCIDENTS: usize = 64;
const MAX_RETRIES_PER_GUILD: usize = 8;
const MAX_MESSAGES: usize = 256;
const MAX_LIFETIME: Duration = Duration::from_secs(30);
const DELIVERY_RETRY: Duration = Duration::from_secs(10);
const MAX_DELIVERY_RETRY: Duration = Duration::from_secs(60);
const JOURNAL_PAGE: usize = 64;

const SUMMARY_TEMPLATE: &str = "Automod cleanup\n\n**User:** {mention} (`{user_id}`)\n**Rule:** {rule} (`{rule_id}`)\n**Action:** {cleanup_action}\n**Deleted:** {cleanup_total}\n**Channels:** {cleanup_channels}\n**Time range:** {cleanup_first} – {cleanup_last}\n**Sample:** {cleanup_sample}\n**Evidence:** {cleanup_evidence}\n**Delete failures:** {cleanup_failures}";

pub(super) const FAILURE_TEMPLATE: &str = "Automod cleanup delete failures\n\n**User:** {mention} (`{user_id}`)\n**Rule:** {rule} (`{rule_id}`)\n**Delete failures:** {cleanup_failures}";

/// A grouped template may deliberately omit retained text or attachments.
pub(super) const SINGLE_DELETE_TEMPLATE: &str = "Automod message deleted\n\n**Channel:** {channel}\n**Rule:** {rule} (`{rule_id}`)\n**Message:** {message_url}\n**Author:** {mention}\n**Time:** `{time}`";

/// Resolve the grouped destination and any additional exclusions on its bulk
/// fallback. Ordinary deletes keep using their existing routes.
pub(super) fn summary_route(
    cfg: &crate::state::CachedGuildConfig,
) -> Option<(ChannelId, Option<&db::LogFilter>)> {
    channel_for(cfg, "automod_delete")
        .map(|channel| (channel, None))
        .or_else(|| {
            channel_for(cfg, "message_bulk_delete")
                .map(|channel| (channel, log_filter_for(cfg, "message_bulk_delete")))
        })
}

pub(super) fn summary_template(cfg: &crate::state::CachedGuildConfig) -> Option<&str> {
    match cfg.templates.get("automod_delete").map(String::as_str) {
        None => Some(SUMMARY_TEMPLATE),
        Some(saved) if saved == default_template("automod_delete") => Some(SUMMARY_TEMPLATE),
        Some(saved) if templates::uses_automod_cleanup(saved) => Some(saved),
        Some(_) => None,
    }
}

type Subject = (u64, u64);
type Message = (u64, u64);
/// Guild, channel and message identity shared by the collector and handler.
pub(crate) type CleanupObservation = (u64, Message);

struct ClaimedDelete {
    observation: Option<CleanupObservation>,
}

struct PendingObservation {
    batch: u64,
    row: CleanupDeleteRecord,
    retry_at: Option<Clock>,
}

#[derive(Default)]
pub(crate) struct CleanupIncidents {
    inner: Mutex<Incidents>,
    wake: tokio::sync::Notify,
    journal: Mutex<JournalWrites>,
    journal_wake: tokio::sync::Notify,
    journal_writer: tokio::sync::Mutex<()>,
    recovery: db::CleanupRecovery,
}

#[derive(Default)]
struct JournalWrites {
    observed: BTreeMap<CleanupObservation, PendingObservation>,
    skipped: BTreeMap<(u64, Message), CleanupDeleteSave>,
    settled: BTreeSet<(u64, i64)>,
    settlement_retry_at: Option<Clock>,
}

fn batch_key(nonce: u64) -> String {
    format!("{nonce:016x}")
}

#[derive(Default)]
struct Incidents {
    next_id: u64,
    active: BTreeMap<u64, (Subject, Incident)>,
    pending: BTreeMap<u64, Option<PendingIncident>>,
}

pub(crate) struct CleanupRule<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub reason: &'a str,
    pub action: &'a str,
    pub join_only: bool,
    pub sample: Option<(u64, u64, &'a str)>,
    pub hash: Option<&'a [u8; 32]>,
}

struct Incident {
    nonce: u64,
    enrollment_id: Option<String>,
    user: u64,
    rule_id: String,
    rule: String,
    reason: String,
    action: String,
    outcome: Option<CaseOutcome>,
    case: Option<CaseId>,
    delay: Duration,
    settle_at: Clock,
    deadline: Clock,
    running: usize,
    purge_from: Option<i64>,
    purge_until: Option<i64>,
    purge_receipt_until: Option<Clock>,
    sample: Option<(u64, u64, String)>,
    hash: Option<String>,
    messages: BTreeMap<Message, Delete>,
}

struct Delete {
    at: chrono::DateTime<chrono::Utc>,
    result: Result<Option<Box<MessageSnapshot>>, &'static str>,
    purge_origin: Option<GatewayDeleteOrigin>,
}

pub(crate) struct Cleanup<'a> {
    incidents: &'a CleanupIncidents,
    subject: Subject,
    id: u64,
    rule_id: String,
    rule_name: String,
    reason: String,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        self.update(|incident| incident.running -= 1);
        self.incidents.wake.notify_one();
    }
}

impl Cleanup<'_> {
    fn update(&self, update: impl FnOnce(&mut Incident)) {
        let mut state = self.incidents.inner.lock();
        if let Some((_, incident)) = state.active.get_mut(&self.id) {
            update(incident);
        }
    }

    pub(crate) fn prepare(
        &self,
        app: &BotState,
        channel: u64,
        message: u64,
    ) -> PreparedAutomodDelete {
        app.automod_delete_marks.prepare_delete(
            self.subject.0,
            channel,
            message,
            &self.rule_id,
            &self.rule_name,
            &self.reason,
        )
    }

    pub(crate) async fn complete_ungrouped(
        &self,
        app: &Arc<BotState>,
        sink: &impl LogDelivery,
        channel: u64,
        message: u64,
        prepared: PreparedAutomodDelete,
    ) {
        complete_prepared_automod_delete(
            app,
            sink,
            self.subject.0,
            channel,
            message,
            &prepared,
            &self.rule_id,
            &self.rule_name,
            &self.reason,
        )
        .await;
    }

    pub(crate) async fn release_ungrouped(
        &self,
        app: &Arc<BotState>,
        sink: &impl LogDelivery,
        channel: u64,
        message: u64,
        prepared: PreparedAutomodDelete,
    ) {
        let guild = self.subject.0;
        let removal =
            app.automod_delete_marks
                .remove_unconfirmed(guild, channel, message, prepared.attempt);
        if let Some(settlement) = removal.released_settlement.as_ref() {
            emit_reconciled_automod_delete(
                app,
                sink,
                guild,
                channel,
                message,
                &settlement.rule_id,
                &settlement.rule_name,
                &settlement.reason,
                settlement.retained.as_ref(),
                settlement.fallback_to_bulk,
            )
            .await;
        }
        replay_unconfirmed_plain_delete(
            app,
            sink,
            GuildId::new(guild),
            ChannelId::new(channel),
            MessageId::new(message),
            &removal,
        )
        .await;
    }

    pub(crate) fn begin_purge(&self, seconds: u32, requested_at_ms: i64) {
        if seconds > 0 {
            self.update(|incident| {
                let from = requested_at_ms - i64::from(seconds) * 1000;
                incident.purge_from = Some(incident.purge_from.map_or(from, |old| old.min(from)));
                incident.purge_until = Some(requested_at_ms);
                incident.purge_receipt_until = Some(incident.deadline);
                incident.touch(Clock::now());
            });
        }
    }

    pub(crate) fn outcome(&self, outcome: CaseOutcome, case: Option<CaseId>) {
        self.update(|incident| {
            if incident.purge_receipt_until.is_some() {
                incident.purge_receipt_until = (outcome != CaseOutcome::Failed)
                    .then(|| (Clock::now() + incident.delay).min(incident.deadline));
            }
            if incident.outcome != Some(CaseOutcome::Succeeded) || outcome == CaseOutcome::Succeeded
            {
                if incident.outcome.as_ref() != Some(&outcome) || case.is_some() {
                    incident.case = case;
                }
                incident.outcome = Some(outcome);
            }
            incident.touch(Clock::now());
        });
    }

    pub(crate) fn case(&self, outcome: &CaseOutcome, case: Option<CaseId>) {
        if let Some(case) = case {
            self.update(|incident| {
                if incident.outcome.as_ref() == Some(outcome) {
                    incident.case = Some(case);
                }
            });
        }
    }

    pub(crate) async fn deleted(
        &self,
        app: &BotState,
        channel: u64,
        message: u64,
        prepared: PreparedAutomodDelete,
        gateway_required: bool,
    ) -> bool {
        let claim = if gateway_required {
            CleanupDeleteClaim::Observed(prepared)
        } else {
            CleanupDeleteClaim::Rest(prepared)
        };
        let mut claimed = None;
        self.update(|incident| {
            if !gateway_required || incident.accepts_purge_receipt() {
                claimed =
                    incident.claim_delete(app, self.subject.0, (channel, message), claim, None);
            }
        });
        if let Some(observation) = claimed.as_ref().and_then(|claim| claim.observation) {
            persist_cleanup_observations(app, &[observation]).await;
        }
        claimed.is_some()
    }

    pub(crate) async fn purge_sweep_gone(
        &self,
        app: &BotState,
        channel: u64,
        message: u64,
        prepared: PreparedAutomodDelete,
    ) -> bool {
        let guild = self.subject.0;
        let mut claimed = None;
        let mut deferred = false;
        self.update(|incident| {
            if let Some(until) = incident.purge_receipt_until
                && Clock::now() < until
            {
                app.recent_messages
                    .defer_purge_echo(guild, channel, message, until);
                deferred = true;
                claimed = incident.claim_delete(
                    app,
                    guild,
                    (channel, message),
                    CleanupDeleteClaim::AlreadyGone(prepared),
                    None,
                );
            }
        });
        if claimed.is_some() || !deferred {
            app.recent_messages.forget(guild, channel, message);
        }
        if let Some(observation) = claimed.as_ref().and_then(|claim| claim.observation) {
            persist_cleanup_observations(app, &[observation]).await;
        }
        claimed.is_some()
    }

    pub(crate) fn failed(&self, channel: u64, message: u64, reason: &'static str) {
        self.update(|incident| {
            if incident.messages.len() < MAX_MESSAGES {
                incident
                    .messages
                    .entry((channel, message))
                    .or_insert_with(|| Delete {
                        at: chrono::Utc::now(),
                        result: Err(reason),
                        purge_origin: None,
                    });
                incident.touch(Clock::now());
            }
        });
    }
}

impl Incident {
    fn touch(&mut self, now: Clock) {
        self.settle_at = (now + self.delay).min(self.deadline);
    }

    fn accepts_purge_receipt(&self) -> bool {
        self.purge_receipt_until
            .is_some_and(|until| Clock::now() < until)
    }

    fn claim_delete(
        &mut self,
        app: &BotState,
        guild: u64,
        target: Message,
        claim: CleanupDeleteClaim,
        retained: Option<&MessageSnapshot>,
    ) -> Option<ClaimedDelete> {
        let (channel, message) = target;
        if let Some(Delete {
            result: Ok(snapshot),
            purge_origin,
            ..
        }) = self.messages.get_mut(&target)
        {
            if snapshot.is_none() {
                *snapshot = retained.cloned().map(Box::new);
            }
            let mut observation = None;
            if let Some(prepared) = claim.prepared() {
                let promoted =
                    matches!(claim, CleanupDeleteClaim::Rest(_)) && purge_origin.take().is_some();
                app.automod_delete_marks.remove_unconfirmed(
                    guild,
                    channel,
                    message,
                    prepared.attempt,
                );
                if promoted {
                    observation = Some(self.journal_delete(app, guild, target, false));
                }
            }
            return Some(ClaimedDelete { observation });
        }
        if Clock::now() >= self.deadline
            || (self.messages.len() >= MAX_MESSAGES && !self.messages.contains_key(&target))
        {
            return None;
        }
        let (snapshot, purge_origin) = app.automod_delete_marks.claim_cleanup_delete(
            (guild, channel, message),
            claim,
            retained,
        )?;
        self.messages.insert(
            target,
            Delete {
                at: chrono::Utc::now(),
                result: Ok(snapshot.map(Box::new)),
                purge_origin,
            },
        );
        let observation = Some(self.journal_delete(app, guild, target, purge_origin.is_some()));
        self.touch(Clock::now());
        Some(ClaimedDelete { observation })
    }

    fn journal_delete(
        &self,
        app: &BotState,
        guild: u64,
        target: Message,
        purge: bool,
    ) -> CleanupObservation {
        let key = (guild, target);
        let row = CleanupDeleteRecord {
            batch: batch_key(self.nonce),
            guild_id: guild as i64,
            enrollment_id: self.enrollment_id.clone(),
            channel_id: target.0 as i64,
            message_id: target.1 as i64,
            user_id: self.user as i64,
            rule_id: clip(&self.rule_id, 80),
            rule_name: self.rule.clone(),
            observed_at_ms: self.messages[&target].at.timestamp_millis(),
            purge,
        };
        app.cleanup_incidents
            .journal
            .lock()
            .observed
            .entry(key)
            .and_modify(|pending| pending.row.purge = purge)
            .or_insert(PendingObservation {
                batch: self.nonce,
                row,
                retry_at: None,
            });
        // Snapshot-only claims hand off here without waiting for the primary
        // or for any in-flight Discord delivery.
        app.cleanup_incidents.journal_wake.notify_one();
        key
    }
}

pub(crate) async fn begin_cleanup<'a>(
    app: &'a BotState,
    guild: u64,
    user: u64,
    rule: CleanupRule<'_>,
) -> Option<Cleanup<'a>> {
    if !app.cfg.log.group_automod_cleanup_deletes || !app.activation.may_record() {
        return None;
    }
    let cfg = app.configs.get(&app.db, guild).await.ok()?;
    summary_route(&cfg)?;
    let enrollment_id = db::guild_enrollment(&app.db, i64::try_from(guild).ok()?)
        .await
        .ok()?
        .map(|enrollment| enrollment.id);
    let mut state = app.cleanup_incidents.inner.lock();
    if app.activation.is_shutting_down() {
        return None;
    }
    let subject = (guild, user);
    let now = Clock::now();
    let sample = || {
        rule.sample
            .filter(|(_, _, text)| !text.is_empty())
            .map(|(channel, message, text)| (channel, message, clip(text, 240)))
    };
    let handle = |id| Cleanup {
        incidents: &app.cleanup_incidents,
        subject,
        id,
        rule_id: rule.id.to_owned(),
        rule_name: rule.name.to_owned(),
        reason: rule.reason.to_owned(),
    };
    if let Some((&id, (_, incident))) = state
        .active
        .iter_mut()
        .rev()
        .find(|(_, (key, incident))| *key == subject && incident.enrollment_id == enrollment_id)
        && now < incident.settle_at
    {
        if incident.rule_id != rule.id || incident.action != rule.action {
            return None;
        }
        incident.running += 1;
        if incident.sample.is_none() {
            incident.sample = sample();
        }
        incident.touch(now);
        return Some(handle(id));
    }
    if rule.join_only || state.active.len() + state.pending.len() >= MAX_INCIDENTS {
        return None;
    }
    let id = state.next_id;
    let delay = Duration::from_secs(app.cfg.log.automod_cleanup_settle_secs.clamp(5, 10));
    state.active.insert(
        id,
        (
            subject,
            Incident {
                nonce: rand::random(),
                enrollment_id,
                user,
                rule_id: rule.id.to_owned(),
                rule: clip(rule.name, 100),
                reason: rule.reason.to_owned(),
                action: rule.action.to_owned(),
                outcome: None,
                case: None,
                delay,
                settle_at: now + delay,
                deadline: now + MAX_LIFETIME,
                running: 1,
                purge_from: None,
                purge_until: None,
                purge_receipt_until: None,
                sample: sample(),
                hash: rule
                    .hash
                    .map(|hash| hash.iter().map(|byte| format!("{byte:02x}")).collect()),
                messages: BTreeMap::new(),
            },
        ),
    );
    state.next_id = id.wrapping_add(1);
    app.cleanup_incidents.wake.notify_one();
    Some(handle(id))
}

/// Raw Gateway deletes only have message IDs. Use a previously observed author
/// and the message creation time; unknown or unrelated authors stay in their
/// normal single or bulk path.
pub(crate) fn claim_cleanup_gateway(
    app: &BotState,
    guild: GuildId,
    channel: ChannelId,
    message: MessageId,
    retained: Option<&MessageSnapshot>,
    origin: GatewayDeleteOrigin,
) -> Option<CleanupObservation> {
    if !app.cfg.log.group_automod_cleanup_deletes {
        return None;
    }
    let guild = guild.get();
    let channel = channel.get();
    let id = message.get();
    let identity = app
        .recent_messages
        .identity(guild, channel, id)
        .or_else(|| {
            let row = retained?;
            Some((row.fields.author_id?.try_into().ok()?, row.created_at_ms))
        });
    if let Some((author, created)) = identity {
        let mut state = app.cleanup_incidents.inner.lock();
        if let Some((_, incident)) = state.active.values_mut().rev().find(|(subject, incident)| {
            *subject == (guild, author)
                && incident.accepts_purge_receipt()
                && incident
                    .purge_from
                    .zip(incident.purge_until)
                    .is_some_and(|(from, until)| (from..=until).contains(&created))
        }) {
            let _ = incident.claim_delete(
                app,
                guild,
                (channel, id),
                CleanupDeleteClaim::Gateway(origin),
                retained,
            );
        }
    }
    // The collector may already have claimed the echo using a retained row.
    // Persisting that event doesn't require knowing its author again or finding
    // the incident still open for new claims.
    let key = (guild, (channel, id));
    app.cleanup_incidents
        .journal
        .lock()
        .observed
        .contains_key(&key)
        .then_some(key)
}

pub(crate) fn cleanup_delete_failure(error: &serenity::Error) -> &'static str {
    match crate::discord::http_error_response(error) {
        Some((403, _)) => "missing permission",
        Some((404, _)) => "message unavailable",
        Some(_) => "Discord rejected the delete",
        None => "delete outcome unknown",
    }
}

struct PendingIncident {
    subject: Subject,
    incident: Incident,
    fallback: bool,
    retry_at: Clock,
    retry_delay: Duration,
    summary_dispatch: LogDispatch,
}

/// Return partially delivered work to the queue if shutdown cancels this send.
/// Leave the slot reserved while sending so admission still counts it.
struct DeliveryLease<'a> {
    app: &'a BotState,
    id: u64,
    pending: Option<PendingIncident>,
}

impl Drop for DeliveryLease<'_> {
    fn drop(&mut self) {
        let queue = &self.app.cleanup_incidents;
        let mut state = queue.inner.lock();
        let evicted = if let Some(pending) = self.pending.take()
            && !pending.incident.messages.is_empty()
        {
            let guild = pending.subject.0;
            let retrying = !pending.retry_delay.is_zero();
            state.pending.insert(self.id, Some(pending));
            if retrying {
                state.evict_excess_retry(guild)
            } else {
                None
            }
        } else {
            state.pending.remove(&self.id);
            None
        };
        drop(state);
        if let Some(mut evicted) = evicted {
            let discarded = evicted.incident.messages.len();
            evicted.finish_matching(self.app, |_| true);
            record_delivery_failure(self.app);
            tracing::debug!(
                discarded,
                "evicted oldest cleanup delivery from a full guild queue"
            );
        }
        queue.wake.notify_one();
    }
}

impl Incidents {
    fn evict_excess_retry(&mut self, guild: u64) -> Option<PendingIncident> {
        let mut retries = self.pending.iter().filter_map(|(&id, entry)| {
            entry
                .as_ref()
                .filter(|pending| pending.subject.0 == guild && !pending.retry_delay.is_zero())
                .map(|_| id)
        });
        let oldest = retries.next()?;
        if retries.count() >= MAX_RETRIES_PER_GUILD {
            return self.pending.remove(&oldest).flatten();
        }
        None
    }
}

impl CleanupIncidents {
    #[cfg(test)]
    pub(crate) async fn hold_journal_writer(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.journal_writer.lock().await
    }

    async fn wait_until_due(&self) {
        let next = {
            let state = self.inner.lock();
            state
                .active
                .values()
                .map(|(_, incident)| {
                    if incident.running > 0 {
                        incident.deadline
                    } else {
                        incident.settle_at
                    }
                })
                .chain(
                    state
                        .pending
                        .values()
                        .flatten()
                        .map(|pending| pending.retry_at),
                )
                .min()
        };
        let retry = self.journal.lock().settlement_retry_at;
        if let Some(next) = next.into_iter().chain(retry).min() {
            tokio::select! {
                _ = self.wake.notified() => {},
                _ = tokio::time::sleep_until(next) => {},
            }
        } else {
            self.wake.notified().await;
        }
    }

    async fn wait_for_observations(&self) {
        let now = Clock::now();
        let next = self
            .journal
            .lock()
            .observed
            .values()
            .map(|pending| pending.retry_at.unwrap_or(now))
            .min();
        if let Some(next) = next {
            tokio::select! {
                _ = self.journal_wake.notified() => {},
                _ = tokio::time::sleep_until(next) => {},
            }
        } else {
            self.journal_wake.notified().await;
        }
    }

    fn queue_due(&self, now: Clock, shutdown: bool) -> Vec<u64> {
        let mut state = self.inner.lock();
        let Incidents {
            active, pending, ..
        } = &mut *state;
        pending.extend(
            active
                .extract_if(.., |_, (_, incident)| {
                    shutdown
                        || now >= incident.deadline
                        || (incident.running == 0 && now >= incident.settle_at)
                })
                .map(|(id, (subject, incident))| {
                    let fallback = incident.running > 0;
                    (
                        id,
                        Some(PendingIncident {
                            subject,
                            incident,
                            fallback,
                            retry_at: now,
                            retry_delay: Duration::ZERO,
                            summary_dispatch: LogDispatch::Pending,
                        }),
                    )
                }),
        );
        pending
            .iter()
            .filter(|(_, item)| {
                item.as_ref()
                    .is_some_and(|item| shutdown || now >= item.retry_at)
            })
            .map(|(&id, _)| id)
            .collect()
    }

    fn lease(app: &BotState, id: u64) -> Option<DeliveryLease<'_>> {
        let pending = app
            .cleanup_incidents
            .inner
            .lock()
            .pending
            .get_mut(&id)?
            .take()?;
        Some(DeliveryLease {
            app,
            id,
            pending: Some(pending),
        })
    }
}

fn channel_counts(counts: BTreeMap<u64, usize>) -> String {
    let parts = counts
        .into_iter()
        .map(|(channel, count)| format!("<#{channel}> {count}"))
        .collect::<Vec<_>>();
    if parts.is_empty() {
        "—".to_owned()
    } else {
        parts.join(", ")
    }
}

fn summary_vars(
    subject: Subject,
    incident: &Incident,
    filters: [Option<&db::LogFilter>; 2],
    failures_only: bool,
) -> Option<(BTreeMap<String, String>, Vec<i64>)> {
    let (_, user) = subject;
    let mut vars = BTreeMap::from([
        ("mention".into(), format!("<@{user}>")),
        ("user_id".into(), user.to_string()),
        ("rule".into(), incident.rule.clone()),
        ("rule_id".into(), clip(&incident.rule_id, 80)),
    ]);
    if filters
        .into_iter()
        .flatten()
        .any(|filter| excluded(filter, &vars))
    {
        return None;
    }
    let mut channels = BTreeMap::new();
    let mut source_channels = std::collections::BTreeSet::new();
    let mut failures = BTreeMap::<&str, BTreeMap<u64, usize>>::new();
    let mut first = None;
    let mut last = None;
    let mut sample = None;
    let mut evidence_visible = false;
    let mut confirmed = Vec::new();
    for (&(channel, message), item) in &incident.messages {
        if (incident.outcome == Some(CaseOutcome::Failed) && item.purge_origin.is_some())
            || (failures_only && item.result.is_ok())
            || filters
                .into_iter()
                .flatten()
                .any(|filter| channel_id_excluded(filter, Some(&channel.to_string())))
        {
            continue;
        }
        source_channels.insert(channel);
        first = Some(first.map_or(item.at, |first| item.at.min(first)));
        last = Some(last.map_or(item.at, |last| item.at.max(last)));
        match &item.result {
            Ok(row) => {
                confirmed.push(message as i64);
                *channels.entry(channel).or_default() += 1;
                if let Some((source, id, text)) = &incident.sample
                    && (*source, *id) == (channel, message)
                {
                    sample = Some(text.clone());
                } else if sample.is_none()
                    && let Some(row) = row.as_deref()
                    && row.expires_at_ms > chrono::Utc::now().timestamp_millis()
                {
                    sample = row
                        .fields
                        .content
                        .as_deref()
                        .filter(|text| !text.is_empty())
                        .map(|text| clip(text, 240));
                }
                evidence_visible = true;
            }
            Err(reason) => {
                *failures
                    .entry(reason)
                    .or_default()
                    .entry(channel)
                    .or_default() += 1
            }
        }
    }
    let format_time =
        |at: chrono::DateTime<chrono::Utc>| at.format("%Y-%m-%d %H:%M:%S UTC").to_string();
    let failure_count: usize = failures
        .values()
        .flat_map(|channels| channels.values())
        .sum();
    let details = failures
        .into_iter()
        .map(|(reason, channels)| format!("{reason} ({})", channel_counts(channels)))
        .collect::<Vec<_>>()
        .join("; ");
    let action = match (&*incident.action, &incident.outcome) {
        ("ban", Some(CaseOutcome::Succeeded)) => "Banned".into(),
        ("kick", Some(CaseOutcome::Succeeded)) => "Kicked".into(),
        ("ban", Some(CaseOutcome::Failed)) => "Ban failed".into(),
        ("kick", Some(CaseOutcome::Failed)) => "Kick failed".into(),
        (action, Some(CaseOutcome::Uncertain)) => format!("{action} outcome uncertain"),
        (action, Some(outcome)) => format!("{action} {outcome:?}"),
        (action, None) => format!("{action} outcome unavailable"),
    };
    vars.extend([
        ("cleanup_action".to_owned(), action),
        ("cleanup_total".to_owned(), confirmed.len().to_string()),
        ("cleanup_channels".to_owned(), channel_counts(channels)),
        ("cleanup_first".to_owned(), format_time(first?)),
        ("cleanup_last".to_owned(), format_time(last?)),
        (
            "cleanup_sample".to_owned(),
            sample.unwrap_or_else(|| "unavailable".into()),
        ),
        (
            "cleanup_evidence".to_owned(),
            incident
                .hash
                .as_ref()
                .filter(|_| evidence_visible)
                .map(|hash| format!("SHA-256 `{hash}`"))
                .unwrap_or_else(|| "unavailable".into()),
        ),
        (
            "cleanup_failures".to_owned(),
            if failure_count == 0 {
                "0".into()
            } else {
                format!("{failure_count} · {}", clip(&details, 400))
            },
        ),
        (
            "cleanup_source_channels".to_owned(),
            source_channels
                .into_iter()
                .map(|channel| channel.to_string())
                .collect::<Vec<_>>()
                .join(","),
        ),
    ]);
    Some((vars, confirmed))
}

pub(crate) async fn persist_cleanup_observations(app: &BotState, keys: &[CleanupObservation]) {
    for keys in keys.chunks(JOURNAL_PAGE) {
        let ready = {
            let journal = app.cleanup_incidents.journal.lock();
            keys.iter().any(|key| {
                journal
                    .observed
                    .get(key)
                    .is_some_and(|pending| pending.retry_at.is_none())
            })
        };
        if ready {
            persist_observation_chunk(app, keys, false).await;
        }
    }
}

async fn persist_observation_chunk(app: &BotState, keys: &[CleanupObservation], background: bool) {
    let queue = &app.cleanup_incidents;
    let _writer = queue.journal_writer.lock().await;
    let now = Clock::now();
    let records: Vec<_> = {
        let journal = queue.journal.lock();
        keys.iter()
            .filter_map(|key| {
                let pending = journal.observed.get(key)?;
                pending
                    .retry_at
                    .is_none_or(|at| background && at <= now)
                    .then(|| (*key, pending.row.clone()))
            })
            .collect()
    };
    if records.is_empty() {
        return;
    }
    let owner = app.activation.instance().to_owned();
    let rows = records.iter().map(|(_, row)| row.clone()).collect();
    let outcomes = db::save_cleanup_deletes(&app.db, owner, rows).await;
    let retry_at = Clock::now() + DELIVERY_RETRY;
    let mut journal = queue.journal.lock();
    match outcomes {
        Ok(outcomes) => {
            for ((key, row), outcome) in records.into_iter().zip(outcomes) {
                if let Some(pending) = journal.observed.get_mut(&key)
                    && pending.row == row
                {
                    let batch = pending.batch;
                    if outcome == CleanupDeleteSave::WriteFenced {
                        pending.retry_at = Some(retry_at);
                    } else {
                        journal.observed.remove(&key);
                    }
                    if outcome != CleanupDeleteSave::Saved {
                        journal.skipped.insert((batch, key.1), outcome);
                    }
                }
            }
        }
        Err(error) => {
            tracing::warn!(%error, "could not journal confirmed cleanup deletes");
            for (key, row) in records {
                if let Some(pending) = journal.observed.get_mut(&key)
                    && pending.row == row
                {
                    pending.retry_at = Some(retry_at);
                }
            }
        }
    }
}

async fn persist_cleanup_queue(app: &BotState, shutdown: bool) {
    let queue = &app.cleanup_incidents;
    let now = Clock::now();
    let (fresh, mut due) = {
        let journal = queue.journal.lock();
        let mut fresh = Vec::new();
        let mut due = Vec::new();
        for (&key, pending) in &journal.observed {
            match pending.retry_at {
                None => fresh.push(key),
                Some(at) if at <= now => due.push((at, key)),
                Some(_) => {}
            }
        }
        (fresh, due)
    };
    for keys in fresh.chunks(JOURNAL_PAGE) {
        persist_observation_chunk(app, keys, true).await;
    }
    due.sort_unstable();
    let limit = if shutdown { due.len() } else { JOURNAL_PAGE };
    let keys: Vec<_> = due.into_iter().take(limit).map(|(_, key)| key).collect();
    for keys in keys.chunks(JOURNAL_PAGE) {
        persist_observation_chunk(app, keys, true).await;
    }
}

async fn settle_cleanup_queue(app: &BotState, shutdown: bool) {
    let queue = &app.cleanup_incidents;
    if queue.journal.lock().settled.is_empty() {
        return;
    }
    let _writer = queue.journal_writer.lock().await;
    let settled = {
        let journal = queue.journal.lock();
        if !shutdown
            && journal
                .settlement_retry_at
                .is_some_and(|at| at > Clock::now())
        {
            return;
        }
        journal.settled.clone()
    };
    for targets in settled.into_iter().collect::<Vec<_>>().chunks(256) {
        let keys = targets
            .iter()
            .map(|&(nonce, message)| (batch_key(nonce), message))
            .collect();
        if let Err(error) = db::settle_cleanup_deletes(&app.db, keys).await {
            tracing::warn!(%error, "could not settle cleanup journal");
            queue.journal.lock().settlement_retry_at = Some(Clock::now() + DELIVERY_RETRY);
            return;
        }
        let mut journal = queue.journal.lock();
        for key in targets {
            journal.settled.remove(key);
        }
    }
    queue.journal.lock().settlement_retry_at = None;
}

pub(crate) async fn flush(app: &Arc<BotState>, sink: &impl LogDelivery, shutdown: bool) {
    let due = app.cleanup_incidents.queue_due(Clock::now(), shutdown);
    persist_cleanup_queue(app, shutdown).await;
    for id in due {
        let Some(mut lease) = CleanupIncidents::lease(app, id) else {
            continue;
        };
        let pending = lease.pending.as_mut().expect("leased incident");
        pending.deliver(app, sink, shutdown).await;
        pending.retry_delay = (pending.retry_delay * 2).clamp(DELIVERY_RETRY, MAX_DELIVERY_RETRY);
        pending.retry_at = Clock::now() + pending.retry_delay;
        if let LogDispatch::RateLimited(until) = pending.summary_dispatch {
            pending.retry_at = pending.retry_at.max(until);
        }
    }
    settle_cleanup_queue(app, shutdown).await;
}

impl PendingIncident {
    fn finish(&mut self, app: &BotState, target: Message) {
        if let Some(item) = self.incident.messages.remove(&target)
            && item.result.is_ok()
        {
            let mut journal = app.cleanup_incidents.journal.lock();
            let batch = self.incident.nonce;
            let key = (self.subject.0, target);
            if journal
                .observed
                .get(&key)
                .is_some_and(|pending| pending.batch == batch)
            {
                journal.observed.remove(&key);
            }
            journal.skipped.remove(&(batch, target));
            journal.settled.insert((batch, target.1 as i64));
            drop(journal);
            app.automod_delete_marks
                .finish_cleanup_delete((self.subject.0, target.0, target.1));
        }
    }

    fn finish_matching(&mut self, app: &BotState, dispose: impl Fn(&Delete) -> bool) {
        let targets = self
            .incident
            .messages
            .iter()
            .filter(|(_, item)| dispose(item))
            .map(|(&target, _)| target)
            .collect::<Vec<_>>();
        for target in targets {
            self.finish(app, target);
        }
    }

    fn finish_summary(&mut self, app: &BotState, uncertain: bool) {
        let rejected = self.incident.outcome == Some(CaseOutcome::Failed);
        // Rejected-ban purge echoes have an independent ordinary-log route.
        self.finish_matching(app, |item| !rejected || item.purge_origin.is_none());
        self.summary_dispatch = LogDispatch::Pending;
        self.fallback = true;
        if uncertain {
            record_delivery_failure(app);
        }
    }

    async fn deliver(&mut self, app: &Arc<BotState>, sink: &impl LogDelivery, shutdown: bool) {
        // Shutdown can interrupt the worker after handing the summary to the
        // transport. Its acknowledgement may be lost even though Discord posted it.
        if self.summary_dispatch == LogDispatch::MayHaveSent {
            self.finish_summary(app, true);
        }
        if self.incident.messages.is_empty() {
            return;
        }
        // If the primary could not save the observations, keep the batch in
        // memory for retry. A dispatched summary must cover durable records.
        let (awaiting_journal, fenced, retired) = {
            let batch = self.incident.nonce;
            let targets = (batch, (0, 0))..=(batch, (u64::MAX, u64::MAX));
            let journal = app.cleanup_incidents.journal.lock();
            let awaiting = self.incident.messages.keys().any(|&target| {
                journal
                    .observed
                    .get(&(self.subject.0, target))
                    .is_some_and(|pending| pending.batch == batch)
            });
            let mut fenced = false;
            let mut retired = false;
            for (_, outcome) in journal.skipped.range(targets) {
                fenced |= *outcome == CleanupDeleteSave::WriteFenced;
                retired |= *outcome == CleanupDeleteSave::Retired;
            }
            (awaiting, fenced, retired)
        };
        if retired {
            self.finish_matching(app, |_| true);
            return;
        }
        self.fallback |= fenced;
        if awaiting_journal && (!shutdown || fenced) {
            return;
        }
        let (guild, _) = self.subject;
        let Ok(cfg) = app.configs.get(&app.db, guild).await else {
            return;
        };

        let rejected = self.incident.outcome == Some(CaseOutcome::Failed);
        if rejected {
            self.replay_rejected_purge(app, sink, &cfg).await;
        }

        let route = summary_route(&cfg);
        let bulk_filter = route.and_then(|(_, filter)| filter);
        let filter = log_filter_for(&cfg, "automod_delete");
        let summary_filters = [filter, bulk_filter];
        // Ordinary bulk fallback still counts confirmed deletes excluded by user.
        let filtered = self
            .incident
            .messages
            .iter()
            .filter(|(target, item)| {
                !(rejected && item.purge_origin.is_some())
                    && (filter
                        .is_some_and(|filter| excluded_delete(filter, self.subject, target.0))
                        || (item.result.is_err()
                            && bulk_filter.is_some_and(|filter| {
                                excluded_delete(filter, self.subject, target.0)
                            })))
            })
            .map(|(&target, _)| target)
            .collect::<Vec<_>>();
        for target in filtered {
            self.finish(app, target);
        }

        if !shutdown && !self.fallback {
            if let Some((mut vars, confirmed)) =
                summary_vars(self.subject, &self.incident, summary_filters, false)
            {
                if let Some(case) = &self.incident.case {
                    let id = case.as_str();
                    let case_display =
                        jmodguard::templates::public_webui_url(&app.cfg.bot.about_webui_url)
                            .map(|base| {
                                format!("[Open case]({base}/?list=cases&guild={guild}&case={id})")
                            })
                            .unwrap_or_else(|| format!("Case `{id}`"));
                    vars.entry("cleanup_action".into()).and_modify(|action| {
                        action.push_str(" · ");
                        action.push_str(&case_display);
                    });
                }
                vars.insert(
                    "cleanup_nonce".into(),
                    format!("cleanup-s{:016x}", self.incident.nonce),
                );
                let batch = batch_key(self.incident.nonce);
                let journal = db::CleanupJournalKey {
                    guild_id: guild as i64,
                    enrollment_id: self.incident.enrollment_id.as_deref(),
                    batch: &batch,
                    messages: &confirmed,
                    recovery: None,
                };
                let delivery = emit_cleanup_summary(
                    app,
                    sink,
                    GuildId::new(guild),
                    vars,
                    &mut self.summary_dispatch,
                    journal,
                )
                .await;
                match delivery {
                    SummaryDelivery::Sent | SummaryDelivery::Ambiguous => {
                        self.finish_summary(app, delivery == SummaryDelivery::Ambiguous);
                        return;
                    }
                    SummaryDelivery::Retry => return,
                    SummaryDelivery::Fallback => {}
                }
            }
            self.fallback = true;
        }

        let targets = self.incident.messages.keys().copied().collect::<Vec<_>>();
        for target in targets {
            let item = &self.incident.messages[&target];
            if !(rejected && item.purge_origin.is_some())
                && let Ok(retained) = &item.result
                && self
                    .individual(target)
                    .emit(app, sink, &cfg, retained.as_deref(), "automod_delete")
                    .await
            {
                self.finish(app, target);
            }
        }

        if let Some((mut vars, _)) =
            summary_vars(self.subject, &self.incident, summary_filters, true)
        {
            vars.insert("cleanup_failure_notice".into(), String::new());
            vars.insert(
                "cleanup_nonce".into(),
                format!("cleanup-f{:016x}", self.incident.nonce),
            );
            let scope = self.individual((0, 0));
            let mut dispatch = LogDispatch::Pending;
            let attempt = LogAttempt::new(&mut dispatch, Some((&app.db, scope.journal(&[]))));
            if route.is_none()
                || emit_to_observed_with_audit(
                    app,
                    sink,
                    GuildId::new(guild),
                    "automod_delete",
                    vars,
                    None,
                    Admission::WaitForShutdown,
                    || true,
                    false,
                    None,
                    Some(attempt),
                )
                .await
                .0 == Delivery::Sent
            {
                self.incident.messages.retain(|_, item| item.result.is_ok());
            }
        }
    }

    async fn replay_rejected_purge(
        &mut self,
        app: &Arc<BotState>,
        sink: &impl LogDelivery,
        cfg: &crate::state::CachedGuildConfig,
    ) {
        let mut bulk = BTreeMap::<u64, Vec<Message>>::new();
        let targets = self.incident.messages.keys().copied().collect::<Vec<_>>();
        for target in targets {
            let item = &self.incident.messages[&target];
            match (item.purge_origin, &item.result) {
                (Some(GatewayDeleteOrigin::Bulk), Ok(_)) => {
                    bulk.entry(target.0).or_default().push(target)
                }
                (Some(GatewayDeleteOrigin::Single), Ok(retained))
                    if self
                        .individual(target)
                        .emit(app, sink, cfg, retained.as_deref(), "message_delete")
                        .await =>
                {
                    self.finish(app, target);
                }
                _ => {}
            }
        }
        for (source, targets) in bulk {
            let entries = targets.iter().filter_map(|target| {
                let row = self.incident.messages[target].result.as_ref().ok()?;
                Some((target.1, row.as_deref()))
            });
            if self
                .individual((source, 0))
                .emit_bulk(app, sink, cfg, targets.len(), entries)
                .await
            {
                for target in targets {
                    self.finish(app, target);
                }
            }
        }
    }

    fn individual(&self, target: Message) -> IndividualCleanup<'_> {
        IndividualCleanup {
            subject: self.subject,
            target,
            rule_id: &self.incident.rule_id,
            rule: &self.incident.rule,
            reason: &self.incident.reason,
            observed_at: None,
            batch: batch_key(self.incident.nonce),
            enrollment_id: self.incident.enrollment_id.as_deref(),
            recovery: None,
        }
    }
}

struct IndividualCleanup<'a> {
    subject: Subject,
    target: Message,
    rule_id: &'a str,
    rule: &'a str,
    reason: &'a str,
    observed_at: Option<chrono::DateTime<chrono::Utc>>,
    batch: String,
    enrollment_id: Option<&'a str>,
    recovery: Option<(&'a str, &'a db::CleanupRecovery)>,
}

impl IndividualCleanup<'_> {
    fn journal<'a>(&'a self, messages: &'a [i64]) -> db::CleanupJournalKey<'a> {
        db::CleanupJournalKey {
            guild_id: self.subject.0 as i64,
            enrollment_id: self.enrollment_id,
            batch: &self.batch,
            messages,
            recovery: self.recovery,
        }
    }

    /// A true result is a delivered report or an explicit routing/filter decision.
    async fn emit(
        &self,
        app: &Arc<BotState>,
        sink: &impl LogDelivery,
        cfg: &crate::state::CachedGuildConfig,
        retained: Option<&MessageSnapshot>,
        event: &str,
    ) -> bool {
        let (source, message) = self.target;
        if log_filter_for(cfg, event)
            .is_some_and(|filter| excluded_delete(filter, self.subject, source))
        {
            return true;
        }
        if channel_for(cfg, event).is_none() {
            return self
                .emit_bulk(app, sink, cfg, 1, [(message, retained)])
                .await;
        }
        let (guild, user) = self.subject;
        let retained = settle_automod_retained(app, guild, source, message, retained);
        let guild = GuildId::new(guild);
        let channel = ChannelId::new(source);
        let id = MessageId::new(message);
        let mut report = if event == "message_delete" {
            let mut report = message_delete_report(app, guild, channel, id);
            apply_retained_delete(&mut report, retained.as_ref());
            report
        } else {
            automod_delete_report(
                app,
                guild,
                channel,
                id,
                self.rule_id,
                self.rule,
                self.reason,
                retained.as_ref(),
            )
        };
        report.vars.extend(templates::user_vars(
            user,
            &user.to_string(),
            &user.to_string(),
        ));
        report
            .vars
            .insert("cleanup_nonce".into(), format!("cleanup-m{message:016x}"));
        if let Some(observed_at) = self.observed_at {
            report.vars.insert("time".into(), observed_at.to_rfc3339());
        }
        let mut dispatch = LogDispatch::Pending;
        let messages = [message as i64];
        let attempt = LogAttempt::new(&mut dispatch, Some((&app.db, self.journal(&messages))));
        let (delivery, _) = emit_to_observed_with_audit(
            app,
            sink,
            guild,
            event,
            report.vars,
            None,
            Admission::WaitForShutdown,
            || true,
            false,
            None,
            Some(attempt),
        )
        .await;
        if dispatch == LogDispatch::MayHaveSent && delivery != Delivery::Sent {
            record_delivery_failure(app);
            return true;
        }
        delivery == Delivery::Sent
    }
    async fn emit_bulk<'a>(
        &self,
        app: &Arc<BotState>,
        sink: &impl LogDelivery,
        cfg: &crate::state::CachedGuildConfig,
        count: usize,
        entries: impl IntoIterator<Item = (u64, Option<&'a MessageSnapshot>)> + Send,
    ) -> bool {
        let (guild, user) = self.subject;
        let source = self.target.0;
        let event = "message_bulk_delete";
        let filter = log_filter_for(cfg, event);
        if channel_for(cfg, event).is_none()
            || filter.is_some_and(|filter| channel_id_excluded(filter, Some(&source.to_string())))
        {
            return true;
        }
        let filtered = filter.is_some_and(|filter| filter.users.contains(&user.to_string()));
        let mut transcript = BulkTranscript::new(count);
        if !filtered && let Ok(author) = i64::try_from(user) {
            // The author is known even where snapshot storage had no row.
            transcript.included_authors.insert(author);
        }
        let mut messages = Vec::with_capacity(count);
        for (message, retained) in entries {
            messages.push(message as i64);
            let id = MessageId::new(message);
            if filtered {
                transcript.push_filtered();
            } else if let Some(row) = settle_automod_retained(app, guild, source, message, retained)
            {
                transcript.push(id, Some(&row));
            } else {
                transcript.push_unknown(id);
            }
        }
        emit_retained_bulk_with_journal(
            app,
            sink,
            GuildId::new(guild),
            ChannelId::new(source),
            transcript,
            Some(self.journal(&messages)),
        )
        .await
            != BulkDeliveryOutcome::Failed
    }
}

impl<'a> From<&'a CleanupDeleteRecord> for IndividualCleanup<'a> {
    fn from(record: &'a CleanupDeleteRecord) -> Self {
        Self {
            subject: (record.guild_id as u64, record.user_id as u64),
            target: (record.channel_id as u64, record.message_id as u64),
            rule_id: &record.rule_id,
            rule: &record.rule_name,
            reason: "Automod cleanup interrupted by restart",
            observed_at: chrono::DateTime::from_timestamp_millis(record.observed_at_ms),
            batch: record.batch.clone(),
            enrollment_id: record.enrollment_id.as_deref(),
            recovery: None,
        }
    }
}

fn excluded_delete(filter: &db::LogFilter, (_, user): Subject, channel: u64) -> bool {
    filter.users.contains(&user.to_string())
        || channel_id_excluded(filter, Some(&channel.to_string()))
}

async fn recover_cleanup_page(
    app: &Arc<BotState>,
    sink: &impl LogDelivery,
    after: i64,
) -> anyhow::Result<(Option<i64>, bool)> {
    let records =
        db::recovered_cleanup_deletes(&app.db, app.activation.instance().into(), after).await?;
    let last = records.last().map(|(sequence, _)| *sequence);
    let mut retry = false;
    for (_, record) in records {
        let guild = record.guild_id as u64;
        let Ok(cfg) = app.configs.get(&app.db, guild).await else {
            retry = true;
            continue;
        };
        // The active membership may have changed since the page was read.
        let Ok(enrollment) = db::guild_enrollment(&app.db, record.guild_id).await else {
            retry = true;
            continue;
        };
        if enrollment.map(|enrollment| enrollment.id) != record.enrollment_id {
            retry = true;
            continue;
        }
        let mut notice = IndividualCleanup::from(&record);
        notice.recovery = Some((app.activation.instance(), &app.cleanup_incidents.recovery));
        // Without the outcome of the interrupted enforcement, a purge echo
        // has known author identity but might have been an unrelated delete.
        let event = if record.purge {
            "message_delete"
        } else {
            "automod_delete"
        };
        if notice.emit(app, sink, &cfg, None, event).await {
            db::settle_cleanup_deletes(&app.db, vec![(record.batch, record.message_id)]).await?;
        } else {
            retry = true;
        }
    }
    Ok((last, retry))
}

async fn recover_cleanup_reports(app: &Arc<BotState>, sink: &impl LogDelivery) {
    let mut after = 0;
    let mut retry = app.cleanup_incidents.recovery.has_pending_claims();
    loop {
        match recover_cleanup_page(app, sink, after).await {
            Ok((Some(last), failed)) => {
                after = last;
                retry |= failed;
                continue;
            }
            Ok((None, _)) if !retry => loop {
                match db::prune_cleanup_journal(
                    &app.db,
                    app.activation.instance().into(),
                    &app.cleanup_incidents.recovery,
                )
                .await
                {
                    Ok(256) => continue,
                    Ok(_) => return,
                    Err(error) => {
                        tracing::warn!(%error, "could not prune the cleanup delete journal");
                        break;
                    }
                }
            },
            Ok((None, _)) => {}
            Err(error) => tracing::warn!(%error, "could not recover pending cleanup deletes"),
        }
        #[cfg(test)]
        let _ = TEST_RECOVERY_WAITING.try_with(|notify| notify.notify_one());
        tokio::time::sleep(DELIVERY_RETRY).await;
        after = 0;
        // Snapshot before scanning. A release can make an earlier row
        // replayable after a page has already passed its cursor.
        retry = app.cleanup_incidents.recovery.has_pending_claims();
    }
}

#[cfg(test)]
tokio::task_local! {
    static TEST_RECOVERY_WAITING: Arc<tokio::sync::Notify>;
}

pub async fn run_cleanup_reports(app: Arc<BotState>, http: Arc<serenity::http::Http>) {
    run_cleanup_worker(&app, http.as_ref()).await;
}

async fn run_cleanup_worker(app: &Arc<BotState>, sink: &impl LogDelivery) {
    let journal = async {
        loop {
            app.cleanup_incidents.wait_for_observations().await;
            if app.activation.is_shutting_down() {
                break;
            }
            persist_cleanup_queue(app, false).await;
        }
    };
    let live = async {
        while !app.activation.is_shutting_down() {
            app.cleanup_incidents.wait_until_due().await;
            if !app.activation.is_shutting_down() {
                flush(app, sink, false).await;
            }
        }
    };
    tokio::select! {
        _ = crate::shutdown_signal() => {},
        _ = async { tokio::join!(recover_cleanup_reports(app, sink), journal, live); } => {},
    }
}

pub async fn flush_cleanup_on_shutdown(app: &Arc<BotState>, http: &serenity::http::Http) {
    flush(app, http, true).await;
}

#[cfg(test)]
mod tests;
