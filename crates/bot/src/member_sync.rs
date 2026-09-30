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

// Import and audit recovery share one guild in persisted pacing/ETA metadata.
fn ready_guild_count(guilds: impl Iterator<Item = GuildId>) -> u32 {
    guilds.collect::<HashSet<_>>().len() as u32
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

async fn settle_target_check(
    app: &Arc<BotState>,
    sink: &impl crate::logging::LogDelivery,
    guild_id: GuildId,
    store: &db::guild_data::GuildDataStore,
    scope: db::guild_data::GuildDataScope,
    check: db::guild_data::MemberTargetCheck,
    verification: MemberVerification,
) -> anyhow::Result<()> {
    let suppressed_remove_absent = check.kind == MemberVerifyKind::Member
        && check.reason == "mismatched_departure"
        && !verification.present;
    let settled = store
        .settle_member_target_check(
            scope.clone(),
            check.clone(),
            verification,
            chrono::Utc::now().timestamp_millis(),
        )
        .await?;
    if settled.is_none() && !suppressed_remove_absent {
        return Ok(());
    }
    let profile = match store.get_member(scope, check.user_id).await {
        Ok(profile) => profile,
        Err(error) => {
            tracing::warn!(
                guild_id = guild_id.get(),
                user_id = check.user_id,
                "verified departure profile read failed; logging with user ID: {error:#}"
            );
            None
        }
    };
    let departure = settled.unwrap_or_else(|| {
        // Snapshot settlement does not return an event. Preserve the captured
        // remove time, and never borrow a join date from a later directory revision.
        let joined_at_ms = profile
            .as_ref()
            .filter(|profile| {
                !profile.is_current && profile.revision == check.revision.saturating_add(1)
            })
            .and_then(|profile| profile.joined_at_ms)
            .filter(|joined| *joined > 0 && *joined <= check.requested_at_ms);
        db::guild_data::MemberDeparture {
            user_id: check.user_id,
            joined_at_ms,
            departed_at_ms: check.requested_at_ms,
            completed_stay_ms: joined_at_ms.map(|joined| check.requested_at_ms - joined),
        }
    });
    crate::logging::emit_verified_member_departure(
        app,
        sink,
        guild_id,
        &departure,
        profile.as_ref(),
    )
    .await;
    Ok(())
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
pub async fn run(
    app: Arc<BotState>,
    http: Arc<Http>,
    cache: Arc<serenity::cache::Cache>,
    rate_limit_until_ms: Arc<AtomicI64>,
    sync_bot_id: Arc<AtomicI64>,
) {
    // A READY generation is acknowledged per enrollment only after its
    // directory status and any required coverage gap have been recorded.
    let mut ready_coverage = ReadyCoverage::default();
    let mut backfilled = HashSet::<(i64, String)>::new();
    let mut offset = 0usize;
    let mut cleanup_offset = 0usize;
    let worker_id = format!(
        "{:032x}{:08x}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(1) as u128,
        std::process::id()
    );
    let startup_jitter = (chrono::Utc::now().timestamp_subsec_millis() % 15) as u64;
    let mut next_request = tokio::time::Instant::now()
        + Duration::from_secs(
            u64::from(app.cfg.member_sync.min_request_interval_secs) + startup_jitter,
        );
    let mut consecutive_failures = 0u32;
    let mut cleanup_turn = false;
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let bot_id = cache.current_user().id.get() as i64;
        if bot_id <= 1 {
            continue;
        }
        sync_bot_id.store(bot_id, Ordering::Relaxed);
        let mut guilds = cache.guilds();
        guilds.sort_unstable();
        if guilds.is_empty() {
            continue;
        }
        let ready_generation = app.gateway_ready_generation.load(Ordering::Relaxed);
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut ready = Vec::new();
        let mut available = Vec::new();
        let mut initial_estimates = Vec::new();
        for guild_id in guilds {
            let gid = guild_id.get() as i64;
            let Some(enrollment) = db::guild_enrollment(&app.db, gid).await.ok().flatten() else {
                continue;
            };
            let Ok((store, scope)) = app.tenant.live(enrollment.clone()).await else {
                continue;
            };
            member_directory::flush_pending_gaps(&app, &store, &scope).await;
            let key = (gid, enrollment.id.clone());
            let (newly_initialized, session_replaced) =
                ready_coverage.pending(&key, ready_generation);
            let status = if newly_initialized {
                match store
                    .initialize_member_directory(scope.clone(), bot_id, now_ms)
                    .await
                {
                    Ok(status) => status,
                    Err(error) => {
                        tracing::warn!(
                            guild_id = gid,
                            "member directory initialization deferred: {error:#}"
                        );
                        continue;
                    }
                }
            } else {
                match store
                    .member_sync_status(scope.clone(), bot_id, now_ms)
                    .await
                {
                    Ok(status) => status,
                    Err(error) => {
                        tracing::warn!(
                            guild_id = gid,
                            "member directory status unavailable: {error:#}"
                        );
                        continue;
                    }
                }
            };
            if coverage_gap_for_session(
                newly_initialized,
                session_replaced,
                status.initial_completed_at.is_some() || status.last_progress_at.is_some(),
            ) && let Err(error) = store
                .mark_member_reconciliation_needed(
                    scope.clone(),
                    "gateway continuity interrupted",
                    now_ms,
                )
                .await
            {
                tracing::warn!(
                    guild_id = gid,
                    "member directory coverage gap could not be recorded: {error:#}"
                );
                continue;
            }
            ready_coverage.acknowledge(key, ready_generation);
            let backfill_key = (gid, enrollment.id.clone());
            if !backfilled.contains(&backfill_key) {
                match store
                    .backfill_member_case_index(scope.clone(), bot_id, 100)
                    .await
                {
                    Ok(true) => {
                        backfilled.insert(backfill_key);
                    }
                    Ok(false) => {}
                    Err(error) => tracing::warn!(
                        guild_id = gid,
                        "member case index backfill deferred: {error:#}"
                    ),
                }
            }
            available.push((guild_id, store.clone(), scope.clone()));
            for work in due_work(
                &status,
                now_ms,
                i64::from(app.cfg.member_sync.min_request_interval_secs) * 1000,
            ) {
                ready.push((guild_id, store.clone(), scope.clone(), work));
            }
            if newly_initialized
                && status.run_kind.as_deref() == Some("initial")
                && status.phase.as_deref() == Some("members")
                && status.status == "queued"
                && status.processed == 0
                && status.initial_completed_at.is_none()
                && let Some(member_count) = cache
                    .guild(guild_id)
                    .map(|guild| guild.member_count as i64)
                    .filter(|count| *count > 0)
            {
                initial_estimates.push((guild_id, store, scope, member_count));
            }
        }
        let active_guild_count =
            ready_guild_count(ready.iter().map(|(guild_id, _, _, _)| *guild_id));
        // Initial hints and committed pages use the same admitted scheduling share.
        for (guild_id, store, scope, member_count) in initial_estimates {
            if let Err(error) = store
                .set_initial_member_estimate(
                    scope,
                    bot_id,
                    db::guild_data::MemberInitialEstimate {
                        member_count,
                        page_size: app.cfg.member_sync.page_size,
                        pace_ms: i64::from(app.cfg.member_sync.min_request_interval_secs) * 1000,
                        active_guild_count,
                        now_ms,
                    },
                )
                .await
            {
                tracing::warn!(
                    guild_id = guild_id.get(),
                    "initial member estimate unavailable: {error:#}"
                );
            }
        }
        if tokio::time::Instant::now() < next_request {
            continue;
        }
        if rate_limit_until_ms.load(Ordering::Relaxed) > chrono::Utc::now().timestamp_millis() {
            continue;
        }
        let pace = Duration::from_secs(u64::from(app.cfg.member_sync.min_request_interval_secs));
        if cleanup_turn || ready.is_empty() {
            let mut cleaned = false;
            for step in 0..available.len() {
                let (guild_id, store, scope) =
                    &available[(cleanup_offset + step) % available.len()];
                let targeted = match store.next_member_target_check(scope.clone(), now_ms).await {
                    Ok(check) => check,
                    Err(error) => {
                        tracing::warn!(
                            guild_id = guild_id.get(),
                            "member target check lookup failed: {error:#}"
                        );
                        None
                    }
                };
                if let Some(check) = targeted {
                    let Some(request_claim) = reserve_request(&app, bot_id, &worker_id).await
                    else {
                        cleaned = true;
                        break;
                    };
                    if rate_limit_until_ms.load(Ordering::Relaxed)
                        > chrono::Utc::now().timestamp_millis()
                    {
                        finish_request(&app, request_claim, &rate_limit_until_ms).await;
                        cleaned = true;
                        break;
                    }
                    cleaned = true;
                    cleanup_offset = cleanup_offset.wrapping_add(step + 1);
                    next_request = tokio::time::Instant::now() + pace;
                    match fetch_target_check(&app, &http, *guild_id, &check).await {
                        Ok(verification) => {
                            match settle_target_check(
                                &app,
                                http.as_ref(),
                                *guild_id,
                                store,
                                scope.clone(),
                                check.clone(),
                                verification,
                            )
                            .await
                            {
                                Ok(()) => {}
                                Err(error) => {
                                    tracing::warn!(
                                        guild_id = guild_id.get(),
                                        "member target check settlement failed: {error:#}"
                                    );
                                    if matches!(
                                        error.downcast_ref::<db::guild_data::MemberSyncError>(),
                                        Some(db::guild_data::MemberSyncError::Stale)
                                    ) && check.reason != "ambiguous_departure"
                                        && let Err(queue_error) = store
                                            .refresh_stale_member_target_check(
                                                scope.clone(),
                                                check.clone(),
                                                chrono::Utc::now().timestamp_millis(),
                                            )
                                            .await
                                    {
                                        tracing::warn!(
                                            guild_id = guild_id.get(),
                                            "stale target check requeue failed: {queue_error:#}"
                                        );
                                    }
                                }
                            }
                        }
                        Err(error) => {
                            let (_, denied) = retry_reason(&error);
                            let delay = if denied {
                                Duration::from_secs(3600)
                            } else {
                                retry_delay(&app, 1)
                            };
                            let retry_at = chrono::Utc::now()
                                .timestamp_millis()
                                .saturating_add(delay.as_millis() as i64)
                                .max(rate_limit_until_ms.load(Ordering::Relaxed));
                            if let Err(save_error) = store
                                .defer_member_target_check(scope.clone(), check, retry_at)
                                .await
                            {
                                tracing::warn!(
                                    guild_id = guild_id.get(),
                                    "member target check retry checkpoint failed: {save_error:#}"
                                );
                            }
                        }
                    }
                    finish_request(&app, request_claim, &rate_limit_until_ms).await;
                    break;
                }
                let pending = match store
                    .next_member_pending_ban_verification(scope.clone(), now_ms)
                    .await
                {
                    Ok(pending) => pending,
                    Err(error) => {
                        tracing::warn!(
                            guild_id = guild_id.get(),
                            "pending ban verification lookup failed: {error:#}"
                        );
                        None
                    }
                };
                if let Some(candidate) = pending {
                    let Some(request_claim) = reserve_request(&app, bot_id, &worker_id).await
                    else {
                        cleaned = true;
                        break;
                    };
                    if rate_limit_until_ms.load(Ordering::Relaxed)
                        > chrono::Utc::now().timestamp_millis()
                    {
                        finish_request(&app, request_claim, &rate_limit_until_ms).await;
                        cleaned = true;
                        break;
                    }
                    cleaned = true;
                    cleanup_offset = cleanup_offset.wrapping_add(step + 1);
                    next_request = tokio::time::Instant::now() + pace;
                    let result = tokio::time::timeout(
                        Duration::from_secs(u64::from(app.cfg.member_sync.request_timeout_secs)),
                        http.get_ban(*guild_id, UserId::new(candidate.user_id as u64)),
                    )
                    .await;
                    match result {
                        Ok(Ok(ban)) => {
                            if let Err(error) = store
                                .settle_member_pending_ban_verification(
                                    scope.clone(),
                                    candidate,
                                    ban.is_some(),
                                    chrono::Utc::now().timestamp_millis(),
                                )
                                .await
                            {
                                tracing::warn!(
                                    guild_id = guild_id.get(),
                                    "pending ban verification settlement failed: {error:#}"
                                );
                            }
                        }
                        result => {
                            let (_reason, denied) = match &result {
                                Ok(Err(error)) => retry_reason_http(error),
                                _ => ("discord_unavailable", false),
                            };
                            let delay = if denied {
                                Duration::from_secs(3600)
                            } else {
                                retry_delay(&app, 1)
                            };
                            let retry_at = chrono::Utc::now()
                                .timestamp_millis()
                                .saturating_add(delay.as_millis() as i64)
                                .max(rate_limit_until_ms.load(Ordering::Relaxed));
                            if let Err(error) = store
                                .defer_member_pending_ban_verification(
                                    scope.clone(),
                                    candidate,
                                    retry_at,
                                )
                                .await
                            {
                                tracing::warn!(
                                    guild_id = guild_id.get(),
                                    "pending ban verification retry checkpoint failed: {error:#}"
                                );
                            }
                        }
                    }
                    finish_request(&app, request_claim, &rate_limit_until_ms).await;
                    break;
                }
                let candidate = match store
                    .next_member_cleanup_candidate(scope.clone(), now_ms)
                    .await
                {
                    Ok(Some(candidate)) => candidate,
                    Ok(None) => continue,
                    Err(error) => {
                        tracing::warn!(
                            guild_id = guild_id.get(),
                            "member cleanup lookup failed: {error:#}"
                        );
                        continue;
                    }
                };
                let Some(request_claim) = reserve_request(&app, bot_id, &worker_id).await else {
                    cleaned = true;
                    break;
                };
                if rate_limit_until_ms.load(Ordering::Relaxed)
                    > chrono::Utc::now().timestamp_millis()
                {
                    finish_request(&app, request_claim, &rate_limit_until_ms).await;
                    cleaned = true;
                    break;
                }
                cleaned = true;
                cleanup_offset = cleanup_offset.wrapping_add(step + 1);
                next_request = tokio::time::Instant::now() + pace;
                let result = tokio::time::timeout(
                    Duration::from_secs(u64::from(app.cfg.member_sync.request_timeout_secs)),
                    http.get_ban(*guild_id, UserId::new(candidate.user_id as u64)),
                )
                .await;
                match result {
                    Ok(Ok(ban)) => {
                        if let Err(error) = store
                            .settle_member_cleanup(
                                scope.clone(),
                                candidate,
                                ban.is_some(),
                                chrono::Utc::now().timestamp_millis(),
                            )
                            .await
                        {
                            tracing::warn!(
                                guild_id = guild_id.get(),
                                "member cleanup settlement failed: {error:#}"
                            );
                        }
                    }
                    result => {
                        let (reason, denied) = match &result {
                            Ok(Err(error)) => retry_reason_http(error),
                            _ => ("discord_unavailable", false),
                        };
                        let delay = if denied {
                            Duration::from_secs(3600)
                        } else {
                            retry_delay(&app, 1)
                        };
                        tracing::warn!(
                            guild_id = guild_id.get(),
                            user_id = candidate.user_id,
                            reason,
                            "member cleanup verification deferred"
                        );
                        let retry_at = chrono::Utc::now()
                            .timestamp_millis()
                            .saturating_add(delay.as_millis() as i64)
                            .max(rate_limit_until_ms.load(Ordering::Relaxed));
                        if let Err(error) = store
                            .defer_member_cleanup(scope.clone(), candidate, retry_at)
                            .await
                        {
                            tracing::warn!(
                                guild_id = guild_id.get(),
                                "member cleanup retry checkpoint failed: {error:#}"
                            );
                        }
                    }
                }
                finish_request(&app, request_claim, &rate_limit_until_ms).await;
                break;
            }
            cleanup_turn = false;
            if cleaned {
                continue;
            }
        }
        if ready.is_empty() {
            tokio::time::sleep(Duration::from_secs(8)).await;
            continue;
        }
        let Some(request_claim) = reserve_request(&app, bot_id, &worker_id).await else {
            continue;
        };
        if rate_limit_until_ms.load(Ordering::Relaxed) > chrono::Utc::now().timestamp_millis() {
            finish_request(&app, request_claim, &rate_limit_until_ms).await;
            continue;
        }
        next_request = tokio::time::Instant::now() + pace;
        let mut attempted = false;
        for _ in 0..ready.len() {
            let (guild_id, store, scope, work) = ready[offset % ready.len()].clone();
            offset = offset.wrapping_add(1);
            let gid = guild_id.get() as i64;
            let now_ms = chrono::Utc::now().timestamp_millis();
            if work == ReadyWork::AuditRecovery {
                let claim = match store
                    .claim_member_audit_recovery(
                        scope.clone(),
                        bot_id,
                        worker_id.clone(),
                        now_ms,
                        request_lease_ms(app.cfg.member_sync.request_timeout_secs),
                    )
                    .await
                {
                    Ok(Some(claim)) => claim,
                    Ok(None) => continue,
                    Err(error) => {
                        tracing::warn!(
                            guild_id = gid,
                            "member audit recovery claim failed: {error:#}"
                        );
                        continue;
                    }
                };
                attempted = true;
                cleanup_turn = true;
                next_request = tokio::time::Instant::now() + pace;
                let timeout =
                    Duration::from_secs(u64::from(app.cfg.member_sync.request_timeout_secs));
                match fetch_audit_page(
                    &http,
                    guild_id,
                    claim.cursor.as_deref(),
                    claim.not_before_ms,
                    timeout,
                )
                .await
                {
                    Ok(page) => {
                        let claim_for_defer = claim.clone();
                        let scope_for_defer = scope.clone();
                        if let Err(error) = store
                            .commit_member_audit_recovery_page(
                                scope,
                                claim,
                                page.audits,
                                (page.next_cursor, page.terminal),
                                chrono::Utc::now().timestamp_millis(),
                                pace.as_millis() as i64,
                            )
                            .await
                        {
                            tracing::warn!(
                                guild_id = gid,
                                "member audit recovery page discarded: {error:#}"
                            );
                            let now_ms = chrono::Utc::now().timestamp_millis();
                            let reason = if matches!(
                                error.downcast_ref::<db::guild_data::MemberSyncError>(),
                                Some(db::guild_data::MemberSyncError::Stale)
                            ) {
                                "directory_changed"
                            } else {
                                "commit_failed"
                            };
                            if let Err(defer_error) = store
                                .defer_member_audit_recovery(
                                    scope_for_defer,
                                    claim_for_defer,
                                    reason.to_owned(),
                                    now_ms,
                                    now_ms,
                                )
                                .await
                            {
                                tracing::warn!(
                                    guild_id = gid,
                                    "member audit recovery claim release failed: {defer_error:#}"
                                );
                            }
                        } else {
                            consecutive_failures = 0;
                        }
                    }
                    Err(error) => {
                        consecutive_failures = consecutive_failures.saturating_add(1);
                        let (reason, denied) =
                            retry_reason_with_rate_limit(&error, &rate_limit_until_ms);
                        let delay = if denied {
                            Duration::from_secs(3600)
                        } else {
                            retry_delay(&app, consecutive_failures)
                        };
                        let next_ms = chrono::Utc::now()
                            .timestamp_millis()
                            .saturating_add(delay.max(pace).as_millis() as i64)
                            .max(rate_limit_until_ms.load(Ordering::Relaxed));
                        if let Err(save_error) = store
                            .defer_member_audit_recovery(
                                scope,
                                claim,
                                reason.to_owned(),
                                next_ms,
                                chrono::Utc::now().timestamp_millis(),
                            )
                            .await
                        {
                            tracing::warn!(
                                guild_id = gid,
                                "member audit recovery retry checkpoint failed: {save_error:#}"
                            );
                        }
                    }
                }
                break;
            }
            let claim = match store
                .claim_member_sync(
                    scope.clone(),
                    bot_id,
                    worker_id.clone(),
                    now_ms,
                    request_lease_ms(app.cfg.member_sync.request_timeout_secs),
                )
                .await
            {
                Ok(Some(claim)) => claim,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(guild_id = gid, "member sync claim failed: {error:#}");
                    continue;
                }
            };
            attempted = true;
            cleanup_turn = true;
            // Set the next local slot before HTTP so errors and cancellations
            // cannot cause an immediate second Discord request.
            next_request = tokio::time::Instant::now()
                + Duration::from_secs(u64::from(app.cfg.member_sync.min_request_interval_secs));
            let member_estimate = cache
                .guild(guild_id)
                .map(|g| g.member_count as i64)
                .filter(|n| *n > 0);
            match fetch_page(
                &app,
                &http,
                guild_id,
                &claim,
                member_estimate,
                active_guild_count,
            )
            .await
            {
                Ok(page) => match store
                    .commit_member_page(
                        scope.clone(),
                        claim.clone(),
                        page,
                        chrono::Utc::now().timestamp_millis(),
                    )
                    .await
                {
                    Ok(_) => {
                        consecutive_failures = 0;
                    }
                    Err(error) => {
                        tracing::warn!(guild_id = gid, "member sync page discarded: {error:#}");
                        // Every discarded page releases this claim. A stale revision
                        // retries on the shared pace; any other commit failure backs
                        // off instead of holding the lease until it expires.
                        let stale = matches!(
                            error.downcast_ref::<db::guild_data::MemberSyncError>(),
                            Some(db::guild_data::MemberSyncError::Stale)
                        );
                        if !stale {
                            consecutive_failures = consecutive_failures.saturating_add(1);
                        }
                        let now_ms = chrono::Utc::now().timestamp_millis();
                        let delay = if stale {
                            Duration::ZERO
                        } else {
                            retry_delay(&app, consecutive_failures).max(Duration::from_secs(
                                u64::from(app.cfg.member_sync.min_request_interval_secs),
                            ))
                        };
                        let next_ms = now_ms
                            .saturating_add(delay.as_millis() as i64)
                            .max(rate_limit_until_ms.load(Ordering::Relaxed));
                        if let Err(defer_error) = store
                            .defer_member_sync(
                                scope,
                                claim,
                                if stale {
                                    "directory_changed"
                                } else {
                                    "commit_failed"
                                }
                                .to_owned(),
                                next_ms,
                                now_ms,
                            )
                            .await
                        {
                            tracing::warn!(
                                guild_id = gid,
                                "member page claim release failed: {defer_error:#}"
                            );
                        }
                    }
                },
                Err(error) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    tracing::warn!(guild_id = gid, "member sync request failed: {error:#}");
                    let (reason, denied) =
                        retry_reason_with_rate_limit(&error, &rate_limit_until_ms);
                    let delay = if denied {
                        Duration::from_secs(3600)
                    } else {
                        retry_delay(&app, consecutive_failures)
                    };
                    let delay = delay.max(Duration::from_secs(u64::from(
                        app.cfg.member_sync.min_request_interval_secs,
                    )));
                    next_request = tokio::time::Instant::now()
                        + Duration::from_secs(u64::from(
                            app.cfg.member_sync.min_request_interval_secs,
                        ));
                    let next_ms = chrono::Utc::now()
                        .timestamp_millis()
                        .saturating_add(delay.as_millis() as i64)
                        .max(rate_limit_until_ms.load(Ordering::Relaxed));
                    if let Err(save_error) = store
                        .defer_member_sync(
                            scope,
                            claim,
                            reason.to_owned(),
                            next_ms,
                            chrono::Utc::now().timestamp_millis(),
                        )
                        .await
                    {
                        tracing::warn!(
                            guild_id = gid,
                            "member sync retry checkpoint failed: {save_error:#}"
                        );
                    }
                }
            }
            break;
        }
        finish_request(&app, request_claim, &rate_limit_until_ms).await;
        if !attempted {
            tokio::time::sleep(Duration::from_secs(8)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct LeaveSink(std::sync::Mutex<Vec<serde_json::Value>>);

    #[serenity::async_trait]
    impl crate::logging::LogDelivery for LeaveSink {
        async fn send(
            &self,
            _channel: serenity::all::ChannelId,
            message: serenity::all::CreateMessage,
        ) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(serde_json::to_value(message)?);
            Ok(())
        }
    }

    #[tokio::test]
    async fn mismatched_departure_emits_only_after_verified_absence() {
        use crate::{
            config::BotProcessConfig,
            db::{
                Db,
                guild_data::{MemberDepartureDecision, MemberObservation},
            },
        };
        use serenity::all::User;
        let directory =
            std::env::temp_dir().join(format!("verified_leave_{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut cfg = BotProcessConfig::default();
        cfg.db.path = directory.join("core.db");
        let app = BotState::new(cfg.clone(), Db::open(&cfg.db.path, 2).unwrap()).unwrap();
        let enrollment = db::enroll_guild(&app.db, 10, 100).await.unwrap().unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();
        let (store, scope) = app.tenant.live(enrollment).await.unwrap();
        let removed_at = chrono::Utc::now().timestamp_millis() - 60_000;
        store
            .initialize_member_directory(scope.clone(), 42, removed_at - 1000)
            .await
            .unwrap();
        db::upsert_log_channel(&app.db, 10, "member_leave", 456)
            .await
            .unwrap();
        db::upsert_template(&app.db, 10, "member_leave", "{user}|{duration}|{time}")
            .await
            .unwrap();
        let sink = LeaveSink::default();
        for (id, present) in [(7, false), (8, true)] {
            let observation = MemberObservation {
                user_id: id,
                username: Some(Some("saved-user".into())),
                global_name: None,
                nickname: None,
                joined_at_ms: Some(removed_at - 3_600_000),
                observed_at_ms: removed_at - 1000,
                is_join: false,
            };
            store
                .observe_member(scope.clone(), observation.clone())
                .await
                .unwrap();
            let mut user = User::default();
            user.id = UserId::new(id as u64);
            assert!(matches!(
                member_directory::observe_departure(
                    &app,
                    10,
                    &user,
                    removed_at,
                    Some(removed_at - 7_200_000)
                )
                .await,
                Some(MemberDepartureDecision::NeedsVerification)
            ));
            let check = store
                .next_member_target_check(scope.clone(), removed_at)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(check.reason, "mismatched_departure");
            assert_eq!(check.requested_at_ms, removed_at);
            let verification = MemberVerification {
                user_id: id,
                revision: check.revision,
                kind: MemberVerifyKind::Member,
                present,
                member: present.then_some(observation),
                ban: None,
            };
            settle_target_check(
                &app,
                &sink,
                GuildId::new(10),
                &store,
                scope.clone(),
                check.clone(),
                verification.clone(),
            )
            .await
            .unwrap();
            // The original queued check cannot be replayed into a second leave.
            assert!(
                settle_target_check(
                    &app,
                    &sink,
                    GuildId::new(10),
                    &store,
                    scope.clone(),
                    check,
                    verification
                )
                .await
                .is_err()
            );
            assert_eq!(
                store
                    .get_member(scope.clone(), id)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_current,
                present
            );
        }
        let messages = std::mem::take(&mut *sink.0.lock().unwrap());
        drop((store, app));
        std::fs::remove_dir_all(directory).unwrap();
        assert_eq!(
            messages.len(),
            1,
            "verified absence must emit the suppressed leave exactly once"
        );
        let content = messages[0]["content"].as_str().unwrap();
        assert!(content.contains("saved-user"));
        assert!(content.contains("|1 hour|"));
        assert!(
            content.contains(
                &chrono::DateTime::<chrono::Utc>::from_timestamp_millis(removed_at)
                    .unwrap()
                    .format("%b %d, %Y • %H:%M:%S UTC")
                    .to_string()
            )
        );
    }

    #[tokio::test]
    async fn initial_eta_counts_only_ready_enrolled_guilds() {
        use crate::{config::BotProcessConfig, db::Db};
        use serenity::all::{Cache, CurrentUser, Guild, GuildCreateEvent, UserUpdateEvent};

        let directory =
            std::env::temp_dir().join(format!("member_sync_eta_{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut cfg = BotProcessConfig::default();
        cfg.db.path = directory.join("core.db");
        let app = BotState::new(cfg.clone(), Db::open(&cfg.db.path, 2).unwrap()).unwrap();
        let enrollment = db::enroll_guild(&app.db, 10, 100).await.unwrap().unwrap();
        app.tenant.activate(enrollment.clone()).await.unwrap();
        let (store, scope) = app.tenant.live(enrollment).await.unwrap();
        // Guild 11 is unenrolled; guild 12 has no admitted tenant placement.
        let unavailable = db::enroll_guild(&app.db, 12, 100).await.unwrap().unwrap();
        assert!(app.tenant.live(unavailable).await.is_err());
        let cache = Arc::new(Cache::new());
        let mut user = CurrentUser::default();
        user.id = UserId::new(42);
        let mut event: UserUpdateEvent =
            serde_json::from_value(serde_json::to_value(user).unwrap()).unwrap();
        cache.update(&mut event);
        for id in [10, 11, 12] {
            let mut guild = Guild::default();
            guild.id = GuildId::new(id);
            guild.member_count = 200;
            let mut event: GuildCreateEvent =
                serde_json::from_value(serde_json::to_value(guild).unwrap()).unwrap();
            cache.update(&mut event);
        }
        let worker = tokio::spawn(run(
            app.clone(),
            Arc::new(Http::new("unused")),
            cache,
            Arc::new(AtomicI64::new(i64::MAX)), // Suppress all Discord requests.
            Arc::new(AtomicI64::new(0)),
        ));
        let status = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let status = store
                    .member_sync_status(scope.clone(), 42, chrono::Utc::now().timestamp_millis())
                    .await
                    .unwrap();
                if status.total_estimate.is_some() {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        worker.abort();
        let _ = worker.await;
        let status = status.unwrap();
        drop((store, app));
        std::fs::remove_dir_all(directory).unwrap();
        assert_eq!(status.total_estimate, Some(200));
        assert_eq!(status.phase.as_deref(), Some("members"));
        assert_eq!(status.status, "queued");
        // Two member pages plus the terminal probe, with one scheduling participant.
        assert_eq!(
            status.phase_finish_at.unwrap() - status.started_at.unwrap(),
            45_000
        );
    }

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

    fn completed_import_with_audit_recovery() -> db::guild_data::MemberSyncStatus {
        db::guild_data::MemberSyncStatus {
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
        }
    }

    #[test]
    fn audit_only_recovery_is_ready_after_completed_import() {
        let status = completed_import_with_audit_recovery();
        assert_eq!(due_work(&status, 3, 15_000), vec![ReadyWork::AuditRecovery]);
    }

    #[test]
    fn ready_guild_count_does_not_double_count_import_and_audit_recovery() {
        assert_eq!(ready_guild_count(std::iter::empty()), 0);
        let mut status = completed_import_with_audit_recovery();
        status.status = "queued".into();
        status.phase = Some("members".into());
        status.run_kind = Some("reconcile".into());
        let work = due_work(&status, 3, 15_000);
        assert_eq!(work, vec![ReadyWork::Import, ReadyWork::AuditRecovery]);
        let mut ready: Vec<_> = work
            .into_iter()
            .map(|work| (GuildId::new(10), work))
            .collect();
        assert_eq!(ready_guild_count(ready.iter().map(|(guild, _)| *guild)), 1);

        // Another guild with only audit recovery adds one scheduling share.
        ready.extend(
            due_work(&completed_import_with_audit_recovery(), 3, 15_000)
                .into_iter()
                .map(|work| (GuildId::new(11), work)),
        );
        assert_eq!(ready.len(), 3);
        assert_eq!(ready_guild_count(ready.iter().map(|(guild, _)| *guild)), 2);
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
