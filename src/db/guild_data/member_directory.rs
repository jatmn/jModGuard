//! Scoped, persistent member observations and one-time initial import metadata.
pub(super) const MEMBER_CLEANUP_CANDIDATE_SQL: &str = "SELECT d.user_id,d.revision,d.cleanup_member_checked_at_ms,d.departed_at_ms FROM guild_member_directory d WHERE d.guild_id=?1 AND d.enrollment_id=?2 AND d.is_current=0 AND d.is_banned=0 AND d.ever_banned=0 AND d.cleanup_after_ms<=?3 AND (?4=1 OR coalesce(d.cleanup_member_checked_at_ms,-1)<max(coalesce(?5,0),coalesce(d.departed_at_ms,0))) AND NOT EXISTS(SELECT 1 FROM guild_member_pending_bans p WHERE p.guild_id=d.guild_id AND p.enrollment_id=d.enrollment_id AND p.user_id=d.user_id) AND NOT EXISTS(SELECT 1 FROM guild_member_target_checks t WHERE t.guild_id=d.guild_id AND t.enrollment_id=d.enrollment_id AND t.user_id=d.user_id) ORDER BY d.cleanup_after_ms,d.user_id LIMIT 1";

use super::tenant_logs::verify_scope;
use super::*;
use crate::db::write_budget::MemberSqlWrites;
use anyhow::bail;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
#[cfg(test)]
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

#[cfg(test)]
type CoverageGapHook = (std::path::PathBuf, Box<dyn Fn() + Send + Sync>);

#[cfg(test)]
static COVERAGE_GAP_AFTER_SCOPE_VERIFY: Mutex<Option<CoverageGapHook>> = Mutex::new(None);

#[cfg(test)]
static COVERAGE_GAP_AFTER_REVISION_SNAPSHOT: Mutex<Option<CoverageGapHook>> = Mutex::new(None);

#[cfg(test)]
static COVERAGE_GAP_AFTER_RECEIPT_ADMISSION: Mutex<Option<CoverageGapHook>> = Mutex::new(None);

#[cfg(test)]
static COVERAGE_GAP_AFTER_WRITER_FENCE: Mutex<Option<CoverageGapHook>> = Mutex::new(None);

#[cfg(test)]
static COVERAGE_GAP_AFTER_BATCH: Mutex<Option<CoverageGapHook>> = Mutex::new(None);

#[cfg(test)]
fn set_coverage_gap_hook(
    slot: &Mutex<Option<CoverageGapHook>>,
    database: &std::path::Path,
    hook: Option<Box<dyn Fn() + Send + Sync>>,
) {
    *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
        hook.map(|hook| (database.to_path_buf(), hook));
}

#[cfg(test)]
fn run_coverage_gap_hook(slot: &Mutex<Option<CoverageGapHook>>, database: &std::path::Path) {
    let guard = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((path, hook)) = guard.as_ref()
        && path == database
    {
        hook();
    }
}

#[cfg(test)]
static COVERAGE_GAP_HOOK_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
pub(crate) async fn test_coverage_gap_hook_lock() -> tokio::sync::MutexGuard<'static, ()> {
    COVERAGE_GAP_HOOK_MUTEX.lock().await
}

#[cfg(test)]
pub(crate) fn test_set_coverage_gap_after_scope_verify(
    database: &std::path::Path,
    hook: Option<Box<dyn Fn() + Send + Sync>>,
) {
    set_coverage_gap_hook(&COVERAGE_GAP_AFTER_SCOPE_VERIFY, database, hook);
}

#[cfg(test)]
pub(crate) fn test_set_coverage_gap_after_revision_snapshot(
    database: &std::path::Path,
    hook: Option<Box<dyn Fn() + Send + Sync>>,
) {
    set_coverage_gap_hook(&COVERAGE_GAP_AFTER_REVISION_SNAPSHOT, database, hook);
}

#[cfg(test)]
pub(crate) fn test_set_coverage_gap_after_receipt_admission(
    database: &std::path::Path,
    hook: Option<Box<dyn Fn() + Send + Sync>>,
) {
    set_coverage_gap_hook(&COVERAGE_GAP_AFTER_RECEIPT_ADMISSION, database, hook);
}

#[cfg(test)]
pub(crate) fn test_set_coverage_gap_after_writer_fence(
    database: &std::path::Path,
    hook: Option<Box<dyn Fn() + Send + Sync>>,
) {
    set_coverage_gap_hook(&COVERAGE_GAP_AFTER_WRITER_FENCE, database, hook);
}

#[cfg(test)]
pub(crate) fn test_set_coverage_gap_after_batch(
    database: &std::path::Path,
    hook: Option<Box<dyn Fn() + Send + Sync>>,
) {
    set_coverage_gap_hook(&COVERAGE_GAP_AFTER_BATCH, database, hook);
}

#[cfg(test)]
fn run_coverage_gap_after_scope_verify_hook(database: &std::path::Path) {
    run_coverage_gap_hook(&COVERAGE_GAP_AFTER_SCOPE_VERIFY, database);
}

#[cfg(test)]
fn run_coverage_gap_after_revision_snapshot_hook(database: &std::path::Path) {
    run_coverage_gap_hook(&COVERAGE_GAP_AFTER_REVISION_SNAPSHOT, database);
}

#[cfg(not(test))]
fn run_coverage_gap_after_scope_verify_hook(_database: &std::path::Path) {}

#[cfg(test)]
fn run_coverage_gap_after_receipt_admission_hook(database: &std::path::Path) {
    run_coverage_gap_hook(&COVERAGE_GAP_AFTER_RECEIPT_ADMISSION, database);
}

#[cfg(not(test))]
fn run_coverage_gap_after_revision_snapshot_hook(_database: &std::path::Path) {}

#[cfg(not(test))]
fn run_coverage_gap_after_receipt_admission_hook(_database: &std::path::Path) {}

#[cfg(test)]
fn run_coverage_gap_after_writer_fence_hook(database: &std::path::Path) {
    run_coverage_gap_hook(&COVERAGE_GAP_AFTER_WRITER_FENCE, database);
}

#[cfg(not(test))]
fn run_coverage_gap_after_writer_fence_hook(_database: &std::path::Path) {}

#[cfg(test)]
fn run_coverage_gap_after_batch_hook(database: &std::path::Path) {
    run_coverage_gap_hook(&COVERAGE_GAP_AFTER_BATCH, database);
}

#[cfg(not(test))]
fn run_coverage_gap_after_batch_hook(_database: &std::path::Path) {}

/// Guild whose member-sync statements are being counted. Unset until a test asks.
static MEMBER_SYNC_WATCHED_GUILD: AtomicI64 = AtomicI64::new(i64::MIN);
static MEMBER_SYNC_WATCHED_STATEMENTS: AtomicU64 = AtomicU64::new(0);

/// Start counting member-sync statements for one guild. One watcher per process.
pub fn begin_member_sync_statement_count(guild_id: i64) {
    MEMBER_SYNC_WATCHED_GUILD.store(guild_id, Ordering::Release);
    MEMBER_SYNC_WATCHED_STATEMENTS.store(0, Ordering::Release);
}

/// Statements noted for `guild_id` since [`begin_member_sync_statement_count`].
pub fn member_sync_statement_count(guild_id: i64) -> u64 {
    if MEMBER_SYNC_WATCHED_GUILD.load(Ordering::Acquire) != guild_id {
        return 0;
    }
    MEMBER_SYNC_WATCHED_STATEMENTS.load(Ordering::Acquire)
}

fn note_member_sync_statement(guild_id: i64) {
    if MEMBER_SYNC_WATCHED_GUILD.load(Ordering::Acquire) == guild_id {
        MEMBER_SYNC_WATCHED_STATEMENTS.fetch_add(1, Ordering::Release);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberObservation {
    pub user_id: i64,
    /// Outer None means the event omitted this field; Some(None) clears it.
    pub username: Option<Option<String>>,
    pub global_name: Option<Option<String>>,
    pub nickname: Option<Option<String>>,
    pub joined_at_ms: Option<i64>,
    pub observed_at_ms: i64,
    pub is_join: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberBanSnapshot {
    pub user_id: i64,
    pub username: Option<String>,
    pub global_name: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberAuditAction {
    Ban,
    Unban,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberAuditSnapshot {
    pub entry_id: i64,
    pub user_id: i64,
    pub executor_id: Option<i64>,
    pub action: MemberAuditAction,
    pub occurred_at_ms: i64,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemberDeparture {
    pub user_id: i64,
    pub joined_at_ms: Option<i64>,
    pub departed_at_ms: i64,
    pub completed_stay_ms: Option<i64>,
}

#[derive(Debug, Clone)]
pub enum MemberDepartureDecision {
    Applied(Option<MemberDeparture>),
    NeedsVerification,
}

#[derive(Debug, Clone)]
pub struct MemberTargetCheck {
    pub user_id: i64,
    pub kind: MemberVerifyKind,
    /// -1 denotes that no directory row existed when the check was queued.
    pub revision: i64,
    pub reason: String,
    /// Queue time for most reasons. For `ambiguous_departure` member checks this is the
    /// gateway remove `departed_at_ms` captured when the check was created.
    pub requested_at_ms: i64,
    pub expected_stay_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MemberCleanupCandidate {
    pub user_id: i64,
    pub revision: i64,
}

#[derive(Debug, Clone)]
pub struct MemberPendingBanCandidate {
    pub user_id: i64,
    pub operation_id: String,
    pub started_at_ms: i64,
    pub revision: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberStatusFilter {
    Current,
    Banned,
    PreviouslyBanned,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberSort {
    JoinedNewest,
    JoinedOldest,
}

#[derive(Debug, Clone)]
pub struct MemberQuery {
    pub q: Option<String>,
    pub status: MemberStatusFilter,
    pub joined_from_ms: Option<i64>,
    pub joined_to_ms: Option<i64>,
    pub sort: MemberSort,
    pub after: Option<String>,
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemberProfile {
    pub user_id: String,
    pub display_name: String,
    pub username: Option<String>,
    pub global_name: Option<String>,
    pub nickname: Option<String>,
    pub is_current: bool,
    pub is_banned: bool,
    pub ever_banned: bool,
    pub current_ban_reason: Option<String>,
    pub joined_at_ms: Option<i64>,
    pub departed_at_ms: Option<i64>,
    pub observed_at_ms: i64,
    pub revision: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemberPage {
    pub members: Vec<MemberProfile>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemberHistoryEntry {
    pub kind: String,
    pub occurred_at_ms: Option<i64>,
    /// Time at which an unknown-time state was verified, never event time.
    pub observed_at_ms: Option<i64>,
    pub stay_id: Option<String>,
    pub observation_id: Option<String>,
    pub case_id: Option<String>,
    pub joined_at_ms: Option<i64>,
    pub departed_at_ms: Option<i64>,
    pub moderator_id: Option<String>,
    pub reason: Option<String>,
    pub outcome: Option<String>,
    pub source_kind: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemberHistoryPage {
    pub entries: Vec<MemberHistoryEntry>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberSyncPhase {
    Members,
    Bans,
    Verify,
    Audit,
    Complete,
}
impl MemberSyncPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Members => "members",
            Self::Bans => "bans",
            Self::Verify => "verify",
            Self::Audit => "audit",
            Self::Complete => "complete",
        }
    }
    fn parse(s: &str) -> Result<Self> {
        match s {
            "members" => Ok(Self::Members),
            "bans" => Ok(Self::Bans),
            "verify" => Ok(Self::Verify),
            "audit" => Ok(Self::Audit),
            "complete" => Ok(Self::Complete),
            _ => bail!("invalid member sync phase"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberVerifyKind {
    Member,
    Ban,
}
impl MemberVerifyKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Member => "member",
            Self::Ban => "ban",
        }
    }
    fn parse(value: &str) -> Result<Self> {
        match value {
            "member" => Ok(Self::Member),
            "ban" => Ok(Self::Ban),
            _ => bail!("invalid member target check"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemberVerification {
    pub user_id: i64,
    pub revision: i64,
    pub kind: MemberVerifyKind,
    pub present: bool,
    pub member: Option<MemberObservation>,
    pub ban: Option<MemberBanSnapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberSyncAction {
    Pause,
    Resume,
    Retry,
    Reconcile,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemberSyncStatus {
    pub initial_completed_at: Option<i64>,
    pub reconciliation_needed: bool,
    pub reconciliation_reason: Option<String>,
    pub audit_recovery_pending: bool,
    pub audit_recovery_status: Option<String>,
    pub audit_recovery_reason: Option<String>,
    pub audit_recovery_phase: Option<String>,
    pub audit_recovery_processed: i64,
    pub audit_recovery_next_attempt_at: Option<i64>,
    pub audit_recovery_last_progress_at: Option<i64>,
    pub run_id: Option<String>,
    pub run_kind: Option<String>,
    pub phase: Option<String>,
    pub status: String,
    pub status_reason: Option<String>,
    pub progress_revision: i64,
    pub processed: i64,
    pub member_processed: Option<i64>,
    pub ban_processed: Option<i64>,
    pub verify_processed: Option<i64>,
    pub audit_processed: Option<i64>,
    pub local_history_complete: bool,
    pub inserted: i64,
    pub updated: i64,
    pub unchanged: i64,
    pub total_estimate: Option<i64>,
    pub total_is_estimated: bool,
    pub percent: Option<f64>,
    pub started_at: Option<i64>,
    pub last_progress_at: Option<i64>,
    pub as_of: i64,
    pub next_attempt_at: Option<i64>,
    pub phase_remaining_seconds: Option<i64>,
    pub phase_finish_at: Option<i64>,
    pub overall_remaining_seconds: Option<i64>,
    pub overall_finish_at: Option<i64>,
    pub estimate_reason: Option<String>,
    pub last_success_at: Option<i64>,
    pub last_success_processed: Option<i64>,
    pub last_success_inserted: Option<i64>,
    pub last_success_updated: Option<i64>,
    pub last_success_unchanged: Option<i64>,
    pub unseen_candidate_count: i64,
}

impl MemberSyncStatus {
    pub fn audit_pagination_stalled(&self) -> bool {
        self.audit_recovery_status.as_deref() == Some("needs_attention")
            && self.audit_recovery_reason.as_deref() == Some("pagination_stalled")
    }

    pub fn allows_member_sync_retry(&self) -> bool {
        matches!(self.status.as_str(), "needs_attention" | "waiting")
            || (self.status == "complete" && self.audit_pagination_stalled())
    }

    /// True when a Retry on a completed import re-queued stalled audit recovery.
    pub fn member_sync_retry_requeued_audit(&self) -> bool {
        self.status == "complete" && self.audit_recovery_status.as_deref() == Some("queued")
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MemberInitialEstimate {
    pub member_count: i64,
    pub page_size: u16,
    pub pace_ms: i64,
    pub active_guild_count: u32,
    pub now_ms: i64,
}

#[derive(Debug, Clone)]
pub struct MemberSyncClaim {
    pub bot_id: i64,
    pub run_id: String,
    pub phase: MemberSyncPhase,
    pub cursor: Option<String>,
    pub directory_revision: i64,
    pub generation: i64,
    pub worker_id: String,
    pub verify_user_id: Option<i64>,
    pub verify_revision: Option<i64>,
    pub verify_kind: Option<MemberVerifyKind>,
    pub not_before_ms: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct MemberAuditRecoveryClaim {
    pub bot_id: i64,
    pub run_id: String,
    pub cursor: Option<String>,
    pub generation: i64,
    pub worker_id: String,
    pub not_before_ms: i64,
}

#[derive(Debug, Clone)]
pub struct MemberSyncPage {
    pub phase: MemberSyncPhase,
    pub expected_cursor: Option<String>,
    pub next_cursor: Option<String>,
    pub members: Vec<MemberObservation>,
    pub bans: Vec<MemberBanSnapshot>,
    pub audits: Vec<MemberAuditSnapshot>,
    pub terminal: bool,
    pub total_estimate: Option<i64>,
    pub page_size: u16,
    pub pace_ms: i64,
    pub active_guild_count: u32,
    pub verification: Option<MemberVerification>,
}

/// Whether a membership observation opened a new stay or first profile row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemberUpsertOutcome {
    pub membership_refresh: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum MemberSyncError {
    #[error("member import checkpoint or worker claim changed")]
    Stale,
    #[error("member import belongs to another bot identity")]
    BotIdentity,
    #[error("invalid member directory request")]
    Invalid,
    #[error("member import job is missing; operator attention is required")]
    MissingJob,
}

fn checked_scope(store: &GuildDataStore, scope: &GuildDataScope) -> Result<()> {
    store.check_target(&scope.placement)
}

fn profile(row: &Row) -> Result<MemberProfile> {
    let id: i64 = row.get(0)?;
    let username: Option<String> = row.get(1)?;
    let global_name: Option<String> = row.get(2)?;
    let nickname: Option<String> = row.get(3)?;
    Ok(MemberProfile {
        user_id: id.to_string(),
        display_name: nickname
            .clone()
            .or(global_name.clone())
            .or(username.clone())
            .unwrap_or_else(|| id.to_string()),
        username,
        global_name,
        nickname,
        is_current: row.get(4)?,
        is_banned: row.get(5)?,
        ever_banned: row.get(6)?,
        current_ban_reason: row.get(11)?,
        joined_at_ms: row.get(7)?,
        departed_at_ms: row.get(8)?,
        observed_at_ms: row.get(9)?,
        revision: row.get(10)?,
    })
}

const PROFILE_SQL: &str = "user_id,username,global_name,nickname,is_current,is_banned,ever_banned,joined_at_ms,departed_at_ms,observed_at_ms,revision,current_ban_reason";

fn normalized_name(
    username: &Option<String>,
    global: &Option<String>,
    nick: &Option<String>,
) -> String {
    format!(
        "{} {} {}",
        username.as_deref().unwrap_or(""),
        global.as_deref().unwrap_or(""),
        nick.as_deref().unwrap_or("")
    )
    .to_lowercase()
}

fn member_search_user_id(raw: &str) -> Option<i64> {
    let token = raw
        .strip_prefix("<@")
        .and_then(|v| v.strip_suffix('>'))
        .map(|v| v.trim_start_matches('!'))
        .unwrap_or(raw)
        .trim();
    if token.is_empty() {
        return None;
    }
    if token.chars().all(|c| c.is_ascii_digit()) {
        return token.parse::<i64>().ok().filter(|id| *id > 0);
    }
    None
}

fn bounded_name(name: Option<String>) -> Result<Option<String>> {
    if let Some(value) = &name {
        anyhow::ensure!(value.len() <= 256, MemberSyncError::Invalid);
    }
    Ok(name)
}

fn update_ban_identity(
    conn: &impl SqlExecutor,
    scope: &GuildDataScope,
    user_id: i64,
    username: &str,
    global_name: &Option<String>,
) -> Result<()> {
    let nickname: Option<String> = conn.query_row(
        "SELECT nickname FROM guild_member_directory WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",
        params![scope.guild_id(), scope.enrollment.id.as_str(), user_id],
        |r| Ok(r.get(0)?),
    )?;
    let search_text = normalized_name(&Some(username.to_owned()), global_name, &nickname);
    conn.member_execute(
        "UPDATE guild_member_directory SET username=?4,global_name=?5,search_text=?6 WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",
        params![scope.guild_id(), scope.enrollment.id.as_str(), user_id, username, global_name.clone(), search_text],
    )?;
    Ok(())
}

fn random_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}

fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

const COVERAGE_GAP_MARK_GROWTH: u64 = 32 * 1024 * 1024;

// Stable batch error markers are shared by SQL generation and classification;
// provider error wrappers and user-facing error wording are not the contract.
const COVERAGE_GAP_RESTORE_ERROR: &str = "jmodguard_coverage_gap_restore";
const COVERAGE_GAP_MEMBERSHIP_ERROR: &str = "jmodguard_coverage_gap_membership";
const COVERAGE_GAP_LEASE_ERROR: &str = "jmodguard_coverage_gap_lease";

fn coverage_gap_receipt_admission(conn: &impl SqlExecutor) -> Result<bool> {
    // A single autocommit query covers both table presence and the fence. A
    // missing table is expected on guild stores; other errors fail admission.
    match conn
        .query_row(
            "SELECT value FROM meta WHERE key='restore_fence'",
            (),
            |row| Ok(row.get::<String>(0)?),
        )
        .optional()
    {
        Ok(value) => {
            anyhow::ensure!(value.as_deref() != Some("1"), "restore is in progress");
            Ok(true)
        }
        Err(error)
            if error
                .chain()
                .any(|cause| cause.to_string().contains("no such table: meta")) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn coverage_gap_batch_error(error: anyhow::Error) -> anyhow::Error {
    for cause in error.chain() {
        let message = cause.to_string();
        if message.contains(COVERAGE_GAP_RESTORE_ERROR) {
            return anyhow::anyhow!("restore is in progress");
        }
        if message.contains(COVERAGE_GAP_MEMBERSHIP_ERROR) {
            return TenantDataError::Membership.into();
        }
        if message.contains(COVERAGE_GAP_LEASE_ERROR) {
            return crate::db_compat::ActiveBotLeaseRejected.into();
        }
    }
    error
}

fn coverage_gap_read_epoch(conn: &impl SqlExecutor) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT generation FROM guild_writer_epoch WHERE singleton=1",
        (),
        |row| Ok(row.get(0)?),
    )
    .optional()
}

/// Fail-fast lease and epoch admission before `BEGIN IMMEDIATE`.
///
/// These reads do not authorize the commit. The generation is what the batch
/// must still see. The owner, when one was admitted, is the only owner commit
/// may accept.
fn coverage_gap_writer_prelude(
    core: Option<&Db>,
    conn: &crate::db_compat::Connection,
) -> Result<(Option<i64>, Option<String>)> {
    let mut admitted_owner = None;
    if let Some(core) = core
        && let Some(owner) = core.leased_owner()
    {
        GuildDataStore::read_core_lease(core, &owner)?;
        admitted_owner = Some(owner);
    }
    let generation = conn.expected_writer_generation();
    if let Some(generation) = generation {
        let stored = coverage_gap_read_epoch(conn)?;
        if stored != Some(generation) {
            return Err(crate::db_compat::ActiveBotLeaseRejected.into());
        }
    }
    Ok((generation, admitted_owner))
}

fn coverage_gap_writer_fence_sql(generation: Option<i64>) -> String {
    let Some(generation) = generation else {
        return String::new();
    };
    format!(
        "SELECT CASE WHEN (SELECT generation FROM guild_writer_epoch WHERE singleton=1)={generation} THEN 1 ELSE json_set('{{}}', '{COVERAGE_GAP_LEASE_ERROR}', 1) END;"
    )
}

fn ensure_seed_bot(
    conn: &impl SqlExecutor,
    scope: &GuildDataScope,
    bot_id: i64,
) -> Result<Option<Option<i64>>> {
    note_member_sync_statement(scope.guild_id());
    let row = conn.query_row("SELECT enrollment_id,bot_id,initial_completed_at_ms FROM guild_member_seed WHERE guild_id=?1", params![scope.guild_id()], |r| Ok((r.get::<String>(0)?,r.get::<i64>(1)?,r.get::<Option<i64>>(2)?))).optional()?;
    match row {
        Some((enrollment, stored_bot, completed)) => {
            anyhow::ensure!(
                enrollment == scope.enrollment.id,
                TenantDataError::Membership
            );
            anyhow::ensure!(stored_bot == bot_id, MemberSyncError::BotIdentity);
            Ok(Some(completed))
        }
        None => Ok(None),
    }
}

fn status(
    conn: &impl SqlExecutor,
    scope: &GuildDataScope,
    bot_id: i64,
    now_ms: i64,
    count_unseen: bool,
) -> Result<MemberSyncStatus> {
    let seed = ensure_seed_bot(conn, scope, bot_id)?;
    let mut out = MemberSyncStatus {
        initial_completed_at: seed.flatten(),
        reconciliation_needed: false,
        reconciliation_reason: None,
        audit_recovery_pending: false,
        audit_recovery_status: None,
        audit_recovery_reason: None,
        audit_recovery_phase: None,
        audit_recovery_processed: 0,
        audit_recovery_next_attempt_at: None,
        audit_recovery_last_progress_at: None,
        run_id: None,
        run_kind: None,
        phase: None,
        status: "needs_attention".into(),
        status_reason: Some("member import metadata is missing".into()),
        progress_revision: 0,
        processed: 0,
        member_processed: None,
        ban_processed: None,
        verify_processed: None,
        audit_processed: None,
        local_history_complete: false,
        inserted: 0,
        updated: 0,
        unchanged: 0,
        total_estimate: None,
        total_is_estimated: true,
        percent: None,
        started_at: None,
        last_progress_at: None,
        as_of: now_ms,
        next_attempt_at: None,
        phase_remaining_seconds: None,
        phase_finish_at: None,
        overall_remaining_seconds: None,
        overall_finish_at: None,
        estimate_reason: Some("import state requires attention".into()),
        last_success_at: None,
        last_success_processed: None,
        last_success_inserted: None,
        last_success_updated: None,
        last_success_unchanged: None,
        unseen_candidate_count: 0,
    };
    if seed.is_none() {
        note_member_sync_statement(scope.guild_id());
        let eligible:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM guild_member_feature WHERE guild_id=?1 AND enrollment_id=?2 AND seed_admitted=0)",params![scope.guild_id(),scope.enrollment.id.as_str()],|r|Ok(r.get(0)?))?;
        if eligible {
            out.status = "queued".into();
            out.phase = Some("members".into());
            out.run_kind = Some("initial".into());
            out.status_reason = Some("awaiting first bot initialization".into());
            out.estimate_reason = Some("member count estimate is unavailable".into());
        }
        return Ok(out);
    }
    note_member_sync_statement(scope.guild_id());
    let coverage: (bool, Option<String>, bool, Option<i64>) = conn.query_row(
        "SELECT reconciliation_needed,reconciliation_reason,case_backfill_done,gap_at_ms FROM guild_member_seed WHERE guild_id=?1",
        params![scope.guild_id()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    out.reconciliation_needed = coverage.0;
    out.reconciliation_reason = coverage.1;
    out.local_history_complete = coverage.2;
    note_member_sync_statement(scope.guild_id());
    out.audit_recovery_pending = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM guild_member_audit_recovery WHERE guild_id=?1 AND enrollment_id=?2 AND status IN ('queued','running','waiting','needs_attention') AND NOT (status='needs_attention' AND status_reason='pagination_stalled') AND coalesce(next_attempt_at_ms,0)<=?3 AND coalesce(request_after_ms,0)<=?3 AND coalesce(lease_until_ms,0)<=?3)",
        params![scope.guild_id(), scope.enrollment.id.as_str(), now_ms],
        |r| Ok(r.get(0)?),
    )?;
    note_member_sync_statement(scope.guild_id());
    if let Some((state,reason,cursor,processed,next,request_after,last_progress))=conn.query_row("SELECT status,status_reason,cursor,processed,next_attempt_at_ms,request_after_ms,last_progress_at_ms FROM guild_member_audit_recovery WHERE guild_id=?1 AND enrollment_id=?2",params![scope.guild_id(),scope.enrollment.id.as_str()],|r|Ok((r.get::<String>(0)?,r.get::<Option<String>>(1)?,r.get::<Option<String>>(2)?,r.get::<i64>(3)?,r.get::<Option<i64>>(4)?,r.get::<Option<i64>>(5)?,r.get::<Option<i64>>(6)?))).optional()? {
        out.audit_recovery_phase=if state=="complete" {None}else{Some(if cursor.as_deref().is_some_and(|v|v.starts_with("unban:")){"unban"}else{"ban"}.into())};
        out.audit_recovery_status=Some(state);out.audit_recovery_reason=reason;out.audit_recovery_processed=processed;out.audit_recovery_next_attempt_at=next.into_iter().chain(request_after).max();out.audit_recovery_last_progress_at=last_progress;
    }
    struct SyncRow {
        run_id: String,
        kind: String,
        phase: String,
        state: String,
        reason: Option<String>,
        processed: i64,
        inserted: i64,
        updated: i64,
        unchanged: i64,
        total: Option<i64>,
        revision: i64,
        started: Option<i64>,
        progress: Option<i64>,
        next: Option<i64>,
        last_success: Option<i64>,
        page_size: i64,
        pace_ms: i64,
        share: i64,
        lease_until: Option<i64>,
        request_after: Option<i64>,
        member_processed: Option<i64>,
        ban_processed: Option<i64>,
        verify_processed: Option<i64>,
        audit_processed: Option<i64>,
        member_page_interval_ms: Option<i64>,
        last_success_processed: Option<i64>,
        last_success_inserted: Option<i64>,
        last_success_updated: Option<i64>,
        last_success_unchanged: Option<i64>,
    }
    note_member_sync_statement(scope.guild_id());
    let row = conn.query_row(
        "SELECT run_id,run_kind,phase,status,status_reason,processed,inserted,updated,unchanged,total_estimate,progress_revision,started_at_ms,last_progress_at_ms,next_attempt_at_ms,last_success_at_ms,page_size,pace_ms,active_guild_count,lease_until_ms,request_after_ms,member_processed,ban_processed,verify_processed,audit_processed,member_page_interval_ms,last_success_processed,last_success_inserted,last_success_updated,last_success_unchanged FROM guild_member_sync WHERE guild_id=?1 AND enrollment_id=?2 AND bot_id=?3",
        params![scope.guild_id(), scope.enrollment.id.as_str(), bot_id],
        |r| {
            Ok(SyncRow {
                run_id: r.get(0)?,
                kind: r.get(1)?,
                phase: r.get(2)?,
                state: r.get(3)?,
                reason: r.get(4)?,
                processed: r.get(5)?,
                inserted: r.get(6)?,
                updated: r.get(7)?,
                unchanged: r.get(8)?,
                total: r.get(9)?,
                revision: r.get(10)?,
                started: r.get(11)?,
                progress: r.get(12)?,
                next: r.get(13)?,
                last_success: r.get(14)?,
                page_size: r.get(15)?,
                pace_ms: r.get(16)?,
                share: r.get(17)?,
                lease_until: r.get(18)?,
                request_after: r.get(19)?,
                member_processed: r.get(20)?,
                ban_processed: r.get(21)?,
                verify_processed: r.get(22)?,
                audit_processed: r.get(23)?,
                member_page_interval_ms: r.get(24)?,
                last_success_processed: r.get(25)?,
                last_success_inserted: r.get(26)?,
                last_success_updated: r.get(27)?,
                last_success_unchanged: r.get(28)?,
            })
        },
    ).optional()?;
    if let Some(SyncRow {
        run_id,
        kind,
        phase,
        state,
        reason,
        processed,
        inserted,
        updated,
        unchanged,
        total,
        revision,
        started,
        progress,
        next,
        last_success,
        page_size,
        pace_ms,
        share,
        lease_until,
        request_after,
        member_processed,
        ban_processed,
        verify_processed,
        audit_processed,
        member_page_interval_ms,
        last_success_processed,
        last_success_inserted,
        last_success_updated,
        last_success_unchanged,
    }) = row
    {
        out.run_id = Some(run_id);
        out.run_kind = Some(kind.clone());
        out.phase = Some(phase.clone());
        out.status = state.clone();
        out.status_reason = reason;
        out.processed = processed;
        out.member_processed = member_processed.or_else(|| {
            if phase == "members" {
                Some(processed)
            } else {
                total
            }
        });
        out.ban_processed = ban_processed;
        out.verify_processed = verify_processed;
        out.audit_processed = audit_processed;
        out.inserted = inserted;
        out.updated = updated;
        out.unchanged = unchanged;
        out.total_estimate = total;
        // The member stage writes its exact traversed count when it ends. Before
        // then, Discord's guild member count is only a preliminary estimate.
        out.total_is_estimated = phase == "members" || total.is_none();
        out.progress_revision = revision;
        out.started_at = started;
        out.last_progress_at = progress;
        out.next_attempt_at = if state == "complete" {
            None
        } else {
            next.into_iter().chain(request_after).max()
        };
        out.last_success_at = last_success;
        out.last_success_processed = last_success_processed;
        out.last_success_inserted = last_success_inserted;
        out.last_success_updated = last_success_updated;
        out.last_success_unchanged = last_success_unchanged;
        // The stored sync column is not maintained. The Members card counts the
        // same due absence rows the cleanup worker selects. The bot idle loop
        // does not need that count to decide whether work is due.
        if count_unseen {
            let history_ready = out.initial_completed_at.is_some() && coverage.2;
            note_member_sync_statement(scope.guild_id());
            out.unseen_candidate_count = conn.query_row(
                "SELECT COUNT(*) FROM guild_member_directory d WHERE d.guild_id=?1 AND d.enrollment_id=?2 AND d.is_current=0 AND d.is_banned=0 AND d.ever_banned=0 AND d.cleanup_after_ms<=?3 AND (?4=1 OR coalesce(d.cleanup_member_checked_at_ms,-1)<max(coalesce(?5,0),coalesce(d.departed_at_ms,0))) AND NOT EXISTS(SELECT 1 FROM guild_member_pending_bans p WHERE p.guild_id=d.guild_id AND p.enrollment_id=d.enrollment_id AND p.user_id=d.user_id) AND NOT EXISTS(SELECT 1 FROM guild_member_target_checks t WHERE t.guild_id=d.guild_id AND t.enrollment_id=d.enrollment_id AND t.user_id=d.user_id)",
                params![scope.guild_id(), scope.enrollment.id.as_str(), now_ms, i64::from(history_ready), coverage.3],
                |r| Ok(r.get(0)?),
            )?;
        }
        if state == "running" && lease_until.is_some_and(|v| v < now_ms) {
            out.status = "waiting".into();
            out.status_reason = Some("worker unavailable; waiting for a new claim".into());
        }
        if phase == "members"
            && matches!(out.status.as_str(), "queued" | "running" | "waiting")
            && out.status_reason.as_deref() != Some("worker unavailable; waiting for a new claim")
        {
            if let Some(total) = total.filter(|n| *n >= 0) {
                out.percent = Some(if total == 0 {
                    0.0
                } else {
                    ((processed as f64 / total as f64) * 100.0).min(99.0)
                });
                // Use the last committed page as a fixed ETA anchor. A queued
                // interval is normal during global pacing, but it cannot prove
                // indefinite worker liveness. Never slide the finish forward
                // merely because the dashboard asked for fresh status.
                // Measured adjacent committed roster pages include normal
                // competing target/cleanup slots on the same request gate.
                let cycle_ms = pace_ms
                    .saturating_mul(share)
                    .max(member_page_interval_ms.unwrap_or(0));
                let recent_window_ms =
                    60_000_i64.max(cycle_ms.saturating_mul(2).saturating_add(30_000));
                // Before the first page, only a newly bot-initialized initial
                // run with a supplied count has a usable preliminary anchor.
                let anchor_progress = progress.or_else(|| {
                    (kind == "initial" && processed == 0 && total >= 0)
                        .then_some(started)
                        .flatten()
                });
                let recent_progress = anchor_progress.is_some_and(|at| {
                    at <= now_ms && now_ms.saturating_sub(at) <= recent_window_ms
                });
                let pages_remaining = (page_size > 0)
                    .then(|| ((total - processed).max(0) + page_size - 1) / page_size + 1);
                if out.status == "waiting" {
                    out.estimate_reason = Some(
                        match out.status_reason.as_deref() {
                            Some("rate_limited") => "rate_limited",
                            Some("permission_denied") => "permission_denied",
                            Some("discord_unavailable") => "discord_unavailable",
                            Some("worker unavailable; waiting for a new claim") => {
                                "worker_unconfirmed"
                            }
                            _ => "waiting for retry; remaining work cannot be estimated",
                        }
                        .into(),
                    );
                    if out.status_reason.as_deref() == Some("rate_limited")
                        && recent_progress
                        && let Some(pages) = pages_remaining
                        && let Some(deadline) =
                            out.next_attempt_at.filter(|deadline| *deadline > now_ms)
                    {
                        let finish = deadline.saturating_add(pages.saturating_mul(cycle_ms));
                        out.phase_finish_at = Some(finish);
                        out.phase_remaining_seconds =
                            Some(finish.saturating_sub(now_ms).saturating_add(999) / 1000);
                    }
                } else if matches!(out.status.as_str(), "queued" | "running")
                    && recent_progress
                    && let Some(pages) = pages_remaining
                {
                    let anchor = anchor_progress
                        .unwrap()
                        .max(next.unwrap_or(0))
                        .max(request_after.unwrap_or(0));
                    let finish = anchor.saturating_add(pages.saturating_mul(cycle_ms));
                    if finish > now_ms {
                        out.phase_remaining_seconds =
                            Some(finish.saturating_sub(now_ms).saturating_add(999) / 1000);
                        out.phase_finish_at = Some(finish);
                        out.estimate_reason = Some(
                            "preliminary member-stage estimate; later stages have unknown totals"
                                .into(),
                        );
                    } else {
                        out.estimate_reason = Some("worker_unconfirmed".into());
                    }
                } else {
                    out.estimate_reason = Some("worker_unconfirmed".into());
                }
            } else {
                out.estimate_reason = Some("member count estimate is unavailable".into());
            }
        } else if state == "complete" {
            out.percent = Some(100.0);
            out.estimate_reason = None;
        } else {
            out.estimate_reason = Some("remaining work cannot be estimated in this state".into());
        }
    }
    Ok(out)
}

impl GuildDataStore {
    pub async fn begin_member_ban_attempt(
        &self,
        scope: GuildDataScope,
        user_id: i64,
        operation_id: String,
        now_ms: i64,
    ) -> Result<()> {
        checked_scope(self, &scope)?;
        anyhow::ensure!(
            user_id > 0 && now_ms > 0 && super::tenant_logs::valid_token(&operation_id),
            MemberSyncError::Invalid
        );
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn|{let mut tx=conn.transaction_primary_read()?;verify_scope(&tx,&expected,&scope,true)?;tx.ensure_mutation_receipt()?;
            tx.member_execute("INSERT OR IGNORE INTO guild_member_pending_bans(guild_id,enrollment_id,user_id,operation_id,started_at_ms,retry_after_ms) VALUES(?1,?2,?3,?4,?5,?6)",params![scope.guild_id(),scope.enrollment.id.as_str(),user_id,operation_id,now_ms,now_ms.saturating_add(300_000)])?;tx.commit()
        }).await
    }

    pub async fn finish_member_ban_attempt(
        &self,
        scope: GuildDataScope,
        user_id: i64,
        operation_id: &str,
        outcome: CaseOutcome,
        now_ms: i64,
    ) -> Result<()> {
        checked_scope(self, &scope)?;
        anyhow::ensure!(
            user_id > 0 && now_ms > 0 && super::tenant_logs::valid_token(operation_id),
            MemberSyncError::Invalid
        );
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        let operation_id = operation_id.to_string();
        self.db.with_conn(move |conn|{let mut tx=conn.transaction_primary_read()?;verify_scope(&tx,&expected,&scope,true)?;tx.ensure_mutation_receipt()?;
            let started:Option<i64>=tx.query_row("SELECT started_at_ms FROM guild_member_pending_bans WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3 AND operation_id=?4",params![scope.guild_id(),scope.enrollment.id.as_str(),user_id,operation_id.as_str()],|r|Ok(r.get(0)?)).optional()?;
            let Some(started)=started else {tx.finish_read()?;return Ok(())};
            match outcome {
                CaseOutcome::Succeeded=>{
                    tx.member_execute("INSERT OR IGNORE INTO guild_member_directory(guild_id,enrollment_id,user_id,observed_at_ms) VALUES(?1,?2,?3,?4)",params![scope.guild_id(),scope.enrollment.id.as_str(),user_id,now_ms])?;
                    let newer:bool=tx.query_row("SELECT coalesce(ban_observed_at_ms,0)>?4 AND is_banned=0 FROM guild_member_directory WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),user_id,started],|r|Ok(r.get(0)?))?;
                    tx.member_execute("UPDATE guild_member_directory SET is_banned=CASE WHEN coalesce(ban_observed_at_ms,0)>?5 THEN is_banned ELSE 1 END,ever_banned=1,cleanup_after_ms=NULL,observed_at_ms=MAX(observed_at_ms,?4),revision=revision+1 WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),user_id,now_ms,started])?;
                    if !newer { close_current_for_ban(&tx,&scope,user_id,now_ms)?; }
                    if newer {let revision:i64=tx.query_row("SELECT revision FROM guild_member_directory WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),user_id],|r|Ok(r.get(0)?))?;queue_target_check_tx(&tx,&scope,user_id,MemberVerifyKind::Ban,"own_ban_order_uncertain",revision,now_ms)?;}
                    bump_revision(&tx,&scope)?;
                    tx.member_execute("DELETE FROM guild_member_pending_bans WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3 AND operation_id=?4",params![scope.guild_id(),scope.enrollment.id.as_str(),user_id,operation_id.as_str()])?;
                }
                CaseOutcome::Failed=>{tx.member_execute("DELETE FROM guild_member_pending_bans WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3 AND operation_id=?4",params![scope.guild_id(),scope.enrollment.id.as_str(),user_id,operation_id.as_str()])?;}
                CaseOutcome::Attempted|CaseOutcome::Uncertain=>{}
            }
            tx.commit()
        }).await
    }

    pub async fn next_member_pending_ban_verification(
        &self,
        scope: GuildDataScope,
        now_ms: i64,
    ) -> Result<Option<MemberPendingBanCandidate>> {
        checked_scope(self, &scope)?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn|{let tx=conn.transaction_primary_snapshot()?;verify_scope(&tx,&expected,&scope,true)?;
            let row=tx.query_row("SELECT p.user_id,p.operation_id,p.started_at_ms,d.revision FROM guild_member_pending_bans p LEFT JOIN guild_member_directory d ON d.guild_id=p.guild_id AND d.enrollment_id=p.enrollment_id AND d.user_id=p.user_id WHERE p.guild_id=?1 AND p.enrollment_id=?2 AND p.retry_after_ms<=?3 ORDER BY p.retry_after_ms,p.user_id LIMIT 1",params![scope.guild_id(),scope.enrollment.id.as_str(),now_ms],|r|Ok(MemberPendingBanCandidate{user_id:r.get(0)?,operation_id:r.get(1)?,started_at_ms:r.get(2)?,revision:r.get(3)?})).optional()?;
            tx.finish_read()?;Ok(row)
        }).await
    }

    pub async fn settle_member_pending_ban_verification(
        &self,
        scope: GuildDataScope,
        candidate: MemberPendingBanCandidate,
        banned: bool,
        now_ms: i64,
    ) -> Result<()> {
        checked_scope(self, &scope)?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn|{let mut tx=conn.transaction_primary_read()?;verify_scope(&tx,&expected,&scope,true)?;tx.ensure_mutation_receipt()?;
            let current:Option<i64>=tx.query_row("SELECT revision FROM guild_member_directory WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),candidate.user_id],|r|Ok(r.get(0)?)).optional()?;
            anyhow::ensure!(current==candidate.revision,MemberSyncError::Stale);
            let removed=tx.member_execute("DELETE FROM guild_member_pending_bans WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3 AND operation_id=?4 AND started_at_ms=?5",params![scope.guild_id(),scope.enrollment.id.as_str(),candidate.user_id,candidate.operation_id,candidate.started_at_ms])?;
            anyhow::ensure!(removed==1,MemberSyncError::Stale);
            if banned {
                tx.member_execute("INSERT OR IGNORE INTO guild_member_directory(guild_id,enrollment_id,user_id,observed_at_ms) VALUES(?1,?2,?3,?4)",params![scope.guild_id(),scope.enrollment.id.as_str(),candidate.user_id,now_ms])?;
                let was_banned:bool=tx.query_row("SELECT is_banned FROM guild_member_directory WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),candidate.user_id],|r|Ok(r.get(0)?))?;
                if !was_banned {record_undated_ban_present(&tx,&scope,candidate.user_id,now_ms)?;}
                tx.member_execute("UPDATE guild_member_directory SET is_banned=1,ever_banned=1,cleanup_after_ms=NULL,revision=revision+1 WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),candidate.user_id])?;
                close_current_for_ban(&tx,&scope,candidate.user_id,now_ms)?;
            }else{
                tx.member_execute("UPDATE guild_member_directory SET cleanup_after_ms=CASE WHEN is_current=0 AND ever_banned=0 THEN ?4 ELSE cleanup_after_ms END,revision=revision+1 WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),candidate.user_id,now_ms.saturating_add(300_000)])?;
            }
            bump_revision(&tx,&scope)?;tx.commit()
        }).await
    }

    pub async fn defer_member_pending_ban_verification(
        &self,
        scope: GuildDataScope,
        candidate: MemberPendingBanCandidate,
        next_attempt_at_ms: i64,
    ) -> Result<()> {
        checked_scope(self, &scope)?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn|{let mut tx=conn.transaction_primary_read()?;verify_scope(&tx,&expected,&scope,true)?;tx.ensure_mutation_receipt()?;
            let changed=tx.member_execute("UPDATE guild_member_pending_bans SET retry_after_ms=?6 WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3 AND operation_id=?4 AND started_at_ms=?5",params![scope.guild_id(),scope.enrollment.id.as_str(),candidate.user_id,candidate.operation_id,candidate.started_at_ms,next_attempt_at_ms])?;
            anyhow::ensure!(changed==1,MemberSyncError::Stale);tx.commit()
        }).await
    }

    /// Resume a bounded migration of existing selected cases into the subject index.
    /// The cursor and projection commit together; replay is harmless.
    pub async fn backfill_member_case_index(
        &self,
        scope: GuildDataScope,
        bot_id: i64,
        limit: u32,
    ) -> Result<bool> {
        checked_scope(self, &scope)?;
        anyhow::ensure!(
            bot_id > 0 && (1..=100).contains(&limit),
            MemberSyncError::Invalid
        );
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn|{let mut tx=conn.transaction_primary_read()?;verify_scope(&tx,&expected,&scope,true)?;ensure_seed_bot(&tx,&scope,bot_id)?;
            let (after,done):(Option<String>,bool)=tx.query_row("SELECT case_backfill_after,case_backfill_done FROM guild_member_seed WHERE guild_id=?1 AND enrollment_id=?2",params![scope.guild_id(),scope.enrollment.id.as_str()],|r|Ok((r.get(0)?,r.get(1)?)))?;
            if done {tx.finish_read()?;return Ok(true)}
            let mut stmt=tx.prepare("SELECT case_id FROM guild_cases WHERE guild_id=?1 AND enrollment_id=?2 AND case_id>?3 ORDER BY case_id LIMIT ?4")?;
            let rows=stmt.query_map(params![scope.guild_id(),scope.enrollment.id.as_str(),after.as_deref().unwrap_or(""),limit as i64],|r|Ok(r.get::<String>(0)?))?;
            let mut last=None;let mut count=0;for row in rows{last=Some(row?);count+=1;}drop(stmt);
            tx.ensure_mutation_receipt()?;
            if let Some(last)=&last {
                tx.member_execute("INSERT OR IGNORE INTO guild_member_case_index(guild_id,enrollment_id,user_id,case_id,action_kind,occurred_at_ms) SELECT guild_id,enrollment_id,CAST(json_extract(payload,'$.subject_id') AS INTEGER),case_id,json_extract(payload,'$.action'),CAST(json_extract(payload,'$.occurred_at_ms') AS INTEGER) FROM guild_cases WHERE guild_id=?1 AND enrollment_id=?2 AND case_id>?3 AND case_id<=?4 AND deleted=0 AND json_valid(payload)",params![scope.guild_id(),scope.enrollment.id.as_str(),after.as_deref().unwrap_or(""),last.as_str()])?;
                tx.member_execute("INSERT INTO guild_member_directory(guild_id,enrollment_id,user_id,ever_banned,observed_at_ms) SELECT ?1,?2,CAST(json_extract(payload,'$.subject_id') AS INTEGER),1,MIN(CAST(json_extract(payload,'$.occurred_at_ms') AS INTEGER)) FROM guild_cases WHERE guild_id=?1 AND enrollment_id=?2 AND case_id>?3 AND case_id<=?4 AND deleted=0 AND json_valid(payload) AND json_extract(payload,'$.outcome')='succeeded' AND (json_extract(payload,'$.action')='ban' OR (json_extract(payload,'$.action')='unban' AND (source_key LIKE 'discord:%' OR json_type(payload,'$.source.gateway_observation') IS NOT NULL OR EXISTS(SELECT 1 FROM guild_case_sources s WHERE s.guild_id=guild_cases.guild_id AND s.enrollment_id=guild_cases.enrollment_id AND s.case_id=guild_cases.case_id AND s.source_kind=1)))) GROUP BY CAST(json_extract(payload,'$.subject_id') AS INTEGER) HAVING true ON CONFLICT(guild_id,enrollment_id,user_id) DO UPDATE SET ever_banned=1,cleanup_after_ms=NULL,revision=revision+1",params![scope.guild_id(),scope.enrollment.id.as_str(),after.as_deref().unwrap_or(""),last.as_str()])?;
                bump_revision(&tx,&scope)?;
            }
            let finished=count<limit;
            tx.member_execute("UPDATE guild_member_seed SET case_backfill_after=?3,case_backfill_done=?4 WHERE guild_id=?1 AND enrollment_id=?2",params![scope.guild_id(),scope.enrollment.id.as_str(),last.or(after),finished])?;
            if finished {
                let pending=tx.query_row("SELECT run_kind,phase,status FROM guild_member_sync WHERE guild_id=?1 AND enrollment_id=?2 AND bot_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),bot_id],|r|Ok((r.get::<String>(0)?,r.get::<String>(1)?,r.get::<String>(2)?))).optional()?;
                if let Some((kind,phase,state))=pending && phase=="complete" && state=="waiting" {
                    tx.member_execute("UPDATE guild_member_sync SET status='complete',status_reason=NULL,completed_at_ms=coalesce(completed_at_ms,unixepoch()*1000),last_success_at_ms=coalesce(completed_at_ms,unixepoch()*1000),last_success_processed=processed,last_success_inserted=inserted,last_success_updated=updated,last_success_unchanged=unchanged,progress_revision=progress_revision+1 WHERE guild_id=?1",params![scope.guild_id()])?;
                    if kind=="reconcile" {tx.member_execute("UPDATE guild_member_seed SET reconciliation_needed=0,reconciliation_reason=NULL,gap_at_ms=NULL WHERE guild_id=?1 AND gap_generation=(SELECT reconcile_gap_generation FROM guild_member_sync WHERE guild_id=?1)",params![scope.guild_id()])?;}
                    if kind=="initial"{tx.member_execute("UPDATE guild_member_seed SET initial_completed_at_ms=coalesce(initial_completed_at_ms,unixepoch()*1000) WHERE guild_id=?1",params![scope.guild_id()])?;}
                    if kind=="initial"{tx.member_execute("INSERT OR IGNORE INTO guild_member_audit_recovery(guild_id,enrollment_id,bot_id,run_id,started_at_ms,status,next_attempt_at_ms) SELECT guild_id,enrollment_id,bot_id,lower(hex(randomblob(16))),unixepoch()*1000,'queued',unixepoch()*1000 FROM guild_member_seed WHERE guild_id=?1 AND reconciliation_needed=1 AND initial_completed_at_ms IS NOT NULL",params![scope.guild_id()])?;}
                }
            }
            tx.commit()?;Ok(finished)
        }).await
    }

    pub async fn mark_member_reconciliation_needed(
        &self,
        scope: GuildDataScope,
        reason: &str,
        now_ms: i64,
    ) -> Result<()> {
        checked_scope(self, &scope)?;
        anyhow::ensure!(
            !reason.is_empty() && reason.len() <= 160 && now_ms > 0 && !reason.contains('\0'),
            MemberSyncError::Invalid
        );
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        let reason = reason.to_string();
        let core = self.core.clone();
        let hook_database = self.db.path().to_path_buf();
        self.db.with_conn(move |conn| {
            // Membership and placement checks are autocommit reads so they do not
            // extend the remote interactive write transaction.
            verify_scope(conn, &expected, &scope, true)?;
            run_coverage_gap_after_scope_verify_hook(&hook_database);
            let revision_before: i64 = {
                let tx = conn.transaction_primary_snapshot()?;
                let revision = tx.query_row(
                    "SELECT directory_revision FROM guild_member_seed WHERE guild_id=?1 AND enrollment_id=?2",
                    params![scope.guild_id(), scope.enrollment.id.as_str()],
                    |r| Ok(r.get(0)?),
                )?;
                tx.finish_read()?;
                revision
            };
            run_coverage_gap_after_revision_snapshot_hook(&hook_database);
            // Receipt admission is an autocommit read. A restore fence or a
            // transport error fails here, before BEGIN IMMEDIATE.
            let meta_present = coverage_gap_receipt_admission(conn)?;
            run_coverage_gap_after_receipt_admission_hook(&hook_database);
            // Pre-BEGIN lease and epoch reads fail fast. They do not authorize
            // the commit. The batch compares the captured epoch under the guild
            // writer lock, which prevents an epoch raise until this transaction ends.
            // The final core probe requires the admitted owner; commit does not repeat
            // the epoch query or promise instantaneous cross-database revocation.
            let (writer_generation, admitted_owner) =
                coverage_gap_writer_prelude(core.as_ref(), conn)?;
            run_coverage_gap_after_writer_fence_hook(&hook_database);
            let mut tx = conn.transaction_primary_read()?;
            // The shared guild probe reads the lease twice so a test can change
            // it between those reads. The second snapshot does not fit in this
            // mark's remote lifetime, so commit checks the admitted owner once.
            // A store with no admitted owner keeps whatever probe it already has.
            tx.suppress_commit_writer_recheck(false, writer_generation.is_some());
            if let Some(owner) = admitted_owner {
                let Some(core) = core.clone() else {
                    return Err(crate::db_compat::ActiveBotLeaseRejected.into());
                };
                tx.set_commit_lease_probe(Some(std::sync::Arc::new(move |commit| {
                    // Check the owner admitted before BEGIN, even if the
                    // in-memory slot has since been replaced or cleared.
                    Self::commit_with_core_writer_lease(&core, &owner, commit)
                })));
            }
            let (receipt_sql, receipt_prune) = tx
                .stage_mutation_receipt(meta_present)?
                .unwrap_or_default();
            let restore_check = if receipt_sql.is_empty() {
                String::new()
            } else {
                format!("SELECT CASE WHEN changes()=1 THEN 1 ELSE json_set('{{}}', '{COVERAGE_GAP_RESTORE_ERROR}', 1) END;")
            };
            // The write lock now freezes schema changes. If admission saw no
            // meta table, refuse a newly created one rather than inserting an
            // unfenced receipt. Match SQLite's case-insensitive identifiers.
            // A retry will read its restore fence normally.
            let meta_check = if meta_present {
                String::new()
            } else {
                format!("SELECT CASE WHEN NOT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='meta' COLLATE NOCASE) THEN 1 ELSE json_set('{{}}', '{COVERAGE_GAP_RESTORE_ERROR}', 1) END;")
            };
            // RAISE() needs a trigger; remote libSQL forbids temporary objects.
            // The invalid JSON path aborts the batch only on a failed check.
            let mark_check = format!("SELECT CASE WHEN changes()=1 THEN 1 ELSE json_set('{{}}', '{COVERAGE_GAP_MEMBERSHIP_ERROR}', 1) END;");
            let guild_id = scope.guild_id();
            let enrollment = sql_literal(scope.enrollment.id.as_str());
            let reason_sql = sql_literal(&reason);
            let run_id = sql_literal(&random_id());
            let joined_at_ns = scope.enrollment.joined_at_ns;
            let expected_revision = revision_before + 1;
            let fence_sql = coverage_gap_writer_fence_sql(writer_generation);
            // Receipt admission, the one-row mark check, and audit recovery
            // share one batch. The 32 MiB batch bound covers the seed row that
            // the removed COUNT used to budget on its own.
            let batch = format!(
                "{meta_check}{fence_sql}{receipt_sql}{restore_check}{receipt_prune}UPDATE guild_member_seed SET reconciliation_needed=1,reconciliation_reason={reason_sql},gap_at_ms=MAX(coalesce(gap_at_ms,0),{now_ms}),gap_generation=gap_generation+1,directory_revision=directory_revision+1 WHERE guild_id={guild_id} AND enrollment_id={enrollment} AND directory_revision={revision_before} AND EXISTS (SELECT 1 FROM guild_memberships m WHERE m.guild_id={guild_id} AND m.enrollment_id={enrollment} AND m.active=1 AND m.joined_at_ns={joined_at_ns});
{mark_check}
INSERT INTO guild_member_audit_recovery(guild_id,enrollment_id,bot_id,run_id,started_at_ms,status,next_attempt_at_ms) SELECT {guild_id},{enrollment},s.bot_id,{run_id},{now_ms},'queued',{now_ms} FROM guild_member_seed s WHERE s.guild_id={guild_id} AND s.enrollment_id={enrollment} AND s.directory_revision={expected_revision} AND s.reconciliation_needed=1 AND s.reconciliation_reason={reason_sql} AND s.initial_completed_at_ms IS NOT NULL ON CONFLICT(guild_id) DO UPDATE SET run_id=excluded.run_id,started_at_ms=excluded.started_at_ms,cursor=NULL,status='queued',status_reason=NULL,processed=0,next_attempt_at_ms=excluded.next_attempt_at_ms,request_after_ms=NULL,generation=guild_member_audit_recovery.generation+1,lease_owner=NULL,lease_until_ms=NULL WHERE guild_member_audit_recovery.status='complete';
UPDATE guild_member_audit_recovery SET restart_requested_at_ms=MAX(coalesce(restart_requested_at_ms,0),{now_ms}) WHERE guild_id={guild_id} AND status!='complete' AND (cursor IS NOT NULL OR status='running') AND EXISTS(SELECT 1 FROM guild_member_seed s WHERE s.guild_id={guild_id} AND s.enrollment_id={enrollment} AND s.directory_revision={expected_revision} AND s.reconciliation_needed=1 AND s.reconciliation_reason={reason_sql});"
            );
            if let Err(error) = tx.execute_batch_bounded(&batch, COVERAGE_GAP_MARK_GROWTH) {
                return Err(coverage_gap_batch_error(error));
            }
            run_coverage_gap_after_batch_hook(&hook_database);
            tx.commit()
        })
        .await
    }

    pub async fn initialize_member_directory(
        &self,
        scope: GuildDataScope,
        bot_id: i64,
        now_ms: i64,
    ) -> Result<MemberSyncStatus> {
        checked_scope(self, &scope)?;
        anyhow::ensure!(bot_id > 0 && now_ms > 0, MemberSyncError::Invalid);
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| {
            let mut tx=conn.transaction_primary_read()?; verify_scope(&tx,&expected,&scope,true)?;
            let admission=tx.query_row("SELECT enrollment_id,seed_admitted FROM guild_member_feature WHERE guild_id=?1",params![scope.guild_id()],|r|Ok((r.get::<String>(0)?,r.get::<bool>(1)?))).optional()?;
            let eligible=matches!(admission,Some((ref id,false)) if id==&scope.enrollment.id);
            if ensure_seed_bot(&tx,&scope,bot_id)?.is_none() && eligible {
                tx.ensure_mutation_receipt()?;
                tx.member_execute("INSERT INTO guild_member_seed(guild_id,enrollment_id,bot_id,initialized_at_ms) VALUES(?1,?2,?3,?4)",params![scope.guild_id(),scope.enrollment.id.as_str(),bot_id,now_ms])?;
                tx.member_execute("INSERT INTO guild_member_sync(guild_id,enrollment_id,bot_id,run_id,run_kind,phase,status,started_at_ms,next_attempt_at_ms,request_after_ms,member_processed,ban_processed,verify_processed,audit_processed) VALUES(?1,?2,?3,?4,'initial','members','queued',?5,?5,?5,0,0,0,0)",params![scope.guild_id(),scope.enrollment.id.as_str(),bot_id,random_id(),now_ms])?;
                tx.member_execute("UPDATE guild_member_feature SET seed_admitted=1 WHERE guild_id=?1 AND enrollment_id=?2 AND seed_admitted=0",params![scope.guild_id(),scope.enrollment.id.as_str()])?;
                tx.commit()?;
            } else { tx.finish_read()?; }
            let tx=conn.transaction_primary_snapshot()?; verify_scope(&tx,&expected,&scope,true)?; let out=status(&tx,&scope,bot_id,now_ms,false)?; tx.finish_read()?; Ok(out)
        }).await
    }

    /// Gateway bot user id already persisted for this enrollment's directory work.
    pub async fn persisted_member_sync_bot_id(&self, scope: GuildDataScope) -> Result<Option<i64>> {
        checked_scope(self, &scope)?;
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let tx = conn.transaction_primary_snapshot()?;
                verify_scope(&tx, &expected, &scope, true)?;
                let from_seed: Option<i64> = tx
                    .query_row(
                        "SELECT bot_id FROM guild_member_seed WHERE guild_id=?1 AND enrollment_id=?2",
                        params![scope.guild_id(), scope.enrollment.id.as_str()],
                        |r| Ok(r.get(0)?),
                    )
                    .optional()?;
                if from_seed.is_some() {
                    tx.finish_read()?;
                    return Ok(from_seed);
                }
                let from_sync: Option<i64> = tx
                    .query_row(
                        "SELECT bot_id FROM guild_member_sync WHERE guild_id=?1 AND enrollment_id=?2",
                        params![scope.guild_id(), scope.enrollment.id.as_str()],
                        |r| Ok(r.get(0)?),
                    )
                    .optional()?;
                tx.finish_read()?;
                Ok(from_sync)
            })
            .await
    }

    pub async fn member_sync_status(
        &self,
        scope: GuildDataScope,
        bot_id: i64,
        now_ms: i64,
    ) -> Result<MemberSyncStatus> {
        checked_scope(self, &scope)?;
        anyhow::ensure!(bot_id > 0, MemberSyncError::Invalid);
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let tx = conn.transaction_primary_snapshot()?;
                verify_scope(&tx, &expected, &scope, true)?;
                let out = status(&tx, &scope, bot_id, now_ms, true)?;
                tx.finish_read()?;
                Ok(out)
            })
            .await
    }

    /// Idle-loop status. Scheduling uses phase, state, and the saved attempt
    /// times. It does not count absence candidates; that backlog is for the
    /// dashboard read.
    pub async fn member_sync_worker_status(
        &self,
        scope: GuildDataScope,
        bot_id: i64,
        now_ms: i64,
    ) -> Result<MemberSyncStatus> {
        checked_scope(self, &scope)?;
        anyhow::ensure!(bot_id > 0, MemberSyncError::Invalid);
        let expected = self.expected.clone();
        let guild = scope.guild_id();
        self.db
            .with_conn(move |conn| {
                let tx = conn.transaction_primary_snapshot()?;
                let fenced: i64 = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM guild_local_write_fence WHERE guild_id=?1)",
                    params![guild],
                    |row| Ok(row.get(0)?),
                )?;
                if fenced == 1 {
                    tx.finish_read()?;
                    return Err(crate::db::guild_stores::GuildWriteFenced.into());
                }
                verify_scope(&tx, &expected, &scope, true)?;
                let out = status(&tx, &scope, bot_id, now_ms, false)?;
                tx.finish_read()?;
                Ok(out)
            })
            .await
    }

    /// Record the guild-local fence before any core fence row.
    pub async fn record_local_write_fence(&self, guild: i64, fenced_at_ms: i64) -> Result<()> {
        anyhow::ensure!(guild > 0 && fenced_at_ms > 0, MemberSyncError::Invalid);
        self.db
            .with_conn(move |conn| {
                let mut tx = conn.transaction_primary_read()?;
                tx.ensure_mutation_receipt()?;
                tx.execute_rows(
                    "INSERT INTO guild_local_write_fence(guild_id, fenced_at_ms) VALUES(?1, ?2)
                     ON CONFLICT(guild_id) DO NOTHING",
                    params![guild, fenced_at_ms],
                    1,
                )?;
                tx.commit()
            })
            .await
    }

    /// Supply a known roster count for the first queued page. This is a
    /// one-time, recent bot initialization hint, never a worker heartbeat.
    pub async fn set_initial_member_estimate(
        &self,
        scope: GuildDataScope,
        bot_id: i64,
        estimate: MemberInitialEstimate,
    ) -> Result<()> {
        let MemberInitialEstimate {
            member_count,
            page_size,
            pace_ms,
            active_guild_count,
            now_ms,
        } = estimate;
        checked_scope(self, &scope)?;
        anyhow::ensure!(
            bot_id > 0
                && member_count >= 0
                && (1..=500).contains(&page_size)
                && pace_ms >= 15_000
                && active_guild_count > 0
                && now_ms > 0,
            MemberSyncError::Invalid
        );
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| {
            let mut tx=conn.transaction_primary_read()?;
            verify_scope(&tx,&expected,&scope,true)?;
            ensure_seed_bot(&tx,&scope,bot_id)?;
            let eligible:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM guild_member_sync WHERE guild_id=?1 AND enrollment_id=?2 AND bot_id=?3 AND run_kind='initial' AND phase='members' AND status='queued' AND processed=0 AND last_progress_at_ms IS NULL AND total_estimate IS NULL AND started_at_ms BETWEEN ?4 AND ?5)",params![scope.guild_id(),scope.enrollment.id.as_str(),bot_id,now_ms.saturating_sub(60_000),now_ms],|r|Ok(r.get(0)?))?;
            if !eligible {tx.finish_read()?;return Ok(())}
            tx.ensure_mutation_receipt()?;
            tx.member_execute("UPDATE guild_member_sync SET total_estimate=?4,page_size=?5,pace_ms=?6,active_guild_count=?7,progress_revision=progress_revision+1 WHERE guild_id=?1 AND enrollment_id=?2 AND bot_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),bot_id,member_count,page_size as i64,pace_ms,active_guild_count as i64])?;
            tx.commit()
        }).await
    }

    pub async fn observe_member(
        &self,
        scope: GuildDataScope,
        observation: MemberObservation,
    ) -> Result<MemberUpsertOutcome> {
        checked_scope(self, &scope)?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let mut tx = conn.transaction_primary_read()?;
                verify_scope(&tx, &expected, &scope, true)?;
                tx.ensure_mutation_receipt()?;
                let outcome = upsert_member(&tx, &scope, &observation, "observed")?;
                tx.commit()?;
                Ok(outcome)
            })
            .await
    }

    pub async fn member_case_index_complete(&self, scope: GuildDataScope) -> Result<bool> {
        checked_scope(self, &scope)?;
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let tx = conn.transaction_primary_snapshot()?;
                verify_scope(&tx, &expected, &scope, true)?;
                let done: bool = tx
                    .query_row(
                        "SELECT case_backfill_done FROM guild_member_seed WHERE guild_id=?1 AND enrollment_id=?2",
                        params![scope.guild_id(), scope.enrollment.id.as_str()],
                        |r| Ok(r.get(0)?),
                    )
                    .optional()?
                    .unwrap_or(false);
                tx.finish_read()?;
                Ok(done)
            })
            .await
    }

    pub async fn depart_member(
        &self,
        scope: GuildDataScope,
        user_id: i64,
        departed_at_ms: i64,
    ) -> Result<Option<MemberDeparture>> {
        match self
            .depart_member_inner(scope, user_id, departed_at_ms, None, false)
            .await?
        {
            MemberDepartureDecision::Applied(value) => Ok(value),
            MemberDepartureDecision::NeedsVerification => Ok(None),
        }
    }

    pub async fn depart_member_checked(
        &self,
        scope: GuildDataScope,
        user_id: i64,
        departed_at_ms: i64,
        expected_joined_at_ms: Option<i64>,
    ) -> Result<MemberDepartureDecision> {
        self.depart_member_inner(scope, user_id, departed_at_ms, expected_joined_at_ms, true)
            .await
    }

    async fn depart_member_inner(
        &self,
        scope: GuildDataScope,
        user_id: i64,
        departed_at_ms: i64,
        expected_joined_at_ms: Option<i64>,
        strict: bool,
    ) -> Result<MemberDepartureDecision> {
        checked_scope(self, &scope)?;
        anyhow::ensure!(user_id > 0 && departed_at_ms > 0, MemberSyncError::Invalid);
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| {
            let mut tx=conn.transaction_primary_read()?; verify_scope(&tx,&expected,&scope,true)?;
            let row=tx.query_row("SELECT is_current,joined_at_ms,current_stay_id,ever_banned,revision FROM guild_member_directory WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),user_id],|r|Ok((r.get::<bool>(0)?,r.get::<Option<i64>>(1)?,r.get::<Option<String>>(2)?,r.get::<bool>(3)?,r.get::<i64>(4)?))).optional()?;
            let Some((current,joined,stay,retained,revision))=row else {
                // A remove observed while the first roster page is in flight
                // must invalidate that page even when no profile exists yet.
                let seeded:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM guild_member_seed WHERE guild_id=?1 AND enrollment_id=?2)",params![scope.guild_id(),scope.enrollment.id.as_str()],|r|Ok(r.get(0)?))?;
                if seeded {
                    tx.ensure_mutation_receipt()?;
                    tx.member_execute("INSERT INTO guild_member_directory(guild_id,enrollment_id,user_id,is_current,departed_at_ms,observed_at_ms,cleanup_after_ms) VALUES(?1,?2,?3,0,?4,?4,?5)",params![scope.guild_id(),scope.enrollment.id.as_str(),user_id,departed_at_ms,departed_at_ms.saturating_add(300_000)])?;
                    queue_target_check_tx(&tx,&scope,user_id,MemberVerifyKind::Member,"no_row_departure",0,departed_at_ms)?;
                    bump_revision(&tx,&scope)?;
                    tx.commit()?;
                } else { tx.finish_read()?; }
                return Ok(MemberDepartureDecision::Applied(None));
            };
            // A confirmed ban already removed this member from the current
            // list, but the last stay remains pending until an actual Gateway
            // remove supplies a departure observation.
            let pending_ban_stay = !current && retained && stay.is_some();
            if !current && !pending_ban_stay { tx.finish_read()?; return Ok(MemberDepartureDecision::Applied(None)); }
            // The Gateway remove event often has no member payload. A current
            // saved stay can still supply its actual join time. A supplied
            // conflicting token, an impossible timestamp, or a missing stay is
            // concrete evidence that the event cannot safely close this stay.
            let rejoined_without_token = if strict && expected_joined_at_ms.is_none() {
                match stay.as_deref() {
                    Some(stay) => tx.query_row("SELECT provenance IN ('observed_rejoin','reconciled_rejoin','imported_rejoin') FROM guild_member_stays WHERE guild_id=?1 AND enrollment_id=?2 AND stay_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),stay],|r|Ok(r.get::<bool>(0)?)).optional()?.unwrap_or(true),
                    None => true,
                }
            } else { false };
            if strict && (expected_joined_at_ms.is_some_and(|v| Some(v)!=joined) || joined.is_some_and(|v|v>departed_at_ms) || stay.is_none() || rejoined_without_token) {
                tx.ensure_mutation_receipt()?;
                let reason=if rejoined_without_token {"ambiguous_departure"}else{"mismatched_departure"};
                // Store the gateway remove time in requested_at_ms for later settlement.
                queue_target_check_tx(&tx,&scope,user_id,MemberVerifyKind::Member,reason,revision,departed_at_ms)?;
                if rejoined_without_token {
                    // Fence earlier verification results without advancing the captured departure time.
                    tx.member_execute("UPDATE guild_member_directory SET revision=revision+1 WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),user_id])?;
                    tx.member_execute("UPDATE guild_member_target_checks SET expected_stay_id=coalesce(expected_stay_id,?5),row_revision=row_revision+1 WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3 AND kind='member' AND row_revision=?4",params![scope.guild_id(),scope.enrollment.id.as_str(),user_id,revision,stay.as_deref()])?;
                    bump_revision(&tx,&scope)?;
                }
                tx.commit()?;
                return Ok(MemberDepartureDecision::NeedsVerification);
            }
            tx.ensure_mutation_receipt()?;
            if let Some(stay)=stay { tx.member_execute("UPDATE guild_member_stays SET departed_at_ms=?4,ended_unknown=0 WHERE guild_id=?1 AND enrollment_id=?2 AND stay_id=?3 AND departed_at_ms IS NULL",params![scope.guild_id(),scope.enrollment.id.as_str(),stay,departed_at_ms])?; }
            tx.member_execute("UPDATE guild_member_directory SET is_current=0,departed_at_ms=?4,current_stay_id=NULL,cleanup_after_ms=?5,observed_at_ms=MAX(observed_at_ms,?4),revision=revision+1 WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),user_id,departed_at_ms,if retained {None} else {Some(departed_at_ms.saturating_add(300_000))}])?;
            bump_revision(&tx,&scope)?; tx.commit()?;
            Ok(MemberDepartureDecision::Applied(Some(MemberDeparture{user_id,joined_at_ms:joined,departed_at_ms,completed_stay_ms:joined.filter(|j|*j<=departed_at_ms).map(|j|departed_at_ms-j)})))
        }).await
    }

    pub async fn observe_ban(
        &self,
        scope: GuildDataScope,
        user_id: i64,
        observed_at_ms: i64,
    ) -> Result<bool> {
        self.set_ban(scope, user_id, observed_at_ms, true, None, false)
            .await
    }
    pub async fn observe_unban(
        &self,
        scope: GuildDataScope,
        user_id: i64,
        observed_at_ms: i64,
    ) -> Result<bool> {
        self.set_ban(scope, user_id, observed_at_ms, false, None, false)
            .await
    }
    pub async fn observe_ban_contested(
        &self,
        scope: GuildDataScope,
        user_id: i64,
        observed_at_ms: i64,
        username: String,
        global_name: Option<String>,
    ) -> Result<bool> {
        self.set_ban(
            scope,
            user_id,
            observed_at_ms,
            true,
            Some((username, global_name)),
            true,
        )
        .await
    }
    pub async fn observe_unban_contested(
        &self,
        scope: GuildDataScope,
        user_id: i64,
        observed_at_ms: i64,
        username: String,
        global_name: Option<String>,
    ) -> Result<bool> {
        self.set_ban(
            scope,
            user_id,
            observed_at_ms,
            false,
            Some((username, global_name)),
            true,
        )
        .await
    }
    pub async fn observe_ban_with_identity(
        &self,
        scope: GuildDataScope,
        user_id: i64,
        observed_at_ms: i64,
        username: String,
        global_name: Option<String>,
    ) -> Result<bool> {
        self.set_ban(
            scope,
            user_id,
            observed_at_ms,
            true,
            Some((username, global_name)),
            false,
        )
        .await
    }
    pub async fn observe_unban_with_identity(
        &self,
        scope: GuildDataScope,
        user_id: i64,
        observed_at_ms: i64,
        username: String,
        global_name: Option<String>,
    ) -> Result<bool> {
        self.set_ban(
            scope,
            user_id,
            observed_at_ms,
            false,
            Some((username, global_name)),
            false,
        )
        .await
    }
    async fn set_ban(
        &self,
        scope: GuildDataScope,
        user_id: i64,
        observed_at_ms: i64,
        banned: bool,
        identity: Option<(String, Option<String>)>,
        order_uncertain: bool,
    ) -> Result<bool> {
        checked_scope(self, &scope)?;
        anyhow::ensure!(user_id > 0 && observed_at_ms > 0, MemberSyncError::Invalid);
        let identity = identity
            .map(|(username, global_name)| {
                anyhow::ensure!(
                    !username.is_empty() && username.len() <= 256,
                    MemberSyncError::Invalid
                );
                Ok((username, bounded_name(global_name)?))
            })
            .transpose()?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| { let mut tx=conn.transaction_primary_read()?; verify_scope(&tx,&expected,&scope,true)?;
            let old=tx.query_row("SELECT is_banned,ban_observed_at_ms,revision FROM guild_member_directory WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),user_id],|r|Ok((r.get::<bool>(0)?,r.get::<Option<i64>>(1)?,r.get::<i64>(2)?))).optional()?;
            if let Some((state,time,revision))=old {
                if time.is_some_and(|v|v>observed_at_ms || (v==observed_at_ms && state!=banned)) {
                    tx.ensure_mutation_receipt()?;
                    queue_target_check_tx(&tx,&scope,user_id,MemberVerifyKind::Ban,"ban_order_uncertain",revision,observed_at_ms)?;
                    tx.commit()?;
                    return Ok(true);
                }
                if state==banned {
                    tx.ensure_mutation_receipt()?;
                    tx.member_execute("UPDATE guild_member_directory SET ever_banned=1,cleanup_after_ms=NULL,ban_observed_at_ms=?4,observed_at_ms=MAX(observed_at_ms,?4),revision=revision+1 WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),user_id,observed_at_ms])?;
                    if let Some((username,global_name))=&identity { update_ban_identity(&tx,&scope,user_id,username,global_name)?; }
                    if banned { close_current_for_ban(&tx,&scope,user_id,observed_at_ms)?; }
                    if order_uncertain {
                        let revision: i64 = tx.query_row("SELECT revision FROM guild_member_directory WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3", params![scope.guild_id(), scope.enrollment.id.as_str(), user_id], |r| Ok(r.get(0)?))?;
                        queue_target_check_tx(&tx, &scope, user_id, MemberVerifyKind::Ban, "ban_order_uncertain", revision, observed_at_ms)?;
                    }
                    bump_revision(&tx,&scope)?;tx.commit()?;return Ok(true)
                }
            }
            tx.ensure_mutation_receipt()?;
            tx.member_execute("INSERT OR IGNORE INTO guild_member_directory(guild_id,enrollment_id,user_id,observed_at_ms,ever_banned) VALUES(?1,?2,?3,?4,1)",params![scope.guild_id(),scope.enrollment.id.as_str(),user_id,observed_at_ms])?;
            tx.member_execute("UPDATE guild_member_directory SET is_banned=?4,ever_banned=1,current_ban_reason=CASE WHEN ?4 THEN current_ban_reason ELSE NULL END,cleanup_after_ms=NULL,observed_at_ms=MAX(observed_at_ms,?5),ban_observed_at_ms=?5,revision=revision+1 WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),user_id,banned,observed_at_ms])?;
            if order_uncertain {
                let revision: i64 = tx.query_row("SELECT revision FROM guild_member_directory WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3", params![scope.guild_id(), scope.enrollment.id.as_str(), user_id], |r| Ok(r.get(0)?))?;
                queue_target_check_tx(&tx, &scope, user_id, MemberVerifyKind::Ban, "ban_order_uncertain", revision, observed_at_ms)?;
            }
            if let Some((username,global_name))=&identity { update_ban_identity(&tx,&scope,user_id,username,global_name)?; }
            if banned { close_current_for_ban(&tx,&scope,user_id,observed_at_ms)?; }
            if banned {tx.member_execute("DELETE FROM guild_member_pending_bans WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),user_id])?;}
            bump_revision(&tx,&scope)?; tx.commit()?;Ok(true)
        }).await
    }
}

fn bump_revision(conn: &impl SqlExecutor, scope: &GuildDataScope) -> Result<()> {
    note_member_sync_statement(scope.guild_id());
    conn.member_execute("UPDATE guild_member_seed SET directory_revision=directory_revision+1 WHERE guild_id=?1 AND enrollment_id=?2",params![scope.guild_id(),scope.enrollment.id.as_str()])?;
    Ok(())
}

fn audit_gap_recovery_pending(conn: &impl SqlExecutor, scope: &GuildDataScope) -> Result<bool> {
    // A bounded audit cursor may still reveal an intervening ban and unban.
    // A current-state ban lookup alone cannot establish ordinary retention.
    conn.query_row("SELECT EXISTS(SELECT 1 FROM guild_member_audit_recovery WHERE guild_id=?1 AND enrollment_id=?2 AND status!='complete')",params![scope.guild_id(),scope.enrollment.id.as_str()],|r|Ok(r.get(0)?))
}

// A confirmed current ban proves absence, but does not provide a leave time.
fn close_current_for_ban(
    conn: &impl SqlExecutor,
    scope: &GuildDataScope,
    user_id: i64,
    observed_at_ms: i64,
) -> Result<()> {
    let stay:Option<Option<String>>=conn.query_row("SELECT current_stay_id FROM guild_member_directory WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3 AND is_current=1",params![scope.guild_id(),scope.enrollment.id.as_str(),user_id],|r|Ok(r.get(0)?)).optional()?;
    let Some(stay) = stay else {
        return Ok(());
    };
    if let Some(stay) = stay {
        conn.member_execute("UPDATE guild_member_stays SET ended_unknown=1 WHERE guild_id=?1 AND enrollment_id=?2 AND stay_id=?3 AND departed_at_ms IS NULL",params![scope.guild_id(),scope.enrollment.id.as_str(),stay])?;
    }
    conn.member_execute("INSERT INTO guild_member_state_observations(guild_id,enrollment_id,observation_id,user_id,kind,observed_at_ms) VALUES(?1,?2,?3,?4,'member_absent',?5)",params![scope.guild_id(),scope.enrollment.id.as_str(),random_id(),user_id,observed_at_ms])?;
    conn.member_execute("UPDATE guild_member_directory SET is_current=0,departed_at_ms=NULL,cleanup_after_ms=NULL,revision=revision+1 WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),user_id])?;
    Ok(())
}

// Snapshot/verification only: the historical ban event time is unknown. A
// canonical latest ban case already supplies the dated action in the timeline.
fn record_undated_ban_present(
    conn: &impl SqlExecutor,
    scope: &GuildDataScope,
    user_id: i64,
    observed_at_ms: i64,
) -> Result<()> {
    conn.member_execute("INSERT INTO guild_member_state_observations(guild_id,enrollment_id,observation_id,user_id,kind,observed_at_ms) SELECT ?1,?2,?3,?4,'ban_present',?5 WHERE coalesce((SELECT i.action_kind FROM guild_member_case_index i JOIN guild_cases c USING(guild_id,enrollment_id,case_id) WHERE i.guild_id=?1 AND i.enrollment_id=?2 AND i.user_id=?4 AND c.deleted=0 AND json_valid(c.payload) AND json_extract(c.payload,'$.outcome')='succeeded' ORDER BY i.occurred_at_ms DESC,i.case_id DESC LIMIT 1),'')!='ban'",params![scope.guild_id(),scope.enrollment.id.as_str(),random_id(),user_id,observed_at_ms])?;
    Ok(())
}

fn queue_target_check_tx(
    conn: &impl SqlExecutor,
    scope: &GuildDataScope,
    user_id: i64,
    kind: MemberVerifyKind,
    reason: &str,
    revision: i64,
    now_ms: i64,
) -> Result<()> {
    // Repeated removals for the same revision retain the first event time and stay.
    // A newer revision replaces that capture so it cannot reuse an older stay.
    note_member_sync_statement(scope.guild_id());
    conn.member_execute("INSERT INTO guild_member_target_checks(guild_id,enrollment_id,user_id,kind,reason,row_revision,requested_at_ms,next_attempt_at_ms,expected_stay_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?7,NULL) ON CONFLICT(guild_id,enrollment_id,user_id,kind) DO UPDATE SET reason=excluded.reason,row_revision=excluded.row_revision,requested_at_ms=CASE WHEN guild_member_target_checks.reason='ambiguous_departure' AND excluded.reason='ambiguous_departure' AND guild_member_target_checks.row_revision=excluded.row_revision THEN guild_member_target_checks.requested_at_ms WHEN excluded.reason='ambiguous_departure' THEN excluded.requested_at_ms ELSE MAX(guild_member_target_checks.requested_at_ms+1,excluded.requested_at_ms) END,next_attempt_at_ms=MIN(guild_member_target_checks.next_attempt_at_ms,excluded.next_attempt_at_ms),expected_stay_id=CASE WHEN guild_member_target_checks.reason='ambiguous_departure' AND excluded.reason='ambiguous_departure' AND guild_member_target_checks.row_revision=excluded.row_revision THEN guild_member_target_checks.expected_stay_id ELSE NULL END",params![scope.guild_id(),scope.enrollment.id.as_str(),user_id,kind.as_str(),reason,revision,now_ms])?;
    Ok(())
}

fn settle_verified_state(
    conn: &impl SqlExecutor,
    scope: &GuildDataScope,
    v: &MemberVerification,
    now_ms: i64,
) -> Result<()> {
    anyhow::ensure!(v.user_id > 0 && now_ms > 0, MemberSyncError::Invalid);
    match v.kind {
        MemberVerifyKind::Member if v.present => {
            let obs = v.member.as_ref().ok_or(MemberSyncError::Invalid)?;
            anyhow::ensure!(obs.user_id == v.user_id, MemberSyncError::Invalid);
            upsert_member(conn, scope, obs, "reconciled")?;
        }
        MemberVerifyKind::Member => {
            let prior:Option<(bool,Option<String>)>=conn.query_row("SELECT is_current,current_stay_id FROM guild_member_directory WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),v.user_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            let stay = prior.as_ref().and_then(|(_, stay)| stay.clone());
            if let Some(stay) = stay {
                conn.member_execute("UPDATE guild_member_stays SET ended_unknown=1 WHERE guild_id=?1 AND enrollment_id=?2 AND stay_id=?3 AND departed_at_ms IS NULL",params![scope.guild_id(),scope.enrollment.id.as_str(),stay])?;
            }
            if prior.is_some() {
                conn.member_execute("UPDATE guild_member_directory SET is_current=0,current_stay_id=CASE WHEN is_current=0 AND ever_banned=1 THEN current_stay_id ELSE NULL END,departed_at_ms=CASE WHEN is_current=1 THEN NULL ELSE departed_at_ms END,cleanup_after_ms=CASE WHEN ever_banned=1 THEN NULL WHEN is_current=1 THEN MAX(coalesce(cleanup_after_ms,0),?4) ELSE cleanup_after_ms END,cleanup_member_checked_at_ms=?5,revision=revision+1 WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),v.user_id,now_ms.saturating_add(300_000),now_ms])?;
                if prior.is_some_and(|(current, _)| current) {
                    conn.member_execute("INSERT INTO guild_member_state_observations(guild_id,enrollment_id,observation_id,user_id,kind,observed_at_ms) VALUES(?1,?2,?3,?4,'member_absent',?5)",params![scope.guild_id(),scope.enrollment.id.as_str(),random_id(),v.user_id,now_ms])?;
                }
                bump_revision(conn, scope)?;
            }
        }
        MemberVerifyKind::Ban if v.present => {
            let ban = v.ban.as_ref().ok_or(MemberSyncError::Invalid)?;
            anyhow::ensure!(ban.user_id == v.user_id, MemberSyncError::Invalid);
            upsert_ban_page(conn, scope, std::slice::from_ref(ban), now_ms)?;
        }
        MemberVerifyKind::Ban => {
            conn.member_execute("UPDATE guild_member_directory SET is_banned=0,current_ban_reason=NULL,ban_observed_at_ms=?4,revision=revision+1 WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),v.user_id,now_ms])?;
            conn.member_execute("INSERT INTO guild_member_state_observations(guild_id,enrollment_id,observation_id,user_id,kind,observed_at_ms) VALUES(?1,?2,?3,?4,'ban_absent',?5)",params![scope.guild_id(),scope.enrollment.id.as_str(),random_id(),v.user_id,now_ms])?;
            bump_revision(conn, scope)?;
        }
    }
    Ok(())
}

pub(super) fn confirmed_ban_history_needs_retention(
    conn: &impl SqlExecutor,
    scope: &GuildDataScope,
    source_key: &str,
) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM guild_case_sources s JOIN guild_cases c USING(guild_id,enrollment_id,case_id) LEFT JOIN guild_member_directory d ON d.guild_id=s.guild_id AND d.enrollment_id=s.enrollment_id AND d.user_id=CAST(json_extract(c.payload,'$.subject_id') AS INTEGER) WHERE s.guild_id=?1 AND s.enrollment_id=?2 AND s.source_key=?3 AND c.deleted=0 AND json_valid(c.payload) AND json_extract(c.payload,'$.outcome')='succeeded' AND json_extract(c.payload,'$.action') IN ('ban','unban') AND CAST(json_extract(c.payload,'$.subject_id') AS INTEGER)>0 AND CAST(json_extract(c.payload,'$.occurred_at_ms') AS INTEGER)>0 AND (d.user_id IS NULL OR d.ever_banned=0 OR d.cleanup_after_ms IS NOT NULL))",
        params![scope.guild_id(), scope.enrollment.id.as_str(), source_key],
        |r| Ok(r.get(0)?),
    )
}

pub(super) fn retain_confirmed_ban_history(
    conn: &impl SqlExecutor,
    scope: &GuildDataScope,
    source_key: &str,
) -> Result<()> {
    let row: Option<(i64, i64)> = conn.query_row(
        "SELECT CAST(json_extract(c.payload,'$.subject_id') AS INTEGER), CAST(json_extract(c.payload,'$.occurred_at_ms') AS INTEGER) FROM guild_case_sources s JOIN guild_cases c USING(guild_id,enrollment_id,case_id) WHERE s.guild_id=?1 AND s.enrollment_id=?2 AND s.source_key=?3 AND c.deleted=0 AND json_valid(c.payload) AND json_extract(c.payload,'$.outcome')='succeeded' AND json_extract(c.payload,'$.action') IN ('ban','unban')",
        params![scope.guild_id(), scope.enrollment.id.as_str(), source_key],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ).optional()?;
    let Some((user_id, occurred_at_ms)) = row else {
        return Ok(());
    };
    if user_id <= 0 || occurred_at_ms <= 0 {
        return Ok(());
    }
    conn.member_execute("INSERT INTO guild_member_directory(guild_id,enrollment_id,user_id,ever_banned,observed_at_ms) VALUES(?1,?2,?3,1,?4) ON CONFLICT(guild_id,enrollment_id,user_id) DO UPDATE SET ever_banned=1,cleanup_after_ms=NULL,revision=revision+1 WHERE guild_member_directory.ever_banned=0 OR guild_member_directory.cleanup_after_ms IS NOT NULL", params![scope.guild_id(), scope.enrollment.id.as_str(), user_id, occurred_at_ms])?;
    Ok(())
}

fn ingest_member_audits(
    conn: &impl SqlExecutor,
    scope: &GuildDataScope,
    bot_id: i64,
    audits: &[MemberAuditSnapshot],
    now_ms: i64,
) -> Result<(i64, i64)> {
    let mut inserted = 0;
    let mut unchanged = 0;
    for audit in audits {
        anyhow::ensure!(
            audit.entry_id > 0
                && audit.user_id > 0
                && audit.occurred_at_ms > 0
                && audit.executor_id.is_none_or(|v| v > 0)
                && audit.reason.as_ref().is_none_or(|v| v.len() <= 4096),
            MemberSyncError::Invalid
        );
        let source = CaseSource::DiscordAudit {
            entry_id: audit.entry_id,
        };
        let key = source.key()?;
        if super::case_sources::source_match(conn, scope, &key)?.is_some() {
            retain_confirmed_ban_history(conn, scope, &key)?;
            unchanged += 1;
            continue;
        }
        let action = match audit.action {
            MemberAuditAction::Ban => "ban",
            MemberAuditAction::Unban => "unban",
        };
        if audit.executor_id == Some(bot_id) {
            // The bot's application case may exist before its exact audit
            // source has been linked. Preserve the audit entry by its own ID;
            // only an exact later source link can consume this unresolved row.
            let prior=conn.query_row("SELECT user_id,action,occurred_at_ms,executor_id,reason FROM guild_member_unresolved_audits WHERE guild_id=?1 AND enrollment_id=?2 AND entry_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),audit.entry_id],|r|Ok((r.get::<i64>(0)?,r.get::<String>(1)?,r.get::<i64>(2)?,r.get::<Option<i64>>(3)?,r.get::<Option<String>>(4)?))).optional()?;
            if let Some((user_id, old_action, occurred, executor, reason)) = prior {
                anyhow::ensure!(
                    user_id == audit.user_id
                        && old_action == action
                        && occurred == audit.occurred_at_ms
                        && executor == audit.executor_id
                        && reason == audit.reason,
                    MemberSyncError::Invalid
                );
                unchanged += 1;
                continue;
            }
            conn.member_execute("INSERT INTO guild_member_unresolved_audits(guild_id,enrollment_id,entry_id,user_id,action,occurred_at_ms,executor_id,reason) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![scope.guild_id(),scope.enrollment.id.as_str(),audit.entry_id,audit.user_id,action,audit.occurred_at_ms,audit.executor_id,audit.reason.as_deref()])?;
        } else {
            let data = CaseData {
                ban_attempt_tag: None,
                source,
                action: match audit.action {
                    MemberAuditAction::Ban => CaseAction::Ban,
                    MemberAuditAction::Unban => CaseAction::Unban,
                },
                outcome: CaseOutcome::Succeeded,
                subject_id: audit.user_id,
                actor_id: audit.executor_id,
                occurred_at_ms: audit.occurred_at_ms,
                reason: audit.reason.clone(),
                evidence: CaseEvidence::Unavailable,
            };
            let payload = serde_json::to_string(&data)?;
            let case_id = random_id();
            conn.member_execute(
                "INSERT INTO guild_cases(guild_id,enrollment_id,case_id,source_key,payload,deleted) VALUES(?1,?2,?3,?4,?5,0)",
                params![
                    scope.guild_id(),
                    scope.enrollment.id.as_str(),
                    case_id.as_str(),
                    key,
                    payload.as_str()
                ],
            )?;
            conn.member_execute(
                "INSERT OR IGNORE INTO guild_member_case_index(guild_id,enrollment_id,user_id,case_id,action_kind,occurred_at_ms) SELECT ?1,?2,CAST(json_extract(?3,'$.subject_id') AS INTEGER),?4,json_extract(?3,'$.action'),CAST(json_extract(?3,'$.occurred_at_ms') AS INTEGER) WHERE json_valid(?3) AND json_extract(?3,'$.subject_id') IS NOT NULL AND json_extract(?3,'$.action') IS NOT NULL",
                params![
                    scope.guild_id(),
                    scope.enrollment.id.as_str(),
                    payload.as_str(),
                    case_id.as_str()
                ],
            )?;
        }
        conn.member_execute("INSERT INTO guild_member_directory(guild_id,enrollment_id,user_id,ever_banned,observed_at_ms) VALUES(?1,?2,?3,1,?4) ON CONFLICT(guild_id,enrollment_id,user_id) DO UPDATE SET ever_banned=1,cleanup_after_ms=NULL,revision=revision+1",params![scope.guild_id(),scope.enrollment.id.as_str(),audit.user_id,now_ms])?;
        inserted += 1;
        bump_revision(conn, scope)?;
    }
    Ok((inserted, unchanged))
}

fn upsert_member(
    conn: &impl SqlExecutor,
    scope: &GuildDataScope,
    obs: &MemberObservation,
    provenance: &str,
) -> Result<MemberUpsertOutcome> {
    anyhow::ensure!(
        obs.user_id > 0 && obs.observed_at_ms > 0 && obs.joined_at_ms.is_none_or(|v| v > 0),
        MemberSyncError::Invalid
    );
    let row=conn.query_row("SELECT username,global_name,nickname,is_current,joined_at_ms,current_stay_id,observed_at_ms,revision,departed_at_ms,cleanup_member_checked_at_ms,is_banned FROM guild_member_directory WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),obs.user_id],|r|Ok((r.get::<Option<String>>(0)?,r.get::<Option<String>>(1)?,r.get::<Option<String>>(2)?,r.get::<bool>(3)?,r.get::<Option<i64>>(4)?,r.get::<Option<String>>(5)?,r.get::<i64>(6)?,r.get::<i64>(7)?,r.get::<Option<i64>>(8)?,r.get::<Option<i64>>(9)?,r.get::<bool>(10)?))).optional()?;
    let Some((
        old_user,
        old_global,
        old_nick,
        current,
        old_join,
        old_stay,
        last_observed,
        revision,
        old_departed,
        checked_absent_at,
        was_banned,
    )) = row
    else {
        let username = bounded_name(obs.username.clone().flatten())?;
        let global = bounded_name(obs.global_name.clone().flatten())?;
        let nickname = bounded_name(obs.nickname.clone().flatten())?;
        let stay = random_id();
        let search_text = normalized_name(&username, &global, &nickname);
        conn.member_execute("INSERT INTO guild_member_directory(guild_id,enrollment_id,user_id,username,global_name,nickname,search_text,is_current,joined_at_ms,current_stay_id,observed_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,1,?8,?9,?10)",params![scope.guild_id(),scope.enrollment.id.as_str(),obs.user_id,username,global,nickname,search_text,obs.joined_at_ms,stay.as_str(),obs.observed_at_ms])?;
        conn.member_execute("INSERT INTO guild_member_stays(guild_id,enrollment_id,stay_id,user_id,joined_at_ms,provenance) VALUES(?1,?2,?3,?4,?5,?6)",params![scope.guild_id(),scope.enrollment.id.as_str(),stay,obs.user_id,obs.joined_at_ms,provenance])?;
        bump_revision(conn, scope)?;
        return Ok(MemberUpsertOutcome {
            membership_refresh: true,
        });
    };
    // An older in-flight observation cannot undo a newer Gateway mutation.
    if obs.observed_at_ms < last_observed {
        if provenance == "observed" {
            queue_target_check_tx(
                conn,
                scope,
                obs.user_id,
                MemberVerifyKind::Member,
                "member_order_uncertain",
                revision,
                obs.observed_at_ms,
            )?;
        }
        return Ok(MemberUpsertOutcome {
            membership_refresh: false,
        });
    }
    // A late payload from a prior stay may have a newer delivery timestamp.
    // Its names belong to that prior stay, not this current search row.
    let older_join = current
        && obs
            .joined_at_ms
            .zip(old_join)
            .is_some_and(|(new, old)| new < old);
    if older_join {
        if provenance == "observed" {
            queue_target_check_tx(
                conn,
                scope,
                obs.user_id,
                MemberVerifyKind::Member,
                "older_join_token",
                revision,
                obs.observed_at_ms,
            )?;
        }
        return Ok(MemberUpsertOutcome {
            membership_refresh: false,
        });
    }
    let username = bounded_name(obs.username.clone().unwrap_or(old_user.clone()))?;
    let global = bounded_name(obs.global_name.clone().unwrap_or(old_global.clone()))?;
    let nickname = bounded_name(obs.nickname.clone().unwrap_or(old_nick.clone()))?;
    // An observed join/update can establish a new stay only when its actual
    // Discord join token is later than every known boundary of the prior stay.
    // A delayed old-instance update must not resurrect a departed member.
    let no_prior_stay = old_join.is_none()
        && old_departed.is_none()
        && checked_absent_at.is_none()
        && old_stay.is_none();
    let newer_join = obs.joined_at_ms.is_some_and(|joined| {
        old_join.is_none_or(|old| joined > old)
            && old_departed.is_none_or(|departed| joined > departed)
            && checked_absent_at.is_none_or(|checked| joined > checked)
    });
    // A direct join event without Discord's timestamp still establishes a new
    // unknown-date stay when it arrives after the saved departure observation.
    let untimed_new_join =
        obs.is_join && obs.joined_at_ms.is_none() && obs.observed_at_ms > last_observed;
    let rejoin = !current
        && (provenance == "reconciled"
            || newer_join
            || untimed_new_join
            || obs.is_join && no_prior_stay);
    let uncertain_absent_update = !current && !rejoin && provenance == "observed";
    let changed_instance = current
        && obs
            .joined_at_ms
            .zip(old_join)
            .is_some_and(|(new, old)| new > old);
    let rotate = rejoin || changed_instance;
    let clear_ban = was_banned && (current || rejoin);
    let joined = if rotate {
        obs.joined_at_ms
    } else if !current || older_join {
        old_join
    } else {
        obs.joined_at_ms.or(old_join)
    };
    let changed = rotate
        || username != old_user
        || global != old_global
        || nickname != old_nick
        || joined != old_join
        || clear_ban;
    let stay = if rotate {
        Some(random_id())
    } else {
        old_stay.clone()
    };
    if rotate {
        let retained:bool=conn.query_row("SELECT ever_banned FROM guild_member_directory WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),obs.user_id],|r|Ok(r.get(0)?))?;
        if !retained {
            conn.member_execute("DELETE FROM guild_member_stays WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),obs.user_id])?;
            conn.member_execute("DELETE FROM guild_member_state_observations WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),obs.user_id])?;
        } else if changed_instance {
            if let Some(old_stay) = &old_stay {
                conn.member_execute("UPDATE guild_member_stays SET ended_unknown=1 WHERE guild_id=?1 AND enrollment_id=?2 AND stay_id=?3 AND departed_at_ms IS NULL",params![scope.guild_id(),scope.enrollment.id.as_str(),old_stay.as_str()])?;
            }
            conn.member_execute("INSERT INTO guild_member_state_observations(guild_id,enrollment_id,observation_id,user_id,kind,observed_at_ms) VALUES(?1,?2,?3,?4,'membership_changed',?5)",params![scope.guild_id(),scope.enrollment.id.as_str(),random_id(),obs.user_id,obs.observed_at_ms])?;
        }
        conn.member_execute("INSERT INTO guild_member_stays(guild_id,enrollment_id,stay_id,user_id,joined_at_ms,provenance) VALUES(?1,?2,?3,?4,?5,?6)",params![scope.guild_id(),scope.enrollment.id.as_str(),stay.as_deref(),obs.user_id,obs.joined_at_ms,format!("{provenance}_rejoin")])?;
    } else if current
        && old_join.is_none()
        && joined.is_some()
        && let Some(stay) = &stay
    {
        conn.member_execute("UPDATE guild_member_stays SET joined_at_ms=?4 WHERE guild_id=?1 AND enrollment_id=?2 AND stay_id=?3 AND joined_at_ms IS NULL",params![scope.guild_id(),scope.enrollment.id.as_str(),stay.as_str(),joined])?;
    }
    let advanced_observation = !changed && obs.observed_at_ms > last_observed;
    if changed {
        let search_text = normalized_name(&username, &global, &nickname);
        conn.member_execute("UPDATE guild_member_directory SET username=?4,global_name=?5,nickname=?6,search_text=?7,is_current=?8,joined_at_ms=?9,departed_at_ms=CASE WHEN ?8 THEN NULL ELSE departed_at_ms END,current_stay_id=?10,cleanup_after_ms=CASE WHEN ?8 THEN NULL ELSE cleanup_after_ms END,observed_at_ms=?11,is_banned=CASE WHEN ?12 THEN 0 ELSE is_banned END,current_ban_reason=CASE WHEN ?12 THEN NULL ELSE current_ban_reason END,ban_observed_at_ms=CASE WHEN ?12 THEN MAX(coalesce(ban_observed_at_ms,0),?11) ELSE ban_observed_at_ms END,revision=revision+1 WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),obs.user_id,username,global,nickname,search_text,current||rejoin,joined,stay,obs.observed_at_ms,clear_ban])?;
        bump_revision(conn, scope)?;
    } else if advanced_observation {
        // A later positive observation also invalidates an in-flight absence check.
        conn.member_execute("UPDATE guild_member_directory SET observed_at_ms=?4,revision=revision+1 WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),obs.user_id,obs.observed_at_ms])?;
        bump_revision(conn, scope)?;
    }
    if clear_ban {
        queue_target_check_tx(
            conn,
            scope,
            obs.user_id,
            MemberVerifyKind::Ban,
            "member_present_ban_state_uncertain",
            revision + i64::from(changed || advanced_observation),
            obs.observed_at_ms,
        )?;
    }
    if uncertain_absent_update {
        queue_target_check_tx(
            conn,
            scope,
            obs.user_id,
            MemberVerifyKind::Member,
            "positive_update_join_uncertain",
            revision + i64::from(changed || advanced_observation),
            obs.observed_at_ms,
        )?;
    }
    Ok(MemberUpsertOutcome {
        membership_refresh: rotate,
    })
}

// Bound each SQL statement, while the outer page transaction still commits one
// complete Discord page and cursor together. This also bounds escaped JSON input.
pub(crate) const MEMBER_PAGE_WRITE_BATCH_SIZE: usize = 16;

/// One scoped transaction and bounded set-based writes per Discord page.
fn upsert_member_page(
    conn: &impl SqlExecutor,
    scope: &GuildDataScope,
    members: &[MemberObservation],
) -> Result<(i64, i64, i64)> {
    let mut totals = (0, 0, 0);
    for batch in members.chunks(MEMBER_PAGE_WRITE_BATCH_SIZE) {
        let (inserted, updated, unchanged) = upsert_member_batch(conn, scope, batch)?;
        totals.0 += inserted;
        totals.1 += updated;
        totals.2 += unchanged;
    }
    Ok(totals)
}

fn upsert_member_batch(
    conn: &impl SqlExecutor,
    scope: &GuildDataScope,
    members: &[MemberObservation],
) -> Result<(i64, i64, i64)> {
    if members.is_empty() {
        return Ok((0, 0, 0));
    }
    let ids: Vec<i64> = members.iter().map(|m| m.user_id).collect();
    let ids_json = serde_json::to_string(&ids)?;
    note_member_sync_statement(scope.guild_id());
    let mut stmt=conn.prepare("SELECT user_id,username,global_name,nickname,is_current,joined_at_ms,current_stay_id,observed_at_ms,ever_banned,revision,departed_at_ms,cleanup_member_checked_at_ms,is_banned FROM guild_member_directory WHERE guild_id=?1 AND enrollment_id=?2 AND user_id IN (SELECT CAST(value AS INTEGER) FROM json_each(?3))")?;
    let rows = stmt.query_map(
        params![scope.guild_id(), scope.enrollment.id.as_str(), ids_json],
        |r| {
            Ok((
                r.get::<i64>(0)?,
                r.get::<Option<String>>(1)?,
                r.get::<Option<String>>(2)?,
                r.get::<Option<String>>(3)?,
                r.get::<bool>(4)?,
                r.get::<Option<i64>>(5)?,
                r.get::<Option<String>>(6)?,
                r.get::<i64>(7)?,
                r.get::<bool>(8)?,
                r.get::<i64>(9)?,
                r.get::<Option<i64>>(10)?,
                r.get::<Option<i64>>(11)?,
                r.get::<bool>(12)?,
            ))
        },
    )?;
    let mut existing = HashMap::new();
    for row in rows {
        let row = row?;
        existing.insert(row.0, row);
    }
    drop(stmt);
    let mut changed = Vec::new();
    let mut observed_only = Vec::new();
    let mut inserted = 0;
    let mut updated = 0;
    let mut unchanged = 0;
    let mut advanced = false;
    for obs in members {
        anyhow::ensure!(
            obs.user_id > 0 && obs.observed_at_ms > 0 && obs.joined_at_ms.is_none_or(|v| v > 0),
            MemberSyncError::Invalid
        );
        if let Some((
            _,
            old_user,
            old_global,
            old_nick,
            current,
            old_join,
            old_stay,
            old_observed,
            ever_banned,
            revision,
            old_departed,
            checked_absent_at,
            was_banned,
        )) = existing.get(&obs.user_id)
        {
            if obs.observed_at_ms < *old_observed {
                unchanged += 1;
                continue;
            }
            let username = bounded_name(obs.username.clone().unwrap_or(old_user.clone()))?;
            let global = bounded_name(obs.global_name.clone().unwrap_or(old_global.clone()))?;
            let nickname = bounded_name(obs.nickname.clone().unwrap_or(old_nick.clone()))?;
            let no_prior_stay = old_join.is_none()
                && old_departed.is_none()
                && checked_absent_at.is_none()
                && old_stay.is_none();
            let newer_join = obs.joined_at_ms.is_some_and(|joined| {
                old_join.is_none_or(|old| joined > old)
                    && old_departed.is_none_or(|departed| joined > departed)
                    && checked_absent_at.is_none_or(|checked| joined > checked)
            });
            let rejoin = !*current && (newer_join || obs.is_join && no_prior_stay);
            let older_current = *current
                && obs
                    .joined_at_ms
                    .zip(*old_join)
                    .is_some_and(|(new, old)| new < old);
            if older_current || !*current && !rejoin {
                queue_target_check_tx(
                    conn,
                    scope,
                    obs.user_id,
                    MemberVerifyKind::Member,
                    "page_join_uncertain",
                    *revision,
                    obs.observed_at_ms,
                )?;
                unchanged += 1;
                continue;
            }
            let changed_instance = *current
                && obs
                    .joined_at_ms
                    .zip(*old_join)
                    .is_some_and(|(new, old)| new > old);
            let rotate = rejoin || changed_instance;
            let joined = if rotate {
                obs.joined_at_ms
            } else {
                obs.joined_at_ms.or(*old_join)
            };
            let is_current = *current || rejoin;
            let clear_ban = *was_banned && is_current;
            let changed_row = rotate
                || username != *old_user
                || global != *old_global
                || nickname != *old_nick
                || joined != *old_join
                || clear_ban;
            if !changed_row {
                if obs.observed_at_ms > *old_observed {
                    observed_only.push(serde_json::json!({
                        "id": obs.user_id,
                        "observed_at_ms": obs.observed_at_ms,
                    }));
                    advanced = true;
                }
                unchanged += 1;
                continue;
            }
            let stay_id = if rotate {
                Some(random_id())
            } else {
                old_stay.clone()
            };
            changed.push(serde_json::json!({"id":obs.user_id,"username":username,"global_name":global,"nickname":nickname,"search_text":normalized_name(&username,&global,&nickname),"is_current":is_current,"joined_at_ms":joined,"current_stay_id":stay_id,"observed_at_ms":obs.observed_at_ms,"revision":revision+1,"clear_ban":clear_ban,"new_stay_id":if rotate{stay_id.clone()}else{None},"new_stay_joined_at_ms":obs.joined_at_ms,"new_stay_provenance":if rotate{"imported_rejoin"}else{"imported"},"purge_prior":rotate && !*ever_banned,"old_stay_id":if changed_instance && *ever_banned{old_stay.clone()}else{None},"unknown_observation_id":if changed_instance && *ever_banned{Some(random_id())}else{None},"fill_stay_id":if *current && old_join.is_none() && joined.is_some(){old_stay.clone()}else{None},"fill_joined_at_ms":joined}));
            updated += 1;
        } else {
            let username = bounded_name(obs.username.clone().flatten())?;
            let global = bounded_name(obs.global_name.clone().flatten())?;
            let nickname = bounded_name(obs.nickname.clone().flatten())?;
            let stay = random_id();
            changed.push(serde_json::json!({"id":obs.user_id,"username":username,"global_name":global,"nickname":nickname,"search_text":normalized_name(&username,&global,&nickname),"is_current":true,"joined_at_ms":obs.joined_at_ms,"current_stay_id":stay,"observed_at_ms":obs.observed_at_ms,"revision":0,"clear_ban":false,"new_stay_id":stay,"new_stay_joined_at_ms":obs.joined_at_ms,"new_stay_provenance":"imported","purge_prior":false,"old_stay_id":null,"unknown_observation_id":null,"fill_stay_id":null,"fill_joined_at_ms":null}));
            inserted += 1;
        }
    }
    if !observed_only.is_empty() {
        let data = serde_json::to_string(&observed_only)?;
        note_member_sync_statement(scope.guild_id());
        conn.member_execute("UPDATE guild_member_directory SET observed_at_ms=(SELECT CAST(json_extract(value,'$.observed_at_ms') AS INTEGER) FROM json_each(?3) WHERE CAST(json_extract(value,'$.id') AS INTEGER)=guild_member_directory.user_id),revision=revision+1 WHERE guild_id=?1 AND enrollment_id=?2 AND user_id IN (SELECT CAST(json_extract(value,'$.id') AS INTEGER) FROM json_each(?3))",params![scope.guild_id(),scope.enrollment.id.as_str(),data])?;
    }
    if changed.is_empty() {
        if advanced {
            bump_revision(conn, scope)?;
        }
        return Ok((inserted, updated, unchanged));
    }
    let data = serde_json::to_string(&changed)?;
    let row_bound = u64::try_from(changed.len()).map_err(|_| MemberSyncError::Invalid)?;
    let guild_id = scope.guild_id();
    note_member_sync_statement(guild_id);
    conn.member_execute("DELETE FROM guild_member_stays WHERE guild_id=?1 AND enrollment_id=?2 AND user_id IN (SELECT CAST(json_extract(value,'$.id') AS INTEGER) FROM json_each(?3) WHERE json_extract(value,'$.purge_prior')=1)",params![scope.guild_id(),scope.enrollment.id.as_str(),data.as_str()])?;
    note_member_sync_statement(guild_id);
    conn.member_execute("DELETE FROM guild_member_state_observations WHERE guild_id=?1 AND enrollment_id=?2 AND user_id IN (SELECT CAST(json_extract(value,'$.id') AS INTEGER) FROM json_each(?3) WHERE json_extract(value,'$.purge_prior')=1)",params![scope.guild_id(),scope.enrollment.id.as_str(),data.as_str()])?;
    note_member_sync_statement(guild_id);
    conn.member_execute("UPDATE guild_member_stays SET ended_unknown=1 WHERE guild_id=?1 AND enrollment_id=?2 AND departed_at_ms IS NULL AND stay_id IN (SELECT json_extract(value,'$.old_stay_id') FROM json_each(?3) WHERE json_extract(value,'$.old_stay_id') IS NOT NULL)",params![scope.guild_id(),scope.enrollment.id.as_str(),data.as_str()])?;
    note_member_sync_statement(guild_id);
    conn.member_execute_rows("INSERT INTO guild_member_state_observations(guild_id,enrollment_id,observation_id,user_id,kind,observed_at_ms) SELECT ?1,?2,json_extract(value,'$.unknown_observation_id'),CAST(json_extract(value,'$.id') AS INTEGER),'membership_changed',CAST(json_extract(value,'$.observed_at_ms') AS INTEGER) FROM json_each(?3) WHERE json_extract(value,'$.unknown_observation_id') IS NOT NULL",params![scope.guild_id(),scope.enrollment.id.as_str(),data.as_str()],row_bound)?;
    note_member_sync_statement(guild_id);
    conn.member_execute_rows("INSERT INTO guild_member_stays(guild_id,enrollment_id,stay_id,user_id,joined_at_ms,provenance) SELECT ?1,?2,json_extract(value,'$.new_stay_id'),CAST(json_extract(value,'$.id') AS INTEGER),CAST(json_extract(value,'$.new_stay_joined_at_ms') AS INTEGER),json_extract(value,'$.new_stay_provenance') FROM json_each(?3) WHERE json_extract(value,'$.new_stay_id') IS NOT NULL",params![scope.guild_id(),scope.enrollment.id.as_str(),data.as_str()],row_bound)?;
    note_member_sync_statement(guild_id);
    conn.member_execute("UPDATE guild_member_stays SET joined_at_ms=(SELECT CAST(json_extract(value,'$.fill_joined_at_ms') AS INTEGER) FROM json_each(?3) WHERE json_extract(value,'$.fill_stay_id')=guild_member_stays.stay_id) WHERE guild_id=?1 AND enrollment_id=?2 AND joined_at_ms IS NULL AND stay_id IN (SELECT json_extract(value,'$.fill_stay_id') FROM json_each(?3) WHERE json_extract(value,'$.fill_stay_id') IS NOT NULL)",params![scope.guild_id(),scope.enrollment.id.as_str(),data.as_str()])?;
    note_member_sync_statement(guild_id);
    conn.member_execute_rows("INSERT INTO guild_member_directory(guild_id,enrollment_id,user_id,username,global_name,nickname,search_text,is_current,joined_at_ms,current_stay_id,observed_at_ms,revision) SELECT ?1,?2,CAST(json_extract(value,'$.id') AS INTEGER),json_extract(value,'$.username'),json_extract(value,'$.global_name'),json_extract(value,'$.nickname'),json_extract(value,'$.search_text'),CAST(json_extract(value,'$.is_current') AS INTEGER),CAST(json_extract(value,'$.joined_at_ms') AS INTEGER),json_extract(value,'$.current_stay_id'),CAST(json_extract(value,'$.observed_at_ms') AS INTEGER),CAST(json_extract(value,'$.revision') AS INTEGER) FROM json_each(?3) WHERE true ON CONFLICT(guild_id,enrollment_id,user_id) DO UPDATE SET username=excluded.username,global_name=excluded.global_name,nickname=excluded.nickname,search_text=excluded.search_text,is_current=excluded.is_current,joined_at_ms=excluded.joined_at_ms,current_stay_id=excluded.current_stay_id,departed_at_ms=CASE WHEN excluded.is_current=1 THEN NULL ELSE guild_member_directory.departed_at_ms END,cleanup_after_ms=CASE WHEN excluded.is_current=1 THEN NULL ELSE guild_member_directory.cleanup_after_ms END,observed_at_ms=excluded.observed_at_ms,revision=excluded.revision",params![scope.guild_id(),scope.enrollment.id.as_str(),data.as_str()],row_bound)?;
    note_member_sync_statement(guild_id);
    conn.member_execute("UPDATE guild_member_directory SET is_banned=0,current_ban_reason=NULL,ban_observed_at_ms=MAX(coalesce(ban_observed_at_ms,0),observed_at_ms) WHERE guild_id=?1 AND enrollment_id=?2 AND user_id IN (SELECT CAST(json_extract(value,'$.id') AS INTEGER) FROM json_each(?3) WHERE json_extract(value,'$.clear_ban')=1)",params![scope.guild_id(),scope.enrollment.id.as_str(),data.as_str()])?;
    note_member_sync_statement(guild_id);
    conn.member_execute_rows("INSERT INTO guild_member_target_checks(guild_id,enrollment_id,user_id,kind,reason,row_revision,requested_at_ms,next_attempt_at_ms) SELECT ?1,?2,CAST(json_extract(value,'$.id') AS INTEGER),'ban','member_present_ban_state_uncertain',CAST(json_extract(value,'$.revision') AS INTEGER),CAST(json_extract(value,'$.observed_at_ms') AS INTEGER),CAST(json_extract(value,'$.observed_at_ms') AS INTEGER) FROM json_each(?3) WHERE json_extract(value,'$.clear_ban')=1 ON CONFLICT(guild_id,enrollment_id,user_id,kind) DO UPDATE SET reason=excluded.reason,row_revision=excluded.row_revision,requested_at_ms=MAX(guild_member_target_checks.requested_at_ms+1,excluded.requested_at_ms),next_attempt_at_ms=MIN(guild_member_target_checks.next_attempt_at_ms,excluded.next_attempt_at_ms),expected_stay_id=NULL",params![scope.guild_id(),scope.enrollment.id.as_str(),data.as_str()],row_bound)?;
    bump_revision(conn, scope)?;
    Ok((inserted, updated, unchanged))
}

fn upsert_ban_page(
    conn: &impl SqlExecutor,
    scope: &GuildDataScope,
    bans: &[MemberBanSnapshot],
    now_ms: i64,
) -> Result<(i64, i64, i64)> {
    let mut totals = (0, 0, 0);
    for batch in bans.chunks(MEMBER_PAGE_WRITE_BATCH_SIZE) {
        let (inserted, updated, unchanged) = upsert_ban_batch(conn, scope, batch, now_ms)?;
        totals.0 += inserted;
        totals.1 += updated;
        totals.2 += unchanged;
    }
    Ok(totals)
}

fn upsert_ban_batch(
    conn: &impl SqlExecutor,
    scope: &GuildDataScope,
    bans: &[MemberBanSnapshot],
    now_ms: i64,
) -> Result<(i64, i64, i64)> {
    if bans.is_empty() {
        return Ok((0, 0, 0));
    }
    let ids = serde_json::to_string(&bans.iter().map(|b| b.user_id).collect::<Vec<_>>())?;
    let mut stmt=conn.prepare("SELECT user_id,is_banned,username,global_name,current_ban_reason,nickname,is_current,ban_observed_at_ms FROM guild_member_directory WHERE guild_id=?1 AND enrollment_id=?2 AND user_id IN (SELECT CAST(value AS INTEGER) FROM json_each(?3))")?;
    let rows = stmt.query_map(
        params![scope.guild_id(), scope.enrollment.id.as_str(), ids],
        |r| {
            Ok((
                r.get::<i64>(0)?,
                r.get::<bool>(1)?,
                r.get::<Option<String>>(2)?,
                r.get::<Option<String>>(3)?,
                r.get::<Option<String>>(4)?,
                r.get::<Option<String>>(5)?,
                r.get::<bool>(6)?,
                r.get::<Option<i64>>(7)?,
            ))
        },
    )?;
    let mut existing = HashMap::new();
    for row in rows {
        let row = row?;
        existing.insert(row.0, row);
    }
    drop(stmt);
    let mut changed = Vec::new();
    let mut observed_only = Vec::new();
    let mut inserted = 0;
    let mut updated = 0;
    let mut unchanged = 0;
    let mut advanced = false;
    for ban in bans {
        anyhow::ensure!(ban.user_id > 0, MemberSyncError::Invalid);
        anyhow::ensure!(
            ban.reason.as_ref().is_none_or(|v| v.len() <= 4096),
            MemberSyncError::Invalid
        );
        let old = existing.get(&ban.user_id);
        let username = bounded_name(
            ban.username
                .clone()
                .or_else(|| old.and_then(|r| r.2.clone())),
        )?;
        let global = bounded_name(
            ban.global_name
                .clone()
                .or_else(|| old.and_then(|r| r.3.clone())),
        )?;
        if old.is_some_and(|r| r.1 && !r.6 && r.2 == username && r.3 == global && r.4 == ban.reason)
        {
            if old.is_some_and(|r| now_ms > r.7.unwrap_or(0)) {
                observed_only.push(ban.user_id);
                advanced = true;
            }
            unchanged += 1;
            continue;
        }
        if old.is_none() {
            inserted += 1
        } else {
            updated += 1
        }
        let nickname = old.and_then(|r| r.5.clone());
        let search_text = normalized_name(&username, &global, &nickname);
        changed.push(serde_json::json!({"id":ban.user_id,"username":username,"global_name":global,"search_text":search_text,"reason":ban.reason,"new_ban_state":old.is_none_or(|r|!r.1)}));
    }
    if !observed_only.is_empty() {
        let ids = serde_json::to_string(&observed_only)?;
        note_member_sync_statement(scope.guild_id());
        conn.member_execute("UPDATE guild_member_directory SET observed_at_ms=MAX(observed_at_ms,?4),ban_observed_at_ms=?4,revision=revision+1 WHERE guild_id=?1 AND enrollment_id=?2 AND user_id IN (SELECT CAST(value AS INTEGER) FROM json_each(?3))",params![scope.guild_id(),scope.enrollment.id.as_str(),ids,now_ms])?;
    }
    if changed.is_empty() {
        if advanced {
            bump_revision(conn, scope)?;
        }
        return Ok((inserted, updated, unchanged));
    }
    let data = serde_json::to_string(&changed)?;
    let row_bound = u64::try_from(changed.len()).map_err(|_| MemberSyncError::Invalid)?;
    conn.member_execute_rows("INSERT INTO guild_member_state_observations(guild_id,enrollment_id,observation_id,user_id,kind,observed_at_ms) SELECT ?1,?2,lower(hex(randomblob(16))),CAST(json_extract(value,'$.id') AS INTEGER),'ban_present',?4 FROM json_each(?3) WHERE json_extract(value,'$.new_ban_state')=1 AND coalesce((SELECT i.action_kind FROM guild_member_case_index i JOIN guild_cases c USING(guild_id,enrollment_id,case_id) WHERE i.guild_id=?1 AND i.enrollment_id=?2 AND i.user_id=CAST(json_extract(value,'$.id') AS INTEGER) AND c.deleted=0 AND json_valid(c.payload) AND json_extract(c.payload,'$.outcome')='succeeded' ORDER BY i.occurred_at_ms DESC,i.case_id DESC LIMIT 1),'')!='ban'",params![scope.guild_id(),scope.enrollment.id.as_str(),data.as_str(),now_ms],row_bound)?;
    conn.member_execute("UPDATE guild_member_stays SET ended_unknown=1 WHERE guild_id=?1 AND enrollment_id=?2 AND departed_at_ms IS NULL AND stay_id IN (SELECT d.current_stay_id FROM guild_member_directory d WHERE d.guild_id=?1 AND d.enrollment_id=?2 AND d.is_current=1 AND d.user_id IN (SELECT CAST(json_extract(value,'$.id') AS INTEGER) FROM json_each(?3)))",params![scope.guild_id(),scope.enrollment.id.as_str(),data.as_str()])?;
    conn.member_execute("INSERT INTO guild_member_state_observations(guild_id,enrollment_id,observation_id,user_id,kind,observed_at_ms) SELECT ?1,?2,lower(hex(randomblob(16))),d.user_id,'member_absent',?4 FROM guild_member_directory d WHERE d.guild_id=?1 AND d.enrollment_id=?2 AND d.is_current=1 AND d.user_id IN (SELECT CAST(json_extract(value,'$.id') AS INTEGER) FROM json_each(?3))",params![scope.guild_id(),scope.enrollment.id.as_str(),data.as_str(),now_ms])?;
    conn.member_execute_rows("INSERT INTO guild_member_directory(guild_id,enrollment_id,user_id,username,global_name,search_text,is_current,is_banned,ever_banned,current_ban_reason,observed_at_ms,ban_observed_at_ms) SELECT ?1,?2,CAST(json_extract(value,'$.id') AS INTEGER),json_extract(value,'$.username'),json_extract(value,'$.global_name'),json_extract(value,'$.search_text'),0,1,1,json_extract(value,'$.reason'),?4,?4 FROM json_each(?3) WHERE true ON CONFLICT(guild_id,enrollment_id,user_id) DO UPDATE SET is_current=0,departed_at_ms=NULL,is_banned=1,ever_banned=1,username=excluded.username,global_name=excluded.global_name,search_text=excluded.search_text,current_ban_reason=excluded.current_ban_reason,cleanup_after_ms=NULL,observed_at_ms=MAX(guild_member_directory.observed_at_ms,?4),ban_observed_at_ms=?4,revision=guild_member_directory.revision+1",params![scope.guild_id(),scope.enrollment.id.as_str(),data,now_ms],row_bound)?;
    bump_revision(conn, scope)?;
    Ok((inserted, updated, unchanged))
}

impl GuildDataStore {
    pub async fn claim_member_audit_recovery(
        &self,
        scope: GuildDataScope,
        bot_id: i64,
        worker_id: String,
        now_ms: i64,
        lease_ms: i64,
    ) -> Result<Option<MemberAuditRecoveryClaim>> {
        checked_scope(self, &scope)?;
        anyhow::ensure!(
            bot_id > 0
                && now_ms > 0
                && (15_000..=300_000).contains(&lease_ms)
                && !worker_id.is_empty()
                && worker_id.len() <= 64,
            MemberSyncError::Invalid
        );
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn|{let mut tx=conn.transaction_primary_read()?;verify_scope(&tx,&expected,&scope,true)?;
            let completed=ensure_seed_bot(&tx,&scope,bot_id)?.flatten();
            if completed.is_none(){tx.finish_read()?;return Ok(None)}
            let row=tx.query_row("SELECT run_id,cursor,generation,lease_owner,lease_until_ms,next_attempt_at_ms,request_after_ms,status,started_at_ms,status_reason FROM guild_member_audit_recovery WHERE guild_id=?1 AND enrollment_id=?2 AND bot_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),bot_id],|r|Ok((r.get::<String>(0)?,r.get::<Option<String>>(1)?,r.get::<i64>(2)?,r.get::<Option<String>>(3)?,r.get::<Option<i64>>(4)?,r.get::<Option<i64>>(5)?,r.get::<Option<i64>>(6)?,r.get::<String>(7)?,r.get::<i64>(8)?,r.get::<Option<String>>(9)?))).optional()?;
            let Some((run_id,cursor,generation,owner,until,next,request_after,state,started_at,status_reason))=row else {tx.finish_read()?;return Ok(None)};
            if state=="needs_attention" && status_reason.as_deref()==Some("pagination_stalled") {tx.finish_read()?;return Ok(None)}
            if !matches!(state.as_str(),"queued"|"running"|"waiting"|"needs_attention") || next.is_some_and(|v|v>now_ms) || request_after.is_some_and(|v|v>now_ms) || owner.is_some()&&until.is_some_and(|v|v>now_ms) {tx.finish_read()?;return Ok(None)}
            tx.ensure_mutation_receipt()?;let generation=generation+1;
            tx.member_execute("UPDATE guild_member_audit_recovery SET status='running',lease_owner=?4,lease_until_ms=?5,generation=?6 WHERE guild_id=?1 AND enrollment_id=?2 AND bot_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),bot_id,worker_id.as_str(),now_ms.saturating_add(lease_ms),generation])?;
            tx.commit()?;Ok(Some(MemberAuditRecoveryClaim{bot_id,run_id,cursor,generation,worker_id,not_before_ms:started_at.saturating_sub(45*86_400_000)}))
        }).await
    }

    pub async fn commit_member_audit_recovery_page(
        &self,
        scope: GuildDataScope,
        claim: MemberAuditRecoveryClaim,
        audits: Vec<MemberAuditSnapshot>,
        page_position: (Option<String>, bool),
        now_ms: i64,
        pace_ms: i64,
    ) -> Result<()> {
        let (next_cursor, terminal) = page_position;
        checked_scope(self, &scope)?;
        anyhow::ensure!(
            now_ms > 0
                && pace_ms >= 15_000
                && audits.len() <= 50
                && next_cursor.as_ref().is_none_or(|v| v.len() <= 80),
            MemberSyncError::Invalid
        );
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn|{let mut tx=conn.transaction_primary_read()?;verify_scope(&tx,&expected,&scope,true)?;ensure_seed_bot(&tx,&scope,claim.bot_id)?;
            let row=tx.query_row("SELECT run_id,cursor,generation,lease_owner,lease_until_ms FROM guild_member_audit_recovery WHERE guild_id=?1 AND enrollment_id=?2 AND bot_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),claim.bot_id],|r|Ok((r.get::<String>(0)?,r.get::<Option<String>>(1)?,r.get::<i64>(2)?,r.get::<Option<String>>(3)?,r.get::<Option<i64>>(4)?))).optional()?;
            anyhow::ensure!(row.is_some_and(|(run,cursor,generation,owner,until)|run==claim.run_id && cursor==claim.cursor && generation==claim.generation && owner.as_deref()==Some(claim.worker_id.as_str()) && until.is_some_and(|v|v>=now_ms)),MemberSyncError::Stale);
            tx.ensure_mutation_receipt()?;ingest_member_audits(&tx,&scope,claim.bot_id,&audits,now_ms)?;
            let ban_stage=!claim.cursor.as_deref().is_some_and(|c|c.starts_with("unban:"));
            let followup:Option<i64>=tx.query_row("SELECT restart_requested_at_ms FROM guild_member_audit_recovery WHERE guild_id=?1",params![scope.guild_id()],|r|Ok(r.get(0)?))?;
            let restart=terminal&&!ban_stage&&followup.is_some();
            let completed=terminal&&!ban_stage&&!restart;
            let cursor=if terminal&&ban_stage {Some("unban:".to_string())}else if terminal{None}else{next_cursor};
            tx.member_execute("UPDATE guild_member_audit_recovery SET cursor=?4,status=?5,status_reason=NULL,lease_owner=NULL,lease_until_ms=NULL,next_attempt_at_ms=?6,request_after_ms=?6,processed=CASE WHEN ?9 THEN 0 ELSE processed+?7 END,last_progress_at_ms=?8,run_id=CASE WHEN ?9 THEN lower(hex(randomblob(16))) ELSE run_id END,started_at_ms=CASE WHEN ?9 THEN ?8 ELSE started_at_ms END,generation=CASE WHEN ?9 THEN generation+1 ELSE generation END,restart_requested_at_ms=CASE WHEN ?9 THEN NULL ELSE restart_requested_at_ms END WHERE guild_id=?1 AND enrollment_id=?2 AND bot_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),claim.bot_id,cursor,if completed{"complete"}else{"queued"},if completed{None}else{Some(now_ms.saturating_add(pace_ms))},audits.len() as i64,now_ms,restart])?;
            tx.commit()
        }).await
    }

    pub async fn defer_member_audit_recovery(
        &self,
        scope: GuildDataScope,
        claim: MemberAuditRecoveryClaim,
        reason: String,
        next_attempt_at_ms: i64,
        now_ms: i64,
    ) -> Result<()> {
        checked_scope(self, &scope)?;
        anyhow::ensure!(
            reason.len() <= 160 && next_attempt_at_ms >= now_ms,
            MemberSyncError::Invalid
        );
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn|{let mut tx=conn.transaction_primary_read()?;verify_scope(&tx,&expected,&scope,true)?;tx.ensure_mutation_receipt()?;
            let state=if reason=="permission_denied"||reason=="pagination_stalled"{"needs_attention"}else{"waiting"};
            let changed=tx.member_execute("UPDATE guild_member_audit_recovery SET status=?9,status_reason=?4,next_attempt_at_ms=?5,request_after_ms=MAX(coalesce(request_after_ms,0),?5),lease_owner=NULL,lease_until_ms=NULL WHERE guild_id=?1 AND enrollment_id=?2 AND bot_id=?3 AND run_id=?6 AND generation=?7 AND lease_owner=?8",params![scope.guild_id(),scope.enrollment.id.as_str(),claim.bot_id,reason,next_attempt_at_ms,claim.run_id,claim.generation,claim.worker_id,state])?;
            anyhow::ensure!(changed==1,MemberSyncError::Stale);tx.commit()
        }).await
    }

    pub async fn queue_member_target_check(
        &self,
        scope: GuildDataScope,
        user_id: i64,
        kind: MemberVerifyKind,
        reason: &str,
        now_ms: i64,
    ) -> Result<()> {
        checked_scope(self, &scope)?;
        anyhow::ensure!(
            user_id > 0 && now_ms > 0 && !reason.is_empty() && reason.len() <= 160,
            MemberSyncError::Invalid
        );
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        let reason = reason.to_string();
        self.db.with_conn(move |conn|{let mut tx=conn.transaction_primary_read()?;verify_scope(&tx,&expected,&scope,true)?;tx.ensure_mutation_receipt()?;
            let revision:i64=tx.query_row("SELECT revision FROM guild_member_directory WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),user_id],|r|Ok(r.get(0)?)).optional()?.unwrap_or(-1);
            queue_target_check_tx(&tx,&scope,user_id,kind,&reason,revision,now_ms)?;tx.commit()
        }).await
    }

    /// Refresh a stale check only while it is still the same queued work item.
    /// A newer affected-user event may have replaced it during the HTTP request.
    pub async fn refresh_stale_member_target_check(
        &self,
        scope: GuildDataScope,
        check: MemberTargetCheck,
        now_ms: i64,
    ) -> Result<bool> {
        checked_scope(self, &scope)?;
        anyhow::ensure!(now_ms > 0, MemberSyncError::Invalid);
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| {let mut tx=conn.transaction_primary_read()?;verify_scope(&tx,&expected,&scope,true)?;
            let saved=tx.query_row("SELECT row_revision,reason,requested_at_ms,expected_stay_id FROM guild_member_target_checks WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3 AND kind=?4",params![scope.guild_id(),scope.enrollment.id.as_str(),check.user_id,check.kind.as_str()],|r|Ok((r.get::<i64>(0)?,r.get::<String>(1)?,r.get::<i64>(2)?,r.get::<Option<String>>(3)?))).optional()?;
            if !saved.as_ref().is_some_and(|(revision,reason,requested,stay)| *revision==check.revision && reason==&check.reason && *requested==check.requested_at_ms && stay==&check.expected_stay_id) {
                tx.finish_read()?;return Ok(false);
            }
            let revision:i64=tx.query_row("SELECT revision FROM guild_member_directory WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),check.user_id],|r|Ok(r.get(0)?)).optional()?.unwrap_or(-1);
            if revision==check.revision {tx.finish_read()?;return Ok(false);}
            tx.ensure_mutation_receipt()?;
            queue_target_check_tx(
                &tx,
                &scope,
                check.user_id,
                check.kind,
                &check.reason,
                revision,
                check.requested_at_ms,
            )?;
            tx.commit()?;Ok(true)
        }).await
    }

    pub async fn next_member_target_check(
        &self,
        scope: GuildDataScope,
        now_ms: i64,
    ) -> Result<Option<MemberTargetCheck>> {
        checked_scope(self, &scope)?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn|{let tx=conn.transaction_primary_snapshot()?;verify_scope(&tx,&expected,&scope,true)?;
            let row=tx.query_row("SELECT user_id,kind,row_revision,reason,requested_at_ms,expected_stay_id FROM guild_member_target_checks WHERE guild_id=?1 AND enrollment_id=?2 AND next_attempt_at_ms<=?3 ORDER BY next_attempt_at_ms,user_id LIMIT 1",params![scope.guild_id(),scope.enrollment.id.as_str(),now_ms],|r|Ok((r.get::<i64>(0)?,r.get::<String>(1)?,r.get::<i64>(2)?,r.get::<String>(3)?,r.get::<i64>(4)?,r.get::<Option<String>>(5)?))).optional()?;
            tx.finish_read()?;row.map(|(user_id,kind,revision,reason,requested_at_ms,expected_stay_id)|Ok(MemberTargetCheck{user_id,kind:MemberVerifyKind::parse(&kind)?,revision,reason,requested_at_ms,expected_stay_id})).transpose()
        }).await
    }

    pub async fn settle_member_target_check(
        &self,
        scope: GuildDataScope,
        check: MemberTargetCheck,
        verification: MemberVerification,
        now_ms: i64,
    ) -> Result<Option<MemberDeparture>> {
        checked_scope(self, &scope)?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        anyhow::ensure!(
            check.user_id == verification.user_id && check.kind == verification.kind,
            MemberSyncError::Invalid
        );
        let expected = self.expected.clone();
        self.db.with_conn(move |conn|{let mut tx=conn.transaction_primary_read()?;verify_scope(&tx,&expected,&scope,true)?;
            let saved=tx.query_row("SELECT row_revision,reason,requested_at_ms,expected_stay_id FROM guild_member_target_checks WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3 AND kind=?4",params![scope.guild_id(),scope.enrollment.id.as_str(),check.user_id,check.kind.as_str()],|r|Ok((r.get::<i64>(0)?,r.get::<String>(1)?,r.get::<i64>(2)?,r.get::<Option<String>>(3)?))).optional()?;
            anyhow::ensure!(saved.as_ref().is_some_and(|(revision,reason,requested,stay)| *revision==check.revision && reason==&check.reason && *requested==check.requested_at_ms && stay==&check.expected_stay_id) && verification.revision==check.revision,MemberSyncError::Stale);
            let actual=tx.query_row("SELECT revision FROM guild_member_directory WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),check.user_id],|r|Ok(r.get::<i64>(0)?)).optional()?;
            if actual.unwrap_or(-1)!=check.revision && check.reason=="ambiguous_departure" {
                // A later stay/state superseded the captured removal. Dismiss
                // only this exact check; never rebind an old event to new state.
                tx.ensure_mutation_receipt()?;
                tx.member_execute("DELETE FROM guild_member_target_checks WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3 AND kind=?4 AND row_revision=?5 AND expected_stay_id IS ?6 AND requested_at_ms=?7",params![scope.guild_id(),scope.enrollment.id.as_str(),check.user_id,check.kind.as_str(),check.revision,check.expected_stay_id.as_deref(),check.requested_at_ms])?;
                tx.commit()?;
                return Ok(None);
            }
            anyhow::ensure!(actual.unwrap_or(-1)==check.revision,MemberSyncError::Stale);
            tx.ensure_mutation_receipt()?;
            let mut departure=None;
            if check.reason=="ambiguous_departure" && check.kind==MemberVerifyKind::Member && !verification.present {
                // Gateway remove time was stored in requested_at_ms when the check was queued.
                let departure_observed_at_ms = check.requested_at_ms;
                let row=tx.query_row("SELECT is_current,joined_at_ms,current_stay_id,ever_banned FROM guild_member_directory WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),check.user_id],|r|Ok((r.get::<bool>(0)?,r.get::<Option<i64>>(1)?,r.get::<Option<String>>(2)?,r.get::<bool>(3)?))).optional()?;
                if let Some((current,joined,Some(stay),retained))=row && (current || retained) && Some(stay.as_str())==check.expected_stay_id.as_deref() && joined.is_none_or(|v|v<=departure_observed_at_ms) {
                    tx.member_execute("UPDATE guild_member_stays SET departed_at_ms=?4,ended_unknown=0 WHERE guild_id=?1 AND enrollment_id=?2 AND stay_id=?3 AND departed_at_ms IS NULL",params![scope.guild_id(),scope.enrollment.id.as_str(),stay,departure_observed_at_ms])?;
                    tx.member_execute("UPDATE guild_member_directory SET is_current=0,current_stay_id=NULL,departed_at_ms=?4,cleanup_after_ms=?5,cleanup_member_checked_at_ms=?6,observed_at_ms=MAX(observed_at_ms,?4),revision=revision+1 WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),check.user_id,departure_observed_at_ms,if retained{None}else{Some(departure_observed_at_ms.saturating_add(300_000))},now_ms])?;
                    bump_revision(&tx,&scope)?;
                    departure=Some(MemberDeparture{user_id:check.user_id,joined_at_ms:joined,departed_at_ms:departure_observed_at_ms,completed_stay_ms:joined.map(|v|departure_observed_at_ms-v)});
                } else { settle_verified_state(&tx,&scope,&verification,now_ms)?; }
            } else { settle_verified_state(&tx,&scope,&verification,now_ms)?; }
            tx.member_execute("DELETE FROM guild_member_target_checks WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3 AND kind=?4 AND row_revision=?5 AND expected_stay_id IS ?6 AND requested_at_ms=?7",params![scope.guild_id(),scope.enrollment.id.as_str(),check.user_id,check.kind.as_str(),check.revision,check.expected_stay_id.as_deref(),check.requested_at_ms])?;
            tx.commit()?;
            Ok(departure)
        }).await
    }

    pub async fn defer_member_target_check(
        &self,
        scope: GuildDataScope,
        check: MemberTargetCheck,
        next_attempt_at_ms: i64,
    ) -> Result<()> {
        checked_scope(self, &scope)?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn|{let mut tx=conn.transaction_primary_read()?;verify_scope(&tx,&expected,&scope,true)?;tx.ensure_mutation_receipt()?;
            let changed=tx.member_execute("UPDATE guild_member_target_checks SET next_attempt_at_ms=?5 WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3 AND kind=?4 AND row_revision IS ?6 AND expected_stay_id IS ?7 AND requested_at_ms=?8",params![scope.guild_id(),scope.enrollment.id.as_str(),check.user_id,check.kind.as_str(),next_attempt_at_ms,check.revision,check.expected_stay_id.as_deref(),check.requested_at_ms])?;
            anyhow::ensure!(changed==1,MemberSyncError::Stale);tx.commit()
        }).await
    }

    pub async fn next_member_cleanup_candidate(
        &self,
        scope: GuildDataScope,
        now_ms: i64,
    ) -> Result<Option<MemberCleanupCandidate>> {
        checked_scope(self, &scope)?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn|{let mut tx=conn.transaction_primary_read()?;verify_scope(&tx,&expected,&scope,true)?;
            // A real departure is eligible after its grace. Keep affected-user
            // checks paced during initial import, but retain the saved stay
            // until available ban/audit and local case history are processed.
            let gate:Option<(Option<i64>,Option<i64>,bool)>=tx.query_row("SELECT gap_at_ms,initial_completed_at_ms,case_backfill_done FROM guild_member_seed WHERE guild_id=?1 AND enrollment_id=?2",params![scope.guild_id(),scope.enrollment.id.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            let Some((gap_at,completed,backfill_done))=gate else {tx.finish_read()?;return Ok(None)};
            let history_ready=completed.is_some() && backfill_done;
            if audit_gap_recovery_pending(&tx,&scope)? {tx.finish_read()?;return Ok(None)}
            // During initial history import, already checked candidates wait
            // without hiding the next unverified departure in the queue.
            let row=tx.query_row(MEMBER_CLEANUP_CANDIDATE_SQL,params![scope.guild_id(),scope.enrollment.id.as_str(),now_ms,history_ready,gap_at],|r|Ok((r.get::<i64>(0)?,r.get::<i64>(1)?,r.get::<Option<i64>>(2)?,r.get::<Option<i64>>(3)?))).optional()?;
            if let Some((user_id,revision,checked_at,departed_at))=row {
                let check_since=if !history_ready{Some(gap_at.unwrap_or(0).max(departed_at.unwrap_or(0)))}else{gap_at};
                if check_since.is_some_and(|boundary|checked_at.is_none_or(|checked|checked<boundary)) {
                    tx.ensure_mutation_receipt()?;queue_target_check_tx(&tx,&scope,user_id,MemberVerifyKind::Member,if !history_ready{"cleanup_before_initial_coverage"}else{"cleanup_during_coverage_gap"},revision,now_ms)?;tx.commit()?;return Ok(None);
                }
                // An absent/currently-unbanned check cannot disprove an older
                // ban/unban still waiting in the bounded initial audit pages.
                if !history_ready {tx.finish_read()?;return Ok(None)}
                tx.finish_read()?;return Ok(Some(MemberCleanupCandidate{user_id,revision}));
            }
            tx.finish_read()?;Ok(None)
        }).await
    }

    pub async fn settle_member_cleanup(
        &self,
        scope: GuildDataScope,
        candidate: MemberCleanupCandidate,
        banned: bool,
        now_ms: i64,
    ) -> Result<()> {
        checked_scope(self, &scope)?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn|{let mut tx=conn.transaction_primary_read()?;verify_scope(&tx,&expected,&scope,true)?;tx.ensure_mutation_receipt()?;
            let row=tx.query_row("SELECT is_current,is_banned,ever_banned,revision,cleanup_after_ms,cleanup_member_checked_at_ms FROM guild_member_directory WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),candidate.user_id],|r|Ok((r.get::<bool>(0)?,r.get::<bool>(1)?,r.get::<bool>(2)?,r.get::<i64>(3)?,r.get::<Option<i64>>(4)?,r.get::<Option<i64>>(5)?))).optional()?;
            let Some((current,was_banned,retained,rev,deadline,checked_at))=row else{return Err(MemberSyncError::Stale.into())};
            anyhow::ensure!(!current && !was_banned && !retained && rev==candidate.revision && deadline.is_some_and(|v|v<=now_ms),MemberSyncError::Stale);
            anyhow::ensure!(!audit_gap_recovery_pending(&tx,&scope)?,MemberSyncError::Stale);
            let (gap_at,completed,backfill_done):(Option<i64>,Option<i64>,bool)=tx.query_row("SELECT gap_at_ms,initial_completed_at_ms,case_backfill_done FROM guild_member_seed WHERE guild_id=?1 AND enrollment_id=?2",params![scope.guild_id(),scope.enrollment.id.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
            anyhow::ensure!(completed.is_some() && backfill_done,MemberSyncError::Stale);
            anyhow::ensure!(gap_at.is_none_or(|boundary|checked_at.is_some_and(|checked|checked>=boundary)),MemberSyncError::Stale);
            let pending:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM guild_member_pending_bans WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3)",params![scope.guild_id(),scope.enrollment.id.as_str(),candidate.user_id],|r|Ok(r.get(0)?))?;
            anyhow::ensure!(!pending,MemberSyncError::Stale);
            let target:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM guild_member_target_checks WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3)",params![scope.guild_id(),scope.enrollment.id.as_str(),candidate.user_id],|r|Ok(r.get(0)?))?;
            anyhow::ensure!(!target,MemberSyncError::Stale);
            if banned {record_undated_ban_present(&tx,&scope,candidate.user_id,now_ms)?;tx.member_execute("UPDATE guild_member_directory SET is_banned=1,ever_banned=1,cleanup_after_ms=NULL,revision=revision+1 WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),candidate.user_id])?;}
            else {tx.member_execute("DELETE FROM guild_member_stays WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),candidate.user_id])?;tx.member_execute("DELETE FROM guild_member_state_observations WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),candidate.user_id])?;tx.member_execute("DELETE FROM guild_member_directory WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),candidate.user_id])?;}
            bump_revision(&tx,&scope)?;tx.commit()
        }).await
    }

    pub async fn defer_member_cleanup(
        &self,
        scope: GuildDataScope,
        candidate: MemberCleanupCandidate,
        next_attempt_at_ms: i64,
    ) -> Result<()> {
        checked_scope(self, &scope)?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn|{let mut tx=conn.transaction_primary_read()?;verify_scope(&tx,&expected,&scope,true)?;tx.ensure_mutation_receipt()?;
            let changed=tx.member_execute("UPDATE guild_member_directory SET cleanup_after_ms=?5,revision=revision+1 WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3 AND revision=?4 AND is_current=0 AND is_banned=0 AND ever_banned=0",params![scope.guild_id(),scope.enrollment.id.as_str(),candidate.user_id,candidate.revision,next_attempt_at_ms])?;
            anyhow::ensure!(changed==1,MemberSyncError::Stale);bump_revision(&tx,&scope)?;tx.commit()
        }).await
    }

    pub async fn list_members(
        &self,
        scope: GuildDataScope,
        query: MemberQuery,
    ) -> Result<MemberPage> {
        checked_scope(self, &scope)?;
        anyhow::ensure!(
            query.limit > 0
                && query.limit <= 100
                && query.q.as_ref().is_none_or(|q| q.len() <= 128)
                && query.after.as_ref().is_none_or(|v| v.len() <= 64),
            MemberSyncError::Invalid
        );
        let raw_q = query.q.as_deref().unwrap_or("").trim();
        let search = raw_q.to_lowercase();
        let mention_id = member_search_user_id(raw_q);
        let (after_join, after_id) = if let Some(cursor) = &query.after {
            let (j, id) = cursor.split_once(':').ok_or(MemberSyncError::Invalid)?;
            (j.parse::<i64>()?, id.parse::<i64>()?)
        } else {
            (
                if query.sort == MemberSort::JoinedNewest {
                    i64::MAX
                } else {
                    i64::MIN
                },
                if query.sort == MemberSort::JoinedNewest {
                    i64::MAX
                } else {
                    i64::MIN
                },
            )
        };
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| {
            let tx=conn.transaction_primary_snapshot()?; verify_scope(&tx,&expected,&scope,true)?;
            let status=match query.status { MemberStatusFilter::Current=>"is_current=1",MemberStatusFilter::Banned=>"is_banned=1",MemberStatusFilter::PreviouslyBanned=>"ever_banned=1 AND is_banned=0",MemberStatusFilter::All=>"1=1" };
            let order=if query.sort==MemberSort::JoinedNewest { ("<", "DESC") } else { (">", "ASC") };
            let sql=format!("SELECT {PROFILE_SQL} FROM guild_member_directory WHERE guild_id=?1 AND enrollment_id=?2 AND {status} AND (?3='' OR user_id=?4 OR instr(search_text,?3)>0) AND (?5 IS NULL OR joined_at_ms>=?5) AND (?6 IS NULL OR joined_at_ms<=?6) AND (coalesce(joined_at_ms,0),user_id) {} (?7,?8) ORDER BY coalesce(joined_at_ms,0) {},user_id {} LIMIT ?9",order.0,order.1,order.1);
            let mut stmt=tx.prepare(&sql)?;
            let rows=stmt.query_map(params![scope.guild_id(),scope.enrollment.id.as_str(),search,mention_id,query.joined_from_ms,query.joined_to_ms,after_join,after_id,query.limit as i64+1],profile)?;
            let mut members=Vec::new(); for row in rows { members.push(row?); } drop(stmt);
            let more=members.len()>query.limit as usize; if more { members.pop(); }
            let next_cursor=if more { members.last().map(|p|format!("{}:{}",p.joined_at_ms.unwrap_or(0),p.user_id)) } else {None};
            tx.finish_read()?; Ok(MemberPage{members,next_cursor})
        }).await
    }

    pub async fn get_member(
        &self,
        scope: GuildDataScope,
        user_id: i64,
    ) -> Result<Option<MemberProfile>> {
        checked_scope(self, &scope)?;
        anyhow::ensure!(user_id > 0, MemberSyncError::Invalid);
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| { let tx=conn.transaction_primary_snapshot()?; verify_scope(&tx,&expected,&scope,true)?;
            let sql=format!("SELECT {PROFILE_SQL} FROM guild_member_directory WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3");
            let found=tx.query_row(&sql,params![scope.guild_id(),scope.enrollment.id.as_str(),user_id],profile).optional()?; tx.finish_read()?; Ok(found)
        }).await
    }

    pub async fn member_history(
        &self,
        scope: GuildDataScope,
        user_id: i64,
        after: Option<String>,
        limit: u32,
    ) -> Result<MemberHistoryPage> {
        checked_scope(self, &scope)?;
        anyhow::ensure!(
            user_id > 0
                && limit > 0
                && limit <= 100
                && after.as_ref().is_none_or(|v| v.len() <= 128),
            MemberSyncError::Invalid
        );
        let (after_time, after_kind, after_id) = if let Some(cursor) = after {
            let mut parts = cursor.splitn(3, ':');
            let time = parts
                .next()
                .ok_or(MemberSyncError::Invalid)?
                .parse::<i64>()?;
            let kind = parts.next().ok_or(MemberSyncError::Invalid)?.to_string();
            let id = parts.next().ok_or(MemberSyncError::Invalid)?.to_string();
            (time, kind, id)
        } else {
            (i64::MAX, String::from("z"), String::from("z"))
        };
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| { let tx=conn.transaction_primary_snapshot()?; verify_scope(&tx,&expected,&scope,true)?;
            let sql="SELECT kind,occurred_at_ms,observed_at_ms,id,joined_at_ms,departed_at_ms,moderator_id,reason,outcome,source_kind FROM (
                SELECT 'join' AS kind,joined_at_ms AS occurred_at_ms,NULL AS observed_at_ms,stay_id AS id,joined_at_ms,departed_at_ms,NULL AS moderator_id,NULL AS reason,NULL AS outcome,NULL AS source_kind,coalesce(joined_at_ms,0) AS sort_at_ms FROM guild_member_stays WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3
                UNION ALL SELECT 'departure',departed_at_ms,NULL,stay_id,joined_at_ms,departed_at_ms,NULL,NULL,NULL,NULL,departed_at_ms FROM guild_member_stays WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3 AND departed_at_ms IS NOT NULL
                UNION ALL SELECT i.action_kind,i.occurred_at_ms,NULL,i.case_id,NULL,NULL,json_extract(c.payload,'$.actor_id'),json_extract(c.payload,'$.reason'),json_extract(c.payload,'$.outcome'),CASE WHEN c.source_key LIKE 'discord:%' THEN 'discord_audit' WHEN EXISTS(SELECT 1 FROM guild_case_sources s WHERE s.guild_id=i.guild_id AND s.enrollment_id=i.enrollment_id AND s.case_id=i.case_id AND s.source_kind=1) THEN 'application_audit_linked' WHEN json_type(c.payload,'$.source.gateway_observation') IS NOT NULL THEN 'gateway_observation_unlinked' ELSE 'application' END,coalesce(i.occurred_at_ms,0) FROM guild_member_case_index i JOIN guild_cases c USING(guild_id,enrollment_id,case_id) WHERE i.guild_id=?1 AND i.enrollment_id=?2 AND i.user_id=?3 AND c.deleted=0
                UNION ALL SELECT action,occurred_at_ms,NULL,CAST(entry_id AS TEXT),NULL,NULL,CAST(executor_id AS TEXT),reason,'succeeded','discord_audit_unresolved',occurred_at_ms FROM guild_member_unresolved_audits WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3
                UNION ALL SELECT CASE WHEN kind='member_absent' THEN 'member_state_observation' WHEN kind='ban_absent' THEN 'ban_absent_state_observation' WHEN kind='ban_present' THEN 'ban_present_state_observation' ELSE 'membership_state_observation' END,NULL,observed_at_ms,observation_id,NULL,NULL,NULL,NULL,NULL,'state_reconciliation',observed_at_ms FROM guild_member_state_observations WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3
                ) WHERE (sort_at_ms,kind,id)<(?4,?5,?6) ORDER BY sort_at_ms DESC,kind DESC,id DESC LIMIT ?7";
            let mut stmt=tx.prepare(sql)?;
            let rows=stmt.query_map(params![scope.guild_id(),scope.enrollment.id.as_str(),user_id,after_time,after_kind,after_id,limit as i64+1],|r|{ let kind:String=r.get(0)?; let id:String=r.get(3)?;let source_kind:Option<String>=r.get(9)?;let is_stay=kind=="join"||kind=="departure";let is_state=kind.ends_with("_state_observation");let is_unresolved=source_kind.as_deref()==Some("discord_audit_unresolved"); Ok(MemberHistoryEntry{case_id:if is_stay||is_state||is_unresolved{None}else{Some(id.clone())},stay_id:if is_stay{Some(id.clone())}else{None},observation_id:if is_state||is_unresolved{Some(id)}else{None},kind,occurred_at_ms:r.get(1)?,observed_at_ms:r.get(2)?,joined_at_ms:r.get(4)?,departed_at_ms:r.get(5)?,moderator_id:r.get(6)?,reason:r.get(7)?,outcome:r.get(8)?,source_kind}) })?;
            let mut entries=Vec::new();for row in rows {entries.push(row?);}drop(stmt);
            let more=entries.len()>limit as usize;if more{entries.pop();}
            let next_cursor=if more{entries.last().map(|e|format!("{}:{}:{}",e.occurred_at_ms.or(e.observed_at_ms).unwrap_or(0),e.kind,e.case_id.as_ref().or(e.stay_id.as_ref()).or(e.observation_id.as_ref()).unwrap()))}else{None};
            tx.finish_read()?;Ok(MemberHistoryPage{entries,next_cursor})
        }).await
    }

    fn member_case_row(row: &Row) -> Result<MemberHistoryEntry> {
        Ok(MemberHistoryEntry {
            kind: row.get(0)?,
            occurred_at_ms: row.get(1)?,
            observed_at_ms: None,
            stay_id: None,
            observation_id: None,
            case_id: Some(row.get::<String>(2)?),
            joined_at_ms: None,
            departed_at_ms: None,
            moderator_id: row.get(3)?,
            reason: row.get(4)?,
            outcome: row.get(5)?,
            source_kind: row.get(6)?,
        })
    }

    /// Index-ordered case lookup: bound live cases before joining payloads.
    pub(crate) fn member_case_entries_lookup_sql() -> &'static str {
        "SELECT i.action_kind,i.occurred_at_ms,i.case_id,json_extract(c.payload,'$.actor_id'),json_extract(c.payload,'$.reason'),json_extract(c.payload,'$.outcome'),CASE WHEN c.source_key LIKE 'discord:%' THEN 'discord_audit' WHEN EXISTS(SELECT 1 FROM guild_case_sources s WHERE s.guild_id=c.guild_id AND s.enrollment_id=c.enrollment_id AND s.case_id=c.case_id AND s.source_kind=1) THEN 'application_audit_linked' WHEN json_type(c.payload,'$.source.gateway_observation') IS NOT NULL THEN 'gateway_observation_unlinked' ELSE 'application' END FROM (SELECT i.guild_id,i.enrollment_id,i.user_id,i.case_id,i.action_kind,i.occurred_at_ms FROM guild_member_case_index i INDEXED BY guild_member_cases_user INNER JOIN guild_cases c ON c.guild_id=i.guild_id AND c.enrollment_id=i.enrollment_id AND c.case_id=i.case_id AND c.deleted=0 AND length(CAST(c.payload AS BLOB))<=65536 WHERE i.guild_id=?1 AND i.enrollment_id=?2 AND i.user_id=?3 ORDER BY i.occurred_at_ms DESC,i.case_id DESC LIMIT ?4) AS i JOIN guild_cases c ON c.guild_id=i.guild_id AND c.enrollment_id=i.enrollment_id AND c.case_id=i.case_id AND c.deleted=0 AND length(CAST(c.payload AS BLOB))<=65536 ORDER BY i.occurred_at_ms DESC,i.case_id DESC"
    }

    fn member_case_entries_oversized_sql() -> &'static str {
        "SELECT EXISTS(SELECT 1 FROM (SELECT i.case_id FROM guild_member_case_index i INDEXED BY guild_member_cases_user WHERE i.guild_id=?1 AND i.enrollment_id=?2 AND i.user_id=?3 ORDER BY i.occurred_at_ms DESC,i.case_id DESC LIMIT ?4) recent JOIN guild_cases c ON c.guild_id=?1 AND c.enrollment_id=?2 AND c.case_id=recent.case_id AND c.deleted=0 WHERE length(CAST(c.payload AS BLOB))>65536)"
    }

    /// Bounded member-case lookup while `case_backfill_done` is false. Uses the
    /// partial `guild_member_case_index` window (same plan as `member_case_entries`)
    /// instead of scanning `guild_cases` by subject JSON.
    pub(crate) fn member_subject_cases_lookup_sql() -> &'static str {
        Self::member_case_entries_lookup_sql()
    }

    fn member_subject_cases_oversized_sql() -> &'static str {
        Self::member_case_entries_oversized_sql()
    }

    /// Recent moderation cases for one member, newest first. Fetches at most
    /// `limit` rows and reports whether a newer ninth case exists.
    pub async fn member_case_entries(
        &self,
        scope: GuildDataScope,
        user_id: i64,
        limit: u32,
    ) -> Result<(Vec<MemberHistoryEntry>, bool)> {
        checked_scope(self, &scope)?;
        anyhow::ensure!(
            user_id > 0 && limit > 0 && limit <= 8,
            MemberSyncError::Invalid
        );
        let fetch = limit + 1;
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let tx = conn.transaction_primary_snapshot()?;
                verify_scope(&tx, &expected, &scope, true)?;
                let oversize: bool = tx.query_row(
                    GuildDataStore::member_case_entries_oversized_sql(),
                    params![
                        scope.guild_id(),
                        scope.enrollment.id.as_str(),
                        user_id,
                        fetch as i64
                    ],
                    |r| Ok(r.get(0)?),
                )?;
                anyhow::ensure!(!oversize, MemberSyncError::Invalid);
                let sql = GuildDataStore::member_case_entries_lookup_sql();
                let mut stmt = tx.prepare(sql)?;
                let rows = stmt.query_map(
                    params![
                        scope.guild_id(),
                        scope.enrollment.id.as_str(),
                        user_id,
                        fetch as i64
                    ],
                    Self::member_case_row,
                )?;
                let mut entries = Vec::new();
                for row in rows {
                    entries.push(row?);
                }
                drop(stmt);
                let truncated = entries.len() > limit as usize;
                if truncated {
                    entries.pop();
                }
                tx.finish_read()?;
                Ok((entries, truncated))
            })
            .await
    }

    /// Reads indexed cases for one member while backfill is still running.
    pub async fn member_subject_case_entries(
        &self,
        scope: GuildDataScope,
        user_id: i64,
        limit: u32,
    ) -> Result<(Vec<MemberHistoryEntry>, bool)> {
        checked_scope(self, &scope)?;
        anyhow::ensure!(
            user_id > 0 && limit > 0 && limit <= 8,
            MemberSyncError::Invalid
        );
        let fetch = limit + 1;
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let tx = conn.transaction_primary_snapshot()?;
                verify_scope(&tx, &expected, &scope, true)?;
                let oversize: bool = tx.query_row(
                    GuildDataStore::member_subject_cases_oversized_sql(),
                    params![
                        scope.guild_id(),
                        scope.enrollment.id.as_str(),
                        user_id,
                        fetch as i64
                    ],
                    |r| Ok(r.get(0)?),
                )?;
                anyhow::ensure!(!oversize, MemberSyncError::Invalid);
                let sql = GuildDataStore::member_subject_cases_lookup_sql();
                let mut stmt = tx.prepare(sql)?;
                let rows = stmt.query_map(
                    params![
                        scope.guild_id(),
                        scope.enrollment.id.as_str(),
                        user_id,
                        fetch as i64
                    ],
                    Self::member_case_row,
                )?;
                let mut entries = Vec::new();
                for row in rows {
                    entries.push(row?);
                }
                drop(stmt);
                let truncated = entries.len() > limit as usize;
                if truncated {
                    entries.pop();
                }
                tx.finish_read()?;
                Ok((entries, truncated))
            })
            .await
    }

    pub async fn claim_member_sync(
        &self,
        scope: GuildDataScope,
        bot_id: i64,
        worker_id: String,
        now_ms: i64,
        lease_ms: i64,
    ) -> Result<Option<MemberSyncClaim>> {
        checked_scope(self, &scope)?;
        anyhow::ensure!(
            bot_id > 0
                && now_ms > 0
                && (15_000..=300_000).contains(&lease_ms)
                && !worker_id.is_empty()
                && worker_id.len() <= 64,
            MemberSyncError::Invalid
        );
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| { let mut tx=conn.transaction_primary_read()?; verify_scope(&tx,&expected,&scope,true)?; ensure_seed_bot(&tx,&scope,bot_id)?;
            let row=tx.query_row("SELECT run_id,phase,cursor,generation,lease_owner,lease_until_ms,next_attempt_at_ms,request_after_ms,status,started_at_ms FROM guild_member_sync WHERE guild_id=?1 AND enrollment_id=?2 AND bot_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),bot_id],|r|Ok((r.get::<String>(0)?,r.get::<String>(1)?,r.get::<Option<String>>(2)?,r.get::<i64>(3)?,r.get::<Option<String>>(4)?,r.get::<Option<i64>>(5)?,r.get::<Option<i64>>(6)?,r.get::<Option<i64>>(7)?,r.get::<String>(8)?,r.get::<i64>(9)?))).optional()?;
            let Some((run_id,phase,cursor,generation,owner,until,next,request_after,state,started_at))=row else {tx.finish_read()?;return Ok(None)};
            if !matches!(state.as_str(),"queued"|"running"|"waiting") || next.is_some_and(|v|v>now_ms) || request_after.is_some_and(|v|v>now_ms) || owner.is_some()&&until.is_some_and(|v|v>now_ms) {tx.finish_read()?;return Ok(None)}
            let phase=MemberSyncPhase::parse(&phase)?; if phase==MemberSyncPhase::Complete {tx.finish_read()?;return Ok(None)}
            tx.ensure_mutation_receipt()?;
            let new_generation=generation+1;
            tx.member_execute("UPDATE guild_member_sync SET status='running',lease_owner=?4,lease_until_ms=?5,generation=?6,progress_revision=progress_revision+1 WHERE guild_id=?1 AND enrollment_id=?2 AND bot_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),bot_id,worker_id.as_str(),now_ms.saturating_add(lease_ms),new_generation])?;
            let directory_revision=tx.query_row("SELECT directory_revision FROM guild_member_seed WHERE guild_id=?1",params![scope.guild_id()],|r|Ok(r.get(0)?))?;
            let (verify_user_id,verify_revision,verify_kind)=if phase==MemberSyncPhase::Verify {
                let (kind,last)=cursor.as_deref().and_then(|c|c.split_once(':')).map(|(k,id)|(k,id.parse::<i64>().unwrap_or(0))).unwrap_or(("m",0));
                let member=if kind=="m" {tx.query_row("SELECT user_id,revision FROM guild_member_directory WHERE guild_id=?1 AND enrollment_id=?2 AND is_current=1 AND coalesce(last_seen_run,'')!=?3 AND user_id>?4 ORDER BY user_id LIMIT 1",params![scope.guild_id(),scope.enrollment.id.as_str(),run_id.as_str(),last],|r|Ok((r.get::<i64>(0)?,r.get::<i64>(1)?))).optional()?}else{None};
                if let Some((id,rev))=member{(Some(id),Some(rev),Some(MemberVerifyKind::Member))}
                else{let after=if kind=="b" {last}else{0};let ban=tx.query_row("SELECT user_id,revision FROM guild_member_directory WHERE guild_id=?1 AND enrollment_id=?2 AND is_banned=1 AND coalesce(last_ban_seen_run,'')!=?3 AND user_id>?4 ORDER BY user_id LIMIT 1",params![scope.guild_id(),scope.enrollment.id.as_str(),run_id.as_str(),after],|r|Ok((r.get::<i64>(0)?,r.get::<i64>(1)?))).optional()?;
                    if let Some((id,rev))=ban{(Some(id),Some(rev),Some(MemberVerifyKind::Ban))}else{(None,None,None)}}
            }else{(None,None,None)};
            tx.commit()?;Ok(Some(MemberSyncClaim{bot_id,run_id,phase,cursor,directory_revision,generation:new_generation,worker_id,verify_user_id,verify_revision,verify_kind,not_before_ms:if phase==MemberSyncPhase::Audit{Some(started_at.saturating_sub(45*86_400_000))}else{None}}))
        }).await
    }

    pub async fn commit_member_page(
        &self,
        scope: GuildDataScope,
        claim: MemberSyncClaim,
        page: MemberSyncPage,
        now_ms: i64,
    ) -> Result<MemberSyncStatus> {
        checked_scope(self, &scope)?;
        anyhow::ensure!(
            now_ms > 0
                && page.members.len() <= 500
                && page.bans.len() <= 500
                && page.audits.len() <= 50
                && page.total_estimate.is_none_or(|v| v >= 0)
                && (1..=500).contains(&page.page_size)
                && page.pace_ms >= 15_000
                && page.active_guild_count > 0,
            MemberSyncError::Invalid
        );
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| {let mut tx=conn.transaction_primary_read()?;verify_scope(&tx,&expected,&scope,true)?;
            let row=tx.query_row("SELECT run_id,phase,cursor,generation,lease_owner,lease_until_ms,run_kind,processed FROM guild_member_sync WHERE guild_id=?1 AND enrollment_id=?2 AND bot_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),claim.bot_id],|r|Ok((r.get::<String>(0)?,r.get::<String>(1)?,r.get::<Option<String>>(2)?,r.get::<i64>(3)?,r.get::<Option<String>>(4)?,r.get::<Option<i64>>(5)?,r.get::<String>(6)?,r.get::<i64>(7)?))).optional()?;
            let current_revision:i64=tx.query_row("SELECT directory_revision FROM guild_member_seed WHERE guild_id=?1",params![scope.guild_id()],|r|Ok(r.get(0)?))?;
            let Some((run,phase,cursor,generation,owner,until,run_kind,processed_before))=row else {return Err(MemberSyncError::Stale.into())};
            anyhow::ensure!(run==claim.run_id && phase==claim.phase.as_str() && phase==page.phase.as_str() && cursor==claim.cursor && cursor==page.expected_cursor && generation==claim.generation && owner.as_deref()==Some(claim.worker_id.as_str()) && until.is_some_and(|v|v>=now_ms) && current_revision==claim.directory_revision,MemberSyncError::Stale);
            let prior_member_progress:Option<i64>=if page.phase==MemberSyncPhase::Members {tx.query_row("SELECT last_progress_at_ms FROM guild_member_sync WHERE guild_id=?1 AND enrollment_id=?2",params![scope.guild_id(),scope.enrollment.id.as_str()],|r|Ok(r.get(0)?))?}else{None};
            anyhow::ensure!(match page.phase {MemberSyncPhase::Members=>page.bans.is_empty()&&page.audits.is_empty()&&page.verification.is_none(),MemberSyncPhase::Bans=>page.members.is_empty()&&page.audits.is_empty()&&page.verification.is_none(),MemberSyncPhase::Verify=>page.members.is_empty()&&page.bans.is_empty()&&page.audits.is_empty(),MemberSyncPhase::Audit=>page.members.is_empty()&&page.bans.is_empty()&&page.verification.is_none(),MemberSyncPhase::Complete=>false},MemberSyncError::Invalid);
            tx.ensure_mutation_receipt()?;
            let mut inserted=0_i64;let mut updated=0_i64;let mut unchanged=0_i64;
            let (new_members,updated_members,unchanged_members)=upsert_member_page(&tx,&scope,&page.members)?;
            inserted+=new_members;updated+=updated_members;unchanged+=unchanged_members;
            for batch in page.members.chunks(MEMBER_PAGE_WRITE_BATCH_SIZE){let ids=serde_json::to_string(&batch.iter().map(|m|m.user_id).collect::<Vec<_>>())?;
                note_member_sync_statement(scope.guild_id());
                tx.member_execute("UPDATE guild_member_directory SET last_seen_run=?4 WHERE guild_id=?1 AND enrollment_id=?2 AND user_id IN (SELECT CAST(value AS INTEGER) FROM json_each(?3))",params![scope.guild_id(),scope.enrollment.id.as_str(),ids,claim.run_id.as_str()])?;
            }
            let (new_bans,updated_bans,unchanged_bans)=upsert_ban_page(&tx,&scope,&page.bans,now_ms)?;
            inserted+=new_bans;updated+=updated_bans;unchanged+=unchanged_bans;
            for batch in page.bans.chunks(MEMBER_PAGE_WRITE_BATCH_SIZE){let ids=serde_json::to_string(&batch.iter().map(|b|b.user_id).collect::<Vec<_>>())?;
                tx.member_execute("UPDATE guild_member_directory SET last_ban_seen_run=?4 WHERE guild_id=?1 AND enrollment_id=?2 AND user_id IN (SELECT CAST(value AS INTEGER) FROM json_each(?3))",params![scope.guild_id(),scope.enrollment.id.as_str(),ids,claim.run_id.as_str()])?;
            }
            if page.phase==MemberSyncPhase::Verify {
                if let Some(v)=&page.verification {
                    anyhow::ensure!(Some(v.user_id)==claim.verify_user_id && Some(v.revision)==claim.verify_revision && Some(v.kind)==claim.verify_kind && !page.terminal,MemberSyncError::Stale);
                    let actual_rev=tx.query_row("SELECT revision FROM guild_member_directory WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),v.user_id],|r|Ok(r.get::<i64>(0)?)).optional()?;
                    anyhow::ensure!(actual_rev==Some(v.revision),MemberSyncError::Stale);
                    settle_verified_state(&tx,&scope,v,now_ms)?;
                    let seen_column=match v.kind{MemberVerifyKind::Member=>"last_seen_run",MemberVerifyKind::Ban=>"last_ban_seen_run"};
                    tx.member_execute(&format!("UPDATE guild_member_directory SET {seen_column}=?4 WHERE guild_id=?1 AND enrollment_id=?2 AND user_id=?3"),params![scope.guild_id(),scope.enrollment.id.as_str(),v.user_id,claim.run_id.as_str()])?;
                    updated+=1;
                } else {anyhow::ensure!(claim.verify_user_id.is_none() && page.terminal,MemberSyncError::Stale);}
            }
            let (new_audits,known_audits)=ingest_member_audits(&tx,&scope,claim.bot_id,&page.audits,now_ms)?;
            inserted+=new_audits;unchanged+=known_audits;
            let audit_ban_stage=page.phase==MemberSyncPhase::Audit && !claim.cursor.as_deref().is_some_and(|c|c.starts_with("unban:"));
            let next_phase=if page.terminal {match page.phase{MemberSyncPhase::Members=>MemberSyncPhase::Bans,MemberSyncPhase::Bans=>if run_kind=="reconcile"{MemberSyncPhase::Verify}else{MemberSyncPhase::Audit},MemberSyncPhase::Verify=>MemberSyncPhase::Audit,MemberSyncPhase::Audit=>if audit_ban_stage{MemberSyncPhase::Audit}else{MemberSyncPhase::Complete},MemberSyncPhase::Complete=>MemberSyncPhase::Complete}}else{page.phase};
            let next_cursor=if page.terminal && audit_ban_stage {Some("unban:".to_string())} else if page.terminal {None} else {page.next_cursor.clone()};
            let completed=next_phase==MemberSyncPhase::Complete;
            let backfill_done:bool=tx.query_row("SELECT case_backfill_done FROM guild_member_seed WHERE guild_id=?1",params![scope.guild_id()],|r|Ok(r.get(0)?))?;
            let settled=completed&&backfill_done;
            let member_total = if page.phase==MemberSyncPhase::Members {
                if page.terminal { Some(processed_before.saturating_add(page.members.len() as i64)) }
                else { page.total_estimate }
            } else { None };
            tx.member_execute("UPDATE guild_member_sync SET phase=?4,status=?5,status_reason=?18,cursor=?6,processed=processed+?7,inserted=inserted+?8,updated=updated+?9,unchanged=unchanged+?10,total_estimate=coalesce(?11,total_estimate),last_progress_at_ms=?12,completed_at_ms=CASE WHEN ?13 THEN ?12 ELSE completed_at_ms END,last_success_at_ms=CASE WHEN ?13 THEN ?12 ELSE last_success_at_ms END,last_success_processed=CASE WHEN ?13 THEN processed+?7 ELSE last_success_processed END,last_success_inserted=CASE WHEN ?13 THEN inserted+?8 ELSE last_success_inserted END,last_success_updated=CASE WHEN ?13 THEN updated+?9 ELSE last_success_updated END,last_success_unchanged=CASE WHEN ?13 THEN unchanged+?10 ELSE last_success_unchanged END,lease_owner=NULL,lease_until_ms=NULL,next_attempt_at_ms=CASE WHEN ?13 THEN NULL ELSE ?14 END,request_after_ms=?14,page_size=?15,pace_ms=?16,active_guild_count=?17,progress_revision=progress_revision+1 WHERE guild_id=?1 AND enrollment_id=?2 AND bot_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),claim.bot_id,next_phase.as_str(),if settled{"complete"}else if completed{"waiting"}else{"queued"},next_cursor,(inserted+updated+unchanged),inserted,updated,unchanged,member_total,now_ms,settled,now_ms.saturating_add(page.pace_ms),page.page_size as i64,page.pace_ms,page.active_guild_count as i64,if completed&&!backfill_done{Some("finishing local case history") }else{None}])?;
            match page.phase {
                MemberSyncPhase::Members=>{
                    let max_recent=60_000_i64.max(page.pace_ms.saturating_mul(page.active_guild_count as i64).saturating_mul(4));
                    let interval=prior_member_progress.and_then(|prior|{let elapsed=now_ms.saturating_sub(prior);(elapsed>0 && elapsed<=max_recent).then_some(elapsed)});
                    tx.member_execute("UPDATE guild_member_sync SET member_processed=coalesce(member_processed,?2)+?3,member_page_interval_ms=?4 WHERE guild_id=?1",params![scope.guild_id(),processed_before,page.members.len() as i64,interval])?;
                }
                MemberSyncPhase::Bans=>{tx.member_execute("UPDATE guild_member_sync SET ban_processed=ban_processed+?2 WHERE guild_id=?1",params![scope.guild_id(),page.bans.len() as i64])?;}
                MemberSyncPhase::Verify=>{tx.member_execute("UPDATE guild_member_sync SET verify_processed=verify_processed+?2 WHERE guild_id=?1",params![scope.guild_id(),i64::from(page.verification.is_some())])?;}
                MemberSyncPhase::Audit=>{tx.member_execute("UPDATE guild_member_sync SET audit_processed=audit_processed+?2 WHERE guild_id=?1",params![scope.guild_id(),page.audits.len() as i64])?;}
                MemberSyncPhase::Complete=>{}
            }
            if settled {
                tx.member_execute("UPDATE guild_member_seed SET initial_completed_at_ms=CASE WHEN initial_completed_at_ms IS NULL AND (SELECT run_kind FROM guild_member_sync WHERE guild_id=?1)='initial' THEN ?2 ELSE initial_completed_at_ms END WHERE guild_id=?1",params![scope.guild_id(),now_ms])?;
                if run_kind=="reconcile" {tx.member_execute("UPDATE guild_member_seed SET reconciliation_needed=0,reconciliation_reason=NULL,gap_at_ms=NULL WHERE guild_id=?1 AND gap_generation=(SELECT reconcile_gap_generation FROM guild_member_sync WHERE guild_id=?1)",params![scope.guild_id()])?;}
            }
            if settled && run_kind=="initial" {tx.member_execute("INSERT OR IGNORE INTO guild_member_audit_recovery(guild_id,enrollment_id,bot_id,run_id,started_at_ms,status,next_attempt_at_ms) SELECT guild_id,enrollment_id,bot_id,lower(hex(randomblob(16))),?2,'queued',?2 FROM guild_member_seed WHERE guild_id=?1 AND reconciliation_needed=1 AND initial_completed_at_ms IS NOT NULL",params![scope.guild_id(),now_ms])?;}
            tx.commit()?;let read=conn.transaction_primary_read()?;verify_scope(&read,&expected,&scope,true)?;let out=status(&read,&scope,claim.bot_id,now_ms,true)?;read.finish_read()?;Ok(out)
        }).await
    }

    pub async fn defer_member_sync(
        &self,
        scope: GuildDataScope,
        claim: MemberSyncClaim,
        reason: String,
        next_attempt_at_ms: i64,
        now_ms: i64,
    ) -> Result<()> {
        checked_scope(self, &scope)?;
        anyhow::ensure!(
            reason.len() <= 160 && next_attempt_at_ms >= now_ms,
            MemberSyncError::Invalid
        );
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn|{let mut tx=conn.transaction_primary_read()?;verify_scope(&tx,&expected,&scope,true)?;tx.ensure_mutation_receipt()?;
            let state=if reason=="permission_denied"||reason=="pagination_stalled"{"needs_attention"}else{"waiting"};
            let changed=tx.member_execute("UPDATE guild_member_sync SET status=?11,status_reason=?4,next_attempt_at_ms=?5,request_after_ms=MAX(coalesce(request_after_ms,0),?5),lease_owner=NULL,lease_until_ms=NULL,progress_revision=progress_revision+1 WHERE guild_id=?1 AND enrollment_id=?2 AND bot_id=?3 AND run_id=?6 AND generation=?7 AND lease_owner=?8 AND phase=?9 AND cursor IS ?10",params![scope.guild_id(),scope.enrollment.id.as_str(),claim.bot_id,reason,next_attempt_at_ms,claim.run_id,claim.generation,claim.worker_id,claim.phase.as_str(),claim.cursor,state])?;
            anyhow::ensure!(changed==1,MemberSyncError::Stale);tx.commit()
        }).await
    }

    pub async fn member_sync_action(
        &self,
        scope: GuildDataScope,
        bot_id: i64,
        action: MemberSyncAction,
        operation_key: String,
        now_ms: i64,
    ) -> Result<MemberSyncStatus> {
        checked_scope(self, &scope)?;
        anyhow::ensure!(
            bot_id > 0 && now_ms > 0 && !operation_key.is_empty() && operation_key.len() <= 128,
            MemberSyncError::Invalid
        );
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn|{let mut tx=conn.transaction_primary_read()?;verify_scope(&tx,&expected,&scope,true)?;let completed=ensure_seed_bot(&tx,&scope,bot_id)?.ok_or(MemberSyncError::Stale)?;
            let current=tx.query_row("SELECT run_kind,status,last_operation_key,lease_until_ms FROM guild_member_sync WHERE guild_id=?1 AND enrollment_id=?2 AND bot_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),bot_id],|r|Ok((r.get::<String>(0)?,r.get::<String>(1)?,r.get::<Option<String>>(2)?,r.get::<Option<i64>>(3)?))).optional()?;
            let Some((kind,state,last_key,lease_until))=current else {return Err(MemberSyncError::MissingJob.into())};
            let audit_stalled:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM guild_member_audit_recovery WHERE guild_id=?1 AND enrollment_id=?2 AND bot_id=?3 AND status='needs_attention' AND status_reason='pagination_stalled')",params![scope.guild_id(),scope.enrollment.id.as_str(),bot_id],|r|Ok(r.get(0)?))?;
            if last_key.as_deref()==Some(operation_key.as_str()) || (action!=MemberSyncAction::Reconcile && action!=MemberSyncAction::Retry && state=="complete") || (action==MemberSyncAction::Retry && state=="complete" && !audit_stalled) {tx.finish_read()?;let read=conn.transaction_primary_read()?;let out=status(&read,&scope,bot_id,now_ms,true)?;read.finish_read()?;return Ok(out)}
            let allowed=match action {MemberSyncAction::Pause=>matches!(state.as_str(),"queued"|"running"|"waiting"),MemberSyncAction::Resume=>state=="paused",MemberSyncAction::Retry=>matches!(state.as_str(),"needs_attention"|"waiting")||(state=="running"&&lease_until.is_some_and(|until|until<now_ms))||(state=="complete"&&audit_stalled),MemberSyncAction::Reconcile=>completed.is_some()&&state=="complete"};
            anyhow::ensure!(allowed,MemberSyncError::Invalid);
            tx.ensure_mutation_receipt()?;
            match action {
                MemberSyncAction::Pause=>{tx.member_execute("UPDATE guild_member_sync SET status='paused',status_reason='paused by administrator',lease_owner=NULL,lease_until_ms=NULL,generation=generation+1,progress_revision=progress_revision+1,last_operation_key=?2 WHERE guild_id=?1",params![scope.guild_id(),operation_key])?;}
                MemberSyncAction::Resume|MemberSyncAction::Retry=>{
                    if action==MemberSyncAction::Retry {tx.member_execute("UPDATE guild_member_audit_recovery SET status='queued',status_reason=NULL,next_attempt_at_ms=?4,request_after_ms=NULL,lease_owner=NULL,lease_until_ms=NULL,generation=generation+1 WHERE guild_id=?1 AND enrollment_id=?2 AND bot_id=?3 AND status='needs_attention' AND status_reason='pagination_stalled'",params![scope.guild_id(),scope.enrollment.id.as_str(),bot_id,now_ms])?;}
                    if state!="complete" {tx.member_execute("UPDATE guild_member_sync SET request_after_ms=CASE WHEN ?4=1 AND status_reason='permission_denied' THEN ?2 ELSE request_after_ms END,status='queued',status_reason=NULL,next_attempt_at_ms=?2,lease_owner=NULL,lease_until_ms=NULL,generation=generation+1,progress_revision=progress_revision+1,last_operation_key=?3 WHERE guild_id=?1",params![scope.guild_id(),now_ms,operation_key,action==MemberSyncAction::Retry])?;}
                    else {tx.member_execute("UPDATE guild_member_sync SET last_operation_key=?2,progress_revision=progress_revision+1 WHERE guild_id=?1",params![scope.guild_id(),operation_key])?;}
                }
                MemberSyncAction::Reconcile=>{tx.member_execute("UPDATE guild_member_sync SET run_id=?2,run_kind='reconcile',phase='members',status='queued',status_reason=NULL,cursor=NULL,processed=0,member_processed=0,ban_processed=0,verify_processed=0,audit_processed=0,member_page_interval_ms=NULL,inserted=0,updated=0,unchanged=0,total_estimate=NULL,started_at_ms=?3,last_progress_at_ms=NULL,completed_at_ms=NULL,next_attempt_at_ms=?3,reconcile_gap_generation=(SELECT gap_generation FROM guild_member_seed WHERE guild_id=?1),lease_owner=NULL,lease_until_ms=NULL,generation=generation+1,progress_revision=progress_revision+1,last_operation_key=?4 WHERE guild_id=?1",params![scope.guild_id(),random_id(),now_ms,operation_key])?;}
            }
            let _=kind;let out=status(&tx,&scope,bot_id,now_ms,true)?;tx.commit()?;Ok(out)
        }).await
    }
}
