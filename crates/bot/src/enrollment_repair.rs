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

/// Discord REST may omit subseconds present in the Gateway observation. Accept
/// that same-second truncation, but keep any later observation as replacement
/// evidence. Exact timestamps and enrollment IDs remain the durable DB fence.
pub(crate) fn membership_joined_at_matches(expected: i64, observed: i64) -> bool {
    const NANOS_PER_SECOND: i64 = 1_000_000_000;
    expected > 0
        && observed > 0
        && expected.div_euclid(NANOS_PER_SECOND) == observed.div_euclid(NANOS_PER_SECOND)
        && observed <= expected
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
    retry: crate::discord::RestRetry,
}
impl EnrollmentRepair {
    pub fn new(config: EnrollmentConfig, now: Instant) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            after: None,
            next: now,
            retry: Default::default(),
        })
    }
    /// Repair can enroll a guild without a Gateway event. Wake the importer
    /// and discard cached absence, including after a partially completed page.
    pub async fn step_for_bot(
        &mut self,
        app: &crate::state::BotState,
        source: &impl MembershipSource,
    ) -> Result<Option<RepairReport>> {
        let mut changed = false;
        let result = self
            .step_with_changes(&app.db, &app.tenant, source, &mut changed)
            .await;
        if changed {
            app.invalidate_member_sync_scope();
        }
        result
    }

    pub async fn step(
        &mut self,
        db: &Db,
        tenant: &crate::tenant::TenantRuntime,
        source: &impl MembershipSource,
    ) -> Result<Option<RepairReport>> {
        self.step_with_changes(db, tenant, source, &mut false).await
    }

    async fn step_with_changes(
        &mut self,
        db: &Db,
        tenant: &crate::tenant::TenantRuntime,
        source: &impl MembershipSource,
        membership_changed: &mut bool,
    ) -> Result<Option<RepairReport>> {
        if Instant::now() < self.next || self.retry.stopped {
            return Ok(None);
        }
        match self.page(db, tenant, source, membership_changed).await {
            Ok(report) => {
                self.retry = Default::default();
                Ok(Some(report))
            }
            Err(error) => {
                let class = crate::discord::classify_retry_error(&error);
                if class == crate::discord::RetryClass::Unauthorized {
                    tracing::error!("enrollment repair stopped: Discord rejected the bot token");
                }
                // Forbidden listings can recover without a membership event.
                self.retry.fail(
                    if class == crate::discord::RetryClass::Unauthorized {
                        class
                    } else {
                        crate::discord::RetryClass::Transient
                    },
                    Instant::now(),
                    u64::from(self.config.retry_secs),
                );
                self.next = self.retry.next.unwrap_or(self.next);
                Err(error)
            }
        }
    }
    async fn page(
        &mut self,
        db: &Db,
        tenant: &crate::tenant::TenantRuntime,
        source: &impl MembershipSource,
        membership_changed: &mut bool,
    ) -> Result<RepairReport> {
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
                Ok(Err(error))
                    if crate::discord::classify_retry_error(&error)
                        == crate::discord::RetryClass::Unauthorized =>
                {
                    return Err(error);
                }
                _ => {
                    report.failed += 1;
                    continue;
                }
            };
            // This same monotonic transition is used by gateway observations.
            // A late REST result cannot resurrect a retired join or replace a newer one.
            match db::enroll_guild_with_change(db, *guild as i64, joined).await {
                Ok((Some(enrollment), changed)) => {
                    let activation = tenant.activate(enrollment).await;
                    // Record committed changes even if activation or a later
                    // member fails. Healthy confirmations do not clear the cache.
                    if changed {
                        *membership_changed = true;
                    }
                    if let Err(error) = activation {
                        tracing::warn!(
                            guild_id = *guild,
                            "guild store activation deferred: {error:#}"
                        );
                        report.failed += 1;
                    } else {
                        report.confirmed += 1;
                    }
                }
                Ok((None, _)) => report.refused += 1,
                Err(error) => {
                    // A lost COMMIT whose receipt could not be reconciled may
                    // have changed membership. Ordinary failures did not.
                    if db::is_database_commit_indeterminate(&error) {
                        *membership_changed = true;
                    }
                    return Err(error.context("membership repair database unavailable"));
                }
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
    tenant: &crate::tenant::TenantRuntime,
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
        if let Some(enrollment) = db::enroll_guild(db, guild, joined).await? {
            tenant.activate(enrollment).await?;
        }
    } else {
        db::retire_guild_enrollment(db, guild, joined).await?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;
