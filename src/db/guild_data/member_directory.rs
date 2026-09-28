//! Scoped, persistent member observations and one-time initial import metadata.
use super::tenant_logs::verify_scope;
use super::*;
use crate::db::write_budget::MemberSqlWrites;
use anyhow::bail;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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

fn ensure_seed_bot(
    conn: &impl SqlExecutor,
    scope: &GuildDataScope,
    bot_id: i64,
) -> Result<Option<Option<i64>>> {
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
    let coverage:(bool,Option<String>,bool) = conn.query_row("SELECT reconciliation_needed,reconciliation_reason,case_backfill_done FROM guild_member_seed WHERE guild_id=?1", params![scope.guild_id()], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    out.reconciliation_needed = coverage.0;
    out.reconciliation_reason = coverage.1;
    out.local_history_complete = coverage.2;
    out.audit_recovery_pending = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM guild_member_audit_recovery WHERE guild_id=?1 AND enrollment_id=?2 AND status IN ('queued','running','waiting','needs_attention') AND NOT (status='needs_attention' AND status_reason='pagination_stalled') AND coalesce(next_attempt_at_ms,0)<=?3 AND coalesce(request_after_ms,0)<=?3 AND coalesce(lease_until_ms,0)<=?3)",
        params![scope.guild_id(), scope.enrollment.id.as_str(), now_ms],
        |r| Ok(r.get(0)?),
    )?;
    if let Some((state,reason,cursor,processed,next,request_after,last_progress))=conn.query_row("SELECT status,status_reason,cursor,processed,next_attempt_at_ms,request_after_ms,last_progress_at_ms FROM guild_member_audit_recovery WHERE guild_id=?1 AND enrollment_id=?2",params![scope.guild_id(),scope.enrollment.id.as_str()],|r|Ok((r.get::<String>(0)?,r.get::<Option<String>>(1)?,r.get::<Option<String>>(2)?,r.get::<i64>(3)?,r.get::<Option<i64>>(4)?,r.get::<Option<i64>>(5)?,r.get::<Option<i64>>(6)?))).optional()? {
        out.audit_recovery_phase=if state=="complete" {None}else{Some(if cursor.as_deref().is_some_and(|v|v.starts_with("unban:")){"unban"}else{"ban"}.into())};
        out.audit_recovery_status=Some(state);out.audit_recovery_reason=reason;out.audit_recovery_processed=processed;out.audit_recovery_next_attempt_at=next.into_iter().chain(request_after).max();out.audit_recovery_last_progress_at=last_progress;
    }
    type SyncRow = (
        String,
        String,
        String,
        String,
        Option<String>,
        i64,
        i64,
        i64,
        i64,
        Option<i64>,
        i64,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        i64,
        i64,
        i64,
        Option<i64>,
        i64,
        Option<i64>,
    );
    let row: Option<SyncRow> = conn.query_row(
        "SELECT run_id,run_kind,phase,status,status_reason,processed,inserted,updated,unchanged,total_estimate,progress_revision,started_at_ms,last_progress_at_ms,next_attempt_at_ms,last_success_at_ms,page_size,pace_ms,active_guild_count,lease_until_ms,unseen_candidate_count,request_after_ms FROM guild_member_sync WHERE guild_id=?1 AND enrollment_id=?2 AND bot_id=?3",
        params![scope.guild_id(),scope.enrollment.id.as_str(),bot_id],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?,r.get(10)?,r.get(11)?,r.get(12)?,r.get(13)?,r.get(14)?,r.get(15)?,r.get(16)?,r.get(17)?,r.get(18)?,r.get(19)?,r.get(20)?))
    ).optional()?;
    if let Some((
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
        _stored_candidates,
        request_after,
    )) = row
    {
        out.run_id = Some(run_id);
        out.run_kind = Some(kind.clone());
        out.phase = Some(phase.clone());
        out.status = state.clone();
        out.status_reason = reason;
        out.processed = processed;
        let [member_processed,ban_processed,verify_processed,audit_processed,member_page_interval_ms]=conn.query_row("SELECT member_processed,ban_processed,verify_processed,audit_processed,member_page_interval_ms FROM guild_member_sync WHERE guild_id=?1 AND enrollment_id=?2 AND bot_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),bot_id],|r|Ok([r.get::<Option<i64>>(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?]))?;
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
        (out.last_success_processed,out.last_success_inserted,out.last_success_updated,out.last_success_unchanged)=conn.query_row("SELECT last_success_processed,last_success_inserted,last_success_updated,last_success_unchanged FROM guild_member_sync WHERE guild_id=?1 AND enrollment_id=?2 AND bot_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),bot_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        // The sync column is not maintained. Count the same due absence rows
        // the cleanup worker selects, so the Members card can show a real backlog.
        let (gap_at, completed_at, backfill_done): (Option<i64>, Option<i64>, bool) = conn.query_row(
            "SELECT gap_at_ms,initial_completed_at_ms,case_backfill_done FROM guild_member_seed WHERE guild_id=?1",
            params![scope.guild_id()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let history_ready = completed_at.is_some() && backfill_done;
        out.unseen_candidate_count = conn.query_row(
            "SELECT COUNT(*) FROM guild_member_directory d WHERE d.guild_id=?1 AND d.enrollment_id=?2 AND d.is_current=0 AND d.is_banned=0 AND d.ever_banned=0 AND d.cleanup_after_ms<=?3 AND (?4=1 OR coalesce(d.cleanup_member_checked_at_ms,-1)<max(coalesce(?5,0),coalesce(d.departed_at_ms,0))) AND NOT EXISTS(SELECT 1 FROM guild_member_pending_bans p WHERE p.guild_id=d.guild_id AND p.enrollment_id=d.enrollment_id AND p.user_id=d.user_id) AND NOT EXISTS(SELECT 1 FROM guild_member_target_checks t WHERE t.guild_id=d.guild_id AND t.enrollment_id=d.enrollment_id AND t.user_id=d.user_id)",
            params![scope.guild_id(), scope.enrollment.id.as_str(), now_ms, i64::from(history_ready), gap_at],
            |r| Ok(r.get(0)?),
        )?;
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
                        && let Some(deadline) =
                            out.next_attempt_at.filter(|deadline| *deadline > now_ms)
                    {
                        let pages = ((total - processed).max(0) + page_size - 1) / page_size + 1;
                        let finish = deadline.saturating_add(pages.saturating_mul(cycle_ms));
                        out.phase_finish_at = Some(finish);
                        out.phase_remaining_seconds =
                            Some(finish.saturating_sub(now_ms).saturating_add(999) / 1000);
                    }
                } else if matches!(out.status.as_str(), "queued" | "running") && recent_progress {
                    let pages = ((total - processed).max(0) + page_size - 1) / page_size + 1;
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
