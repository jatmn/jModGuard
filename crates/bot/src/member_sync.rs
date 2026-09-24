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
    collections::HashSet,
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
    if status.audit_recovery_pending {
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
            let next_cursor = next_member_cursor(&members).or_else(|| claim.cursor.clone());
            let terminal = members.len() < limit as usize;
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
            let next_cursor = next_ban_cursor(&bans).or_else(|| claim.cursor.clone());
            let terminal = bans.len() < usize::from(limit);
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
    error
        .downcast_ref::<serenity::Error>()
        .map(retry_reason_http)
        .unwrap_or(("discord_unavailable", false))
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
        90_000,
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
    let mut initialized = HashSet::<(i64, String)>::new();
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
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut ready = Vec::new();
        let mut available = Vec::new();
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
            let newly_initialized = initialized.insert(key);
            let status = if newly_initialized {
                match store
                    .initialize_member_directory(scope.clone(), bot_id, now_ms)
                    .await
                {
                    Ok(status) => status,
                    Err(error) => {
                        initialized.remove(&(gid, enrollment.id));
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
            if newly_initialized
                && (status.initial_completed_at.is_some() || status.last_progress_at.is_some())
                && let Err(error) = store
                    .mark_member_reconciliation_needed(
                        scope.clone(),
                        "gateway continuity interrupted",
                        now_ms,
                    )
                    .await
            {
                initialized.remove(&(gid, enrollment.id.clone()));
                tracing::warn!(
                    guild_id = gid,
                    "member directory coverage gap could not be recorded: {error:#}"
                );
                continue;
            }
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
                            if let Err(error) = store
                                .settle_member_target_check(
                                    scope.clone(),
                                    check.clone(),
                                    verification,
                                    chrono::Utc::now().timestamp_millis(),
                                )
                                .await
                            {
                                tracing::warn!(
                                    guild_id = guild_id.get(),
                                    "member target check settlement failed: {error:#}"
                                );
                                if matches!(
                                    error.downcast_ref::<db::guild_data::MemberSyncError>(),
                                    Some(db::guild_data::MemberSyncError::Stale)
                                ) && let Err(queue_error) = store
                                    .queue_member_target_check(
                                        scope.clone(),
                                        check.user_id,
                                        check.kind,
                                        &check.reason,
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
        let active_guild_count = ready.len() as u32;
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
                        90_000,
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
                        } else {
                            consecutive_failures = 0;
                        }
                    }
                    Err(error) => {
                        consecutive_failures = consecutive_failures.saturating_add(1);
                        let (reason, denied) = retry_reason(&error);
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
                .claim_member_sync(scope.clone(), bot_id, worker_id.clone(), now_ms, 90_000)
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
                        if matches!(
                            error.downcast_ref::<db::guild_data::MemberSyncError>(),
                            Some(db::guild_data::MemberSyncError::Stale)
                        ) {
                            // A live event may have advanced the guild revision while
                            // HTTP was in flight. The page stays discarded, but release
                            // this exact claim now so the worker can refetch after the
                            // existing shared request pace instead of waiting for its
                            // ninety-second lease to expire. A replaced run or owner
                            // fails the claim fence in defer_member_sync.
                            let now_ms = chrono::Utc::now().timestamp_millis();
                            if let Err(defer_error) = store
                                .defer_member_sync(
                                    scope,
                                    claim,
                                    "directory_changed".to_owned(),
                                    now_ms,
                                    now_ms,
                                )
                                .await
                            {
                                tracing::warn!(
                                    guild_id = gid,
                                    "stale member page claim release failed: {defer_error:#}"
                                );
                            }
                        }
                    }
                },
                Err(error) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    tracing::warn!(guild_id = gid, "member sync request failed: {error:#}");
                    let (reason, denied) = retry_reason(&error);
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
}
