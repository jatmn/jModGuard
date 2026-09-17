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
    async fn membership_matches(&self, job: &BotAction) -> Result<bool>;
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
    async fn membership_matches(&self, job: &BotAction) -> Result<bool> {
        anyhow::ensure!(self.bot_user_id.get() > 1, "bot identity is not ready");
        let member = self
            .http
            .get_member(GuildId::new(job.guild_id as u64), self.bot_user_id)
            .await?;
        Ok(member
            .joined_at
            .and_then(|joined| joined.timestamp_nanos_opt())
            .is_some_and(|joined| Some(joined) == job.joined_at_ns))
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
        Action::SetLogChannel { .. } => Permissions::MANAGE_GUILD,
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
    if !db::begin_bot_action(db, job, chrono::Utc::now().timestamp()).await? {
        return Ok(());
    }
    let mutation_started = std::sync::atomic::AtomicBool::new(false);
    let work = async {
        if chrono::Utc::now().timestamp() >= job.expires_at {
            return Outcome::failed("expired_before_execution");
        }
        match discord.membership_matches(job).await {
            Ok(true) => {}
            Ok(false) => return Outcome::failed("bot_membership_changed"),
            Err(_) => return Outcome::failed("bot_membership_check_unavailable"),
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
            Action::Unban { user_id, reason } => {
                if chrono::Utc::now().timestamp() >= job.expires_at {
                    return Outcome::failed("expired_before_execution");
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
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(u64::from(limits.request_timeout_secs)),
        work,
    )
    .await
    .unwrap_or_else(|_| {
        if mutation_started.load(std::sync::atomic::Ordering::Relaxed) {
            Outcome::uncertain("request_timed_out")
        } else {
            Outcome::failed("validation_timed_out")
        }
    });
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
            let case_outcome = match outcome.state {
                "succeeded" => crate::db::guild_data::CaseOutcome::Succeeded,
                "failed" => crate::db::guild_data::CaseOutcome::Failed,
                _ => crate::db::guild_data::CaseOutcome::Uncertain,
            };
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
            if let (Some(app), Some(case_id)) = (app, case_id) {
                let expects_discord_audit = crate::cases::queued_unban_expects_discord_audit(
                    outcome.state,
                    outcome.code,
                );
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
