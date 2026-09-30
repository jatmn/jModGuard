//! A single fair, deliberately slow Discord directory importer.

use crate::db::guild_data::{
    MemberAuditAction, MemberAuditSnapshot, MemberBanSnapshot, MemberSyncClaim, MemberSyncPage,
    MemberSyncPhase, MemberVerification, MemberVerifyKind,
};
use crate::{db, member_directory, state::BotState};
use serenity::{
    http::{Http, LightMethod, Request, Route},
    model::{
        guild::audit_log::{Action as AuditAction, MemberAction},
        id::{AuditLogEntryId, GuildId, UserId},
    },
};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadyWork {
    Import,
    AuditRecovery,
}

// The claim must outlive the entire configured Discord timeout and leave room
// for the page to be committed before another worker can take the same slot.
fn request_lease_ms(timeout_secs: u32) -> i64 {
    (i64::from(timeout_secs) * 1_000 + 30_000).max(90_000)
}

fn coverage_gap_for_session(
    newly_initialized: bool,
    session_replaced: bool,
    had_progress: bool,
) -> bool {
    had_progress && (newly_initialized || session_replaced)
}

#[derive(Default)]
struct ReadyCoverage {
    acknowledged: HashMap<(i64, String), u64>,
}

impl ReadyCoverage {
    fn pending(&self, key: &(i64, String), generation: u64) -> (bool, bool) {
        let previous = self.acknowledged.get(key).copied();
        (
            previous.is_none(),
            previous.is_some_and(|value| value != generation),
        )
    }

    fn acknowledge(&mut self, key: (i64, String), generation: u64) {
        self.acknowledged.insert(key, generation);
    }
}

fn due_work(
    status: &db::guild_data::MemberSyncStatus,
    now_ms: i64,
    pace_ms: i64,
) -> Vec<ReadyWork> {
    let mut work = Vec::with_capacity(2);
    let import = matches!(status.status.as_str(), "queued" | "running" | "waiting")
        && status
            .next_attempt_at
            .is_none_or(|next| next <= now_ms.saturating_add(pace_ms));
    if import {
        work.push(ReadyWork::Import);
    }
    if status.audit_recovery_pending && !status.audit_pagination_stalled() {
        work.push(ReadyWork::AuditRecovery);
    }
    work
}

fn cursor_id(cursor: &Option<String>) -> anyhow::Result<Option<u64>> {
    cursor
        .as_deref()
        .map(str::parse::<u64>)
        .transpose()
        .map_err(Into::into)
}

fn next_member_cursor(members: &[serenity::model::guild::Member]) -> Option<String> {
    members.last().map(|m| m.user.id.get().to_string())
}

fn next_ban_cursor(bans: &[serenity::model::guild::Ban]) -> Option<String> {
    bans.last().map(|b| b.user.id.get().to_string())
}

/// A full Discord page did not make directional cursor progress.
#[derive(Debug)]
struct PaginationStalled;

impl std::fmt::Display for PaginationStalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("discord pagination cursor did not advance")
    }
}

impl std::error::Error for PaginationStalled {}

/// Member and ban lists page with `after` (the next id must be greater).
/// Audit logs page with `before` (the next id must be smaller).
/// A full page that repeats or moves the wrong way must not be committed.
fn ensure_discord_cursor_advanced(
    claim_cursor: &Option<String>,
    next_cursor: &Option<String>,
    full_page: bool,
) -> anyhow::Result<()> {
    if !full_page {
        return Ok(());
    }
    if discord_cursor_progressed(claim_cursor.as_deref(), next_cursor.as_deref()) {
        Ok(())
    } else {
        Err(PaginationStalled.into())
    }
}

fn discord_cursor_progressed(previous: Option<&str>, next: Option<&str>) -> bool {
    let Some(next) = next else {
        return false;
    };
    let Some(previous) = previous else {
        return true;
    };
    if let (Some((prev_phase, prev_id)), Some((next_phase, next_id))) =
        (audit_cursor_boundary(previous), audit_cursor_boundary(next))
    {
        if prev_phase != next_phase {
            return true;
        }
        return next_id < prev_id;
    }
    match (previous.parse::<u64>(), next.parse::<u64>()) {
        (Ok(prev_id), Ok(next_id)) => next_id > prev_id,
        _ => false,
    }
}

fn audit_cursor_boundary(cursor: &str) -> Option<(&str, u64)> {
    let (phase, raw) = cursor.split_once(':')?;
    if phase != "ban" && phase != "unban" {
        return None;
    }
    let id = if raw.is_empty() {
        u64::MAX
    } else {
        raw.parse().ok()?
    };
    Some((phase, id))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuditStream {
    Ban,
    Unban,
}

impl AuditStream {
    fn action(self) -> AuditAction {
        AuditAction::Member(match self {
            Self::Ban => MemberAction::BanAdd,
            Self::Unban => MemberAction::BanRemove,
        })
    }

    fn prefix(self) -> &'static str {
        match self {
            Self::Ban => "ban",
            Self::Unban => "unban",
        }
    }

    fn history_action(self) -> MemberAuditAction {
        match self {
            Self::Ban => MemberAuditAction::Ban,
            Self::Unban => MemberAuditAction::Unban,
        }
    }
}

fn audit_position(cursor: Option<&str>) -> anyhow::Result<(AuditStream, Option<AuditLogEntryId>)> {
    let Some(cursor) = cursor else {
        return Ok((AuditStream::Ban, None));
    };
    let (phase, before) = cursor
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("invalid audit cursor"))?;
    let stream = match phase {
        "ban" => AuditStream::Ban,
        "unban" => AuditStream::Unban,
        _ => anyhow::bail!("invalid audit cursor phase"),
    };
    let before = if before.is_empty() {
        None
    } else {
        Some(AuditLogEntryId::new(before.parse()?))
    };
    Ok((stream, before))
}

struct AuditPage {
    audits: Vec<MemberAuditSnapshot>,
    next_cursor: Option<String>,
    terminal: bool,
}

async fn fetch_audit_page(
    http: &Http,
    guild_id: GuildId,
    cursor: Option<&str>,
    not_before_ms: i64,
    timeout: Duration,
) -> anyhow::Result<AuditPage> {
    let (stream, before) = audit_position(cursor)?;
    let limit = 50_u8;
    let logs = tokio::time::timeout(
        timeout,
        http.get_audit_logs(guild_id, Some(stream.action()), None, before, Some(limit)),
    )
    .await??;
    let terminal = logs.entries.len() < usize::from(limit)
        || logs
            .entries
            .iter()
            .any(|entry| entry.id.created_at().timestamp_millis() < not_before_ms);
    let next_cursor = logs
        .entries
        .last()
        .map(|entry| format!("{}:{}", stream.prefix(), entry.id.get()))
        .or_else(|| cursor.map(str::to_owned));
    ensure_discord_cursor_advanced(
        &cursor.map(str::to_owned),
        &next_cursor,
        logs.entries.len() >= usize::from(limit),
    )?;
    let audits = logs
        .entries
        .into_iter()
        .filter_map(|entry| {
            if !matches!(
                (stream, entry.action),
                (AuditStream::Ban, AuditAction::Member(MemberAction::BanAdd))
                    | (
                        AuditStream::Unban,
                        AuditAction::Member(MemberAction::BanRemove)
                    )
            ) {
                return None;
            }
            let occurred_at_ms = entry.id.created_at().timestamp_millis();
            if occurred_at_ms < not_before_ms {
                return None;
            }
            let user_id = i64::try_from(entry.target_id?.get())
                .ok()
                .filter(|id| *id > 0)?;
            Some(MemberAuditSnapshot {
                entry_id: i64::try_from(entry.id.get()).ok()?,
                user_id,
                executor_id: i64::try_from(entry.user_id.get()).ok().filter(|id| *id > 0),
                action: stream.history_action(),
                occurred_at_ms,
                reason: entry.reason,
            })
        })
        .collect();
    Ok(AuditPage {
        audits,
        next_cursor,
        terminal,
    })
}

async fn fetch_page(
    app: &BotState,
    http: &Http,
    guild_id: GuildId,
    claim: &MemberSyncClaim,
    member_estimate: Option<i64>,
    active_guild_count: u32,
) -> anyhow::Result<MemberSyncPage> {
    let after = if matches!(
        claim.phase,
        MemberSyncPhase::Verify | MemberSyncPhase::Audit
    ) {
        None
    } else {
        cursor_id(&claim.cursor)?
    };
    let timeout = Duration::from_secs(u64::from(app.cfg.member_sync.request_timeout_secs));
    let observed_at_ms = chrono::Utc::now().timestamp_millis();
    match claim.phase {
        MemberSyncPhase::Members => {
            let limit = u64::from(app.cfg.member_sync.page_size);
            let members = tokio::time::timeout(
                timeout,
                http.get_guild_members(guild_id, Some(limit), after),
            )
            .await??;
            let full_page = members.len() >= limit as usize;
            let next_cursor = next_member_cursor(&members).or_else(|| claim.cursor.clone());
            ensure_discord_cursor_advanced(&claim.cursor, &next_cursor, full_page)?;
            let terminal = !full_page;
            Ok(MemberSyncPage {
                phase: MemberSyncPhase::Members,
                expected_cursor: claim.cursor.clone(),
                next_cursor,
                members: members
                    .iter()
                    .map(|m| member_directory::from_member(m, observed_at_ms, true))
                    .collect(),
                bans: Vec::new(),
                audits: Vec::new(),
                verification: None,
                terminal,
                total_estimate: member_estimate,
                page_size: app.cfg.member_sync.page_size,
                pace_ms: i64::from(app.cfg.member_sync.min_request_interval_secs) * 1000,
                active_guild_count,
            })
        }
        MemberSyncPhase::Bans => {
            // Serenity's get_bans wrapper accepts only u8; its public raw
            // request path retains the shared rate limiter for 1..=500 pages.
            let limit = app.cfg.member_sync.page_size;
            let mut params = vec![("limit", limit.to_string())];
            if let Some(after) = after {
                params.push(("after", after.to_string()));
            }
            let bans: Vec<serenity::model::guild::Ban> = tokio::time::timeout(
                timeout,
                http.fire(
                    Request::new(Route::GuildBans { guild_id }, LightMethod::Get)
                        .params(Some(params)),
                ),
            )
            .await??;
            let full_page = bans.len() >= usize::from(limit);
            let next_cursor = next_ban_cursor(&bans).or_else(|| claim.cursor.clone());
            ensure_discord_cursor_advanced(&claim.cursor, &next_cursor, full_page)?;
            let terminal = !full_page;
            Ok(MemberSyncPage {
                phase: MemberSyncPhase::Bans,
                expected_cursor: claim.cursor.clone(),
                next_cursor,
                members: Vec::new(),
                bans: bans
                    .into_iter()
                    .map(|b| MemberBanSnapshot {
                        user_id: b.user.id.get() as i64,
                        username: Some(b.user.name),
                        global_name: b.user.global_name,
                        reason: b.reason,
                    })
                    .collect(),
                audits: Vec::new(),
                verification: None,
                terminal,
                total_estimate: None,
                page_size: app.cfg.member_sync.page_size,
                pace_ms: i64::from(app.cfg.member_sync.min_request_interval_secs) * 1000,
                active_guild_count,
            })
        }
        MemberSyncPhase::Audit => {
            let page = fetch_audit_page(
                http,
                guild_id,
                claim.cursor.as_deref(),
                claim.not_before_ms.unwrap_or(0),
                timeout,
            )
            .await?;
            Ok(MemberSyncPage {
                phase: MemberSyncPhase::Audit,
                expected_cursor: claim.cursor.clone(),
                next_cursor: page.next_cursor,
                members: Vec::new(),
                bans: Vec::new(),
                audits: page.audits,
                verification: None,
                terminal: page.terminal,
                total_estimate: None,
                page_size: app.cfg.member_sync.page_size,
                pace_ms: i64::from(app.cfg.member_sync.min_request_interval_secs) * 1000,
                active_guild_count,
            })
        }
        MemberSyncPhase::Verify => {
            let Some(user_id) = claim.verify_user_id else {
                return Ok(MemberSyncPage {
                    phase: MemberSyncPhase::Verify,
                    expected_cursor: claim.cursor.clone(),
                    next_cursor: claim.cursor.clone(),
                    members: Vec::new(),
                    bans: Vec::new(),
                    audits: Vec::new(),
                    verification: None,
                    terminal: true,
                    total_estimate: None,
                    page_size: app.cfg.member_sync.page_size,
                    pace_ms: i64::from(app.cfg.member_sync.min_request_interval_secs) * 1000,
                    active_guild_count,
                });
            };
            let revision = claim
                .verify_revision
                .ok_or_else(|| anyhow::anyhow!("missing verification revision"))?;
            let kind = claim
                .verify_kind
                .ok_or_else(|| anyhow::anyhow!("missing verification kind"))?;
            let discord_id = UserId::new(user_id as u64);
            let (present, member, ban) = match kind {
                MemberVerifyKind::Member => {
                    let fetched =
                        tokio::time::timeout(timeout, http.get_member(guild_id, discord_id))
                            .await?;
                    match fetched {
                        Ok(member) => (
                            true,
                            Some(member_directory::from_member(&member, observed_at_ms, true)),
                            None,
                        ),
                        Err(serenity::Error::Http(
                            serenity::http::HttpError::UnsuccessfulRequest(response),
                        )) if response.status_code.as_u16() == 404
                            && response.error.code == 10007 =>
                        {
                            (false, None, None)
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
                MemberVerifyKind::Ban => {
                    let fetched =
                        tokio::time::timeout(timeout, http.get_ban(guild_id, discord_id)).await??;
                    let present = fetched.is_some();
                    let ban = fetched.map(|b| MemberBanSnapshot {
                        user_id,
                        username: Some(b.user.name),
                        global_name: b.user.global_name,
                        reason: b.reason,
                    });
                    (present, None, ban)
                }
            };
            Ok(MemberSyncPage {
                phase: MemberSyncPhase::Verify,
                expected_cursor: claim.cursor.clone(),
                next_cursor: Some(format!(
                    "{}:{user_id}",
                    match kind {
                        MemberVerifyKind::Member => "m",
                        MemberVerifyKind::Ban => "b",
                    }
                )),
                members: Vec::new(),
                bans: Vec::new(),
                audits: Vec::new(),
                verification: Some(MemberVerification {
                    user_id,
                    revision,
                    kind,
                    present,
                    member,
                    ban,
                }),
                terminal: false,
                total_estimate: None,
                page_size: app.cfg.member_sync.page_size,
                pace_ms: i64::from(app.cfg.member_sync.min_request_interval_secs) * 1000,
                active_guild_count,
            })
        }
        MemberSyncPhase::Complete => anyhow::bail!("completed import cannot fetch a page"),
    }
}

fn retry_delay(app: &BotState, failed: u32) -> Duration {
    let shift = failed.min(16);
    let base = u64::from(app.cfg.member_sync.retry_base_secs);
    let max = u64::from(app.cfg.member_sync.retry_max_secs);
    let jitter = (chrono::Utc::now().timestamp_subsec_millis() % 7) as u64;
    Duration::from_secs(
        base.saturating_mul(1_u64 << shift)
            .saturating_add(jitter)
            .min(max),
    )
}

fn retry_reason(error: &anyhow::Error) -> (&'static str, bool) {
    if error.downcast_ref::<PaginationStalled>().is_some() {
        return ("pagination_stalled", false);
    }
    error
        .downcast_ref::<serenity::Error>()
        .map(retry_reason_http)
        .unwrap_or(("discord_unavailable", false))
}

fn retry_reason_with_rate_limit(
    error: &anyhow::Error,
    rate_limit_until_ms: &AtomicI64,
) -> (&'static str, bool) {
    let result = retry_reason(error);
    if result.1 || result.0 == "rate_limited" {
        return result;
    }
    // Serenity waits for Retry-After inside the HTTP future. Our shorter
    // request timeout can cancel that wait, leaving only Elapsed as the error.
    // The callback has already saved the provider's deadline for pacing.
    if error
        .downcast_ref::<tokio::time::error::Elapsed>()
        .is_some()
        && rate_limit_until_ms.load(Ordering::Relaxed) > chrono::Utc::now().timestamp_millis()
    {
        return ("rate_limited", false);
    }
    result
}

fn retry_reason_http(error: &serenity::Error) -> (&'static str, bool) {
    if let serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response)) = error {
        match response.status_code.as_u16() {
            401 | 403 => return ("permission_denied", true),
            429 => return ("rate_limited", false),
            _ => {}
        }
    }
    ("discord_unavailable", false)
}

async fn fetch_target_check(
    app: &BotState,
    http: &Http,
    guild_id: GuildId,
    check: &db::guild_data::MemberTargetCheck,
) -> anyhow::Result<MemberVerification> {
    let timeout = Duration::from_secs(u64::from(app.cfg.member_sync.request_timeout_secs));
    let user = UserId::new(check.user_id as u64);
    let observed_at_ms = chrono::Utc::now().timestamp_millis();
    let (present, member, ban) = match check.kind {
        MemberVerifyKind::Member => {
            let fetched = tokio::time::timeout(timeout, http.get_member(guild_id, user)).await?;
            match fetched {
                Ok(member) => (
                    true,
                    Some(member_directory::from_member(&member, observed_at_ms, true)),
                    None,
                ),
                Err(serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(
                    response,
                ))) if response.status_code.as_u16() == 404 && response.error.code == 10007 => {
                    (false, None, None)
                }
                Err(error) => return Err(error.into()),
            }
        }
        MemberVerifyKind::Ban => {
            let fetched = tokio::time::timeout(timeout, http.get_ban(guild_id, user)).await??;
            let present = fetched.is_some();
            let ban = fetched.map(|b| MemberBanSnapshot {
                user_id: check.user_id,
                username: Some(b.user.name),
                global_name: b.user.global_name,
                reason: b.reason,
            });
            (present, None, ban)
        }
    };
    Ok(MemberVerification {
        user_id: check.user_id,
        revision: check.revision,
        kind: check.kind,
        present,
        member,
        ban,
    })
}

async fn reserve_request(
    app: &BotState,
    bot_id: i64,
    worker_id: &str,
) -> Option<db::MemberSyncRequestClaim> {
    match db::claim_member_sync_request(
        &app.db,
        bot_id,
        worker_id.to_owned(),
        chrono::Utc::now().timestamp_millis(),
        i64::from(app.cfg.member_sync.min_request_interval_secs) * 1000,
        request_lease_ms(app.cfg.member_sync.request_timeout_secs),
    )
    .await
    {
        Ok(claim) => claim,
        Err(error) => {
            tracing::warn!("member sync global request gate unavailable: {error:#}");
            None
        }
    }
}

async fn finish_request(
    app: &BotState,
    claim: db::MemberSyncRequestClaim,
    rate_limit_until_ms: &AtomicI64,
) {
    let deadline = chrono::Utc::now()
        .timestamp_millis()
        .max(rate_limit_until_ms.load(Ordering::Relaxed));
    if let Err(error) = db::finish_member_sync_request(&app.db, claim, deadline).await {
        tracing::warn!("member sync global request gate release failed: {error:#}");
    }
}

/// Run behind the Gateway cache so only confirmed guilds and the current bot
/// identity are admitted. Each loop considers one guild in round-robin order.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_allowed_discord_request_keeps_all_claims_live() {
        let lease = request_lease_ms(120);
        assert!(lease > 120_000);
        assert!(lease <= 300_000);
        assert_eq!(request_lease_ms(30), 90_000);
    }

    #[tokio::test]
    async fn timeout_during_known_retry_after_is_reported_as_rate_limited() {
        let timeout = tokio::time::timeout(Duration::ZERO, std::future::pending::<()>())
            .await
            .unwrap_err();
        let error: anyhow::Error = timeout.into();
        let deadline = AtomicI64::new(chrono::Utc::now().timestamp_millis() + 60_000);
        assert_eq!(
            retry_reason_with_rate_limit(&error, &deadline),
            ("rate_limited", false)
        );
        deadline.store(chrono::Utc::now().timestamp_millis() - 1, Ordering::Relaxed);
        assert_eq!(
            retry_reason_with_rate_limit(&error, &deadline),
            ("discord_unavailable", false)
        );
    }

    #[test]
    fn discord_pagination_rejects_a_repeated_full_page_cursor() {
        let cursor = Some("42".to_string());
        let next = Some("42".to_string());
        assert!(ensure_discord_cursor_advanced(&cursor, &next, true).is_err());
        assert!(ensure_discord_cursor_advanced(&cursor, &Some("43".to_string()), true).is_ok());
        assert!(ensure_discord_cursor_advanced(&cursor, &Some("41".to_string()), true).is_err());
        assert!(ensure_discord_cursor_advanced(&cursor, &next, false).is_ok());
        let audit = Some("ban:100".to_string());
        assert!(ensure_discord_cursor_advanced(&audit, &Some("ban:99".to_string()), true).is_ok());
        assert!(
            ensure_discord_cursor_advanced(&audit, &Some("ban:100".to_string()), true).is_err()
        );
        assert!(
            ensure_discord_cursor_advanced(&audit, &Some("ban:101".to_string()), true).is_err()
        );
        assert!(
            ensure_discord_cursor_advanced(
                &Some("ban:100".to_string()),
                &Some("unban:".to_string()),
                false,
            )
            .is_ok()
        );
        assert!(
            ensure_discord_cursor_advanced(
                &Some("unban:".to_string()),
                &Some("unban:99".to_string()),
                true,
            )
            .is_ok()
        );
        let stalled: anyhow::Error = PaginationStalled.into();
        assert_eq!(retry_reason(&stalled), ("pagination_stalled", false));
    }

    #[test]
    fn replaced_ready_marks_a_gap_without_treating_resume_as_a_new_session() {
        assert!(!coverage_gap_for_session(false, false, true));
        assert!(coverage_gap_for_session(true, false, true));
        assert!(coverage_gap_for_session(false, true, true));
        assert!(!coverage_gap_for_session(false, true, false));
        assert!(!coverage_gap_for_session(true, true, false));
    }

    #[test]
    fn failed_guild_read_keeps_replaced_ready_pending_for_that_enrollment() {
        let mut coverage = ReadyCoverage::default();
        let first = (10, "first".to_owned());
        let second = (11, "second".to_owned());
        assert_eq!(coverage.pending(&first, 1), (true, false));
        coverage.acknowledge(first.clone(), 1);
        coverage.acknowledge(second.clone(), 1);

        assert_eq!(coverage.pending(&first, 2), (false, true));
        // A failed enrollment/status read must leave this generation pending.
        assert_eq!(coverage.pending(&first, 2), (false, true));
        coverage.acknowledge(second.clone(), 2);
        assert_eq!(coverage.pending(&second, 2), (false, false));
        assert_eq!(coverage.pending(&first, 2), (false, true));

        coverage.acknowledge(first.clone(), 2);
        assert_eq!(coverage.pending(&first, 2), (false, false));
        assert_eq!(
            coverage.pending(&(10, "new-enrollment".to_owned()), 2),
            (true, false)
        );
    }

    #[test]
    fn filtered_audit_streams_have_independent_durable_positions() {
        assert_eq!(audit_position(None).unwrap().0, AuditStream::Ban);
        assert!(matches!(
            audit_position(None).unwrap().0.action(),
            AuditAction::Member(MemberAction::BanAdd)
        ));
        assert_eq!(audit_position(Some("ban:")).unwrap().1, None);
        let (kind, before) = audit_position(Some("ban:123")).unwrap();
        assert_eq!(kind, AuditStream::Ban);
        assert_eq!(before.unwrap().get(), 123);
        let (kind, before) = audit_position(Some("unban:456")).unwrap();
        assert_eq!(kind, AuditStream::Unban);
        assert!(matches!(
            kind.action(),
            AuditAction::Member(MemberAction::BanRemove)
        ));
        assert_eq!(before.unwrap().get(), 456);
        assert!(audit_position(Some("123")).is_err());
    }

    #[test]
    fn audit_only_recovery_is_ready_after_completed_import() {
        let status = db::guild_data::MemberSyncStatus {
            initial_completed_at: Some(1),
            reconciliation_needed: true,
            reconciliation_reason: Some("gateway continuity interrupted".into()),
            run_id: Some("completed".into()),
            run_kind: Some("initial".into()),
            phase: Some("complete".into()),
            status: "complete".into(),
            status_reason: None,
            progress_revision: 1,
            processed: 100,
            member_processed: Some(100),
            ban_processed: Some(0),
            verify_processed: Some(0),
            audit_processed: Some(0),
            local_history_complete: true,
            inserted: 100,
            updated: 0,
            unchanged: 0,
            total_estimate: Some(100),
            total_is_estimated: false,
            percent: Some(100.0),
            started_at: Some(1),
            last_progress_at: Some(2),
            as_of: 3,
            next_attempt_at: None,
            phase_remaining_seconds: None,
            phase_finish_at: None,
            overall_remaining_seconds: None,
            overall_finish_at: None,
            estimate_reason: None,
            last_success_at: Some(2),
            last_success_processed: Some(100),
            last_success_inserted: Some(100),
            last_success_updated: Some(0),
            last_success_unchanged: Some(0),
            unseen_candidate_count: 0,
            audit_recovery_pending: true,
            audit_recovery_status: Some("queued".into()),
            audit_recovery_phase: Some("ban".into()),
            audit_recovery_processed: 0,
            audit_recovery_next_attempt_at: Some(3),
            audit_recovery_reason: None,
            audit_recovery_last_progress_at: None,
        };
        assert_eq!(due_work(&status, 3, 15_000), vec![ReadyWork::AuditRecovery]);
    }

    #[test]
    fn stalled_audit_recovery_does_not_schedule_worker_claims() {
        let status = db::guild_data::MemberSyncStatus {
            initial_completed_at: Some(1),
            reconciliation_needed: false,
            reconciliation_reason: None,
            audit_recovery_pending: true,
            audit_recovery_status: Some("needs_attention".into()),
            audit_recovery_reason: Some("pagination_stalled".into()),
            audit_recovery_phase: None,
            audit_recovery_processed: 0,
            audit_recovery_next_attempt_at: None,
            audit_recovery_last_progress_at: None,
            run_id: None,
            run_kind: None,
            phase: None,
            status: "complete".into(),
            status_reason: None,
            progress_revision: 0,
            processed: 0,
            member_processed: None,
            ban_processed: None,
            verify_processed: None,
            audit_processed: None,
            local_history_complete: true,
            inserted: 0,
            updated: 0,
            unchanged: 0,
            total_estimate: None,
            total_is_estimated: false,
            percent: None,
            started_at: None,
            last_progress_at: None,
            as_of: 0,
            next_attempt_at: None,
            phase_remaining_seconds: None,
            phase_finish_at: None,
            overall_remaining_seconds: None,
            overall_finish_at: None,
            estimate_reason: None,
            last_success_at: None,
            last_success_processed: None,
            last_success_inserted: None,
            last_success_updated: None,
            last_success_unchanged: None,
            unseen_candidate_count: 0,
        };
        assert!(status.audit_pagination_stalled());
        assert!(due_work(&status, 0, 15_000).is_empty());
    }
}
