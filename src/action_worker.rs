//! Inactive bot dispatch library. Neither binary schedules this worker yet.
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

/// Trusted library parameters, not active runtime settings. The activation wave
/// must load these from bot configuration before scheduling bounded batches.
#[derive(Debug, Clone)]
pub struct WorkerLimits {
    pub batch_size: u32,
    pub request_timeout_secs: u32,
}
impl Default for WorkerLimits {
    fn default() -> Self {
        Self {
            batch_size: 4,
            request_timeout_secs: 15,
        }
    }
}
impl WorkerLimits {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            (1..=64).contains(&self.batch_size),
            "invalid action worker batch size"
        );
        anyhow::ensure!(
            (1..=120).contains(&self.request_timeout_secs),
            "invalid action request timeout"
        );
        Ok(())
    }
}

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
    guild.owner_id == member.user.id || permissions.administrator() || permissions.contains(needed)
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
pub async fn execute(
    db: &Db,
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
        match &job.action {
            Action::Unban { user_id, reason } => {
                match db::bot_action_can_dispatch(db, job).await {
                    Ok(true) => {}
                    Ok(false) => return Outcome::failed("enrollment_ended"),
                    Err(_) => return Outcome::failed("enrollment_check_unavailable"),
                }
                if chrono::Utc::now().timestamp() >= job.expires_at {
                    return Outcome::failed("expired_before_execution");
                }
                mutation_started.store(true, std::sync::atomic::Ordering::Relaxed);
                discord.unban(job.guild_id, *user_id, reason).await
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
    db::finish_dispatched_bot_action(db, job, outcome.state, outcome.code).await?;
    Ok(())
}
/// Bounded sequential batch. No process restart or lease expiry replays a claim.
pub async fn drain(db: &Db, discord: &impl DiscordActions, limits: &WorkerLimits) -> Result<()> {
    limits.validate()?;
    for _ in 0..limits.batch_size {
        let Some(job) = db::claim_bot_action(db, chrono::Utc::now().timestamp()).await? else {
            break;
        };
        if let Err(error) = execute(db, discord, &job, limits).await {
            tracing::error!(action_id=%job.id,"bot action receipt failed; action will not be resent: {error:#}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
