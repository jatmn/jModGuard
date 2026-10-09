//! Bot dispatch library, scheduled by the bot with bounded sequential batches.
//! Authority is re-read at execution; uncertain Discord outcomes are not replayed.
use crate::{
    bot_actions::Action,
    db::{self, BotAction, Db},
};
use anyhow::Result;
use serenity::model::{
    id::{ChannelId, GuildId, UserId},
    permissions::Permissions,
};

pub use crate::bot_actions::WorkerLimits;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    pub state: &'static str,
    pub code: &'static str,
}
impl Outcome {
    fn failed(code: &'static str) -> Self {
        Self {
            state: "failed",
            code,
        }
    }
    fn succeeded(code: &'static str) -> Self {
        Self {
            state: "succeeded",
            code,
        }
    }
    fn uncertain(code: &'static str) -> Self {
        Self {
            state: "uncertain",
            code,
        }
    }
}
#[serenity::async_trait]
pub trait DiscordActions: Send + Sync {
    fn bot_user_id(&self) -> Option<i64> {
        None
    }
    async fn membership_matches(&self, job: &BotAction) -> Result<bool>;
    /// A positive live membership observation used to repair a missed rejoin.
    /// Implementations may return `None` when they cannot expose the identity.
    async fn membership_joined_at_ns(&self, _job: &BotAction) -> Result<Option<i64>> {
        Ok(None)
    }
    async fn authorized(&self, job: &BotAction) -> Result<bool>;
    async fn valid_log_channel(&self, guild: i64, channel: i64) -> Result<bool>;
    async fn unban(&self, guild: i64, user: i64, reason: &str) -> Outcome;
}
pub struct LiveDiscord<'a> {
    pub http: &'a serenity::http::Http,
    pub bot_user_id: UserId,
}
#[serenity::async_trait]
impl DiscordActions for LiveDiscord<'_> {
    fn bot_user_id(&self) -> Option<i64> {
        i64::try_from(self.bot_user_id.get()).ok()
    }
    async fn membership_matches(&self, job: &BotAction) -> Result<bool> {
        let Some(joined_at_ns) = self.membership_joined_at_ns(job).await? else {
            anyhow::bail!("Discord member has no supported join timestamp");
        };
        Ok(job.joined_at_ns.is_some_and(|expected| {
            crate::enrollment_repair::membership_joined_at_matches(expected, joined_at_ns)
        }))
    }
    async fn membership_joined_at_ns(&self, job: &BotAction) -> Result<Option<i64>> {
        anyhow::ensure!(self.bot_user_id.get() > 1, "bot identity is not ready");
        let member = self
            .http
            .get_member(GuildId::new(job.guild_id as u64), self.bot_user_id)
            .await?;
        Ok(member
            .joined_at
            .and_then(|joined| joined.timestamp_nanos_opt())
            .filter(|joined| *joined > 0))
    }
    async fn authorized(&self, job: &BotAction) -> Result<bool> {
        let guild = self
            .http
            .get_guild(GuildId::new(job.guild_id as u64))
            .await?;
        let member = self
            .http
            .get_member(guild.id, UserId::new(job.actor_id as u64))
            .await?;
        Ok(actor_authorized(&guild, &member, &job.action))
    }
    async fn valid_log_channel(&self, guild: i64, channel: i64) -> Result<bool> {
        let channel = self
            .http
            .get_channel(ChannelId::new(channel as u64))
            .await?;
        Ok(
            matches!(channel,serenity::model::channel::Channel::Guild(c) if c.guild_id.get()==guild as u64 && c.is_text_based()),
        )
    }
    async fn unban(&self, guild: i64, user: i64, reason: &str) -> Outcome {
        match self
            .http
            .remove_ban(
                GuildId::new(guild as u64),
                UserId::new(user as u64),
                Some(reason),
            )
            .await
        {
            Ok(()) => Outcome::succeeded("unbanned"),
            Err(serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(r))) => {
                classify_unban_response(r.status_code.as_u16(), r.error.code)
            }
            Err(_) => Outcome::uncertain("discord_outcome_unknown"),
        }
    }
}
fn actor_authorized(
    guild: &serenity::model::guild::PartialGuild,
    member: &serenity::model::guild::Member,
    action: &Action,
) -> bool {
    let needed = match action {
        Action::Unban { .. } => Permissions::BAN_MEMBERS,
        Action::SetLogChannel { .. } | Action::MemberSync { .. } => Permissions::MANAGE_GUILD,
    };
    let permissions = guild.member_permissions(member);
    if guild.owner_id == member.user.id || permissions.administrator() {
        return true;
    }
    // Serenity's role calculation does not include Discord's member timeout.
    // Timed-out members retain only view/history permissions until it expires.
    if member
        .communication_disabled_until
        .is_some_and(|until| until > serenity::model::Timestamp::now())
    {
        return false;
    }
    permissions.contains(needed)
}

fn classify_unban_response(status: u16, code: isize) -> Outcome {
    if status == 404 && code == 10026 {
        Outcome::succeeded("already_unbanned")
    } else if (400..500).contains(&status) && status != 408 {
        Outcome::failed("discord_rejected")
    } else {
        Outcome::uncertain("discord_outcome_unknown")
    }
}

fn unban_case_outcome(outcome: &Outcome) -> crate::db::guild_data::CaseOutcome {
    use crate::db::guild_data::CaseOutcome;
    if outcome.code == "already_unbanned" {
        // Discord confirmed there was no ban to remove. The job succeeded as
        // an idempotent request, but its case is not evidence of a ban cycle.
        CaseOutcome::Attempted
    } else {
        match outcome.state {
            "succeeded" => CaseOutcome::Succeeded,
            "failed" => CaseOutcome::Failed,
            _ => CaseOutcome::Uncertain,
        }
    }
}

async fn finish_validation_outcome(db: &Db, job: &BotAction, code: &str) -> Result<()> {
    if db::begin_bot_action(db, job, chrono::Utc::now().timestamp()).await? {
        db::finish_dispatched_bot_action(db, job, "failed", code).await?;
    }
    Ok(())
}

/// Execute only an admitted claim. The final transaction checks the claim again;
/// direct log configuration changes may supersede this operation while HTTP runs.
async fn execute_inner(
    db: &Db,
    tenant: Option<&crate::tenant::TenantRuntime>,
    app: Option<&crate::state::BotState>,
    discord: &impl DiscordActions,
    job: &BotAction,
    limits: &WorkerLimits,
) -> Result<()> {
    limits.validate()?;
    anyhow::ensure!(job.is_claimed(), "unclaimed bot action");
    let started = tokio::time::Instant::now();
    let timeout = std::time::Duration::from_secs(u64::from(limits.request_timeout_secs));
    if chrono::Utc::now().timestamp() >= job.expires_at {
        return Ok(());
    }
    let membership = async {
        match discord.membership_joined_at_ns(job).await? {
            Some(joined_at_ns) => Ok::<_, anyhow::Error>((
                job.joined_at_ns.is_some_and(|expected| {
                    crate::enrollment_repair::membership_joined_at_matches(expected, joined_at_ns)
                }),
                Some(joined_at_ns),
            )),
            None => Ok::<_, anyhow::Error>((discord.membership_matches(job).await?, None)),
        }
    };
    match tokio::time::timeout(timeout, membership).await {
        Ok(Ok((true, _))) => {}
        Ok(Ok((false, observed_joined_at_ns))) => {
            tracing::warn!(
                action_id = %job.id,
                guild_id = job.guild_id,
                expected_joined_at_ns = ?job.joined_at_ns,
                observed_joined_at_ns = ?observed_joined_at_ns,
                "bot action membership did not match its captured enrollment"
            );
            // A gateway event can be missed while Discord REST already exposes
            // the replacement join. Repair that positive observation
            // immediately so later actions do not repeat the same stale
            // failure. The claim is still unconsumed, so the enrollment
            // transition cancels it atomically instead of leaving it uncertain.
            if let Some(tenant) = tenant
                && let Some(joined_at_ns) = observed_joined_at_ns
                && Some(joined_at_ns) != job.joined_at_ns
            {
                match db::enroll_guild(db, job.guild_id, joined_at_ns).await {
                    Ok(Some(enrollment)) => {
                        tracing::warn!(
                            action_id = %job.id,
                            guild_id = job.guild_id,
                            expected_joined_at_ns = ?job.joined_at_ns,
                            observed_joined_at_ns = joined_at_ns,
                            "repaired bot enrollment after action membership mismatch"
                        );
                        tenant.activate(enrollment).await?;
                        return Ok(());
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(
                            action_id = %job.id,
                            guild_id = job.guild_id,
                            "bot enrollment repair failed before dispatch: {error:#}"
                        );
                        let code = if error
                            .downcast_ref::<db::guild_stores::GuildWriteFenced>()
                            .is_some()
                        {
                            "guild_move_fenced"
                        } else {
                            "enrollment_check_unavailable"
                        };
                        return finish_validation_outcome(db, job, code).await;
                    }
                }
            }
            return finish_validation_outcome(db, job, "bot_membership_changed").await;
        }
        Ok(Err(_)) => {
            return finish_validation_outcome(db, job, "bot_membership_check_unavailable").await;
        }
        Err(_) => return finish_validation_outcome(db, job, "validation_timed_out").await,
    }
    if !db::begin_bot_action(db, job, chrono::Utc::now().timestamp()).await? {
        return Ok(());
    }
    let mutation_started = std::sync::atomic::AtomicBool::new(false);
    let work = async {
        if chrono::Utc::now().timestamp() >= job.expires_at {
            return Outcome::failed("expired_before_execution");
        }
        match discord.authorized(job).await {
            Ok(true) => {}
            Ok(false) => return Outcome::failed("permission_revoked"),
            Err(_) => return Outcome::failed("permission_check_unavailable"),
        }
        if chrono::Utc::now().timestamp() >= job.expires_at {
            return Outcome::failed("expired_before_execution");
        }
        match db::bot_action_dispatch(db, job).await {
            Ok(db::BotActionDispatch::Ready) => {}
            Ok(db::BotActionDispatch::EnrollmentEnded) => {
                return Outcome::failed("enrollment_ended");
            }
            Ok(db::BotActionDispatch::GuildMoveFenced) => {
                return Outcome::failed("guild_move_fenced");
            }
            Err(_) => return Outcome::failed("enrollment_check_unavailable"),
        }
        match &job.action {
            Action::MemberSync { operation } => {
                let Some(tenant) = tenant else {
                    return Outcome::failed("tenant_unavailable");
                };
                let enrollment = match (&job.enrollment_id, job.joined_at_ns) {
                    (Some(id), Some(joined_at_ns)) => db::GuildEnrollment {
                        guild_id: job.guild_id,
                        id: id.clone(),
                        joined_at_ns,
                    },
                    _ => return Outcome::failed("enrollment_ended"),
                };
                let Ok((store, scope)) = tenant.live(enrollment).await else {
                    return Outcome::failed("enrollment_ended");
                };
                let action = match operation {
                    crate::bot_actions::MemberSyncOperation::Pause => {
                        crate::db::guild_data::MemberSyncAction::Pause
                    }
                    crate::bot_actions::MemberSyncOperation::Resume => {
                        crate::db::guild_data::MemberSyncAction::Resume
                    }
                    crate::bot_actions::MemberSyncOperation::Retry => {
                        crate::db::guild_data::MemberSyncAction::Retry
                    }
                    crate::bot_actions::MemberSyncOperation::Reconcile => {
                        crate::db::guild_data::MemberSyncAction::Reconcile
                    }
                };
                if app.is_some_and(|app| !app.activation.may_record()) {
                    return Outcome::failed("writer_lease_not_held");
                }
                mutation_started.store(true, std::sync::atomic::Ordering::Relaxed);
                let Some(bot_id) = discord.bot_user_id() else {
                    return Outcome::failed("bot_identity_unavailable");
                };
                let outcome = match store
                    .member_sync_action(
                        scope,
                        bot_id,
                        action,
                        job.id.clone(),
                        chrono::Utc::now().timestamp_millis(),
                    )
                    .await
                {
                    Ok(status)
                        if matches!(operation, crate::bot_actions::MemberSyncOperation::Retry)
                            && status.status == "complete" =>
                    {
                        if status.member_sync_retry_requeued_audit() {
                            Outcome::succeeded("member_sync_updated")
                        } else {
                            Outcome::succeeded("member_sync_already_completed")
                        }
                    }
                    Ok(status)
                        if matches!(
                            operation,
                            crate::bot_actions::MemberSyncOperation::Resume
                                | crate::bot_actions::MemberSyncOperation::Pause
                        ) && status.status == "complete" =>
                    {
                        Outcome::succeeded("member_sync_already_completed")
                    }
                    Ok(_) => Outcome::succeeded("member_sync_updated"),
                    Err(error) => {
                        tracing::warn!(action_id = %job.id, guild_id = job.guild_id, "member sync control failed: {error:#}");
                        if matches!(
                            error.downcast_ref::<crate::db::guild_data::MemberSyncError>(),
                            Some(crate::db::guild_data::MemberSyncError::MissingJob)
                        ) {
                            Outcome::failed("member_sync_missing_job")
                        } else if matches!(
                            error.downcast_ref::<crate::db::guild_data::MemberSyncError>(),
                            Some(crate::db::guild_data::MemberSyncError::Invalid)
                        ) {
                            Outcome::failed("member_sync_invalid_operation")
                        } else {
                            Outcome::failed("member_sync_unavailable")
                        }
                    }
                };
                if outcome.state == "succeeded"
                    && let Some(app) = app
                {
                    app.wake_member_sync();
                }
                outcome
            }
            Action::Unban { user_id, reason } => {
                if chrono::Utc::now().timestamp() >= job.expires_at {
                    return Outcome::failed("expired_before_execution");
                }
                if app.is_some_and(|app| !app.activation.may_record()) {
                    return Outcome::failed("writer_lease_not_held");
                }
                let suppression = app.map(|app| {
                    app.suppress_next_gateway_case(
                        crate::state::GatewayCaseAction::Unban,
                        job.guild_id as u64,
                        *user_id as u64,
                    )
                });
                mutation_started.store(true, std::sync::atomic::Ordering::Relaxed);
                let outcome = discord.unban(job.guild_id, *user_id, reason).await;
                if (outcome.state == "failed" || outcome.code == "already_unbanned")
                    && let (Some(app), Some(suppression)) = (app, suppression)
                {
                    app.release_gateway_case_suppression(suppression);
                }
                outcome
            }
            Action::SetLogChannel {
                channel_id: Some(channel),
                ..
            } => match discord.valid_log_channel(job.guild_id, *channel).await {
                Ok(true) if chrono::Utc::now().timestamp() < job.expires_at => {
                    Outcome::succeeded("log_channel_set")
                }
                Ok(true) => Outcome::failed("expired_before_execution"),
                Ok(false) => Outcome::failed("invalid_log_channel"),
                Err(_) => Outcome::failed("channel_check_unavailable"),
            },
            Action::SetLogChannel {
                channel_id: None, ..
            } => Outcome::succeeded("log_channel_cleared"),
        }
    };
    let remaining = timeout.saturating_sub(started.elapsed());
    let outcome = tokio::time::timeout(remaining, work)
        .await
        .unwrap_or_else(|_| {
            if mutation_started.load(std::sync::atomic::Ordering::Relaxed) {
                Outcome::uncertain("request_timed_out")
            } else {
                Outcome::failed("validation_timed_out")
            }
        });
    if app.is_some_and(|app| !app.activation.may_record()) {
        return Err(anyhow::anyhow!("writer lease is not held"));
    }
    if mutation_started.load(std::sync::atomic::Ordering::Relaxed)
        && let Some(tenant) = tenant
        && let Action::Unban { user_id, reason } = &job.action
    {
        let enrollment = match (&job.enrollment_id, job.joined_at_ns) {
            (Some(id), Some(joined_at_ns)) => Some(db::GuildEnrollment {
                guild_id: job.guild_id,
                id: id.clone(),
                joined_at_ns,
            }),
            _ => None,
        };
        let operation_id = crate::db::guild_data::CaseId::try_from(job.id.clone()).ok();
        if let (Some(enrollment), Some(operation_id)) = (enrollment, operation_id) {
            let case_outcome = unban_case_outcome(&outcome);
            let data = crate::db::guild_data::CaseData {
                source: crate::db::guild_data::CaseSource::Application { operation_id },
                action: crate::db::guild_data::CaseAction::Unban,
                outcome: case_outcome,
                subject_id: *user_id,
                actor_id: Some(job.actor_id),
                occurred_at_ms: chrono::Utc::now().timestamp_millis().max(1),
                reason: Some(reason.clone()),
                evidence: crate::db::guild_data::CaseEvidence::Unavailable,
            };
            let case_id = crate::cases::record_data(tenant, enrollment.clone(), data.clone()).await;
            if outcome.state == "succeeded" && outcome.code != "already_unbanned" {
                // A successful Discord unban proves the target was banned;
                // the idempotent 10026 response does not. Persist that state
                // directly because old application unban cases cannot safely
                // distinguish these responses during a later backfill.
                let observed = match tenant.live(enrollment.clone()).await {
                    Ok((store, scope)) => {
                        store
                            .observe_unban(scope, *user_id, data.occurred_at_ms)
                            .await
                    }
                    Err(error) => Err(error),
                };
                if let Err(error) = observed {
                    tracing::warn!(action_id = %job.id, guild_id = job.guild_id, user_id, "confirmed unban state not saved: {error:#}");
                    if let Some(app) = app {
                        app.remember_member_gap(
                            job.guild_id as u64,
                            Some(enrollment.id.clone()),
                            Some(*user_id),
                            "confirmed unban persistence unavailable",
                        );
                    }
                }
            }
            if let (Some(app), Some(case_id)) = (app, case_id) {
                let expects_discord_audit =
                    crate::cases::queued_unban_expects_discord_audit(outcome.state, outcome.code);
                crate::cases::remember_recorded_application(
                    app,
                    enrollment,
                    &data,
                    case_id,
                    expects_discord_audit,
                )
                .await;
            }
        } else {
            tracing::warn!(
                action_id = %job.id,
                guild_id = job.guild_id,
                "selected unban case not recorded: invalid captured action identity"
            );
        }
    }
    if app.is_some_and(|app| !app.activation.may_record()) {
        return Err(anyhow::anyhow!("writer lease is not held"));
    }
    if !db::finish_dispatched_bot_action(db, job, outcome.state, outcome.code).await? {
        tracing::warn!(
            action_id = %job.id,
            reported_state = outcome.state,
            reported_outcome = outcome.code,
            mutation_started = mutation_started.load(std::sync::atomic::Ordering::Relaxed),
            "bot action completion rejected; recorded state preserved; action will not be resent"
        );
    }
    Ok(())
}

pub async fn execute(
    db: &Db,
    discord: &impl DiscordActions,
    job: &BotAction,
    limits: &WorkerLimits,
) -> Result<()> {
    execute_inner(db, None, None, discord, job, limits).await
}

/// Bounded sequential batch. No process restart or lease expiry replays a claim.
#[cfg(test)]
pub async fn drain(db: &Db, discord: &impl DiscordActions, limits: &WorkerLimits) -> Result<()> {
    drain_inner(db, None, None, discord, limits).await
}

/// Production drain with tenant-case recording for attempted remote actions.
pub async fn drain_with_tenant(
    db: &Db,
    tenant: &crate::tenant::TenantRuntime,
    discord: &impl DiscordActions,
    limits: &WorkerLimits,
) -> Result<()> {
    drain_inner(db, Some(tenant), None, discord, limits).await
}

/// Bot scheduler entry point: records tenant cases and correlates gateway echoes.
pub async fn drain_with_state(
    app: &crate::state::BotState,
    discord: &impl DiscordActions,
    limits: &WorkerLimits,
) -> Result<()> {
    drain_inner(&app.db, Some(&app.tenant), Some(app), discord, limits).await
}

async fn drain_inner(
    db: &Db,
    tenant: Option<&crate::tenant::TenantRuntime>,
    app: Option<&crate::state::BotState>,
    discord: &impl DiscordActions,
    limits: &WorkerLimits,
) -> Result<()> {
    limits.validate()?;
    for _ in 0..limits.batch_size {
        let Some(job) = db::claim_bot_action(db, chrono::Utc::now().timestamp()).await? else {
            break;
        };
        if let Err(error) = execute_inner(db, tenant, app, discord, &job, limits).await {
            tracing::error!(action_id=%job.id,"bot action receipt failed; action will not be resent: {error:#}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
