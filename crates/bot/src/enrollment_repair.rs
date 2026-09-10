//! Positive membership repair: bounded REST discovery, never absence-based deletion.
use crate::{
    config::EnrollmentConfig,
    db::{self, Db},
};
use anyhow::{Context as _, Result};
use serenity::{
    http::GuildPagination,
    model::id::{GuildId, UserId},
};
use std::time::{Duration, Instant};

#[serenity::async_trait]
pub trait MembershipSource: Send + Sync {
    async fn guilds_after(&self, after: Option<u64>, limit: u32) -> Result<Vec<u64>>;
    async fn joined_at_ns(&self, guild: u64) -> Result<i64>;
}
pub struct DiscordMembership<'a> {
    pub http: &'a serenity::http::Http,
    pub bot_user_id: UserId,
}
impl<'a> DiscordMembership<'a> {
    /// Serenity's pre-READY cache contains a placeholder user (ID 1), not ID 0.
    /// Never query membership for that placeholder.
    pub fn from_cache(
        http: &'a serenity::http::Http,
        cache: &serenity::cache::Cache,
    ) -> Option<Self> {
        let user = cache.current_user();
        user.bot.then_some(Self {
            http,
            bot_user_id: user.id,
        })
    }
}

#[serenity::async_trait]
impl MembershipSource for DiscordMembership<'_> {
    async fn guilds_after(&self, after: Option<u64>, limit: u32) -> Result<Vec<u64>> {
        Ok(self
            .http
            .get_guilds(
                after.map(|id| GuildPagination::After(GuildId::new(id))),
                Some(u64::from(limit)),
            )
            .await?
            .into_iter()
            .map(|guild| guild.id.get())
            .collect())
    }
    async fn joined_at_ns(&self, guild: u64) -> Result<i64> {
        self.http
            .get_member(GuildId::new(guild), self.bot_user_id)
            .await?
            .joined_at
            .and_then(|joined| joined.timestamp_nanos_opt())
            .filter(|joined| *joined > 0)
            .context("Discord member has no supported join timestamp")
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct RepairReport {
    pub checked: usize,
    pub confirmed: usize,
    pub refused: usize,
    pub failed: usize,
    pub sweep_complete: bool,
}
/// O(1) state between ticks; a page is discarded after use. Restart starts a new sweep.
pub struct EnrollmentRepair {
    config: EnrollmentConfig,
    after: Option<u64>,
    next: Instant,
}
impl EnrollmentRepair {
    pub fn new(config: EnrollmentConfig, now: Instant) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            after: None,
            next: now,
        })
    }
    pub async fn step(
        &mut self,
        db: &Db,
        source: &impl MembershipSource,
    ) -> Result<Option<RepairReport>> {
        if Instant::now() < self.next {
            return Ok(None);
        }
        match self.page(db, source).await {
            Ok(report) => Ok(Some(report)),
            Err(error) => {
                self.next = Instant::now() + Duration::from_secs(u64::from(self.config.retry_secs));
                Err(error)
            }
        }
    }
    async fn page(&mut self, db: &Db, source: &impl MembershipSource) -> Result<RepairReport> {
        let timeout = Duration::from_secs(u64::from(self.config.request_timeout_secs));
        let mut guilds = tokio::time::timeout(
            timeout,
            source.guilds_after(self.after, self.config.batch_size),
        )
        .await
        .context("membership listing timed out")??;
        anyhow::ensure!(
            guilds.len() <= self.config.batch_size as usize,
            "membership listing exceeded page limit"
        );
        guilds.sort_unstable();
        anyhow::ensure!(
            guilds
                .iter()
                .all(|id| *id > self.after.unwrap_or(0) && i64::try_from(*id).is_ok())
                && guilds.windows(2).all(|pair| pair[0] != pair[1]),
            "invalid membership pagination"
        );
        let complete = guilds.len() < self.config.batch_size as usize;
        let mut report = RepairReport {
            checked: guilds.len(),
            sweep_complete: complete,
            ..RepairReport::default()
        };
        for guild in &guilds {
            let joined = match tokio::time::timeout(timeout, source.joined_at_ns(*guild)).await {
                Ok(Ok(joined)) if joined > 0 => joined,
                _ => {
                    report.failed += 1;
                    continue;
                }
            };
            // This same monotonic transition is used by gateway observations.
            // A late REST result cannot resurrect a retired join or replace a newer one.
            match db::enroll_guild(db, *guild as i64, joined).await {
                Ok(Some(_)) => report.confirmed += 1,
                Ok(None) => report.refused += 1,
                Err(error) => return Err(error.context("membership repair database unavailable")),
            }
        }
        self.after = if complete {
            None
        } else {
            guilds.last().copied()
        };
        self.next = Instant::now()
            + Duration::from_secs(u64::from(if complete {
                self.config.sweep_pause_secs
            } else {
                self.config.poll_secs
            }));
        Ok(report)
    }
}

/// Gateway callers pass only the observed membership's join timestamp. Missing
/// delete context cannot safely identify which enrollment ended.
pub async fn observe_gateway_membership(
    db: &Db,
    guild: u64,
    joined_at_ns: Option<i64>,
    unavailable: bool,
    present: bool,
) -> Result<()> {
    if unavailable {
        return Ok(());
    }
    let guild = i64::try_from(guild).context("unsupported guild id")?;
    let joined = joined_at_ns
        .filter(|joined| *joined > 0)
        .context("guild event lacks supported membership context")?;
    if present {
        db::enroll_guild(db, guild, joined).await?;
    } else {
        db::retire_guild_enrollment(db, guild, joined).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
