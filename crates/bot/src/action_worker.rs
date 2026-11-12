//! Bot dispatch library, scheduled by the bot with bounded sequential batches.
//! Authority is re-read at execution; uncertain Discord outcomes are not replayed.
use crate::{
    bot_actions::Action,
    db::{self, BotAction, Db},
};
use anyhow::{Context as _, Result};
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
#[derive(Clone, Copy, PartialEq, Eq)]
enum OutcomeState {
    Succeeded,
    Failed,
    Uncertain,
    Other,
}

impl OutcomeState {
    fn from_reported(value: &str) -> Self {
        match value {
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "uncertain" => Self::Uncertain,
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MutationPhase {
    Validating,
    Started,
}

#[derive(Default)]
struct MutationProgress(std::sync::atomic::AtomicBool);

impl MutationProgress {
    fn start(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn phase(&self) -> MutationPhase {
        if self.0.load(std::sync::atomic::Ordering::Relaxed) {
            MutationPhase::Started
        } else {
            MutationPhase::Validating
        }
    }
}

enum ActionMembership {
    Matches,
    Mismatch { observed_joined_at_ns: Option<i64> },
}

impl ActionMembership {
    fn from_check(matches: bool, observed_joined_at_ns: Option<i64>) -> Self {
        if matches {
            Self::Matches
        } else {
            Self::Mismatch {
                observed_joined_at_ns,
            }
        }
    }
}

impl Outcome {
    fn phase(&self) -> OutcomeState {
        OutcomeState::from_reported(self.state)
    }
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
    /// The bot's own live join timestamp, for work that has no queued job.
    /// `None` means the identity is unavailable.
    async fn guild_joined_at_ns(&self, _guild: i64) -> Result<Option<i64>> {
        Ok(None)
    }
    async fn authorized(&self, job: &BotAction) -> Result<bool>;
    /// The actor's current Discord role IDs, empty while timed out. Only used
    /// when [`Self::authorized`] denies a log-channel change.
    async fn actor_roles(&self, _job: &BotAction) -> Result<Vec<i64>> {
        Ok(Vec::new())
    }
    async fn valid_log_channel(&self, guild: i64, channel: i64) -> Result<bool>;
    async fn unban(&self, guild: i64, user: i64, reason: &str) -> Outcome;
    /// Depart one guild. Live implementations race the Discord call; test
    /// implementations may keep the default refusal.
    async fn leave_guild(&self, guild: i64) -> Outcome {
        let _ = guild;
        Outcome::failed("leave_unavailable")
    }
}
pub struct LiveDiscord<'a> {
    pub http: &'a serenity::http::Http,
    pub bot_user_id: UserId,
    pub activation: &'a crate::activation::Activation,
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
        self.guild_joined_at_ns(job.guild_id).await
    }
    async fn guild_joined_at_ns(&self, guild: i64) -> Result<Option<i64>> {
        anyhow::ensure!(self.bot_user_id.get() > 1, "bot identity is not ready");
        let member = self
            .http
            .get_member(GuildId::new(guild as u64), self.bot_user_id)
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
    async fn actor_roles(&self, job: &BotAction) -> Result<Vec<i64>> {
        let member = self
            .http
            .get_member(
                GuildId::new(job.guild_id as u64),
                UserId::new(job.actor_id as u64),
            )
            .await?;
        if member
            .communication_disabled_until
            .is_some_and(|until| until > serenity::model::Timestamp::now())
        {
            return Ok(Vec::new());
        }
        Ok(member
            .roles
            .iter()
            .filter_map(|role| i64::try_from(role.get()).ok())
            .collect())
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
        match crate::discord::race_discord_call(
            self.activation,
            self.http.remove_ban(
                GuildId::new(guild as u64),
                UserId::new(user as u64),
                Some(reason),
            ),
        )
        .await
        {
            crate::discord::DiscordCall::Cancelled => Outcome::uncertain("discord_outcome_unknown"),
            crate::discord::DiscordCall::Finished(Ok(())) => Outcome::succeeded("unbanned"),
            crate::discord::DiscordCall::Finished(Err(serenity::Error::Http(
                serenity::http::HttpError::UnsuccessfulRequest(r),
            ))) => classify_unban_response(r.status_code.as_u16(), r.error.code),
            crate::discord::DiscordCall::Finished(Err(_)) => {
                Outcome::uncertain("discord_outcome_unknown")
            }
        }
    }
    async fn leave_guild(&self, guild: i64) -> Outcome {
        match crate::discord::race_discord_call(
            self.activation,
            self.http.leave_guild(GuildId::new(guild as u64)),
        )
        .await
        {
            crate::discord::DiscordCall::Cancelled => Outcome::uncertain("discord_outcome_unknown"),
            crate::discord::DiscordCall::Finished(Ok(())) => Outcome::succeeded("left_guild"),
            crate::discord::DiscordCall::Finished(Err(serenity::Error::Http(
                serenity::http::HttpError::UnsuccessfulRequest(r),
            ))) => classify_leave_response(r.status_code.as_u16(), r.error.code),
            crate::discord::DiscordCall::Finished(Err(_)) => {
                Outcome::uncertain("discord_outcome_unknown")
            }
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
        // Unreachable through the worker: leave actions skip the actor check
        // because the platform owner holds no membership in arbitrary guilds.
        Action::LeaveGuild {} => Permissions::empty(),
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

/// A Web UI access role mapped to the settings level may change log channels.
/// Unban and member sync keep their Discord permission requirement.
async fn settings_role_authorized(
    tenant: Option<&crate::tenant::TenantRuntime>,
    discord: &impl DiscordActions,
    job: &BotAction,
) -> Result<bool> {
    if !matches!(job.action, Action::SetLogChannel { .. }) {
        return Ok(false);
    }
    let Some(tenant) = tenant else {
        return Ok(false);
    };
    let roles = discord.actor_roles(job).await?;
    if roles.is_empty() {
        return Ok(false);
    }
    let enrollment = db::GuildEnrollment {
        guild_id: job.guild_id,
        id: job
            .enrollment_id
            .clone()
            .context("action has no enrollment")?,
        joined_at_ns: job.joined_at_ns.context("action has no enrollment")?,
    };
    let mapped = db::settings_access_roles(tenant, enrollment).await?;
    Ok(roles.iter().any(|role| mapped.contains(role)))
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

fn classify_leave_response(status: u16, code: isize) -> Outcome {
    // A missing guild means the bot already departed (or was removed); the
    // removal request succeeded as an idempotent no-op, like already_unbanned.
    if status == 404 && code == 10004 {
        Outcome::succeeded("already_left")
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
        match outcome.phase() {
            OutcomeState::Succeeded => CaseOutcome::Succeeded,
            OutcomeState::Failed => CaseOutcome::Failed,
            OutcomeState::Uncertain | OutcomeState::Other => CaseOutcome::Uncertain,
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
            Some(joined_at_ns) => Ok::<_, anyhow::Error>(ActionMembership::from_check(
                job.joined_at_ns.is_some_and(|expected| {
                    crate::enrollment_repair::membership_joined_at_matches(expected, joined_at_ns)
                }),
                Some(joined_at_ns),
            )),
            None => Ok::<_, anyhow::Error>(ActionMembership::from_check(
                discord.membership_matches(job).await?,
                None,
            )),
        }
    };
    match tokio::time::timeout(timeout, membership).await {
        Ok(Ok(ActionMembership::Matches)) => {}
        Ok(Ok(ActionMembership::Mismatch {
            observed_joined_at_ns,
        })) => {
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
    let mutation = MutationProgress::default();
    let work = async {
        if chrono::Utc::now().timestamp() >= job.expires_at {
            return Outcome::failed("expired_before_execution");
        }
        if matches!(job.action, Action::LeaveGuild {}) {
            // Owner-only platform action admitted by the API's owner gate. The
            // actor is not a guild member, so there is no permission to check.
            // Fresh bot membership above remains required to depart a guild.
        } else {
            match discord.authorized(job).await {
                Ok(true) => {}
                Ok(false) => match settings_role_authorized(tenant, discord, job).await {
                    Ok(true) => {}
                    Ok(false) => return Outcome::failed("permission_revoked"),
                    Err(_) => return Outcome::failed("permission_check_unavailable"),
                },
                Err(_) => return Outcome::failed("permission_check_unavailable"),
            }
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
                mutation.start();
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
                if outcome.phase() == OutcomeState::Succeeded
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
                        None,
                    )
                });
                mutation.start();
                let outcome = discord.unban(job.guild_id, *user_id, reason).await;
                if outcome.phase() == OutcomeState::Succeeded
                    && let (Some(id), Some(joined_at_ns)) = (&job.enrollment_id, job.joined_at_ns)
                {
                    crate::gateway_ban_persistence::retain_confirmed_unban(
                        db.clone(),
                        db::GuildEnrollment {
                            guild_id: job.guild_id,
                            id: id.clone(),
                            joined_at_ns,
                        },
                        *user_id,
                    );
                }

                if (outcome.phase() == OutcomeState::Failed || outcome.code == "already_unbanned")
                    && let (Some(app), Some(suppression)) = (app, suppression)
                {
                    app.release_gateway_case_suppression(suppression);
                }
                outcome
            }
            Action::LeaveGuild {} => {
                if chrono::Utc::now().timestamp() >= job.expires_at {
                    return Outcome::failed("expired_before_execution");
                }
                if app.is_some_and(|app| !app.activation.may_record()) {
                    return Outcome::failed("writer_lease_not_held");
                }
                mutation.start();
                // The confirmed departure retires the membership once its
                // receipt is recorded (below); the gateway event may do so first.
                discord.leave_guild(job.guild_id).await
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
            if mutation.phase() == MutationPhase::Started {
                Outcome::uncertain("request_timed_out")
            } else {
                Outcome::failed("validation_timed_out")
            }
        });
    if app.is_some_and(|app| !app.activation.may_record()) {
        return Err(anyhow::anyhow!("writer lease is not held"));
    }
    if mutation.phase() == MutationPhase::Started
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
                ban_attempt_tag: None,
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
            if outcome.phase() == OutcomeState::Succeeded && outcome.code != "already_unbanned" {
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
    let finished = db::finish_dispatched_bot_action(db, job, outcome.state, outcome.code).await;
    if matches!(finished, Ok(false)) {
        tracing::warn!(
            action_id = %job.id,
            reported_state = outcome.state,
            reported_outcome = outcome.code,
            mutation_started = (mutation.phase() == MutationPhase::Started),
            "bot action completion rejected; recorded state preserved; action will not be resent"
        );
    }
    if matches!(job.action, Action::LeaveGuild {})
        && outcome.phase() == OutcomeState::Succeeded
        && let Some(joined_at_ns) = job.joined_at_ns
    {
        // Discord confirmed this membership is gone. Retire exactly that
        // membership now instead of relying only on the gateway departure
        // event, which a restart in between would lose and leave the server
        // listed as active forever. A newer membership or an event that already
        // retired this one is ignored by the monotonic observation. Retirement
        // follows the receipt attempt so a recorded receipt keeps Discord's
        // outcome; a receipt that failed to save stays running and becomes
        // uncertain, which must not also keep the departed server active.
        retire_confirmed_departure(db, app, job, joined_at_ns).await;
    }
    finished?;
    Ok(())
}

const RETIRE_DEPARTURE_ATTEMPTS: u32 = 3;
const RETIRE_DEPARTURE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(250);

/// Retire one confirmed owner departure. The receipt has been attempted and no
/// claim is ever replayed, so nothing but this call (or the gateway event) can
/// retire the membership: a transient write failure is retried a few times
/// rather than left as a single warning.
async fn retire_confirmed_departure(
    db: &Db,
    app: Option<&crate::state::BotState>,
    job: &BotAction,
    joined_at_ns: i64,
) {
    for attempt in 1..=RETIRE_DEPARTURE_ATTEMPTS {
        if app.is_some_and(|app| !app.activation.may_record()) {
            return;
        }
        match db::retire_guild_enrollment(db, job.guild_id, joined_at_ns).await {
            Ok(()) => return,
            Err(error) => {
                tracing::warn!(
                    action_id = %job.id,
                    guild_id = job.guild_id,
                    attempt,
                    "confirmed departure not retired{}: {error:#}",
                    if attempt == RETIRE_DEPARTURE_ATTEMPTS {
                        "; the gateway event remains the fallback"
                    } else {
                        ""
                    }
                );
            }
        }
        if attempt == RETIRE_DEPARTURE_ATTEMPTS {
            return;
        }
        tokio::time::sleep(RETIRE_DEPARTURE_RETRY_DELAY).await;
    }
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
    drain_inner(&app.db, Some(&app.tenant), Some(app), discord, limits).await?;
    expire_bans(
        &app.db,
        Some(app),
        discord,
        chrono::Utc::now().timestamp(),
        limits,
    )
    .await
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExpiryScan {
    Incomplete,
    Exhausted,
}

// Only a confirmed deadline consumption can construct the dispatch input.
// This carries no proof that Discord has performed an unban.
struct ConsumedExpiry(db::ExpiredBan);

impl ConsumedExpiry {
    async fn claim(db: &Db, due: db::ExpiredBan) -> Result<Option<Self>> {
        if db::claim_expired_ban(db, &due).await? {
            Ok(Some(Self(due)))
        } else {
            Ok(None)
        }
    }
}

async fn expire_bans(
    db: &Db,
    app: Option<&crate::state::BotState>,
    discord: &impl DiscordActions,
    now: i64,
    limits: &WorkerLimits,
) -> Result<()> {
    // Deferred bans stay due and are skipped only for the rest of this pass.
    // A failed membership check skips the whole guild for this pass and, with
    // a bot state, until that guild's saved retry time, so a batch of
    // unavailable guilds cannot spend every pass and starve later guilds.
    // `now` selects due bans; checks run one after another and can each take
    // up to the request timeout, so retry times count from each check's end.
    // Guilds whose retry time has arrived come after every other due ban,
    // earliest retry time first. Enough failing guilds to fill every poll
    // therefore cannot starve guilds that have not failed, or a retry that has
    // waited longer. Each read, found or not, counts against the batch, and
    // one read passes over every earlier retry guild that has no due ban.
    let started = tokio::time::Instant::now();
    let mut deferred = Vec::new();
    let mut deferred_guilds = app
        .map(|app| app.expiry_check_retries.waiting(now))
        .unwrap_or_default();
    let mut retries = app
        .map(|app| app.expiry_check_retries.due(now))
        .unwrap_or_default();
    let mut scan = ExpiryScan::Incomplete;
    for _ in 0..limits.batch_size {
        if app.is_some_and(|app| !app.activation.may_record()) {
            break;
        }
        #[cfg(test)]
        tests::EXPIRY_SELECTIONS.with(|count| count.set(count.get() + 1));
        let Some(due) = db::next_expired_ban(
            db,
            now,
            deferred.clone(),
            deferred_guilds.clone(),
            retries.clone(),
        )
        .await?
        else {
            scan = ExpiryScan::Exhausted;
            break;
        };
        // Reaching a retry guild releases it; the retry guilds before it had
        // no due ban at `now`, so their entries are dropped as stale.
        if let Some(position) = retries
            .iter()
            .position(|guild| *guild == due.enrollment.guild_id)
        {
            if let Some(app) = app {
                app.expiry_check_retries
                    .forget_stale(&retries[..position], now);
            }
            retries.drain(..=position);
        }
        let membership = expiry_membership(db, app, discord, &due, limits).await?;
        if let Some(app) = app {
            // Only an unavailable check waits for a retry time. A check that
            // answered, including one that found and repaired a newer join,
            // clears it.
            if membership == ExpiryMembership::Unavailable {
                let elapsed = i64::try_from(started.elapsed().as_secs()).unwrap_or(i64::MAX);
                app.expiry_check_retries
                    .failed(due.enrollment.guild_id, now.saturating_add(elapsed));
            } else {
                app.expiry_check_retries.succeeded(due.enrollment.guild_id);
            }
        }
        if membership != ExpiryMembership::Matches {
            // A changed join leaves the old membership's deadlines unclaimed;
            // the guild waits for the next poll as before.
            deferred_guilds.push(due.enrollment.guild_id);
            continue;
        }
        // Hold the command-ban slot through dispatch and the mirror update so a
        // concurrent ban command cannot re-arm or replace the ban meanwhile.
        let in_progress = match app.map(|app| {
            crate::member_directory::begin_command_ban_in_progress(
                app,
                due.enrollment.guild_id as u64,
                due.user as u64,
            )
        }) {
            Some(Err(_)) => {
                deferred.push(due.id);
                continue;
            }
            in_progress => in_progress,
        };
        let due_id = due.id;
        let Some(claimed) = ConsumedExpiry::claim(db, due).await? else {
            deferred.push(due_id);
            continue;
        };
        dispatch_expired_ban(db, app, discord, claimed, limits).await?;
        drop(in_progress);
    }
    if scan == ExpiryScan::Exhausted
        && let Some(app) = app
    {
        app.expiry_check_retries.forget_idle(now);
    }
    Ok(())
}

/// First retry delay after a guild's expiry membership check fails.
const EXPIRY_CHECK_RETRY_BASE_SECS: i64 = 60;
/// Longest delay; consecutive failures double the delay up to this bound.
const EXPIRY_CHECK_RETRY_MAX_SECS: i64 = 15 * 60;

#[derive(Clone, Copy, Debug)]
struct ExpiryCheckRetry {
    failures: u32,
    retry_at: i64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExpiryRetryState {
    Waiting,
    Due,
}

impl ExpiryCheckRetry {
    fn state(&self, now: i64) -> ExpiryRetryState {
        if now < self.retry_at {
            ExpiryRetryState::Waiting
        } else {
            ExpiryRetryState::Due
        }
    }
}

/// Per-guild retry time after a failed expiry membership check. In-process
/// only: a restart checks each guild once more. One entry per failing guild;
/// a success removes it. An entry is otherwise dropped only once a pass shows
/// that guild had no due ban: the pass checked every due ban outside waiting
/// guilds without using it, or a read reached a later retry guild past it. A
/// full batch drops no other entry, so a slow pass cannot reset the backoff of
/// guilds that still block later guilds.
#[derive(Default)]
pub(crate) struct ExpiryCheckRetries(
    parking_lot::Mutex<std::collections::HashMap<i64, ExpiryCheckRetry>>,
);

impl ExpiryCheckRetries {
    /// Guilds whose retry time has not yet arrived.
    fn waiting(&self, now: i64) -> Vec<i64> {
        self.0
            .lock()
            .iter()
            .filter(|(_, retry)| retry.state(now) == ExpiryRetryState::Waiting)
            .map(|(guild, _)| *guild)
            .collect()
    }

    /// Guilds whose retry time has arrived, earliest retry time first.
    fn due(&self, now: i64) -> Vec<i64> {
        let mut due: Vec<_> = self
            .0
            .lock()
            .iter()
            .filter(|(_, retry)| retry.state(now) == ExpiryRetryState::Due)
            .map(|(guild, retry)| (retry.retry_at, *guild))
            .collect();
        due.sort_unstable();
        due.into_iter().map(|(_, guild)| guild).collect()
    }

    fn failed(&self, guild: i64, now: i64) {
        let mut retries = self.0.lock();
        let failures = retries
            .get(&guild)
            .map_or(1, |retry| retry.failures.saturating_add(1));
        let delay = EXPIRY_CHECK_RETRY_BASE_SECS
            .saturating_mul(1 << (failures - 1).min(16))
            .min(EXPIRY_CHECK_RETRY_MAX_SECS);
        retries.insert(
            guild,
            ExpiryCheckRetry {
                failures,
                retry_at: now.saturating_add(delay),
            },
        );
    }

    fn succeeded(&self, guild: i64) {
        self.0.lock().remove(&guild);
    }

    /// Drop entries still due at `now` for guilds a read at `now` passed over
    /// because they had no due ban.
    fn forget_stale(&self, guilds: &[i64], now: i64) {
        let mut retries = self.0.lock();
        for guild in guilds {
            if retries
                .get(guild)
                .is_some_and(|retry| retry.state(now) == ExpiryRetryState::Due)
            {
                retries.remove(guild);
            }
        }
    }

    /// After a pass that checked every due ban selected at `now`: an entry
    /// already due at `now` and not checked again had no due ban.
    fn forget_idle(&self, now: i64) {
        self.0
            .lock()
            .retain(|_, retry| retry.state(now) == ExpiryRetryState::Waiting);
    }
}

/// Result of the bot's own-membership check before an expiry is consumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExpiryMembership {
    /// The captured membership is still the bot's current join.
    Matches,
    /// The read failed, timed out or returned no join timestamp.
    Unavailable,
    /// Discord answered with a newer join; the enrollment repair was attempted.
    Changed,
}

/// Confirm the expiry's captured membership with a bounded own-member read
/// before consuming it. An unavailable read defers the expiry. A newer join is
/// repaired like queued actions; the old membership's expiry then stays unclaimed.
async fn expiry_membership(
    db: &Db,
    app: Option<&crate::state::BotState>,
    discord: &impl DiscordActions,
    due: &db::ExpiredBan,
    limits: &WorkerLimits,
) -> Result<ExpiryMembership> {
    let guild = due.enrollment.guild_id;
    let timeout = std::time::Duration::from_secs(u64::from(limits.request_timeout_secs));
    let observed = match tokio::time::timeout(timeout, discord.guild_joined_at_ns(guild)).await {
        Ok(Ok(Some(observed))) => observed,
        result => {
            let reason = match result {
                Ok(Ok(_)) => "no join timestamp".to_owned(),
                Ok(Err(error)) => format!("{error:#}"),
                Err(_) => "timed out".to_owned(),
            };
            tracing::warn!(
                guild,
                user = due.user,
                "automatic unban deferred: bot membership check unavailable: {reason}"
            );
            return Ok(ExpiryMembership::Unavailable);
        }
    };
    if crate::enrollment_repair::membership_joined_at_matches(due.enrollment.joined_at_ns, observed)
    {
        return Ok(ExpiryMembership::Matches);
    }
    tracing::warn!(
        guild,
        user = due.user,
        expected_joined_at_ns = due.enrollment.joined_at_ns,
        observed_joined_at_ns = observed,
        "automatic unban skipped: bot membership changed since the timed ban; moderator follow-up required"
    );
    if let Some(app) = app {
        match db::enroll_guild(db, guild, observed).await {
            Ok(Some(enrollment)) => {
                app.tenant.activate(enrollment).await?;
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    guild,
                    "bot enrollment repair failed before automatic unban: {error:#}"
                );
            }
        }
    }
    Ok(ExpiryMembership::Changed)
}

async fn dispatch_expired_ban(
    db: &Db,
    app: Option<&crate::state::BotState>,
    discord: &impl DiscordActions,
    claimed: ConsumedExpiry,
    limits: &WorkerLimits,
) -> Result<()> {
    let ConsumedExpiry(claimed) = claimed;
    // Claiming clears the old deadline; the ban may have been re-armed,
    // cancelled or replaced, or its membership ended, since it was claimed.
    match db::expired_ban_state(db, &claimed).await? {
        db::ExpiredBanState::Current => {}
        db::ExpiredBanState::Superseded => return Ok(()),
        db::ExpiredBanState::Blocked => {
            tracing::warn!(
                guild = claimed.enrollment.guild_id,
                user = claimed.user,
                "automatic unban skipped: membership ended or guild fenced after the claim; ban remains, moderator follow-up required"
            );
            return Ok(());
        }
    }
    let db::ExpiredBan {
        enrollment, user, ..
    } = claimed;
    let suppression = app.map(|app| {
        app.suppress_next_gateway_case(
            crate::state::GatewayCaseAction::Unban,
            enrollment.guild_id as u64,
            user as u64,
            None,
        )
    });
    // The unban takes effect no earlier than this request, so a ban the member
    // directory observed after it stays ordered after the confirmed unban.
    let requested_at_ms = chrono::Utc::now().timestamp_millis().max(1);
    let outcome = match tokio::time::timeout(
        std::time::Duration::from_secs(u64::from(limits.request_timeout_secs)),
        discord.unban(enrollment.guild_id, user, "Ban duration expired"),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(_) => Outcome::uncertain("request_timed_out"),
    };
    if (outcome.phase() == OutcomeState::Failed || outcome.code == "already_unbanned")
        && let (Some(app), Some(suppression)) = (app, suppression)
    {
        app.release_gateway_case_suppression(suppression);
    }
    if outcome.phase() == OutcomeState::Succeeded
        && let Err(error) = db::mark_enrolled_unbanned(db, Some(enrollment.clone()), user).await
    {
        tracing::warn!(user, "expired ban mirror not saved: {error:#}");
    }
    // As for a queued unban, a confirmed removal (not Discord's idempotent
    // 10026) is saved to the member directory so a missed Gateway callback
    // cannot leave the user shown as banned. It is observed at the request
    // time: a re-ban the directory saw during the request, including one whose
    // contested check already settled, is newer and is kept and rechecked.
    if outcome.phase() == OutcomeState::Succeeded
        && outcome.code != "already_unbanned"
        && let Some(app) = app
    {
        let observed = match app.tenant.live(enrollment.clone()).await {
            Ok((store, scope)) => store.observe_unban(scope, user, requested_at_ms).await,
            Err(error) => Err(error),
        };
        if let Err(error) = observed {
            tracing::warn!(
                guild = enrollment.guild_id,
                user,
                "confirmed automatic unban state not saved: {error:#}"
            );
            app.remember_member_gap(
                enrollment.guild_id as u64,
                Some(enrollment.id.clone()),
                Some(user),
                "confirmed unban persistence unavailable",
            );
        }
    }
    if let Some(app) = app {
        crate::cases::record_application(
            app,
            Some(enrollment.clone()),
            crate::db::guild_data::CaseAction::Unban,
            unban_case_outcome(&outcome),
            user as u64,
            discord.bot_user_id().map(|id| id as u64),
            Some("Ban duration expired".into()),
            crate::db::guild_data::CaseEvidence::Unavailable,
        )
        .await;
    }
    if outcome.phase() != OutcomeState::Succeeded {
        tracing::warn!(
            guild = enrollment.guild_id,
            user,
            outcome = outcome.code,
            "automatic unban failed or uncertain; moderator follow-up required"
        );
    }
    Ok(())
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
