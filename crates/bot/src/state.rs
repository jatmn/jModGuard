//! Bot runtime state and compiled guild configuration cache.

use anyhow::{Context as _, Result};
use dashmap::DashMap;
use parking_lot::Mutex;
use regex::Regex;
use serenity::model::id::ChannelId;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::automod::AutomodEngine;
use crate::config::BotProcessConfig;
use crate::db::{self, Db};
use crate::types::{CommandDef, GuildSettings, RuleDef};

/// In-flight Discord slash re-sync for one durable `slash_registry_version`.
///
/// Success is per-guild; remembering only the failures avoids re-POSTing
/// `set_commands` for every cached guild on every poll (rate-limit feedback)
/// while still refusing to commit the observed version until nothing remains.
struct SlashSyncProgress {
    target: u64,
    /// `None` = first pass (sync every cached guild). `Some` = retry set.
    remaining: Option<HashSet<u64>>,
    /// Config cache already invalidated once for this target.
    invalidated: bool,
}

struct SlashRetry {
    target: u64,
    revision: String,
    retry: crate::discord::RestRetry,
}

#[derive(Default)]
struct StartupNoticeTask {
    stopping: bool,
    task: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum GatewayCaseAction {
    Ban,
    Unban,
}

const GATEWAY_CASE_SUPPRESSION_TTL: Duration = Duration::from_secs(30);
const GATEWAY_MANUAL_BAN_AUDIT_TTL: Duration = Duration::from_secs(120);
const MAX_GATEWAY_MANUAL_BAN_AUDIT_QUEUE: usize = 8;
const MAX_GATEWAY_CASE_SUPPRESSIONS: usize = 4096;
#[derive(Debug, Clone)]
pub(crate) struct GatewayCaseSuppression {
    id: u128,
    released: Arc<AtomicBool>,
}

/// One decision shared by the mirror, case routing and attribution.
pub(crate) struct GatewayBanEcho {
    pub(crate) suppress_case: bool,
    pub(crate) owned_by_bot: bool,
    pub(crate) operation_id: Option<String>,
}

/// Identity captured before awaits, still revocable by its own failed dispatch.
pub(crate) struct GatewayBanEchoCapture {
    echo: GatewayBanEcho,
    released: Option<Arc<AtomicBool>>,
}

impl GatewayBanEchoCapture {
    /// Resolve once before persistence so every consumer uses the same decision.
    pub(crate) fn resolve(self) -> GatewayBanEcho {
        if self
            .released
            .is_some_and(|released| released.load(Ordering::Acquire))
        {
            GatewayBanEcho {
                suppress_case: false,
                owned_by_bot: false,
                operation_id: None,
            }
        } else {
            self.echo
        }
    }
}

struct GatewayCaseEntry {
    action: GatewayCaseAction,
    guild: u64,
    user: u64,
    operation_id: Option<String>,
    expires: Instant,
    released: Arc<AtomicBool>,
}

struct GatewayBanEchoEntry {
    expires: Instant,
    operation_id: Option<String>,
    released: Arc<AtomicBool>,
}

/// Lock stripes stay fixed; registrations and durable peer identities are scoped
/// to each guild/user. Retained delivery owners keep registrations until storage
/// completes or the captured membership is invalidated.
struct GatewayBanFlight {
    started_at_ms: i64,
    subjects: HashMap<(u64, u64), GatewayBanSubject>,
}

struct GatewayBanSubject {
    active: usize,
    active_ids: Vec<db::GatewayBanPeer>,
}

pub(crate) struct GatewayBanObservation {
    pub(crate) delivery_id: db::guild_data::CaseId,
    flight: Arc<Mutex<GatewayBanFlight>>,
    guild: u64,
    user: u64,
    pub(crate) observed: chrono::DateTime<chrono::Utc>,
    released: bool,
    app: std::sync::Weak<BotState>,
    runtime: tokio::runtime::Handle,
    pub(crate) enrollment: Option<db::GuildEnrollment>,
}

impl GatewayBanObservation {
    pub(crate) fn overlapping_deliveries(
        &self,
        enrollment: &db::GuildEnrollment,
    ) -> Vec<db::GatewayBanPeer> {
        self.flight
            .lock()
            .subjects
            .get(&(self.guild, self.user))
            .map(|subject| {
                subject
                    .active_ids
                    .iter()
                    .filter(|peer| {
                        peer.id != self.delivery_id.as_str()
                            && peer.observed_at_ms >= enrollment.joined_at_ns / 1_000_000
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// New deliveries cannot claim audits from before this process started.
    /// Live registrations and durable peer dependencies cover overlap; ordinary
    /// storage failures remain owned by the retained delivery task.
    pub(crate) fn audit_after_ms(&self) -> i64 {
        self.flight.lock().started_at_ms
    }

    pub(crate) async fn finish(mut self) {
        self.release();
        // Cancelling the caller must not discard a saved row's queued audit.
        if let Some(retry) = self.retry() {
            let _ = retry.await;
        }
    }

    fn release(&mut self) {
        if self.released {
            return;
        }
        let mut flight = self.flight.lock();
        if let Some(subject) = flight.subjects.get_mut(&(self.guild, self.user)) {
            subject
                .active_ids
                .retain(|peer| peer.id != self.delivery_id.as_str());
            subject.active = subject.active.saturating_sub(1);
            if subject.active == 0 {
                flight.subjects.remove(&(self.guild, self.user));
            }
        }
        self.released = true;
    }

    fn retry(&self) -> Option<tokio::task::JoinHandle<()>> {
        let app = self.app.upgrade()?;
        let enrollment = self.enrollment.clone()?;
        let (guild, user) = (self.guild, self.user);
        Some(self.runtime.spawn(async move {
            crate::handlers::reconcile_pending_gateway_ban_audits(
                &app,
                Some(enrollment),
                guild,
                user,
            )
            .await;
        }))
    }
}

impl Drop for GatewayBanObservation {
    fn drop(&mut self) {
        if !self.released {
            // The flight lock protects only in-memory bookkeeping. No SQL or
            // async wait can hold it, so release never needs a cleanup thread.
            self.release();
            self.retry();
        }
    }
}

fn gateway_ban_audit_blocked(flight: &GatewayBanFlight, guild: u64, user: u64) -> bool {
    flight
        .subjects
        .get(&(guild, user))
        .is_some_and(|subject| subject.active != 0)
}

fn gateway_ban_flight_index(guild: u64, user: u64, shards: usize) -> usize {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (guild, user).hash(&mut hasher);
    hasher.finish() as usize % shards
}

#[derive(Default)]
struct GatewayManualBanAuditQueue {
    entries: HashMap<(u64, u64), VecDeque<PendingGatewayBanAudit>>,
}

struct PendingGatewayBanAudit {
    entry_id: i64,
    expires: Instant,
    attribution: Option<db::GatewayBanAuditAttribution>,
    valid: Arc<AtomicBool>,
    retrying: Arc<AtomicBool>,
}

struct GatewayBanAuditRetry(Arc<AtomicBool>);

impl Drop for GatewayBanAuditRetry {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Drop for PendingGatewayBanAudit {
    fn drop(&mut self) {
        // Clear, expiry, eviction and acknowledgment also revoke sleeping passes.
        self.valid.store(false, Ordering::Release);
    }
}

impl GatewayManualBanAuditQueue {
    fn prune(&mut self, now: Instant) {
        for deque in self.entries.values_mut() {
            deque.retain(|entry| entry.expires > now);
        }
        self.entries.retain(|_, deque| !deque.is_empty());
    }

    #[cfg(test)]
    fn queue(&mut self, guild: u64, user: u64, entry_id: i64) {
        self.queue_attributed(guild, user, entry_id, None);
    }

    fn queue_attributed(
        &mut self,
        guild: u64,
        user: u64,
        entry_id: i64,
        attribution: Option<db::GatewayBanAuditAttribution>,
    ) {
        if entry_id <= 0 {
            return;
        }
        let now = Instant::now();
        self.prune(now);
        let deque = self.entries.entry((guild, user)).or_default();
        if let Some(entry) = deque.iter_mut().find(|entry| entry.entry_id == entry_id) {
            if entry.attribution.is_none() {
                entry.attribution = attribution;
            }
            return;
        }
        while deque.len() >= MAX_GATEWAY_MANUAL_BAN_AUDIT_QUEUE {
            deque.pop_front();
        }
        deque.push_back(PendingGatewayBanAudit {
            entry_id,
            expires: now + GATEWAY_MANUAL_BAN_AUDIT_TTL,
            attribution,
            valid: Arc::new(AtomicBool::new(true)),
            retrying: Arc::new(AtomicBool::new(false)),
        });
    }

    fn pending(&mut self, guild: u64, user: u64) -> Vec<i64> {
        self.prune(Instant::now());
        self.entries
            .get(&(guild, user))
            .map(|entries| entries.iter().map(|entry| entry.entry_id).collect())
            .unwrap_or_default()
    }

    #[cfg(test)]
    fn clear(&mut self, guild: u64, user: u64) {
        self.entries.remove(&(guild, user));
    }

    fn drop_entry(&mut self, guild: u64, user: u64, entry_id: i64) {
        if entry_id <= 0 {
            return;
        }
        let now = Instant::now();
        self.prune(now);
        let key = (guild, user);
        let Some(deque) = self.entries.get_mut(&key) else {
            return;
        };
        deque.retain(|entry| entry.entry_id != entry_id);
        if deque.is_empty() {
            self.entries.remove(&key);
        }
    }
}

#[derive(Default)]
struct GatewayCaseSuppressions {
    next_token: u128,
    entries: HashMap<u128, GatewayCaseEntry>,
    /// Ban tokens already consumed. A second delivery of the same bot echo
    /// has no token left, but it is still the bot's ban until this expires
    /// or an unban clears it or its dispatch is released. The string is that
    /// attempt's operation id.
    recent_ban_echo: HashMap<(u64, u64), GatewayBanEchoEntry>,
}

/// A write failure cannot be recorded in the same unavailable guild store.
/// Keep its enrollment and affected users until the worker can persist the gap.
#[derive(Clone)]
pub(crate) struct PendingMemberGap {
    pub(crate) guild_id: u64,
    pub(crate) enrollment_id: Option<String>,
    pub(crate) users: Vec<(i64, bool)>,
    pub(crate) reason: String,
    generation: u64,
}

#[derive(Default)]
struct PendingMemberGaps {
    generation: u64,
    by_scope: HashMap<(u64, Option<String>), PendingMemberGap>,
}

impl PendingMemberGaps {
    fn remember(
        &mut self,
        guild_id: u64,
        enrollment_id: Option<String>,
        user_id: Option<i64>,
        reason: &str,
    ) {
        const MAX_AFFECTED_USERS: usize = 1024;
        self.generation = self.generation.wrapping_add(1);
        let key = (guild_id, enrollment_id.clone());
        let gap = self
            .by_scope
            .entry(key)
            .or_insert_with(|| PendingMemberGap {
                guild_id,
                enrollment_id,
                users: Vec::new(),
                reason: reason.to_owned(),
                generation: self.generation,
            });
        gap.generation = self.generation;
        gap.reason = reason.to_owned();
        if let Some(user_id) = user_id.filter(|id| *id > 0)
            && !gap.users.contains(&(user_id, reason.contains("ban")))
            && gap.users.len() < MAX_AFFECTED_USERS
        {
            gap.users.push((user_id, reason.contains("ban")));
        }
    }

    fn snapshot(&self, guild_id: u64) -> Vec<PendingMemberGap> {
        self.by_scope
            .values()
            .filter(|gap| gap.guild_id == guild_id)
            .cloned()
            .collect()
    }

    fn acknowledge(&mut self, gap: &PendingMemberGap) {
        let key = (gap.guild_id, gap.enrollment_id.clone());
        if self
            .by_scope
            .get(&key)
            .is_some_and(|stored| stored.generation == gap.generation)
        {
            self.by_scope.remove(&key);
        }
    }
}

impl GatewayCaseSuppressions {
    fn prune(&mut self, now: Instant) {
        self.entries.retain(|_, entry| entry.expires > now);
        self.recent_ban_echo
            .retain(|_, entry| entry.expires > now && !entry.released.load(Ordering::Acquire));
    }

    fn insert(
        &mut self,
        action: GatewayCaseAction,
        guild: u64,
        user: u64,
        operation_id: Option<String>,
    ) -> GatewayCaseSuppression {
        let now = Instant::now();
        self.prune(now);
        if self.entries.len() >= MAX_GATEWAY_CASE_SUPPRESSIONS
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.expires)
                .map(|(key, _)| *key)
        {
            self.entries.remove(&oldest);
        }
        self.next_token += 1;
        let token = GatewayCaseSuppression {
            id: self.next_token,
            released: Arc::new(AtomicBool::new(false)),
        };
        self.entries.insert(
            token.id,
            GatewayCaseEntry {
                action,
                guild,
                user,
                operation_id,
                expires: now + GATEWAY_CASE_SUPPRESSION_TTL,
                released: Arc::clone(&token.released),
            },
        );
        token
    }

    fn release(&mut self, token: GatewayCaseSuppression) {
        // Consumption must not detach the captured echo from a definitive
        // failure. This flag belongs only to this dispatch, even after unban.
        token.released.store(true, Ordering::Release);
        self.prune(Instant::now());
        self.entries.remove(&token.id);
    }

    fn remove(&mut self, action: GatewayCaseAction, guild: u64, user: u64) -> bool {
        self.prune(Instant::now());
        let token = self
            .entries
            .iter()
            .filter(|(_, entry)| {
                entry.action == action && entry.guild == guild && entry.user == user
            })
            .min_by_key(|(_, entry)| entry.expires)
            .map(|(token, _)| *token);
        let Some(entry) = token.and_then(|token| self.entries.remove(&token)) else {
            return false;
        };
        if entry.action == GatewayCaseAction::Ban {
            self.recent_ban_echo.insert(
                (entry.guild, entry.user),
                GatewayBanEchoEntry {
                    expires: Instant::now() + GATEWAY_CASE_SUPPRESSION_TTL,
                    operation_id: entry.operation_id,
                    released: entry.released,
                },
            );
        }
        true
    }

    /// `Some` when a consumed bot ban echo is still remembered.
    /// The string is that attempt's operation id, when the dispatch saved one.
    #[cfg(test)]
    fn ban_echo_operation(&mut self, guild: u64, user: u64) -> Option<Option<String>> {
        let now = Instant::now();
        self.prune(now);
        self.recent_ban_echo
            .get(&(guild, user))
            .map(|entry| entry.operation_id.clone())
    }

    fn clear_ban_echo(&mut self, guild: u64, user: u64) {
        self.recent_ban_echo.remove(&(guild, user));
        // Pending tokens may belong to a newer dispatch that started before
        // this unban callback. Only consumed echo ownership ends here.
    }
}

#[derive(Clone)]
pub(crate) struct PendingAuditLink {
    pub(crate) action: crate::db::guild_data::CaseAction,
    pub(crate) guild: u64,
    pub(crate) subject: u64,
    pub(crate) case_id: crate::db::guild_data::CaseId,
    pub(crate) operation_id: crate::db::guild_data::CaseId,
    pub(crate) enrollment: db::GuildEnrollment,
}

#[derive(Clone)]
struct PendingDiscordAudit {
    action: crate::db::guild_data::CaseAction,
    guild: u64,
    subject: u64,
    entry_id: i64,
}

/// Application and audit halves matched in-memory but not yet durably linked.
#[derive(Clone)]
pub(crate) struct PendingAuditReady {
    pub(crate) link: PendingAuditLink,
    pub(crate) entry_id: i64,
    pub(crate) audit_token: Option<u128>,
    pub(crate) application_token: Option<u128>,
}

#[derive(Default)]
struct PendingAuditLinks {
    next_token: u128,
    applications: HashMap<u128, (PendingAuditLink, Instant)>,
    audits: HashMap<u128, (PendingDiscordAudit, Instant)>,
    deferred_links: Vec<(PendingAuditReady, Instant)>,
}

impl PendingAuditLinks {
    fn prune(&mut self, now: Instant) {
        self.applications.retain(|_, (_, expires)| *expires > now);
        self.audits.retain(|_, (_, expires)| *expires > now);
        self.deferred_links.retain(|(_, expires)| *expires > now);
    }

    fn defer_link(&mut self, ready: PendingAuditReady) {
        let now = Instant::now();
        self.prune(now);
        self.finalize_link(ready.audit_token, ready.application_token);
        if self.deferred_links.iter().any(|(existing, _)| {
            existing.entry_id == ready.entry_id && existing.link.case_id == ready.link.case_id
        }) {
            return;
        }
        if self.deferred_links.len() >= MAX_GATEWAY_CASE_SUPPRESSIONS
            && let Some(oldest) = self
                .deferred_links
                .iter()
                .enumerate()
                .min_by_key(|(_, (_, expires))| *expires)
                .map(|(index, _)| index)
        {
            self.deferred_links.remove(oldest);
        }
        self.deferred_links
            .push((ready, now + GATEWAY_CASE_SUPPRESSION_TTL));
    }

    fn clear_deferred_link(&mut self, case_id: &crate::db::guild_data::CaseId, entry_id: i64) {
        self.deferred_links
            .retain(|(ready, _)| ready.entry_id != entry_id || ready.link.case_id != *case_id);
    }

    fn deferred_links_for_guild(&mut self, guild: Option<u64>) -> Vec<PendingAuditReady> {
        let now = Instant::now();
        self.prune(now);
        self.deferred_links
            .iter()
            .filter(|(ready, _)| guild.is_none_or(|g| ready.link.guild == g))
            .map(|(ready, _)| ready.clone())
            .collect()
    }

    fn cap<T>(entries: &mut HashMap<u128, (T, Instant)>) {
        if entries.len() >= MAX_GATEWAY_CASE_SUPPRESSIONS
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, (_, expires))| *expires)
                .map(|(key, _)| *key)
        {
            entries.remove(&oldest);
        }
    }

    fn find_matching<T: Clone>(
        entries: &HashMap<u128, (T, Instant)>,
        matches: impl Fn(&T) -> bool,
    ) -> Option<(u128, T)> {
        let token = entries
            .iter()
            .filter(|(_, (value, _))| matches(value))
            .min_by_key(|(_, (_, expires))| *expires)
            .map(|(token, _)| *token)?;
        entries.get(&token).map(|(value, _)| (token, value.clone()))
    }

    fn finalize_link(&mut self, audit_token: Option<u128>, application_token: Option<u128>) {
        self.prune(Instant::now());
        if let Some(token) = audit_token {
            self.audits.remove(&token);
        }
        if let Some(token) = application_token {
            self.applications.remove(&token);
        }
    }

    fn remember_application(&mut self, link: PendingAuditLink) -> Option<PendingAuditReady> {
        let now = Instant::now();
        self.prune(now);
        if let Some((audit_token, audit)) = Self::find_matching(&self.audits, |audit| {
            audit.action == link.action
                && audit.guild == link.guild
                && audit.subject == link.subject
        }) {
            self.audits.remove(&audit_token);
            return Some(PendingAuditReady {
                link,
                entry_id: audit.entry_id,
                audit_token: Some(audit_token),
                application_token: None,
            });
        }
        Self::cap(&mut self.applications);
        self.next_token += 1;
        self.applications
            .insert(self.next_token, (link, now + GATEWAY_CASE_SUPPRESSION_TTL));
        None
    }

    fn remember_audit(
        &mut self,
        action: crate::db::guild_data::CaseAction,
        guild: u64,
        subject: u64,
        entry_id: i64,
    ) -> Option<PendingAuditReady> {
        let now = Instant::now();
        self.prune(now);
        if let Some((application_token, link)) = Self::find_matching(&self.applications, |link| {
            link.action == action && link.guild == guild && link.subject == subject
        }) {
            self.applications.remove(&application_token);
            return Some(PendingAuditReady {
                link,
                entry_id,
                audit_token: None,
                application_token: Some(application_token),
            });
        }
        Self::cap(&mut self.audits);
        self.next_token += 1;
        self.audits.insert(
            self.next_token,
            (
                PendingDiscordAudit {
                    action,
                    guild,
                    subject,
                    entry_id,
                },
                now + GATEWAY_CASE_SUPPRESSION_TTL,
            ),
        );
        None
    }
}

/// Plan for one poll tick of slash re-sync work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashResyncPlan {
    /// Serenity guild cache is still empty (pre-READY); do not invalidate.
    Defer,
    /// Attempt `set_commands` for these guild ids. `invalidate` is true on the
    /// first non-empty attempt for this target (skip if `config_version` already
    /// cleared the cache this tick).
    Sync { guilds: Vec<u64>, invalidate: bool },
}

type ControlActorLookup = dyn Fn(i64, &db::ControlOp) -> Option<db::CachedGuildActor> + Send + Sync;

pub(crate) enum ControlMemberFetch {
    Found(Box<serenity::model::guild::Member>),
    Absent,
    Unavailable,
}

type MemberLookup =
    dyn Fn(u64, u64) -> Pin<Box<dyn Future<Output = ControlMemberFetch> + Send>> + Send + Sync;

enum GuildActorCache {
    Known(db::CachedGuildActor),
    MemberMissing,
    GuildMissing,
}

/// Discord runtime state has no dashboard OAuth client.
/// ```compile_fail
/// fn dashboard(state: &jmodguard_bot::state::BotState) {
///     let _ = &state.oauth;
/// }
/// ```
pub struct BotState {
    pub db: Db,
    /// Pure reads. Rule writes stay on [`Self::db`].
    pub reads: Db,
    pub activation: Arc<crate::activation::Activation>,
    pub tenant: crate::tenant::TenantRuntime,
    pub cfg: Arc<BotProcessConfig>,
    pub configs: ConfigCache,
    pub automod: AutomodEngine,
    pub image_spam: crate::image_spam::Detector,
    pub attachment_burst: crate::attachment_burst::Tracker,
    pub text_spam: crate::text_spam::Detector,
    pub spam_gate: crate::spam_actions::Gate,
    pub log_snippets: crate::logging::SnippetCache,
    pub snapshots: crate::snapshot_capture::SnapshotIngress,
    pub edit_reports: crate::logging::EditReportDedup,
    pub member_joins: crate::logging::MemberJoinContext,
    pub rejoin_note_deliveries: crate::logging::RejoinNoteDeliveries,
    pub member_audits: crate::logging::MemberAuditClaims,
    pub cooldowns: DashMap<(u64, u64, String), Instant>,
    pub log_delivery_slots: std::sync::Arc<tokio::sync::Semaphore>,
    pub log_delivery_failures: AtomicU64,
    pub start: Instant,
    pub slash_synced: AtomicBool,
    startup_notice_task: tokio::sync::Mutex<StartupNoticeTask>,
    /// Last observed durable `config_version` from the DB.
    /// Primed at construction from the live DB so the first poll has no
    /// "unobserved" window where a warm cache can miss a cross-process bump.
    pub last_config_version: AtomicU64,
    last_config_revision: parking_lot::Mutex<String>,
    /// Last observed durable `slash_registry_version` (commands table changes).
    /// Bot re-syncs Discord slash commands only when this advances — not on
    /// every generic `config_version` bump (rules/templates/settings/…).
    pub last_slash_registry_version: AtomicU64,
    last_slash_registry_revision: parking_lot::Mutex<String>,
    force_slash_registry_retry: AtomicBool,
    /// Serializes replica observation plus any primary confirmation. Config
    /// and slash pollers run independently, but an older primary snapshot must
    /// never reconcile after and overwrite a newer one.
    version_reconcile_lock: tokio::sync::Mutex<()>,
    /// Per-target retry state for slash re-sync (see [`SlashSyncProgress`]).
    slash_sync_progress: Mutex<Option<SlashSyncProgress>>,
    slash_unauthorized: AtomicBool,
    slash_retries: Mutex<HashMap<u64, SlashRetry>>,
    /// Last successful Discord payload per guild. Hold this lock across the
    /// versioned read and PUT so startup, join and poll cannot register out of order.
    pub(crate) slash_registrations: tokio::sync::Mutex<HashMap<u64, serde_json::Value>>,
    gateway_case_suppressions: Mutex<GatewayCaseSuppressions>,
    pub(crate) automod_delete_marks: crate::automod_delete_marks::AutomodDeleteMarkAccess,
    gateway_manual_ban_audit_entries: Mutex<GatewayManualBanAuditQueue>,
    gateway_ban_flights: [Arc<Mutex<GatewayBanFlight>>; 256],
    pending_audit_links: Mutex<PendingAuditLinks>,
    pending_member_gaps: Mutex<PendingMemberGaps>,
    /// Increments on each Gateway READY. Resume does not change it.
    pub(crate) gateway_ready_generation: AtomicU64,
    /// Gateway membership/ready and config-version changes. The member-sync
    /// loop drops its enrollment, placement, and fence cache when this moves.
    pub(crate) member_scope_epoch: AtomicU64,
    /// Wakes the member-sync loop before its saved attempt, without dropping
    /// that cache. Dashboard sync controls use this.
    pub(crate) member_sync_wake: tokio::sync::Notify,
    pub(crate) member_sync_rate_limit_until_ms: AtomicI64,
    pub(crate) member_sync_rate_limit_epoch: AtomicU64,
    pub(crate) member_sync_rate_limit_persisted_epoch: AtomicU64,
    member_sync_rate_limit_persist_notify: tokio::sync::Notify,
    pub(crate) member_sync_rate_limit_persist_running: AtomicBool,
    pub(crate) member_sync_bot_id: AtomicI64,
    pub(crate) rate_limit_wait_logged_at: parking_lot::Mutex<HashMap<String, Instant>>,
    ban_observations: Mutex<HashMap<(u64, u64), usize>>,
    /// Shards that have already delivered READY in this process.
    ready_shards: Mutex<HashSet<u32>>,
    /// Test seam. Production reads [`Self::discord_cache`] instead.
    control_actor_lookup: Mutex<Option<Arc<ControlActorLookup>>>,
    member_lookup: Mutex<Option<Arc<MemberLookup>>>,
    discord_cache: Mutex<Option<Arc<serenity::cache::Cache>>>,
    pub(crate) remembered_discord_targets: crate::discord::RememberedDiscordTargets,
    #[cfg(test)]
    gateway_ban_path_phases: Mutex<Vec<&'static str>>,
    #[cfg(test)]
    member_sync_rate_limit_persist_exit_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl BotState {
    pub fn new(cfg: BotProcessConfig, db: Db) -> Result<Arc<Self>> {
        anyhow::ensure!(
            cfg.log.discord_concurrency > 0
                && cfg.log.discord_concurrency <= tokio::sync::Semaphore::MAX_PERMITS,
            "log.discord_concurrency must be positive and within the runtime semaphore limit"
        );
        anyhow::ensure!(
            cfg.log.discord_timeout_secs > 0
                && Instant::now()
                    .checked_add(Duration::from_secs(cfg.log.discord_timeout_secs))
                    .is_some(),
            "log.discord_timeout_secs must be positive"
        );
        let image_spam = crate::image_spam::Detector::new(cfg.image_spam.clone())?;
        let attachment_burst = crate::attachment_burst::Tracker::new(cfg.attachment_burst.clone());
        let tenant =
            crate::tenant::TenantRuntime::open(&db, &cfg.db, &cfg.guild_stores, &cfg.evidence)?;
        let log_delivery_slots =
            std::sync::Arc::new(tokio::sync::Semaphore::new(cfg.log.discord_concurrency));
        // Prime from DB at construction — never leave an "unobserved" sentinel.
        // Cache population is independent of the poll task; skipping the first
        // poll invalidate (old u64::MAX heuristic) let a warm cache miss writes
        // that landed before the first successful poll.
        let primed = db::get_primary_versions_sync(&db)
            .context("failed to read primary version baselines while building BotState")?;
        db.require_config_version(primed.config);
        let mut read_opts = cfg.db_open_options();
        if !db.is_remote() {
            read_opts.path = db.path().to_path_buf();
            read_opts.url.clear();
            read_opts.token.clear();
        }
        read_opts.read_only = true;
        read_opts.migrate = false;
        let reads = Db::open_with(read_opts).context("failed to open the read-only database")?;
        Ok(Arc::new(Self {
            db,
            reads,
            activation: crate::activation::Activation::new(),
            tenant,
            cfg: Arc::new(cfg),
            configs: ConfigCache::new(primed.config, primed.config_revision.clone()),
            automod: AutomodEngine::new(),
            image_spam,
            attachment_burst,
            text_spam: crate::text_spam::Detector::default(),
            spam_gate: crate::spam_actions::Gate::default(),
            log_snippets: crate::logging::SnippetCache::default(),
            snapshots: crate::snapshot_capture::SnapshotIngress::default(),
            edit_reports: crate::logging::EditReportDedup::default(),
            member_joins: crate::logging::MemberJoinContext::default(),
            rejoin_note_deliveries: crate::logging::RejoinNoteDeliveries::default(),
            member_audits: crate::logging::MemberAuditClaims::default(),
            cooldowns: DashMap::new(),
            log_delivery_slots,
            log_delivery_failures: AtomicU64::new(0),
            start: Instant::now(),
            slash_synced: AtomicBool::new(false),
            startup_notice_task: tokio::sync::Mutex::new(StartupNoticeTask::default()),
            last_config_version: AtomicU64::new(primed.config),
            last_config_revision: parking_lot::Mutex::new(primed.config_revision),
            last_slash_registry_version: AtomicU64::new(primed.slash),
            last_slash_registry_revision: parking_lot::Mutex::new(primed.slash_revision),
            force_slash_registry_retry: AtomicBool::new(false),
            version_reconcile_lock: tokio::sync::Mutex::new(()),
            slash_sync_progress: Mutex::new(None),
            slash_unauthorized: AtomicBool::new(false),
            slash_retries: Mutex::new(HashMap::new()),
            slash_registrations: tokio::sync::Mutex::new(HashMap::new()),
            gateway_case_suppressions: Mutex::new(GatewayCaseSuppressions::default()),
            automod_delete_marks: crate::automod_delete_marks::AutomodDeleteMarkAccess {
                inner: Mutex::new(crate::automod_delete_marks::AutomodDeleteMarks::default()),
            },
            gateway_manual_ban_audit_entries: Mutex::new(GatewayManualBanAuditQueue::default()),
            gateway_ban_flights: {
                // A restart lost earlier in-flight observations. New rows must
                // not claim audit actions from before this process started.
                let started_at_ms = chrono::Utc::now().timestamp_millis();
                std::array::from_fn(|_| {
                    Arc::new(Mutex::new(GatewayBanFlight {
                        started_at_ms,
                        subjects: HashMap::new(),
                    }))
                })
            },
            pending_audit_links: Mutex::new(PendingAuditLinks::default()),
            pending_member_gaps: Mutex::new(PendingMemberGaps::default()),
            gateway_ready_generation: AtomicU64::new(0),
            member_scope_epoch: AtomicU64::new(0),
            member_sync_wake: tokio::sync::Notify::new(),
            member_sync_rate_limit_until_ms: AtomicI64::new(0),
            member_sync_rate_limit_epoch: AtomicU64::new(0),
            member_sync_rate_limit_persisted_epoch: AtomicU64::new(0),
            member_sync_rate_limit_persist_notify: tokio::sync::Notify::new(),
            member_sync_rate_limit_persist_running: AtomicBool::new(false),
            member_sync_bot_id: AtomicI64::new(0),
            rate_limit_wait_logged_at: parking_lot::Mutex::new(HashMap::new()),
            ban_observations: Mutex::new(HashMap::new()),
            ready_shards: Mutex::new(HashSet::new()),
            control_actor_lookup: Mutex::new(None),
            member_lookup: Mutex::new(None),
            discord_cache: Mutex::new(None),
            remembered_discord_targets: crate::discord::RememberedDiscordTargets::default(),
            #[cfg(test)]
            gateway_ban_path_phases: Mutex::new(Vec::new()),
            #[cfg(test)]
            member_sync_rate_limit_persist_exit_hook: Mutex::new(None),
        }))
    }

    #[cfg(test)]
    pub(crate) fn set_member_sync_rate_limit_persist_exit_hook(
        &self,
        hook: Option<Arc<dyn Fn() + Send + Sync>>,
    ) {
        *self.member_sync_rate_limit_persist_exit_hook.lock() = hook;
    }

    pub(crate) fn arm_member_sync_rate_limit(&self, deadline: i64) {
        let previous = self
            .member_sync_rate_limit_until_ms
            .fetch_max(deadline, Ordering::Relaxed);
        if deadline > previous {
            self.member_sync_rate_limit_epoch
                .fetch_add(1, Ordering::Release);
        }
    }

    pub(crate) async fn persist_pending_member_sync_rate_limit(&self) {
        self.persist_pending_member_sync_rate_limit_inner(true)
            .await;
    }

    pub(crate) async fn persist_pending_member_sync_rate_limit_quiet(&self) {
        self.persist_pending_member_sync_rate_limit_inner(false)
            .await;
    }

    async fn persist_pending_member_sync_rate_limit_inner(&self, warn_on_failure: bool) {
        let bot_id = self.member_sync_bot_id.load(Ordering::Relaxed);
        if bot_id <= 0 {
            return;
        }
        let epoch = self.member_sync_rate_limit_epoch.load(Ordering::Acquire);
        if epoch
            == self
                .member_sync_rate_limit_persisted_epoch
                .load(Ordering::Acquire)
        {
            return;
        }
        let stored = self.member_sync_rate_limit_until_ms.load(Ordering::Relaxed);
        if stored <= 0 {
            return;
        }
        match crate::db::extend_member_sync_request_after(&self.db, bot_id, stored).await {
            Ok(()) => {
                self.member_sync_rate_limit_persisted_epoch
                    .fetch_max(epoch, Ordering::Release);
            }
            Err(error) => {
                if warn_on_failure {
                    tracing::warn!("failed to persist Discord retry deadline: {error:#}");
                }
            }
        }
    }

    fn member_sync_rate_limit_should_keep_persisting(&self) -> bool {
        let epoch = self.member_sync_rate_limit_epoch.load(Ordering::Acquire);
        if epoch
            == self
                .member_sync_rate_limit_persisted_epoch
                .load(Ordering::Acquire)
        {
            return false;
        }
        if self.member_sync_bot_id.load(Ordering::Relaxed) <= 0 {
            return false;
        }
        let stored = self.member_sync_rate_limit_until_ms.load(Ordering::Relaxed);
        let now_ms = chrono::Utc::now().timestamp_millis();
        stored > now_ms
    }

    fn member_sync_rate_limit_persist_exit(&self) -> bool {
        if self.member_sync_rate_limit_should_keep_persisting() {
            return false;
        }
        #[cfg(test)]
        if let Some(hook) = self.member_sync_rate_limit_persist_exit_hook.lock().clone() {
            hook();
        }
        self.member_sync_rate_limit_persist_running
            .swap(false, Ordering::AcqRel);
        if self.member_sync_rate_limit_should_keep_persisting()
            && !self
                .member_sync_rate_limit_persist_running
                .swap(true, Ordering::AcqRel)
        {
            return false;
        }
        true
    }

    async fn member_sync_rate_limit_persist_loop(self: Arc<Self>) {
        let mut backoff = Duration::from_millis(25);
        const MAX_BACKOFF: Duration = Duration::from_millis(300);
        let mut warned_epoch = 0_u64;
        loop {
            let epoch = self.member_sync_rate_limit_epoch.load(Ordering::Acquire);
            if epoch
                == self
                    .member_sync_rate_limit_persisted_epoch
                    .load(Ordering::Acquire)
            {
                if self.member_sync_rate_limit_persist_exit() {
                    return;
                }
                continue;
            }
            if self.member_sync_bot_id.load(Ordering::Relaxed) <= 0 {
                if self.member_sync_rate_limit_persist_exit() {
                    return;
                }
                continue;
            }
            let warn = epoch != warned_epoch;
            if warn {
                warned_epoch = epoch;
                self.persist_pending_member_sync_rate_limit().await;
            } else {
                self.persist_pending_member_sync_rate_limit_quiet().await;
            }
            if epoch
                == self
                    .member_sync_rate_limit_persisted_epoch
                    .load(Ordering::Acquire)
            {
                backoff = Duration::from_millis(25);
                if self.member_sync_rate_limit_persist_exit() {
                    return;
                }
                continue;
            }
            let stored = self.member_sync_rate_limit_until_ms.load(Ordering::Relaxed);
            let now_ms = chrono::Utc::now().timestamp_millis();
            if stored <= now_ms {
                // A failed write with deadline <= now leaves epoch > persisted_epoch
                // until the next observe or publish_member_sync_bot_id.
                if self.member_sync_rate_limit_persist_exit() {
                    return;
                }
                continue;
            }
            backoff = backoff.min(MAX_BACKOFF);
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = self.member_sync_rate_limit_persist_notify.notified() => {}
            }
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    pub(crate) fn ensure_member_sync_rate_limit_persist_task(self: &Arc<Self>) {
        self.member_sync_rate_limit_persist_notify.notify_one();
        if !self
            .member_sync_rate_limit_persist_running
            .swap(true, Ordering::AcqRel)
        {
            let app = Arc::clone(self);
            tokio::spawn(async move {
                app.member_sync_rate_limit_persist_loop().await;
            });
        }
    }

    pub(crate) async fn publish_member_sync_bot_id(&self, bot_id: i64) {
        if bot_id <= 0 {
            return;
        }
        self.member_sync_bot_id.store(bot_id, Ordering::Relaxed);
        self.persist_pending_member_sync_rate_limit().await;
    }

    pub fn install_discord_cache(&self, cache: Arc<serenity::cache::Cache>) {
        *self.discord_cache.lock() = Some(cache);
    }

    /// Process probes and tests supply a cache view without a live Discord gateway.
    pub fn install_control_actor_lookup(
        &self,
        lookup: impl Fn(i64, &db::ControlOp) -> Option<db::CachedGuildActor> + Send + Sync + 'static,
    ) {
        *self.control_actor_lookup.lock() = Some(Arc::new(lookup));
    }

    pub(crate) fn install_member_lookup<F, Fut>(&self, lookup: F)
    where
        F: Fn(u64, u64) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ControlMemberFetch> + Send + 'static,
    {
        *self.member_lookup.lock() = Some(Arc::new(move |guild_id, user_id| {
            Box::pin(lookup(guild_id, user_id))
        }));
    }

    pub fn install_http_member_lookup(&self, http: Arc<serenity::http::Http>) {
        self.install_member_lookup(move |guild_id, user_id| {
            let http = Arc::clone(&http);
            async move {
                match http
                    .get_member(
                        serenity::model::id::GuildId::new(guild_id),
                        serenity::model::id::UserId::new(user_id),
                    )
                    .await
                {
                    Ok(member) => ControlMemberFetch::Found(Box::new(member)),
                    Err(error) if member_lookup_absent(&error) => ControlMemberFetch::Absent,
                    Err(_) => ControlMemberFetch::Unavailable,
                }
            }
        });
    }

    pub async fn authorize_control_actor(
        &self,
        op: &db::ControlOp,
        actor_user_id: i64,
    ) -> Result<(), &'static str> {
        if self.control_actor_lookup.lock().is_some() {
            let cached = self.cached_control_actor(actor_user_id, op);
            for need in db::control_actor_needs(op) {
                db::decide_control_actor(need, actor_user_id, self.cfg.bot.owner_id, cached)?;
            }
            return Ok(());
        }
        for need in db::control_actor_needs(op) {
            self.authorize_control_need(need, actor_user_id).await?;
        }
        Ok(())
    }

    async fn authorize_control_need(
        &self,
        need: db::ControlActorNeed,
        actor_user_id: i64,
    ) -> Result<(), &'static str> {
        let guild_id = match need {
            db::ControlActorNeed::Process | db::ControlActorNeed::Owner => {
                return db::decide_control_actor(need, actor_user_id, self.cfg.bot.owner_id, None);
            }
            db::ControlActorNeed::ManageGuild(id) | db::ControlActorNeed::GuildAdmin(id) => id,
        };
        match self.cached_guild_actor(guild_id, actor_user_id) {
            GuildActorCache::Known(cached) => {
                db::decide_control_actor(need, actor_user_id, self.cfg.bot.owner_id, Some(cached))
            }
            GuildActorCache::GuildMissing => Err("actor permission could not be confirmed"),
            GuildActorCache::MemberMissing => {
                self.lookup_guild_actor(need, guild_id, actor_user_id).await
            }
        }
    }

    fn cached_guild_actor(&self, guild_id: i64, actor_user_id: i64) -> GuildActorCache {
        if guild_id <= 0 || actor_user_id <= 0 {
            return GuildActorCache::MemberMissing;
        }
        let Some(cache) = self.discord_cache.lock().clone() else {
            return GuildActorCache::GuildMissing;
        };
        let Some(guild) = cache.guild(serenity::model::id::GuildId::new(guild_id as u64)) else {
            return GuildActorCache::GuildMissing;
        };
        let owner = i64::try_from(guild.owner_id.get()).unwrap_or(0);
        if owner == actor_user_id {
            return GuildActorCache::Known(db::CachedGuildActor {
                guild_owner_id: owner,
                permissions: 0,
            });
        }
        let Some(member) = guild
            .members
            .get(&serenity::model::id::UserId::new(actor_user_id as u64))
        else {
            return GuildActorCache::MemberMissing;
        };
        GuildActorCache::Known(db::CachedGuildActor {
            guild_owner_id: owner,
            permissions: permission_bits(guild.member_permissions(member)),
        })
    }

    async fn lookup_guild_actor(
        &self,
        need: db::ControlActorNeed,
        guild_id: i64,
        actor_user_id: i64,
    ) -> Result<(), &'static str> {
        let Some(lookup) = self.member_lookup.lock().clone() else {
            return Err("actor permission could not be confirmed");
        };
        if guild_id <= 0 || actor_user_id <= 0 {
            return Err("actor is not allowed");
        }
        let fetched = tokio::time::timeout(
            Duration::from_secs(2),
            lookup(guild_id as u64, actor_user_id as u64),
        )
        .await;
        match fetched {
            Ok(ControlMemberFetch::Absent) => Err("actor is not allowed"),
            Ok(ControlMemberFetch::Unavailable) | Err(_) => {
                Err("actor permission could not be confirmed")
            }
            Ok(ControlMemberFetch::Found(member)) => {
                let Some(cache) = self.discord_cache.lock().clone() else {
                    return Err("actor permission could not be confirmed");
                };
                let Some(guild) = cache.guild(serenity::model::id::GuildId::new(guild_id as u64))
                else {
                    return Err("actor permission could not be confirmed");
                };
                let owner = i64::try_from(guild.owner_id.get()).unwrap_or(0);
                db::decide_control_actor(
                    need,
                    actor_user_id,
                    self.cfg.bot.owner_id,
                    Some(db::CachedGuildActor {
                        guild_owner_id: owner,
                        permissions: permission_bits(guild.member_permissions(&member)),
                    }),
                )
            }
        }
    }

    fn cached_control_actor(
        &self,
        actor_user_id: i64,
        op: &db::ControlOp,
    ) -> Option<db::CachedGuildActor> {
        if let Some(lookup) = self.control_actor_lookup.lock().clone() {
            return lookup(actor_user_id, op);
        }
        let cache = self.discord_cache.lock().clone()?;
        cached_actor_from_discord(&cache, actor_user_id, op)
    }
}

fn cached_actor_from_discord(
    cache: &serenity::cache::Cache,
    actor_user_id: i64,
    op: &db::ControlOp,
) -> Option<db::CachedGuildActor> {
    let guild_id = match db::control_actor_need(op) {
        db::ControlActorNeed::ManageGuild(id) | db::ControlActorNeed::GuildAdmin(id) if id > 0 => {
            id as u64
        }
        _ => return None,
    };
    if actor_user_id <= 0 {
        return None;
    }
    let guild = cache.guild(serenity::model::id::GuildId::new(guild_id))?;
    let owner = i64::try_from(guild.owner_id.get()).unwrap_or(0);
    if owner == actor_user_id {
        return Some(db::CachedGuildActor {
            guild_owner_id: owner,
            permissions: 0,
        });
    }
    let member = guild
        .members
        .get(&serenity::model::id::UserId::new(actor_user_id as u64))?;
    let permissions = guild.member_permissions(member);
    let mut bits = 0u64;
    if permissions.administrator() {
        bits |= 1 << 3;
    }
    if permissions.manage_guild() {
        bits |= 1 << 5;
    }
    Some(db::CachedGuildActor {
        guild_owner_id: owner,
        permissions: bits,
    })
}

fn permission_bits(permissions: serenity::model::permissions::Permissions) -> u64 {
    let mut bits = 0u64;
    if permissions.administrator() {
        bits |= 1 << 3;
    }
    if permissions.manage_guild() {
        bits |= 1 << 5;
    }
    bits
}

fn member_lookup_absent(error: &serenity::Error) -> bool {
    let serenity::Error::Http(http) = error else {
        return false;
    };
    if http.status_code().is_some_and(|code| code.as_u16() == 404) {
        return true;
    }
    match http {
        serenity::http::HttpError::UnsuccessfulRequest(response) => response.error.code == 10007,
        _ => false,
    }
}

impl BotState {
    /// Register Ready work only while this process can still send start notices.
    pub async fn spawn_startup_notice_task(&self, task: impl Future<Output = ()> + Send + 'static) {
        let mut startup = self.startup_notice_task.lock().await;
        if !startup.stopping {
            startup.task = Some(tokio::spawn(task));
        }
    }

    /// Stop any in-flight Ready work before the same process sends stop notices.
    pub async fn stop_startup_notice_task(&self) {
        let task = {
            let mut startup = self.startup_notice_task.lock().await;
            startup.stopping = true;
            startup.task.take()
        };
        if let Some(task) = task {
            task.abort();
            if let Err(error) = task.await
                && !error.is_cancelled()
            {
                tracing::warn!("startup notice task failed: {error}");
            }
        }
    }

    pub(crate) fn suppress_next_gateway_case(
        &self,
        action: GatewayCaseAction,
        guild: u64,
        user: u64,
        operation_id: Option<&str>,
    ) -> GatewayCaseSuppression {
        self.gateway_case_suppressions.lock().insert(
            action,
            guild,
            user,
            operation_id.filter(|id| !id.is_empty()).map(str::to_owned),
        )
    }

    pub(crate) fn release_gateway_case_suppression(&self, token: GatewayCaseSuppression) {
        self.gateway_case_suppressions.lock().release(token);
    }

    pub(crate) fn consume_gateway_case_suppression(
        &self,
        action: GatewayCaseAction,
        guild: u64,
        user: u64,
    ) -> bool {
        self.gateway_case_suppressions
            .lock()
            .remove(action, guild, user)
    }

    pub(crate) fn consume_gateway_ban_echo(&self, guild: u64, user: u64) -> GatewayBanEchoCapture {
        let mut suppressions = self.gateway_case_suppressions.lock();
        let suppress_case = suppressions.remove(GatewayCaseAction::Ban, guild, user);
        let remembered = suppressions.recent_ban_echo.get(&(guild, user));
        GatewayBanEchoCapture {
            echo: GatewayBanEcho {
                suppress_case,
                owned_by_bot: suppress_case || remembered.is_some(),
                operation_id: remembered.and_then(|entry| entry.operation_id.clone()),
            },
            released: remembered.map(|entry| Arc::clone(&entry.released)),
        }
    }

    /// Operation id saved with a consumed bot ban echo, when one is still remembered.
    #[cfg(test)]
    pub(crate) fn gateway_ban_echo_operation(
        &self,
        guild: u64,
        user: u64,
    ) -> Option<Option<String>> {
        self.gateway_case_suppressions
            .lock()
            .ban_echo_operation(guild, user)
    }

    /// An unban ends the bot echo. A later ban is a new row.
    pub(crate) fn clear_gateway_ban_echo(&self, guild: u64, user: u64) {
        self.gateway_case_suppressions
            .lock()
            .clear_ban_echo(guild, user);
    }

    pub(crate) fn remember_member_gap(
        &self,
        guild_id: u64,
        enrollment_id: Option<String>,
        user_id: Option<i64>,
        reason: &str,
    ) {
        self.pending_member_gaps
            .lock()
            .remember(guild_id, enrollment_id, user_id, reason);
    }

    pub(crate) fn pending_member_gaps(&self, guild_id: u64) -> Vec<PendingMemberGap> {
        self.pending_member_gaps.lock().snapshot(guild_id)
    }

    pub(crate) fn acknowledge_member_gap(&self, gap: &PendingMemberGap) {
        self.pending_member_gaps.lock().acknowledge(gap);
    }

    pub(crate) async fn begin_gateway_ban_observation(
        self: &Arc<Self>,
        guild: u64,
        user: u64,
    ) -> GatewayBanObservation {
        let flight = Arc::clone(self.gateway_ban_flight(guild, user));
        let delivery_id = db::guild_data::CaseId::new();
        let observed;
        {
            // Timestamp and registration are one short critical section. SQL
            // never holds this lock, and no eligible arrival can remain hidden
            // while reconciliation decides that its match window has closed.
            let mut state = flight.lock();
            observed = chrono::Utc::now();
            let subject = state
                .subjects
                .entry((guild, user))
                .or_insert(GatewayBanSubject {
                    active: 0,
                    active_ids: Vec::new(),
                });
            subject.active += 1;
            subject.active_ids.push(db::GatewayBanPeer {
                id: delivery_id.as_str().to_owned(),
                observed_at_ms: observed.timestamp_millis(),
            });
        }
        GatewayBanObservation {
            delivery_id,
            flight,
            guild,
            user,
            observed,
            released: false,
            app: Arc::downgrade(self),
            runtime: tokio::runtime::Handle::current(),
            enrollment: None,
        }
    }

    fn gateway_ban_flight(&self, guild: u64, user: u64) -> &Arc<Mutex<GatewayBanFlight>> {
        &self.gateway_ban_flights
            [gateway_ban_flight_index(guild, user, self.gateway_ban_flights.len())]
    }

    pub(crate) async fn reconcile_gateway_manual_ban_audit_entry(
        &self,
        enrollment: Option<db::GuildEnrollment>,
        guild: u64,
        user: u64,
        entry_id: i64,
    ) -> Result<bool> {
        let flight = Arc::clone(self.gateway_ban_flight(guild, user));
        let enrollment = enrollment.ok_or(db::EnrollmentChanged)?;
        let Some((attribution, valid, expires, retrying)) = self
            .gateway_manual_ban_audit_entries
            .lock()
            .entries
            .get(&(guild, user))
            .and_then(|entries| entries.iter().find(|entry| entry.entry_id == entry_id))
            .map(|entry| {
                (
                    entry.attribution.clone(),
                    Arc::clone(&entry.valid),
                    entry.expires,
                    Arc::clone(&entry.retrying),
                )
            })
        else {
            return Ok(false);
        };
        let audit_at = (entry_id >> 22) + 1_420_070_400_000;
        let mut retry = None;
        let mut backoff = Duration::from_secs(1);
        loop {
            if !valid.load(Ordering::Acquire) || expires <= Instant::now() {
                return Ok(false);
            }
            let blocked = gateway_ban_audit_blocked(&flight.lock(), guild, user);
            // The callback's pre-write pass cannot wait for its own observation.
            // An established error-retry owner stays alive through temporary
            // activity, so another completion cannot lose its only retry.
            if blocked && retry.is_none() {
                return Ok(false);
            }
            let remaining_ms = audit_at + 5_001 - chrono::Utc::now().timestamp_millis();
            if remaining_ms > 0 {
                tokio::time::sleep(
                    Duration::from_millis(remaining_ms as u64)
                        .min(expires.saturating_duration_since(Instant::now())),
                )
                .await;
                continue;
            }
            if !blocked {
                let result = self
                    .db
                    .with_conn({
                        let flight = Arc::clone(&flight);
                        let valid = Arc::clone(&valid);
                        let enrollment = enrollment.clone();
                        let attribution = attribution.clone();
                        move |conn| {
                            {
                                let state = flight.lock();
                                if !valid.load(Ordering::Acquire)
                                    || expires <= Instant::now()
                                    || gateway_ban_audit_blocked(&state, guild, user)
                                    || chrono::Utc::now().timestamp_millis() <= audit_at + 5_000
                                {
                                    return Ok(false);
                                }
                            }
                            // The inclusive match window is closed. SQL serializes
                            // concurrent passes without holding the registration lock.
                            db::reconcile_gateway_manual_ban_audit_entry_conn(
                                conn,
                                &enrollment,
                                guild as i64,
                                user as i64,
                                entry_id,
                                attribution.as_ref(),
                            )
                        }
                    })
                    .await;
                match result {
                    Ok(true) => return Ok(true),
                    Ok(false) if retry.is_none() => return Ok(false),
                    Ok(false) => {}
                    Err(error) => {
                        let transient = matches!(
                            db::startup_retry_kind(&error, self.db.is_remote()),
                            Some(
                                db::StartupRetryKind::SqliteLock
                                    | db::StartupRetryKind::InteractiveTransaction
                                    | db::StartupRetryKind::Transport
                            )
                        ) || db::is_database_operation_timeout(&error)
                            || db::is_database_commit_indeterminate(&error);
                        if !transient {
                            return Err(error);
                        }
                        if retry.is_none() {
                            if retrying
                                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                                .is_err()
                            {
                                return Err(error);
                            }
                            retry = Some(GatewayBanAuditRetry(Arc::clone(&retrying)));
                        }
                        tracing::warn!("gateway manual ban audit will retry: {error:#}");
                    }
                }
            }
            tokio::time::sleep(backoff.min(expires.saturating_duration_since(Instant::now())))
                .await;
            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
    }

    /// Retain audit identities until their own persisted ban can be reconciled.
    #[cfg(test)]
    pub(crate) fn queue_gateway_manual_ban_audit_entry(
        &self,
        guild_id: u64,
        user_id: u64,
        entry_id: i64,
    ) {
        self.gateway_manual_ban_audit_entries
            .lock()
            .queue(guild_id, user_id, entry_id);
    }

    pub(crate) fn queue_attributed_gateway_manual_ban_audit_entry(
        &self,
        guild: u64,
        user: u64,
        entry_id: i64,
        attribution: db::GatewayBanAuditAttribution,
    ) {
        self.gateway_manual_ban_audit_entries
            .lock()
            .queue_attributed(guild, user, entry_id, Some(attribution));
    }

    pub(crate) fn pending_gateway_manual_ban_audit_entries(
        &self,
        guild_id: u64,
        user_id: u64,
    ) -> Vec<i64> {
        self.gateway_manual_ban_audit_entries
            .lock()
            .pending(guild_id, user_id)
    }

    #[cfg(test)]
    pub(crate) fn clear_gateway_manual_ban_audit_entries(&self, guild_id: u64, user_id: u64) {
        self.gateway_manual_ban_audit_entries
            .lock()
            .clear(guild_id, user_id);
    }

    pub(crate) fn drop_gateway_manual_ban_audit_entry(
        &self,
        guild_id: u64,
        user_id: u64,
        entry_id: i64,
    ) {
        self.gateway_manual_ban_audit_entries
            .lock()
            .drop_entry(guild_id, user_id, entry_id);
    }

    #[cfg(test)]
    pub(crate) fn reset_gateway_ban_path_test_phases(&self) {
        self.gateway_ban_path_phases.lock().clear();
    }

    #[cfg(test)]
    pub(crate) fn record_gateway_ban_path_test_phase(&self, phase: &'static str) {
        self.gateway_ban_path_phases.lock().push(phase);
    }

    #[cfg(test)]
    pub(crate) fn gateway_ban_path_test_phases(&self) -> Vec<&'static str> {
        self.gateway_ban_path_phases.lock().clone()
    }

    pub(crate) fn begin_ban_observation(&self, guild_id: u64, user_id: u64) -> bool {
        let mut observations = self.ban_observations.lock();
        let owners = observations.entry((guild_id, user_id)).or_default();
        let contested = *owners > 0;
        *owners = owners
            .checked_add(1)
            .expect("ban observation owners overflow");
        contested
    }

    /// Reserves a command-ban serialization slot without incrementing when one
    /// is already held (rejected overlapping bans must not leak a marker).
    pub(crate) fn try_begin_command_ban_observation(&self, guild_id: u64, user_id: u64) -> bool {
        let mut observations = self.ban_observations.lock();
        let owners = observations.entry((guild_id, user_id)).or_default();
        if *owners > 0 {
            return false;
        }
        *owners = owners
            .checked_add(1)
            .expect("ban observation owners overflow");
        true
    }

    pub(crate) fn end_ban_observation(&self, guild_id: u64, user_id: u64) {
        let mut observations = self.ban_observations.lock();
        if let std::collections::hash_map::Entry::Occupied(mut entry) =
            observations.entry((guild_id, user_id))
        {
            if *entry.get() == 1 {
                entry.remove();
            } else {
                *entry.get_mut() -= 1;
            }
        }
    }

    /// A shard's first READY is startup. A later READY from that same shard is
    /// a replaced session and advances the coverage-gap generation.
    pub(crate) fn note_shard_ready(&self, shard_id: u32) {
        if !self.ready_shards.lock().insert(shard_id) {
            self.gateway_ready_generation
                .fetch_add(1, Ordering::Relaxed);
            self.invalidate_member_sync_scope();
        }
    }

    /// Drop cached member-sync enrollment, placement, and fence state and wake
    /// the idle loop. Gateway membership and config-version changes call this.
    pub(crate) fn invalidate_member_sync_scope(&self) {
        self.member_scope_epoch.fetch_add(1, Ordering::SeqCst);
        self.member_sync_wake.notify_one();
    }

    /// Wake the idle loop so it can see a newly saved attempt. Cached scope stays.
    pub(crate) fn wake_member_sync(&self) {
        self.member_sync_wake.notify_one();
    }

    pub(crate) fn remember_application_audit(
        &self,
        link: PendingAuditLink,
    ) -> Option<PendingAuditReady> {
        self.pending_audit_links.lock().remember_application(link)
    }

    pub(crate) fn remember_discord_audit(
        &self,
        action: crate::db::guild_data::CaseAction,
        guild: u64,
        subject: u64,
        entry_id: i64,
    ) -> Option<PendingAuditReady> {
        self.pending_audit_links
            .lock()
            .remember_audit(action, guild, subject, entry_id)
    }

    pub(crate) fn finalize_pending_audit_link(
        &self,
        audit_token: Option<u128>,
        application_token: Option<u128>,
    ) {
        self.pending_audit_links
            .lock()
            .finalize_link(audit_token, application_token);
    }

    pub(crate) fn defer_pending_audit_link(&self, ready: PendingAuditReady) {
        self.pending_audit_links.lock().defer_link(ready);
    }

    pub(crate) fn clear_deferred_audit_link(
        &self,
        case_id: &crate::db::guild_data::CaseId,
        entry_id: i64,
    ) {
        self.pending_audit_links
            .lock()
            .clear_deferred_link(case_id, entry_id);
    }

    pub(crate) fn deferred_audit_links_for_guild(
        &self,
        guild: Option<u64>,
    ) -> Vec<PendingAuditReady> {
        self.pending_audit_links
            .lock()
            .deferred_links_for_guild(guild)
    }

    /// Invalidates the config cache (call after any config write).
    pub fn invalidate_configs(&self) {
        self.remembered_discord_targets.clear_all();
        self.configs.invalidate();
    }

    /// Loads durable `config_version` from the DB. When it differs from the
    /// last observation (primed at startup), invalidates the guild config cache.
    /// Returns `true` when a change triggered invalidation.
    pub async fn poll_config_version(&self) -> Result<bool> {
        let _version_reconcile = self.version_reconcile_lock.lock().await;
        let remote = db::get_version_state(&self.db).await?;
        let prev = self.last_config_version.load(Ordering::SeqCst);
        let revision_changed = remote.config_revision != *self.last_config_revision.lock();
        if revision_changed
            || remote.config != prev
            || remote.config < self.db.required_config_version()
        {
            let suspected_floor = prev.max(self.db.required_config_version());
            let write_generation = self.db.config_write_generation();
            let primary = db::get_primary_versions(&self.db).await?;
            return Ok(self
                .reconcile_primary_versions(primary, suspected_floor, write_generation)
                .0);
        }
        Ok(false)
    }

    /// Loads durable `slash_registry_version`. Returns the remote version when
    /// it differs from the last **committed** observation (does not advance).
    ///
    /// Callers must [`commit_slash_registry_version`] only after Discord slash
    /// re-sync succeeds. Advancing before sync left failed guilds stale until
    /// the next command-registry write.
    pub async fn pending_slash_registry_version(&self) -> Result<Option<u64>> {
        if self.slash_unauthorized.load(Ordering::SeqCst) {
            return Ok(None);
        }
        let _version_reconcile = self.version_reconcile_lock.lock().await;
        let remote = db::get_version_state(&self.db).await?;
        let prev = self.last_slash_registry_version.load(Ordering::SeqCst);
        let config_revision_changed = remote.config_revision != *self.last_config_revision.lock();
        let slash_revision_changed =
            remote.slash_revision != *self.last_slash_registry_revision.lock();
        if config_revision_changed
            || slash_revision_changed
            || remote.config < self.db.required_config_version()
            || remote.slash != prev
        {
            let suspected_floor = self
                .last_config_version
                .load(Ordering::SeqCst)
                .max(self.db.required_config_version());
            let write_generation = self.db.config_write_generation();
            let primary = db::get_primary_versions(&self.db).await?;
            let (_, slash_changed) =
                self.reconcile_primary_versions(primary, suspected_floor, write_generation);
            let current = self.last_slash_registry_version.load(Ordering::SeqCst);
            Ok(
                (slash_changed || self.force_slash_registry_retry.load(Ordering::SeqCst))
                    .then_some(current),
            )
        } else if self.force_slash_registry_retry.load(Ordering::SeqCst) {
            Ok(Some(prev))
        } else {
            Ok(None)
        }
    }

    fn reconcile_primary_versions(
        &self,
        primary: db::VersionState,
        suspected_floor: u64,
        write_generation: u64,
    ) -> (bool, bool) {
        if !self.db.rebaseline_required_config_version(
            suspected_floor,
            primary.config,
            write_generation,
        ) {
            tracing::debug!(
                "discarding primary version snapshot because a local config write committed concurrently"
            );
            return (false, false);
        }
        let previous_config = self.last_config_version.load(Ordering::SeqCst);
        if primary.config < suspected_floor {
            tracing::warn!(
                previous_config,
                required_config = suspected_floor,
                primary_config = primary.config,
                "primary database confirmed config version below the local floor; rebaselining"
            );
            self.configs
                .rebaseline_version(suspected_floor, primary.config);
        }
        let config_revision_changed = {
            let mut revision = self.last_config_revision.lock();
            if *revision != primary.config_revision {
                *revision = primary.config_revision.clone();
                true
            } else {
                false
            }
        };
        let config_changed = config_revision_changed || primary.config != previous_config;
        if config_changed {
            self.remembered_discord_targets.clear_all();
            self.slash_retries.lock().clear();
            self.last_config_version
                .store(primary.config, Ordering::SeqCst);
            self.invalidate_member_sync_scope();
            if config_revision_changed {
                // Snapshot acceptance carries the revision identity, so a
                // higher pre-failover replica counter cannot bypass rollback
                // recovery and later invalidations cannot forget lineage.
                self.configs
                    .require_primary_revision(primary.config, primary.config_revision.clone());
            } else if primary.config >= previous_config {
                self.configs.require_version(primary.config);
            } else {
                self.configs.invalidate();
            }
        }

        let previous_slash = self.last_slash_registry_version.load(Ordering::SeqCst);
        let slash_revision_changed = {
            let mut revision = self.last_slash_registry_revision.lock();
            if *revision != primary.slash_revision {
                *revision = primary.slash_revision;
                true
            } else {
                false
            }
        };
        let slash_changed = slash_revision_changed || primary.slash != previous_slash;
        if slash_changed {
            self.last_slash_registry_version
                .store(primary.slash, Ordering::SeqCst);
            if primary.slash < previous_slash {
                tracing::warn!(
                    previous_slash,
                    primary_slash = primary.slash,
                    "primary database confirmed slash-registry rollback; forcing Discord reconciliation"
                );
            }
            self.force_slash_registry_retry
                .store(true, Ordering::SeqCst);
            *self.slash_sync_progress.lock() = None;
        }
        (config_changed, slash_changed)
    }

    /// Marks `version` as observed after a successful slash re-sync pass.
    pub fn commit_slash_registry_version(&self, version: u64) {
        self.last_slash_registry_version
            .store(version, Ordering::SeqCst);
        self.force_slash_registry_retry
            .store(false, Ordering::SeqCst);
        // Drop any stale retry set for an older target.
        let mut progress = self.slash_sync_progress.lock();
        if progress.as_ref().is_some_and(|p| p.target == version) {
            *progress = None;
        }
    }

    /// Make the next poll re-sync the current durable command registry. Used
    /// after a transient startup/guild-join sync failure, when no write may
    /// otherwise advance the durable version to trigger another attempt.
    pub fn retry_slash_registry_version(&self) {
        let mut progress = self.slash_sync_progress.lock();
        self.force_slash_registry_retry
            .store(true, Ordering::SeqCst);
        *progress = None;
    }

    pub(crate) fn slash_sync_due(
        &self,
        guild: u64,
        target: u64,
        revision: &str,
        now: Instant,
    ) -> bool {
        if self.slash_unauthorized.load(Ordering::SeqCst) {
            return false;
        }
        let mut retries = self.slash_retries.lock();
        if retries
            .get(&guild)
            .is_some_and(|retry| retry.target != target || retry.revision != revision)
        {
            retries.remove(&guild);
        }
        retries.get(&guild).is_none_or(|retry| retry.retry.due(now))
    }

    pub(crate) fn slash_sync_failed(
        &self,
        guild: u64,
        target: u64,
        revision: &str,
        class: crate::discord::RetryClass,
        response: Option<(u16, isize)>,
    ) {
        use crate::discord::RetryClass;
        if class == RetryClass::Unauthorized {
            if !self.slash_unauthorized.swap(true, Ordering::SeqCst) {
                tracing::error!(
                    "slash sync stopped: Discord rejected the bot token; restart with a valid token"
                );
            }
            return;
        }
        let mut retries = self.slash_retries.lock();
        let retry = retries.entry(guild).or_insert_with(|| SlashRetry {
            target,
            revision: revision.to_owned(),
            retry: Default::default(),
        });
        if class == RetryClass::Permanent && !retry.retry.stopped {
            tracing::warn!(
                guild_id = guild,
                status = response.map(|(status, _)| status),
                discord_code = response.map(|(_, code)| code),
                "slash sync parked until registry/config changes or the bot rejoins"
            );
        }
        retry.retry.fail(class, Instant::now(), 30);
    }

    pub(crate) fn reset_slash_guild_retry(&self, guild: u64) {
        self.slash_retries.lock().remove(&guild);
    }

    pub(crate) async fn forget_slash_guild(&self, guild: u64) {
        // Wait for any in-flight PUT before forgetting its result.
        self.slash_registrations.lock().await.remove(&guild);
        self.reset_slash_guild_retry(guild);
        // Discard an in-flight pass's failure set too: a confirmed leave must
        // not leave reconciliation waiting for that guild to return to cache.
        self.retry_slash_registry_version();
    }

    pub fn acknowledged_slash_registry_version(&self) -> u64 {
        self.last_slash_registry_version.load(Ordering::SeqCst)
    }

    pub fn slash_registry_revision(&self) -> String {
        self.last_slash_registry_revision.lock().clone()
    }

    /// Decide which guilds to `set_commands` for pending registry `remote`.
    ///
    /// `cached` is the current serenity guild-id list (owned `u64`s — never hold
    /// a CacheRef across await). Empty cache → [`SlashResyncPlan::Defer`] without
    /// invalidating. A new `remote` resets progress so every guild is attempted
    /// once; later polls only retry the failure set.
    pub fn plan_slash_resync(&self, remote: u64, cached: &[u64]) -> SlashResyncPlan {
        let mut guard = self.slash_sync_progress.lock();
        if guard.as_ref().map(|p| p.target) != Some(remote) {
            *guard = Some(SlashSyncProgress {
                target: remote,
                remaining: None,
                invalidated: false,
            });
        }
        if cached.is_empty() {
            return SlashResyncPlan::Defer;
        }
        let progress = guard.as_mut().expect("progress set above");
        let guilds = match &progress.remaining {
            None => cached.to_vec(),
            Some(rem) => {
                let pending: Vec<u64> =
                    cached.iter().copied().filter(|g| rem.contains(g)).collect();
                if pending.len() != rem.len() {
                    // Do not partially consume the failure set: otherwise a
                    // successful present guild could acknowledge the version
                    // while another failed guild remains absent from cache.
                    return SlashResyncPlan::Defer;
                }
                pending
            }
        };
        let invalidate = !progress.invalidated;
        if invalidate {
            progress.invalidated = true;
        }
        SlashResyncPlan::Sync { guilds, invalidate }
    }

    /// Record guilds that failed `set_commands` for `remote`. A successful
    /// finish commits the version and clears the force flag while holding the
    /// progress lock, so a concurrent retry request cannot be erased.
    pub fn finish_slash_resync(&self, remote: u64, failed: HashSet<u64>) -> bool {
        let mut guard = self.slash_sync_progress.lock();
        let Some(progress) = guard.as_mut() else {
            // A concurrent guild-create/startup failure clears progress to
            // request a fresh attempt. Never let an older in-flight poll
            // acknowledge and erase that newer retry request.
            return false;
        };
        if progress.target != remote {
            return false;
        }
        if failed.is_empty() {
            *guard = None;
            self.last_slash_registry_version
                .store(remote, Ordering::SeqCst);
            self.force_slash_registry_retry
                .store(false, Ordering::SeqCst);
            true
        } else {
            progress.remaining = Some(failed);
            false
        }
    }
}

// ---------------------------------------------------------------------------
// per-guild config cache
// ---------------------------------------------------------------------------

pub struct CachedGuildConfig {
    pub version: u64,
    pub durable_version: u64,
    pub loaded_at: Instant,
    pub settings: GuildSettings,
    pub rules: HashMap<String, RuleDef>,
    pub commands: HashMap<String, CommandDef>,
    pub templates: HashMap<String, String>,
    pub log_channels: HashMap<String, ChannelId>,
    pub log_filters: HashMap<String, db::LogFilter>,
    pub known_bad_hashes: std::collections::BTreeSet<[u8; 32]>,
    pub regexes: Vec<Regex>,
    pub custom_patterns: HashMap<String, Regex>,
}

impl CachedGuildConfig {
    fn from_parts(version: u64, durable_version: u64, parts: db::ConfigParts) -> Self {
        let mut regexes = Vec::new();
        let mut custom_patterns = HashMap::new();
        for pattern in &parts.badwords {
            // Same compile limits as WebUI / add_badword — legacy DB rows that
            // exceed the budget are skipped so cache load cannot DoS the bot.
            match crate::normalize::compile_user_regex(pattern) {
                Ok(re) => regexes.push(re),
                Err(e) => {
                    tracing::warn!("badword pattern `{pattern}` failed to compile: {e}");
                }
            }
        }
        for (rule_id, rule) in &parts.rules {
            if !jmodguard::custom_rules::is_custom_id(rule_id) {
                continue;
            }
            if rule.params.get("kind").and_then(|v| v.as_str()) != Some("pattern") {
                continue;
            }
            let pattern = rule
                .params
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match crate::normalize::compile_user_regex(pattern) {
                Ok(re) => {
                    custom_patterns.insert(rule_id.clone(), re);
                }
                Err(e) => {
                    tracing::warn!("custom pattern `{rule_id}` failed to compile: {e}");
                }
            }
        }
        Self {
            version,
            durable_version,
            loaded_at: Instant::now(),
            known_bad_hashes: parts.known_bad_images.iter()
                .filter_map(|row| jmodguard::image_spam::parse_hash(&row.hash)).collect(),
            settings: parts.settings,
            rules: parts.rules,
            commands: parts.commands,
            templates: parts.templates,
            log_channels: parts
                .log_channels
                .into_iter()
                .filter_map(|(event, channel_id)| {
                    match u64::try_from(channel_id).ok().filter(|id| *id != 0) {
                        Some(channel_id) => Some((event, ChannelId::new(channel_id))),
                        None => {
                            tracing::warn!(
                                "ignoring invalid persisted log channel id {channel_id} for event {event}"
                            );
                            None
                        }
                    }
                })
                .collect(),
            log_filters: parts.log_filters,
            regexes,
            custom_patterns,
        }
    }

    /// Looks up a command by name or alias (case-insensitive).
    pub fn find_command(&self, name: &str) -> Option<&CommandDef> {
        let lower = name.to_lowercase();
        if let Some(primary) = jmodguard::defaults::builtin_command_for_label(&lower) {
            return self.commands.get(&primary);
        }
        self.commands.values().find(|c| c.matches(&lower))
    }
}

#[derive(Clone)]
pub struct ConfigCache {
    map: DashMap<i64, Arc<CachedGuildConfig>>,
    version: Arc<AtomicU64>,
    ttl: Duration,
    durable_floor: Arc<AtomicU64>,
    publication_lock: Arc<parking_lot::Mutex<()>>,
    accepted_revision: Arc<parking_lot::Mutex<String>>,
}

impl Default for ConfigCache {
    fn default() -> Self {
        Self::new(0, "legacy:0".into())
    }
}

impl ConfigCache {
    pub fn new(durable_floor: u64, revision: String) -> Self {
        Self {
            map: DashMap::new(),
            version: Arc::new(AtomicU64::new(0)),
            ttl: Duration::from_secs(15),
            durable_floor: Arc::new(AtomicU64::new(durable_floor)),
            publication_lock: Arc::new(parking_lot::Mutex::new(())),
            accepted_revision: Arc::new(parking_lot::Mutex::new(revision)),
        }
    }

    pub fn invalidate(&self) {
        let _publication = self.publication_lock.lock();
        self.version.fetch_add(1, Ordering::Relaxed);
        self.map.clear();
    }

    pub fn require_version(&self, version: u64) {
        self.durable_floor.fetch_max(version, Ordering::SeqCst);
        self.invalidate();
    }

    fn require_primary_revision(&self, version: u64, revision: String) {
        let _publication = self.publication_lock.lock();
        self.durable_floor.store(version, Ordering::SeqCst);
        *self.accepted_revision.lock() = revision;
        self.version.fetch_add(1, Ordering::Relaxed);
        self.map.clear();
    }

    fn rebaseline_version(&self, expected: u64, version: u64) {
        let mut current = self.durable_floor.load(Ordering::SeqCst);
        while current > version && current <= expected {
            match self.durable_floor.compare_exchange(
                current,
                version,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
        self.invalidate();
    }

    pub async fn get(&self, db: &Db, guild_id: u64) -> Result<Arc<CachedGuildConfig>> {
        let required = self
            .durable_floor
            .load(Ordering::SeqCst)
            .max(db.required_config_version());
        if let Some(entry) = self.map.get(&(guild_id as i64))
            && entry.version == self.version.load(Ordering::Relaxed)
            && entry.loaded_at.elapsed() < self.ttl
            && entry.durable_version >= required
        {
            return Ok(Arc::clone(&entry));
        }
        let mut primary = false;
        let mut last_observed = 0;
        let mut last_required = required;
        for _ in 0..3 {
            let version = self.version.load(Ordering::Relaxed);
            let (durable_version, revision, parts) = if primary {
                db::load_config_parts_with_version_primary(db, guild_id as i64).await?
            } else {
                db::load_config_parts_with_version(db, guild_id as i64).await?
            };
            let required_after = self
                .durable_floor
                .load(Ordering::SeqCst)
                .max(db.required_config_version());
            last_observed = durable_version;
            last_required = required_after;
            let accepted_revision = self.accepted_revision.lock().clone();

            if primary && revision != accepted_revision {
                self.require_primary_revision(durable_version, revision);
                continue;
            }
            if durable_version < required_after || revision != accepted_revision {
                // Replica lag is expected after a successful remote write. Do
                // not drop moderation/commands while waiting for convergence:
                // retry through one primary-routed, single-snapshot read.
                primary = true;
                continue;
            }
            let cfg = Arc::new(CachedGuildConfig::from_parts(
                version,
                durable_version,
                parts,
            ));
            {
                // Publication and invalidation share one lock, closing the
                // check→insert window that could republish a stale snapshot
                // after a same-process write had cleared the cache.
                let _publication = self.publication_lock.lock();
                let required_now = self
                    .durable_floor
                    .load(Ordering::SeqCst)
                    .max(db.required_config_version());
                let accepted_revision = self.accepted_revision.lock().clone();
                if self.version.load(Ordering::Relaxed) != version
                    || durable_version < required_now
                    || revision != accepted_revision
                {
                    primary = true;
                    continue;
                }
                self.map.insert(guild_id as i64, Arc::clone(&cfg));
                return Ok(cfg);
            }
        }
        anyhow::bail!(
            "database config changed repeatedly while loading (version {last_observed}, expected at least {last_required})"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ban_observation_release_and_restart_keep_registration_bounded() {
        let directory =
            std::env::temp_dir().join(format!("ban_flight_{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("core.db");
        let before_start = chrono::Utc::now().timestamp_millis();
        let app = BotState::new(Default::default(), Db::open(&path, 2).unwrap()).unwrap();
        let observation = app.begin_gateway_ban_observation(10, 55).await;
        assert!(observation.audit_after_ms() >= before_start);
        drop(observation);
        assert!(app.gateway_ban_flight(10, 55).lock().subjects.is_empty());
        // Completed observations release their per-subject registrations.
        for user in 1..4096 {
            app.begin_gateway_ban_observation(10, user)
                .await
                .finish()
                .await;
        }
        assert_eq!(app.gateway_ban_flights.len(), 256);
        for flight in &app.gateway_ban_flights {
            let flight = flight.lock();
            assert!(flight.subjects.values().all(|subject| subject.active == 0));
        }
        drop(app);
        let before_restart = chrono::Utc::now().timestamp_millis();
        let app = BotState::new(Default::default(), Db::open(&path, 2).unwrap()).unwrap();
        let next = app.begin_gateway_ban_observation(10, 55).await;
        assert!(next.audit_after_ms() >= before_restart);
        next.finish().await;
        drop(app);
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn colliding_ban_subjects(shards: usize) -> (u64, u64) {
        let guild = 10u64;
        let left = 55u64;
        let left_shard = gateway_ban_flight_index(guild, left, shards);
        let right = (1..20_000u64)
            .find(|user| {
                *user != left && gateway_ban_flight_index(guild, *user, shards) == left_shard
            })
            .expect("two synthetic users share a flight stripe");
        (left, right)
    }

    #[tokio::test]
    async fn same_stripe_member_does_not_gate_the_other_members_audit() {
        let (left, right) = colliding_ban_subjects(256);
        assert_eq!(
            gateway_ban_flight_index(10, left, 256),
            gateway_ban_flight_index(10, right, 256)
        );
        let audit = 1_212_912_186_163_200_001i64;
        let directory = std::env::temp_dir().join(format!(
            "shared_stripe_audit_{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let app = BotState::new(
            Default::default(),
            Db::open(&directory.join("core.db"), 2).unwrap(),
        )
        .unwrap();
        let scope = db::enroll_guild(&app.db, 10, 100).await.unwrap().unwrap();
        for user in [left, right] {
            db::record_gateway_ban(
                &app.db,
                Some(scope.clone()),
                user as i64,
                None,
                None,
                Some("2024-03-01T00:00:00Z"),
                false,
                None,
                false,
            )
            .await
            .unwrap();
        }
        let other = Some(app.begin_gateway_ban_observation(10, left).await);
        app.queue_gateway_manual_ban_audit_entry(10, right, audit);
        assert!(
            app.reconcile_gateway_manual_ban_audit_entry(Some(scope.clone()), 10, right, audit)
                .await
                .unwrap()
        );
        app.drop_gateway_manual_ban_audit_entry(10, right, audit);
        let right_events = db::query_enrolled_events(
            &app.db,
            scope.clone(),
            Some("member_ban"),
            Some(right as i64),
            20,
        )
        .await
        .unwrap();
        assert!(right_events.iter().any(|event| {
            event
                .data
                .as_deref()
                .is_some_and(|data| data.contains(&audit.to_string()))
        }));
        app.queue_gateway_manual_ban_audit_entry(10, left, audit);
        assert!(
            !app.reconcile_gateway_manual_ban_audit_entry(Some(scope.clone()), 10, left, audit)
                .await
                .unwrap()
        );
        assert_eq!(
            app.pending_gateway_manual_ban_audit_entries(10, left),
            vec![audit]
        );
        if let Some(other) = other {
            other.finish().await;
        }
        drop(app);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn cancelled_saved_ban_retries_its_queued_audit_after_release() {
        let directory = std::env::temp_dir().join(format!(
            "cancelled_saved_ban_{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let app = BotState::new(
            Default::default(),
            Db::open(&directory.join("core.db"), 2).unwrap(),
        )
        .unwrap();
        let scope = db::enroll_guild(&app.db, 10, 100).await.unwrap().unwrap();
        let mut observation = app.begin_gateway_ban_observation(10, 55).await;
        observation.enrollment = Some(scope.clone());
        // A post-startup audit, distinct from the process's uncertainty cutoff.
        observation.observed += chrono::Duration::seconds(1);
        let audit = audit_id_at(observation.observed.timestamp_millis());
        db::record_gateway_ban_with_audit(
            &app.db,
            Some(scope.clone()),
            55,
            None,
            None,
            Some(&observation.observed.to_rfc3339()),
            false,
            None,
            false,
            db::GatewayBanAuditEntries::Observed {
                pending: vec![],
                audit_after_ms: observation.audit_after_ms(),
            },
        )
        .await
        .unwrap();
        app.queue_gateway_manual_ban_audit_entry(10, 55, audit);
        assert!(
            !app.reconcile_gateway_manual_ban_audit_entry(Some(scope.clone()), 10, 55, audit,)
                .await
                .unwrap()
        );
        {
            let finish = observation.finish();
            tokio::pin!(finish);
            tokio::select! {
                biased;
                _ = &mut finish => panic!("audit matched before its window closed"),
                _ = std::future::ready(()) => {}
            }
            // Cancel after release while the detached retry waits for the window.
        }
        tokio::time::timeout(Duration::from_secs(8), async {
            while !app
                .pending_gateway_manual_ban_audit_entries(10, 55)
                .is_empty()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("release must retry without another gateway event");
        assert!(
            db::gateway_manual_ban_audit_entry_is_known(&app.db, Some(scope), 10, 55, audit,)
                .await
                .unwrap()
        );
        drop(app);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn cleared_audit_cannot_reconcile_a_later_ban() {
        for requeue in [false, true] {
            let directory =
                std::env::temp_dir().join(format!("cleared_audit_{:032x}", rand::random::<u128>()));
            std::fs::create_dir_all(&directory).unwrap();
            let app = BotState::new(
                Default::default(),
                Db::open(&directory.join("core.db"), 2).unwrap(),
            )
            .unwrap();
            let scope = db::enroll_guild(&app.db, 10, 100).await.unwrap().unwrap();
            let observed = chrono::Utc::now() + chrono::Duration::seconds(1);
            let audit = audit_id_at(observed.timestamp_millis());
            app.queue_gateway_manual_ban_audit_entry(10, 55, audit);
            let reconcile =
                app.reconcile_gateway_manual_ban_audit_entry(Some(scope.clone()), 10, 55, audit);
            tokio::pin!(reconcile);
            tokio::select! {
                biased;
                _ = &mut reconcile => panic!("audit must wait for its match window"),
                _ = std::future::ready(()) => {}
            }
            // Unban removes the pending audit while its pass is asleep.
            app.clear_gateway_manual_ban_audit_entries(10, 55);
            if requeue {
                // A replay must not revive the old pass's captured work.
                app.queue_gateway_manual_ban_audit_entry(10, 55, audit);
            }
            db::record_gateway_ban(
                &app.db,
                Some(scope.clone()),
                55,
                None,
                None,
                Some(&observed.to_rfc3339()),
                false,
                None,
                false,
            )
            .await
            .unwrap();
            assert!(
                !reconcile.await.unwrap(),
                "cleared pass must not bind a later ban"
            );
            assert!(
                !db::gateway_manual_ban_audit_entry_is_known(
                    &app.db,
                    Some(scope.clone()),
                    10,
                    55,
                    audit,
                )
                .await
                .unwrap()
            );
            if requeue {
                assert!(
                    app.reconcile_gateway_manual_ban_audit_entry(Some(scope), 10, 55, audit,)
                        .await
                        .unwrap(),
                    "fresh replay remains eligible"
                );
            }
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[tokio::test]
    async fn transient_audit_failure_retries_until_recovery_clear_or_expiry() {
        for outcome in ["recover", "clear", "expire"] {
            let directory =
                std::env::temp_dir().join(format!("audit_retry_{:032x}", rand::random::<u128>()));
            std::fs::create_dir_all(&directory).unwrap();
            let app = BotState::new(
                Default::default(),
                Db::open(&directory.join("core.db"), 2).unwrap(),
            )
            .unwrap();
            let scope = db::enroll_guild(&app.db, 10, 100).await.unwrap().unwrap();
            let audit = 1_212_912_186_163_200_001;
            db::record_gateway_ban(
                &app.db,
                Some(scope.clone()),
                55,
                None,
                None,
                Some("2024-03-01T00:00:00Z"),
                false,
                None,
                false,
            )
            .await
            .unwrap();
            app.queue_gateway_manual_ban_audit_entry(10, 55, audit);
            if outcome == "expire" {
                app.gateway_manual_ban_audit_entries
                    .lock()
                    .entries
                    .get_mut(&(10, 55))
                    .unwrap()[0]
                    .expires = Instant::now() + Duration::from_secs(6);
            }
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let blocker = {
                let app = Arc::clone(&app);
                tokio::spawn(async move {
                    app.db
                        .with_conn(move |conn| {
                            let tx = conn.transaction_immediate()?;
                            let _ = entered_tx.send(());
                            let _ = release_rx.recv_timeout(Duration::from_secs(15));
                            tx.commit()
                        })
                        .await
                        .unwrap();
                })
            };
            entered_rx.await.unwrap();
            let callback = {
                let app = Arc::clone(&app);
                let scope = scope.clone();
                tokio::spawn(async move {
                    crate::handlers::reconcile_pending_gateway_ban_audits(
                        &app,
                        Some(scope),
                        10,
                        55,
                    )
                    .await;
                })
            };
            // Hold past SQLite's five-second busy deadline: the first attempt must fail.
            tokio::time::sleep(Duration::from_secs(6)).await;
            let stopped_after_error = callback.is_finished();
            if outcome == "clear" {
                app.clear_gateway_manual_ban_audit_entries(10, 55);
                app.queue_gateway_manual_ban_audit_entry(10, 55, audit);
            }
            release_tx.send(()).unwrap();
            blocker.await.unwrap();
            tokio::time::timeout(Duration::from_secs(8), callback)
                .await
                .unwrap()
                .unwrap();
            if outcome != "expire" {
                assert!(
                    !stopped_after_error,
                    "transient failure must retain an independent retry"
                );
            }
            let known = db::gateway_manual_ban_audit_entry_is_known(
                &app.db,
                Some(scope.clone()),
                10,
                55,
                audit,
            )
            .await
            .unwrap();
            assert_eq!(known, outcome == "recover", "{outcome}");
            if outcome == "clear" {
                assert_eq!(
                    app.pending_gateway_manual_ban_audit_entries(10, 55),
                    vec![audit]
                );
                assert!(
                    app.reconcile_gateway_manual_ban_audit_entry(Some(scope), 10, 55, audit)
                        .await
                        .unwrap()
                );
            } else {
                assert!(
                    app.pending_gateway_manual_ban_audit_entries(10, 55)
                        .is_empty()
                );
            }
            drop(app);
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[tokio::test]
    async fn cancelled_audit_callback_reconciles_without_another_event() {
        let directory =
            std::env::temp_dir().join(format!("cancelled_audit_{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&directory).unwrap();
        let app = BotState::new(
            Default::default(),
            Db::open(&directory.join("core.db"), 2).unwrap(),
        )
        .unwrap();
        let scope = db::enroll_guild(&app.db, 10, 100).await.unwrap().unwrap();
        let mut observation = app.begin_gateway_ban_observation(10, 55).await;
        observation.enrollment = Some(scope.clone());
        observation.observed += chrono::Duration::seconds(1);
        let audit = audit_id_at(observation.observed.timestamp_millis());
        db::record_gateway_ban(
            &app.db,
            Some(scope.clone()),
            55,
            None,
            None,
            Some(&observation.observed.to_rfc3339()),
            false,
            None,
            false,
        )
        .await
        .unwrap();
        // The observation's retry has already completed with an empty queue.
        observation.finish().await;
        {
            let callback = crate::handlers::reconcile_received_gateway_ban_audit(
                &app,
                Some(scope.clone()),
                10,
                55,
                audit,
                db::GatewayBanAuditAttribution {
                    moderator_id: 77,
                    moderator_name: Some("moderator".to_owned()),
                    reason: "manual ban".to_owned(),
                },
            );
            tokio::pin!(callback);
            tokio::select! {
                biased;
                _ = &mut callback => panic!("audit must wait for its match window"),
                _ = std::future::ready(()) => {}
            }
            // Dropping the callback must leave its queued reconciliation running.
        }
        tokio::time::timeout(Duration::from_secs(8), async {
            while !app
                .pending_gateway_manual_ban_audit_entries(10, 55)
                .is_empty()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("audit callback cancellation must not strand its audit");
        assert!(
            db::gateway_manual_ban_audit_entry_is_known(&app.db, Some(scope), 10, 55, audit,)
                .await
                .unwrap()
        );
        drop(app);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn audit_waits_for_delayed_callbacks_before_matching_an_older_ban() {
        let directory =
            std::env::temp_dir().join(format!("audit_window_{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&directory).unwrap();
        let app = BotState::new(
            Default::default(),
            Db::open(&directory.join("core.db"), 2).unwrap(),
        )
        .unwrap();
        let scope = db::enroll_guild(&app.db, 10, 100).await.unwrap().unwrap();
        let observed = chrono::Utc::now();
        let audit = audit_id_at(observed.timestamp_millis());
        // Delayed delivery of the older callback made its saved observation
        // fall inside the newer action's audit window.
        db::record_gateway_ban(
            &app.db,
            Some(scope.clone()),
            55,
            None,
            None,
            Some(&observed.to_rfc3339()),
            false,
            None,
            false,
        )
        .await
        .unwrap();
        app.queue_gateway_manual_ban_audit_entry(10, 55, audit);
        let reconcile = {
            let app = Arc::clone(&app);
            let scope = scope.clone();
            tokio::spawn(async move {
                app.reconcile_gateway_manual_ban_audit_entry(Some(scope), 10, 55, audit)
                    .await
                    .unwrap()
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !reconcile.is_finished(),
            "reconciliation must wait for delayed callbacks"
        );
        assert!(
            !db::gateway_manual_ban_audit_entry_is_known(
                &app.db,
                Some(scope.clone()),
                10,
                55,
                audit
            )
            .await
            .unwrap(),
            "an open match window must not permanently select the older ban"
        );
        let mut later = app.begin_gateway_ban_observation(10, 55).await;
        later.enrollment = Some(scope.clone());
        assert!(later.observed.timestamp_millis() - observed.timestamp_millis() <= 5_000);
        db::record_gateway_ban_with_audit(
            &app.db,
            Some(scope.clone()),
            55,
            None,
            None,
            Some(&later.observed.to_rfc3339()),
            false,
            None,
            true,
            db::GatewayBanAuditEntries::Observed {
                pending: vec![],
                audit_after_ms: later.audit_after_ms(),
            },
        )
        .await
        .unwrap();
        later.finish().await;
        assert!(!reconcile.await.unwrap());
        assert!(
            !db::gateway_manual_ban_audit_entry_is_known(&app.db, Some(scope), 10, 55, audit)
                .await
                .unwrap()
        );
        assert_eq!(
            app.pending_gateway_manual_ban_audit_entries(10, 55),
            vec![audit]
        );
        drop(app);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sql_stall_does_not_delay_ban_observation_or_lose_attribution() {
        let directory =
            std::env::temp_dir().join(format!("sql_flight_stall_{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&directory).unwrap();
        let app = BotState::new(
            Default::default(),
            Db::open(&directory.join("core.db"), 2).unwrap(),
        )
        .unwrap();
        let scope = db::enroll_guild(&app.db, 10, 100).await.unwrap().unwrap();
        let old_time = chrono::Utc::now() - chrono::Duration::seconds(10);
        db::record_gateway_ban(
            &app.db,
            Some(scope.clone()),
            55,
            None,
            None,
            Some(&old_time.to_rfc3339()),
            false,
            None,
            false,
        )
        .await
        .unwrap();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = {
            let app = Arc::clone(&app);
            tokio::spawn(async move {
                app.db
                    .with_conn(move |conn| {
                        let tx = conn.transaction_immediate()?;
                        let _ = entered_tx.send(());
                        let _ = release_rx.recv_timeout(Duration::from_secs(15));
                        tx.commit()
                    })
                    .await
            })
        };
        entered_rx.await.unwrap();
        app.queue_gateway_manual_ban_audit_entry(10, 55, audit_id_at(old_time.timestamp_millis()));
        let reconciliation = {
            let app = Arc::clone(&app);
            let scope = scope.clone();
            tokio::spawn(async move {
                app.reconcile_gateway_manual_ban_audit_entry(
                    Some(scope),
                    10,
                    55,
                    audit_id_at(old_time.timestamp_millis()),
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut observation = tokio::time::timeout(
            Duration::from_secs(1),
            app.begin_gateway_ban_observation(10, 55),
        )
        .await
        .expect("SQL must not hold the callback registration lock");
        observation.enrollment = Some(scope.clone());
        let audit = audit_id_at(observation.observed.timestamp_millis());
        tokio::time::sleep(Duration::from_millis(5_200)).await;
        assert!(
            chrono::Utc::now().timestamp_millis() - observation.observed.timestamp_millis() > 5_000
        );
        release_tx.send(()).unwrap();
        blocker.await.unwrap().unwrap();
        db::record_gateway_ban_with_audit(
            &app.db,
            Some(scope.clone()),
            55,
            None,
            None,
            Some(&observation.observed.to_rfc3339()),
            false,
            None,
            true,
            db::GatewayBanAuditEntries::Observed {
                pending: vec![],
                audit_after_ms: observation.audit_after_ms(),
            },
        )
        .await
        .unwrap();
        app.queue_attributed_gateway_manual_ban_audit_entry(
            10,
            55,
            audit,
            db::GatewayBanAuditAttribution {
                moderator_id: 77,
                moderator_name: Some("moderator".into()),
                reason: "manual reason".into(),
            },
        );
        observation.finish().await;
        // A busy-error retry now waits for the active observation to finish.
        // Join only after releasing it, preserving the original arrival time.
        let _ = tokio::time::timeout(Duration::from_secs(8), reconciliation)
            .await
            .expect("reconciliation must finish after the observation releases")
            .unwrap();
        let rows = db::list_enrolled_bans(&app.db, scope, 10).await.unwrap();
        let row = rows.iter().find(|row| row.active).unwrap();
        assert_eq!(row.moderator_id, Some(77));
        assert_eq!(row.moderator_name.as_deref(), Some("moderator"));
        assert_eq!(row.reason.as_deref(), Some("manual reason"));
        drop(app);
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn users_on_one_stripe(shards: usize, count: usize) -> Vec<u64> {
        let mut users = Vec::with_capacity(count);
        let mut stripe = None;
        for user in 1..1_000_000u64 {
            let index = gateway_ban_flight_index(10, user, shards);
            if stripe.is_none() || stripe == Some(index) {
                stripe = Some(index);
                users.push(user);
                if users.len() == count {
                    return users;
                }
            }
        }
        panic!("not enough synthetic users on one flight stripe");
    }

    fn audit_id_at(ms: i64) -> i64 {
        (ms - 1_420_070_400_000) << 22
    }

    #[tokio::test]
    async fn invalidated_observation_releases_without_fencing_new_membership_or_other_users() {
        for captured in [false, true] {
            let users = users_on_one_stripe(256, 2);
            let directory = std::env::temp_dir()
                .join(format!("retired_ban_owner_{:032x}", rand::random::<u128>()));
            std::fs::create_dir_all(&directory).unwrap();
            let app = BotState::new(
                Default::default(),
                Db::open(&directory.join("core.db"), 2).unwrap(),
            )
            .unwrap();
            let old = db::enroll_guild(&app.db, 10, 100).await.unwrap().unwrap();
            let base = chrono::Utc::now().timestamp_millis() - 20_000;
            app.gateway_ban_flight(10, users[0]).lock().started_at_ms = base - 1;
            let mut observation = app.begin_gateway_ban_observation(10, users[0]).await;
            observation.observed = chrono::DateTime::from_timestamp_millis(base + 1_000).unwrap();
            observation.enrollment = captured.then_some(old);
            db::retire_guild_enrollment(&app.db, 10, 100).await.unwrap();
            let current = db::enroll_guild(&app.db, 10, 200).await.unwrap().unwrap();
            // The retained owner only gives up ordinary work when membership is
            // invalid. Neither captured nor uncaptured exits start a new writer.
            drop(observation);
            assert!(
                app.gateway_ban_flight(10, users[0])
                    .lock()
                    .subjects
                    .is_empty()
            );
            for user in &users {
                let observation = app.begin_gateway_ban_observation(10, *user).await;
                let cutoff = observation.audit_after_ms();
                assert!(
                    cutoff < base,
                    "old work must not seed this membership or stripe"
                );
                db::record_gateway_ban_with_audit(
                    &app.db,
                    Some(current.clone()),
                    *user as i64,
                    None,
                    None,
                    Some(
                        &chrono::DateTime::from_timestamp_millis(base)
                            .unwrap()
                            .to_rfc3339(),
                    ),
                    false,
                    None,
                    false,
                    db::GatewayBanAuditEntries::Observed {
                        pending: vec![],
                        audit_after_ms: cutoff,
                    },
                )
                .await
                .unwrap();
                observation.finish().await;
                let audit = audit_id_at(base);
                app.queue_gateway_manual_ban_audit_entry(10, *user, audit);
                assert!(
                    app.reconcile_gateway_manual_ban_audit_entry(
                        Some(current.clone()),
                        10,
                        *user,
                        audit
                    )
                    .await
                    .unwrap()
                );
            }
            assert!(
                app.gateway_ban_flight(10, users[0])
                    .lock()
                    .subjects
                    .is_empty()
            );
            drop(app);
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[tokio::test]
    async fn active_ban_subjects_remain_independent_above_the_old_capacity() {
        let users = users_on_one_stripe(256, 65);
        let stripe = gateway_ban_flight_index(10, users[0], 256);
        let directory =
            std::env::temp_dir().join(format!("stripe_active_cap_{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&directory).unwrap();
        let app = BotState::new(
            Default::default(),
            Db::open(&directory.join("core.db"), 2).unwrap(),
        )
        .unwrap();
        let scope = db::enroll_guild(&app.db, 10, 100).await.unwrap().unwrap();
        let at_ms = (chrono::Utc::now().timestamp() - 20) * 1000;
        let at = chrono::DateTime::from_timestamp_millis(at_ms)
            .unwrap()
            .to_rfc3339();
        let audit = audit_id_at(at_ms);
        for user in &users {
            db::record_gateway_ban(
                &app.db,
                Some(scope.clone()),
                *user as i64,
                None,
                None,
                Some(&at),
                false,
                None,
                false,
            )
            .await
            .unwrap();
        }
        let mut held = Vec::with_capacity(users.len());
        for user in &users {
            held.push(app.begin_gateway_ban_observation(10, *user).await);
        }
        {
            let flight = app.gateway_ban_flights[stripe].lock();
            assert!(flight.subjects.len() == 65);
            for user in &users {
                assert!(
                    flight
                        .subjects
                        .get(&(10, *user))
                        .is_some_and(|subject| subject.active > 0),
                    "{user}"
                );
            }
        }
        for user in &users {
            app.queue_gateway_manual_ban_audit_entry(10, *user, audit);
            assert!(
                !app.reconcile_gateway_manual_ban_audit_entry(
                    Some(scope.clone()),
                    10,
                    *user,
                    audit
                )
                .await
                .unwrap(),
                "{user}"
            );
        }
        for (user, observation) in users.iter().zip(held) {
            observation.finish().await;
            assert!(
                app.reconcile_gateway_manual_ban_audit_entry(Some(scope.clone()), 10, *user, audit)
                    .await
                    .unwrap(),
                "the eligible audit must match after releasing user {user}"
            );
        }
        drop(app);
        std::fs::remove_dir_all(directory).unwrap();
    }

    use crate::config::BotProcessConfig;
    use crate::db::{self, Db, DbOpenOptions};
    use crate::types::GuildSettings;

    #[test]
    fn overlapping_ban_observations_remain_contested_until_last_owner_finishes() {
        let tmp = std::env::temp_dir().join(format!(
            "jmodguard_ban_owners_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let db = Db::open_with(DbOpenOptions {
            read_only: false,
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: true,
            default_prefix: "!".into(),
            retention_days: 7,
        })
        .unwrap();
        let app = BotState::new(BotProcessConfig::default(), db).unwrap();
        assert!(!app.begin_ban_observation(10, 20));
        assert!(app.begin_ban_observation(10, 20));
        app.end_ban_observation(10, 20);
        assert!(app.begin_ban_observation(10, 20));
        app.end_ban_observation(10, 20);
        app.end_ban_observation(10, 20);
        assert!(!app.begin_ban_observation(10, 20));
        app.end_ban_observation(10, 20);
        drop(app);
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(format!("{}-wal", tmp.display()));
        let _ = std::fs::remove_file(format!("{}-shm", tmp.display()));
        crate::tenant::remove_sidecars(&tmp);
    }

    #[test]
    fn failed_member_observations_remain_queued_until_the_same_generation_is_acknowledged() {
        let mut gaps = PendingMemberGaps::default();
        gaps.remember(
            42,
            Some("enrollment-a".into()),
            Some(7),
            "member join persistence unavailable",
        );
        let first = gaps.snapshot(42).pop().expect("queued gap");
        gaps.remember(
            42,
            Some("enrollment-a".into()),
            Some(8),
            "member departure persistence unavailable",
        );
        gaps.acknowledge(&first);
        let current = gaps.snapshot(42).pop().expect("newer failure retained");
        assert_eq!(current.users, vec![(7, false), (8, false)]);
        gaps.acknowledge(&current);
        assert!(gaps.snapshot(42).is_empty());
    }

    #[test]
    fn gateway_case_suppressions_are_scoped_consumed_and_bounded() {
        let mut suppressions = GatewayCaseSuppressions::default();
        suppressions.insert(GatewayCaseAction::Ban, 10, 20, None);
        assert!(!suppressions.remove(GatewayCaseAction::Unban, 10, 20));
        assert!(suppressions.remove(GatewayCaseAction::Ban, 10, 20));
        assert!(!suppressions.remove(GatewayCaseAction::Ban, 10, 20));

        suppressions.entries.insert(
            0,
            GatewayCaseEntry {
                action: GatewayCaseAction::Ban,
                guild: 30,
                user: 40,
                operation_id: None,
                expires: Instant::now() - Duration::from_secs(1),
                released: Arc::new(AtomicBool::new(false)),
            },
        );
        suppressions.insert(GatewayCaseAction::Unban, 30, 40, None);
        assert!(!suppressions.remove(GatewayCaseAction::Ban, 30, 40));
        assert!(suppressions.remove(GatewayCaseAction::Unban, 30, 40));

        for user in 1..=(MAX_GATEWAY_CASE_SUPPRESSIONS as u64 + 1) {
            suppressions.insert(GatewayCaseAction::Ban, 50, user, None);
        }
        assert_eq!(suppressions.entries.len(), MAX_GATEWAY_CASE_SUPPRESSIONS);
    }

    #[test]
    fn consumed_ban_echo_stays_owned_until_cleared() {
        let mut suppressions = GatewayCaseSuppressions::default();
        suppressions.insert(GatewayCaseAction::Ban, 10, 20, Some("attempt-a".into()));
        assert!(suppressions.remove(GatewayCaseAction::Ban, 10, 20));
        assert_eq!(
            suppressions.ban_echo_operation(10, 20),
            Some(Some("attempt-a".into()))
        );
        assert!(suppressions.ban_echo_operation(10, 20).is_some());
        assert!(suppressions.ban_echo_operation(10, 21).is_none());
        suppressions.insert(GatewayCaseAction::Unban, 10, 21, None);
        assert!(suppressions.remove(GatewayCaseAction::Unban, 10, 21));
        assert!(suppressions.ban_echo_operation(10, 21).is_none());
        suppressions.clear_ban_echo(10, 20);
        assert!(suppressions.ban_echo_operation(10, 20).is_none());
    }

    #[test]
    fn unban_preserves_pending_ban_dispatches_for_its_member() {
        let mut s = GatewayCaseSuppressions::default();
        let old = s.insert(GatewayCaseAction::Ban, 10, 20, Some("old-attempt".into()));
        assert!(s.remove(GatewayCaseAction::Ban, 10, 20));
        // The next dispatch can start before the preceding unban callback.
        s.insert(GatewayCaseAction::Ban, 10, 20, Some("new-attempt".into()));
        s.insert(GatewayCaseAction::Unban, 10, 20, None);
        s.insert(GatewayCaseAction::Ban, 10, 21, Some("other-member".into()));
        s.insert(GatewayCaseAction::Ban, 11, 20, Some("other-guild".into()));
        s.clear_ban_echo(10, 20);
        assert!(s.ban_echo_operation(10, 20).is_none());
        s.release(old.clone());
        assert!(s.remove(GatewayCaseAction::Ban, 10, 20));
        assert_eq!(
            s.ban_echo_operation(10, 20),
            Some(Some("new-attempt".into()))
        );
        assert!(s.remove(GatewayCaseAction::Unban, 10, 20));
        assert!(s.remove(GatewayCaseAction::Ban, 10, 21));
        assert!(s.remove(GatewayCaseAction::Ban, 11, 20));
        s.clear_ban_echo(10, 20);
        s.insert(GatewayCaseAction::Ban, 10, 20, Some("later-attempt".into()));
        s.release(old);
        assert!(s.remove(GatewayCaseAction::Ban, 10, 20));
        assert_eq!(
            s.ban_echo_operation(10, 20),
            Some(Some("later-attempt".into()))
        );
    }

    #[test]
    fn overlapping_gateway_dispatches_keep_individual_ownership() {
        let mut s = GatewayCaseSuppressions::default();
        let a = s.insert(GatewayCaseAction::Ban, 10, 20, None);
        let b = s.insert(GatewayCaseAction::Ban, 10, 20, None);
        s.release(b);
        assert!(s.remove(GatewayCaseAction::Ban, 10, 20));
        assert!(!s.remove(GatewayCaseAction::Ban, 10, 20));
        s.release(a); // Already consumed; cannot release a future marker.
        let a = s.insert(GatewayCaseAction::Ban, 10, 20, None);
        let _b = s.insert(GatewayCaseAction::Ban, 10, 20, None);
        s.release(a);
        assert!(s.remove(GatewayCaseAction::Ban, 10, 20));
        let a = s.insert(GatewayCaseAction::Unban, 10, 20, None);
        assert!(s.remove(GatewayCaseAction::Unban, 10, 20));
        s.insert(GatewayCaseAction::Unban, 10, 20, None);
        s.release(a);
        assert!(s.remove(GatewayCaseAction::Unban, 10, 20));
        s.insert(GatewayCaseAction::Ban, 10, 20, None);
        s.insert(GatewayCaseAction::Ban, 10, 20, None);
        assert!(s.remove(GatewayCaseAction::Ban, 10, 20));
        assert!(s.remove(GatewayCaseAction::Ban, 10, 20));
        assert!(!s.remove(GatewayCaseAction::Ban, 10, 20));
    }

    fn dummy_link(subject: u64) -> PendingAuditLink {
        PendingAuditLink {
            action: crate::db::guild_data::CaseAction::Ban,
            guild: 10,
            subject,
            case_id: crate::db::guild_data::CaseId::new(),
            operation_id: crate::db::guild_data::CaseId::new(),
            enrollment: db::GuildEnrollment {
                guild_id: 10,
                id: "a".repeat(32),
                joined_at_ns: 1,
            },
        }
    }

    #[test]
    fn pending_audit_links_match_oldest_owned_dispatch() {
        let mut pending = PendingAuditLinks::default();
        let first = dummy_link(20);
        let first_id = first.case_id.clone();
        let second = dummy_link(20);
        assert!(pending.remember_application(first).is_none());
        assert!(pending.remember_application(second).is_none());
        let ready = pending
            .remember_audit(crate::db::guild_data::CaseAction::Ban, 10, 20, 123)
            .unwrap();
        assert_eq!(ready.link.case_id, first_id);
        assert_eq!(ready.entry_id, 123);
        pending.finalize_link(ready.audit_token, ready.application_token);
        assert!(
            pending
                .remember_audit(crate::db::guild_data::CaseAction::Unban, 10, 20, 124)
                .is_none()
        );
        assert!(pending.remember_application(dummy_link(21)).is_none());
    }

    #[test]
    fn pending_audit_event_can_arrive_before_the_case() {
        let mut pending = PendingAuditLinks::default();
        assert!(
            pending
                .remember_audit(crate::db::guild_data::CaseAction::Kick, 10, 20, 55)
                .is_none()
        );
        let mut kick = dummy_link(20);
        kick.action = crate::db::guild_data::CaseAction::Kick;
        let ready = pending.remember_application(kick).unwrap();
        assert_eq!(ready.entry_id, 55);
        pending.finalize_link(ready.audit_token, ready.application_token);
        assert!(pending.remember_application(dummy_link(20)).is_none());
    }

    #[test]
    fn pending_audit_match_consumes_live_half() {
        let mut pending = PendingAuditLinks::default();
        pending.remember_audit(crate::db::guild_data::CaseAction::Ban, 10, 20, 123);
        let first = pending.remember_application(dummy_link(20)).unwrap();
        assert_eq!(first.entry_id, 123);
        assert!(pending.remember_application(dummy_link(20)).is_none());
        let later = pending
            .remember_audit(crate::db::guild_data::CaseAction::Ban, 10, 20, 124)
            .unwrap();
        assert_eq!(later.entry_id, 124);
        assert_ne!(later.link.case_id, first.link.case_id);
        assert!(
            pending
                .remember_audit(crate::db::guild_data::CaseAction::Ban, 10, 20, 125)
                .is_none()
        );
    }

    #[test]
    fn deferred_audit_pair_is_quarantined_from_live_maps() {
        let mut pending = PendingAuditLinks::default();
        pending.remember_audit(crate::db::guild_data::CaseAction::Ban, 10, 20, 123);
        let first = pending.remember_application(dummy_link(20)).unwrap();
        let first_id = first.link.case_id.clone();
        pending.defer_link(first);
        assert!(pending.remember_application(dummy_link(20)).is_none());
        let later = pending
            .remember_audit(crate::db::guild_data::CaseAction::Ban, 10, 20, 124)
            .unwrap();
        assert_ne!(later.link.case_id, first_id);
        assert_eq!(later.entry_id, 124);
    }

    #[test]
    fn gateway_unban_clears_queued_manual_ban_audit_entries() {
        let path = std::env::temp_dir().join(format!(
            "gateway_manual_ban_audit_clear_{:032x}",
            rand::random::<u128>()
        ));
        let app =
            BotState::new(Default::default(), crate::db::Db::open(&path, 2).unwrap()).unwrap();
        app.queue_gateway_manual_ban_audit_entry(10, 55, 100);
        app.queue_gateway_manual_ban_audit_entry(10, 55, 101);
        app.clear_gateway_manual_ban_audit_entries(10, 55);
        assert!(
            app.pending_gateway_manual_ban_audit_entries(10, 55)
                .is_empty()
        );
        app.queue_gateway_manual_ban_audit_entry(10, 56, 102);
        app.clear_gateway_manual_ban_audit_entries(10, 55);
        assert_eq!(
            app.pending_gateway_manual_ban_audit_entries(10, 56),
            vec![102]
        );
    }

    #[test]
    fn deferred_audit_queue_is_capped_like_gateway_suppression() {
        let mut pending = PendingAuditLinks::default();
        for subject in 1..=(MAX_GATEWAY_CASE_SUPPRESSIONS as u64 + 1) {
            pending.remember_audit(
                crate::db::guild_data::CaseAction::Ban,
                10,
                subject,
                subject as i64,
            );
            let ready = pending.remember_application(dummy_link(subject)).unwrap();
            pending.defer_link(ready);
        }
        assert_eq!(pending.deferred_links.len(), MAX_GATEWAY_CASE_SUPPRESSIONS);
    }

    fn tokio_test_block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn poll_config_version_invalidates_after_cross_process_write() {
        let tmp = std::env::temp_dir().join(format!(
            "jmodguard_poll_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bot_db = Db::open_with(DbOpenOptions {
            read_only: false,
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: true,
            default_prefix: "!".into(),
            retention_days: 7,
        })
        .unwrap();
        let api_db = Db::open_with(DbOpenOptions {
            read_only: false,
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: false,
            default_prefix: "!".into(),
            retention_days: 7,
        })
        .unwrap();
        let bot = BotState::new(BotProcessConfig::default(), bot_db).unwrap();
        // Already primed at construction — unchanged version must not invalidate.
        assert!(!tokio_test_block_on(bot.poll_config_version()).unwrap());
        tokio_test_block_on(db::patch_settings(
            &api_db,
            7,
            Some(Some("!api".into())),
            None,
            None,
        ))
        .unwrap();
        assert!(tokio_test_block_on(bot.poll_config_version()).unwrap());
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(format!("{}-wal", tmp.display()));
        let _ = std::fs::remove_file(format!("{}-shm", tmp.display()));
        crate::tenant::remove_sidecars(&tmp);
    }

    #[test]
    fn poll_invalidates_warm_cache_even_before_first_poll() {
        // Regression: old u64::MAX "first observation" skip left a warm cache
        // stale when a cross-process write landed before the first successful poll.
        let tmp = std::env::temp_dir().join(format!(
            "jmodguard_poll_warm_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bot_db = Db::open_with(DbOpenOptions {
            read_only: false,
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: true,
            default_prefix: "!".into(),
            retention_days: 7,
        })
        .unwrap();
        let api_db = Db::open_with(DbOpenOptions {
            read_only: false,
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: false,
            default_prefix: "!".into(),
            retention_days: 7,
        })
        .unwrap();
        let bot = BotState::new(BotProcessConfig::default(), bot_db).unwrap();
        let guild_id = 99u64;

        // Warm cache with default prefix — no poll yet.
        let before = tokio_test_block_on(bot.configs.get(&bot.db, guild_id)).unwrap();
        assert_eq!(before.settings.prefix, "!");

        let settings = GuildSettings {
            prefix: "!api-wrote".into(),
            ..Default::default()
        };
        tokio_test_block_on(db::patch_settings(
            &api_db,
            guild_id as i64,
            Some(Some(settings.prefix)),
            None,
            None,
        ))
        .unwrap();

        // First poll after a write must invalidate (primed baseline ≠ remote).
        assert!(tokio_test_block_on(bot.poll_config_version()).unwrap());
        let after = tokio_test_block_on(bot.configs.get(&bot.db, guild_id)).unwrap();
        assert_eq!(after.settings.prefix, "!api-wrote");

        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(format!("{}-wal", tmp.display()));
        let _ = std::fs::remove_file(format!("{}-shm", tmp.display()));
        crate::tenant::remove_sidecars(&tmp);
    }

    #[test]
    fn revision_token_detects_failover_aba_at_same_version() {
        let tmp = std::env::temp_dir().join(format!(
            "jmodguard_revision_aba_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bot_db = Db::open_with(DbOpenOptions {
            read_only: false,
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: true,
            default_prefix: "!".into(),
            retention_days: 7,
        })
        .unwrap();
        let api_db = Db::open_with(DbOpenOptions {
            read_only: false,
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: false,
            default_prefix: "!".into(),
            retention_days: 7,
        })
        .unwrap();
        let bot = BotState::new(BotProcessConfig::default(), bot_db).unwrap();
        let guild_id = 123u64;
        let baseline = bot.last_config_version.load(Ordering::SeqCst);
        assert_eq!(
            tokio_test_block_on(bot.configs.get(&bot.db, guild_id))
                .unwrap()
                .settings
                .prefix,
            "!"
        );

        tokio_test_block_on(db::patch_settings(
            &api_db,
            guild_id as i64,
            Some(Some("aba-visible".into())),
            None,
            None,
        ))
        .unwrap();
        tokio_test_block_on(api_db.with_conn(move |conn| {
            let tx = conn.transaction_immediate()?;
            tx.execute_batch_bounded(
                &format!("UPDATE meta SET value = '{baseline}' WHERE key = 'config_version'"),
                65_536,
            )?;
            tx.commit()
        }))
        .unwrap();

        assert!(
            tokio_test_block_on(bot.poll_config_version()).unwrap(),
            "a new revision must invalidate even when the restored counter reuses the old value"
        );
        assert_eq!(
            tokio_test_block_on(bot.configs.get(&bot.db, guild_id))
                .unwrap()
                .settings
                .prefix,
            "aba-visible"
        );

        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(format!("{}-wal", tmp.display()));
        let _ = std::fs::remove_file(format!("{}-shm", tmp.display()));
        crate::tenant::remove_sidecars(&tmp);
    }

    #[test]
    fn slash_revision_forces_sync_when_counter_is_reused() {
        let tmp = std::env::temp_dir().join(format!(
            "jmodguard_slash_revision_aba_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let db = Db::open_with(DbOpenOptions {
            read_only: false,
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: true,
            default_prefix: "!".into(),
            retention_days: 7,
        })
        .unwrap();
        let app = BotState::new(BotProcessConfig::default(), db).unwrap();
        let versions = db::get_primary_versions_sync(&app.db).unwrap();
        let before_write = app.db.config_write_generation();
        let replacement = "replacement-slash-revision".to_string();
        let mut restored = versions.clone();
        restored.slash_revision = replacement.clone();

        assert_eq!(
            app.reconcile_primary_versions(restored, versions.config, before_write,),
            (false, true)
        );
        assert_eq!(app.slash_registry_revision(), replacement);
        assert!(app.force_slash_registry_retry.load(Ordering::SeqCst));

        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(format!("{}-wal", tmp.display()));
        let _ = std::fs::remove_file(format!("{}-shm", tmp.display()));
        crate::tenant::remove_sidecars(&tmp);
    }

    #[test]
    fn pending_slash_registry_rebaselines_only_after_primary_confirmation() {
        let tmp = std::env::temp_dir().join(format!(
            "jmodguard_slash_pending_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let db = Db::open_with(DbOpenOptions {
            read_only: false,
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: true,
            default_prefix: "!".into(),
            retention_days: 7,
        })
        .unwrap();
        let app = BotState::new(BotProcessConfig::default(), db).unwrap();
        let initial_slash = app.acknowledged_slash_registry_version();
        let initial_config = app.last_config_version.load(Ordering::SeqCst);
        assert_eq!(
            tokio_test_block_on(app.pending_slash_registry_version()).unwrap(),
            None
        );
        let cmd = crate::defaults::custom_command_fixture("slashpending");
        tokio_test_block_on(db::upsert_command(&app.db, 1, "slashpending", &cmd)).unwrap();
        let pending = tokio_test_block_on(app.pending_slash_registry_version())
            .unwrap()
            .expect("command write must surface pending slash version");
        assert_eq!(pending, initial_slash + 1);
        // Peek again — must still be pending (no auto-advance).
        assert_eq!(
            tokio_test_block_on(app.pending_slash_registry_version()).unwrap(),
            Some(initial_slash + 1)
        );
        app.commit_slash_registry_version(pending);
        assert_eq!(
            tokio_test_block_on(app.pending_slash_registry_version()).unwrap(),
            None
        );
        app.commit_slash_registry_version(10);
        assert_eq!(
            tokio_test_block_on(app.pending_slash_registry_version()).unwrap(),
            Some(initial_slash + 1),
            "an immediate primary transaction confirms an authoritative rollback"
        );
        app.last_config_version.store(10, Ordering::SeqCst);
        app.configs.require_version(10);
        assert!(tokio_test_block_on(app.poll_config_version()).unwrap());
        assert_eq!(
            app.last_config_version.load(Ordering::SeqCst),
            initial_config + 1
        );
        tokio_test_block_on(app.configs.get(&app.db, 1)).expect("cache floor must rebaseline");
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(format!("{}-wal", tmp.display()));
        let _ = std::fs::remove_file(format!("{}-shm", tmp.display()));
        crate::tenant::remove_sidecars(&tmp);
    }

    #[test]
    fn primary_rebaseline_rejects_snapshot_across_concurrent_local_write() {
        let tmp = std::env::temp_dir().join(format!(
            "jmodguard_rebaseline_generation_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let db = Db::open_with(DbOpenOptions {
            read_only: false,
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: true,
            default_prefix: "!".into(),
            retention_days: 7,
        })
        .unwrap();
        let app = BotState::new(BotProcessConfig::default(), db).unwrap();
        app.db.require_config_version(11);
        app.configs.require_version(11);
        app.last_config_version.store(11, Ordering::SeqCst);
        let before_write = app.db.config_write_generation();

        tokio_test_block_on(db::patch_settings(
            &app.db,
            77,
            Some(Some("reused-version-write".into())),
            None,
            None,
        ))
        .unwrap();

        assert_eq!(
            app.reconcile_primary_versions(
                db::VersionState {
                    config: 9,
                    slash: 9,
                    config_revision: "rollback-config".into(),
                    slash_revision: "rollback-slash".into(),
                },
                11,
                before_write,
            ),
            (false, false),
            "a primary snapshot taken before a concurrent local commit must be discarded"
        );
        assert_eq!(app.db.required_config_version(), 11);
        assert_eq!(app.last_config_version.load(Ordering::SeqCst), 11);

        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(format!("{}-wal", tmp.display()));
        let _ = std::fs::remove_file(format!("{}-shm", tmp.display()));
        crate::tenant::remove_sidecars(&tmp);
    }

    #[test]
    fn slash_resync_retries_only_failures_and_defers_empty_cache() {
        let tmp = std::env::temp_dir().join(format!(
            "jmodguard_slash_plan_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let db = Db::open_with(DbOpenOptions {
            read_only: false,
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: true,
            default_prefix: "!".into(),
            retention_days: 7,
        })
        .unwrap();
        let app = BotState::new(BotProcessConfig::default(), db).unwrap();
        let remote = 7u64;

        assert_eq!(
            app.plan_slash_resync(remote, &[]),
            SlashResyncPlan::Defer,
            "empty cache must not invalidate or sync"
        );

        match app.plan_slash_resync(remote, &[10, 20, 30]) {
            SlashResyncPlan::Sync { guilds, invalidate } => {
                assert_eq!(guilds, vec![10, 20, 30]);
                assert!(invalidate, "first non-empty pass invalidates once");
            }
            other => panic!("expected Sync, got {other:?}"),
        }
        // Second plan before finish — still full set, but no second invalidate.
        match app.plan_slash_resync(remote, &[10, 20, 30]) {
            SlashResyncPlan::Sync { guilds, invalidate } => {
                assert_eq!(guilds, vec![10, 20, 30]);
                assert!(!invalidate);
            }
            other => panic!("expected Sync, got {other:?}"),
        }

        // A concurrent guild-create failure requests a fresh pass while this
        // one is in flight. The older successful pass must not erase it.
        app.retry_slash_registry_version();
        assert!(!app.finish_slash_resync(remote, HashSet::new()));
        match app.plan_slash_resync(remote, &[10, 20, 30, 40]) {
            SlashResyncPlan::Sync { guilds, .. } => assert_eq!(guilds, vec![10, 20, 30, 40]),
            other => panic!("expected fresh retry plan, got {other:?}"),
        }

        assert!(!app.finish_slash_resync(remote, [20u64].into_iter().collect()));
        match app.plan_slash_resync(remote, &[10, 20, 30]) {
            SlashResyncPlan::Sync { guilds, invalidate } => {
                assert_eq!(guilds, vec![20], "only failures are retried");
                assert!(!invalidate);
            }
            other => panic!("expected Sync, got {other:?}"),
        }

        // A temporarily unavailable guild must keep the durable version
        // pending; otherwise its slash registry is never retried on return.
        assert_eq!(
            app.plan_slash_resync(remote, &[10, 30]),
            SlashResyncPlan::Defer
        );
        assert_eq!(
            app.plan_slash_resync(remote, &[10, 20, 30]),
            SlashResyncPlan::Sync {
                guilds: vec![20],
                invalidate: false,
            }
        );
        match app.plan_slash_resync(remote, &[10, 20, 30]) {
            SlashResyncPlan::Sync { guilds, invalidate } => {
                assert_eq!(guilds, vec![20]);
                assert!(!invalidate);
            }
            other => panic!("expected retry after guild returns, got {other:?}"),
        }

        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(format!("{}-wal", tmp.display()));
        let _ = std::fs::remove_file(format!("{}-shm", tmp.display()));
        crate::tenant::remove_sidecars(&tmp);
    }
}
