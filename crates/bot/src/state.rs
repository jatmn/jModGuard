//! Bot runtime state and compiled guild configuration cache.

use anyhow::{Context as _, Result};
use dashmap::DashMap;
use parking_lot::Mutex;
use regex::Regex;
use serenity::model::id::ChannelId;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
const MAX_GATEWAY_CASE_SUPPRESSIONS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct GatewayCaseSuppression(u128);

/// Ownership captured once for a Gateway ban, before asynchronous work.
pub(crate) struct GatewayBanEcho {
    pub(crate) suppress_case: bool,
    pub(crate) owned_by_bot: bool,
    pub(crate) operation_id: Option<String>,
}

struct GatewayCaseEntry {
    action: GatewayCaseAction,
    guild: u64,
    user: u64,
    operation_id: Option<String>,
    expires: Instant,
}

#[derive(Default)]
struct GatewayCaseSuppressions {
    next_token: u128,
    entries: HashMap<GatewayCaseSuppression, GatewayCaseEntry>,
    /// Ban tokens already consumed. A second delivery of the same bot echo
    /// has no token left, but it is still the bot's ban until this expires
    /// or an unban clears it. The string is that attempt's operation id.
    recent_ban_echo: HashMap<(u64, u64), (Instant, Option<String>)>,
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
            .retain(|_, (expires, _)| *expires > now);
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
        let token = GatewayCaseSuppression(self.next_token);
        self.entries.insert(
            token,
            GatewayCaseEntry {
                action,
                guild,
                user,
                operation_id,
                expires: now + GATEWAY_CASE_SUPPRESSION_TTL,
            },
        );
        token
    }

    fn release(&mut self, token: GatewayCaseSuppression) {
        self.prune(Instant::now());
        self.entries.remove(&token);
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
                (
                    Instant::now() + GATEWAY_CASE_SUPPRESSION_TTL,
                    entry.operation_id,
                ),
            );
        }
        true
    }

    /// `Some` when a consumed bot ban echo is still remembered.
    /// The string is that attempt's operation id, when the dispatch saved one.
    fn ban_echo_operation(&mut self, guild: u64, user: u64) -> Option<Option<String>> {
        let now = Instant::now();
        self.prune(now);
        self.recent_ban_echo
            .get(&(guild, user))
            .map(|(_, operation_id)| operation_id.clone())
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
    pub text_spam: crate::text_spam::Detector,
    pub spam_gate: crate::spam_actions::Gate,
    pub log_snippets: crate::logging::SnippetCache,
    pub snapshots: crate::snapshot_capture::SnapshotIngress,
    pub edit_reports: crate::logging::EditReportDedup,
    pub member_joins: crate::logging::MemberJoinContext,
    pub member_audits: crate::logging::MemberAuditClaims,
    pub cooldowns: DashMap<(u64, u64, String), Instant>,
    pub log_delivery_slots: tokio::sync::Semaphore,
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
    gateway_case_suppressions: Mutex<GatewayCaseSuppressions>,
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
    ban_observations: Mutex<HashMap<(u64, u64), usize>>,
    /// Shards that have already delivered READY in this process.
    ready_shards: Mutex<HashSet<u32>>,
    /// Test seam. Production reads [`Self::discord_cache`] instead.
    control_actor_lookup: Mutex<Option<Arc<ControlActorLookup>>>,
    member_lookup: Mutex<Option<Arc<MemberLookup>>>,
    discord_cache: Mutex<Option<Arc<serenity::cache::Cache>>>,
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
        let tenant =
            crate::tenant::TenantRuntime::open(&db, &cfg.db, &cfg.guild_stores, &cfg.evidence)?;
        let log_delivery_slots = tokio::sync::Semaphore::new(cfg.log.discord_concurrency);
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
            text_spam: crate::text_spam::Detector::default(),
            spam_gate: crate::spam_actions::Gate::default(),
            log_snippets: crate::logging::SnippetCache::default(),
            snapshots: crate::snapshot_capture::SnapshotIngress::default(),
            edit_reports: crate::logging::EditReportDedup::default(),
            member_joins: crate::logging::MemberJoinContext::default(),
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
            gateway_case_suppressions: Mutex::new(GatewayCaseSuppressions::default()),
            pending_audit_links: Mutex::new(PendingAuditLinks::default()),
            pending_member_gaps: Mutex::new(PendingMemberGaps::default()),
            gateway_ready_generation: AtomicU64::new(0),
            member_scope_epoch: AtomicU64::new(0),
            member_sync_wake: tokio::sync::Notify::new(),
            ban_observations: Mutex::new(HashMap::new()),
            ready_shards: Mutex::new(HashSet::new()),
            control_actor_lookup: Mutex::new(None),
            member_lookup: Mutex::new(None),
            discord_cache: Mutex::new(None),
        }))
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

    pub(crate) fn consume_gateway_ban_echo(&self, guild: u64, user: u64) -> GatewayBanEcho {
        let mut suppressions = self.gateway_case_suppressions.lock();
        let suppress_case = suppressions.remove(GatewayCaseAction::Ban, guild, user);
        let operation = suppressions.ban_echo_operation(guild, user);
        GatewayBanEcho {
            suppress_case,
            owned_by_bot: suppress_case || operation.is_some(),
            operation_id: operation.flatten(),
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

    /// Returns whether another ban or unban callback for this user is already
    /// inside its handler. Local start time is not Gateway order.
    pub(crate) fn begin_ban_observation(&self, guild_id: u64, user_id: u64) -> bool {
        let mut observations = self.ban_observations.lock();
        let owners = observations.entry((guild_id, user_id)).or_default();
        let contested = *owners > 0;
        *owners = owners
            .checked_add(1)
            .expect("ban observation owners overflow");
        contested
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
            GatewayCaseSuppression(0),
            GatewayCaseEntry {
                action: GatewayCaseAction::Ban,
                guild: 30,
                user: 40,
                operation_id: None,
                expires: Instant::now() - Duration::from_secs(1),
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
        s.release(old);
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
        let cmd = crate::defaults::default_commands()
            .into_iter()
            .next()
            .expect("defaults");
        tokio_test_block_on(db::upsert_command(&app.db, 1, &cmd.name, &cmd)).unwrap();
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
