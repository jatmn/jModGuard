//! Scoped live directory observations. These writes are independent of log delivery.

use crate::db::guild_data::{MemberDepartureDecision, MemberObservation, MemberVerifyKind};
use crate::{db, state::BotState};
use anyhow::{Context as _, Result};
use serenity::model::{event::GuildMemberUpdateEvent, guild::Member, user::User};

fn timestamp_ms(value: serenity::model::Timestamp) -> Option<i64> {
    value
        .timestamp_millis()
        .gt(&0)
        .then_some(value.timestamp_millis())
}

pub fn from_member(member: &Member, observed_at_ms: i64, is_join: bool) -> MemberObservation {
    MemberObservation {
        user_id: member.user.id.get() as i64,
        username: Some(Some(member.user.name.clone())),
        global_name: Some(member.user.global_name.clone()),
        nickname: Some(member.nick.clone()),
        joined_at_ms: member.joined_at.and_then(timestamp_ms),
        observed_at_ms,
        is_join,
    }
}

fn from_update(event: &GuildMemberUpdateEvent, observed_at_ms: i64) -> MemberObservation {
    MemberObservation {
        user_id: event.user.id.get() as i64,
        username: Some(Some(event.user.name.clone())),
        global_name: Some(event.user.global_name.clone()),
        nickname: Some(event.nick.clone()),
        joined_at_ms: timestamp_ms(event.joined_at),
        observed_at_ms,
        is_join: false,
    }
}

async fn live(
    app: &BotState,
    guild_id: u64,
) -> Option<(
    db::guild_data::GuildDataStore,
    db::guild_data::GuildDataScope,
)> {
    let enrollment = crate::cases::capture(app, guild_id).await?;
    match app.tenant.live(enrollment).await {
        Ok(scope) => Some(scope),
        Err(error) => {
            tracing::warn!(guild_id, "member directory scope unavailable: {error:#}");
            None
        }
    }
}

async fn live_observation(
    app: &BotState,
    guild_id: u64,
    user_id: Option<i64>,
    reason: &str,
) -> Option<(
    db::guild_data::GuildDataStore,
    db::guild_data::GuildDataScope,
)> {
    let enrollment = match db::guild_enrollment(&app.db, guild_id as i64).await {
        Ok(Some(enrollment)) => enrollment,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(
                guild_id,
                "member directory enrollment unavailable: {error:#}"
            );
            app.remember_member_gap(guild_id, None, user_id, reason);
            return None;
        }
    };
    match app.tenant.live(enrollment.clone()).await {
        Ok(scope) => Some(scope),
        Err(error) => {
            tracing::warn!(guild_id, "member directory scope unavailable: {error:#}");
            app.remember_member_gap(guild_id, Some(enrollment.id), user_id, reason);
            None
        }
    }
}

async fn mark_gap(
    app: &BotState,
    store: &db::guild_data::GuildDataStore,
    scope: db::guild_data::GuildDataScope,
    user_id: Option<i64>,
    reason: &str,
) {
    let now_ms = chrono::Utc::now().timestamp_millis();
    if let Err(error) = store
        .mark_member_reconciliation_needed(scope.clone(), reason, now_ms)
        .await
    {
        tracing::warn!("member directory coverage marker deferred: {error:#}");
        app.remember_member_gap(
            scope.guild_id() as u64,
            Some(scope.enrollment().id.clone()),
            user_id,
            reason,
        );
        return;
    }
    if let Some(user_id) = user_id {
        let kind = if reason.contains("ban") {
            MemberVerifyKind::Ban
        } else {
            MemberVerifyKind::Member
        };
        if let Err(error) = store
            .queue_member_target_check(scope.clone(), user_id, kind, reason, now_ms)
            .await
        {
            tracing::warn!(
                guild_id = scope.guild_id(),
                user_id,
                "member target check deferred: {error:#}"
            );
            app.remember_member_gap(
                scope.guild_id() as u64,
                Some(scope.enrollment().id.clone()),
                Some(user_id),
                reason,
            );
        }
    }
}

pub async fn mark_guild_gap(app: &BotState, guild_id: u64, reason: &str) {
    let Some((store, scope)) = live_observation(app, guild_id, None, reason).await else {
        return;
    };
    mark_gap(app, &store, scope, None, reason).await;
}

pub async fn mark_user_gap(app: &BotState, guild_id: u64, user_id: i64, reason: &str) {
    let Some((store, scope)) = live_observation(app, guild_id, Some(user_id), reason).await else {
        return;
    };
    mark_gap(app, &store, scope, Some(user_id), reason).await;
}

pub async fn flush_pending_gaps(
    app: &BotState,
    store: &db::guild_data::GuildDataStore,
    scope: &db::guild_data::GuildDataScope,
) {
    for gap in app.pending_member_gaps(scope.guild_id() as u64) {
        // An old event cannot be applied to a replacement enrollment.
        if gap
            .enrollment_id
            .as_deref()
            .is_some_and(|id| id != scope.enrollment().id)
        {
            app.acknowledge_member_gap(&gap);
            continue;
        }
        let now_ms = chrono::Utc::now().timestamp_millis();
        if let Err(error) = store
            .mark_member_reconciliation_needed(scope.clone(), &gap.reason, now_ms)
            .await
        {
            tracing::warn!(
                guild_id = gap.guild_id,
                "member coverage gap retry failed: {error:#}"
            );
            continue;
        }
        let mut queued = true;
        for (user_id, is_ban) in &gap.users {
            let kind = if *is_ban {
                MemberVerifyKind::Ban
            } else {
                MemberVerifyKind::Member
            };
            if let Err(error) = store
                .queue_member_target_check(scope.clone(), *user_id, kind, &gap.reason, now_ms)
                .await
            {
                tracing::warn!(
                    guild_id = gap.guild_id,
                    user_id,
                    "member target check retry failed: {error:#}"
                );
                queued = false;
                break;
            }
        }
        if queued {
            app.acknowledge_member_gap(&gap);
        }
    }
}

/// Fences ordinary-departure cleanup while an outbound bot ban is in flight.
/// The marker is durable so a crash cannot erase a possible confirmed ban.
pub struct PendingBan {
    store: db::guild_data::GuildDataStore,
    scope: db::guild_data::GuildDataScope,
    user_id: i64,
    operation_id: String,
}

pub async fn begin_ban_attempt(app: &BotState, guild_id: u64, user_id: u64) -> Result<PendingBan> {
    let (store, scope) = live(app, guild_id)
        .await
        .context("member directory scope unavailable")?;
    let operation_id = db::guild_data::CaseId::new().as_str().to_owned();
    store
        .begin_member_ban_attempt(
            scope.clone(),
            user_id as i64,
            operation_id.clone(),
            chrono::Utc::now().timestamp_millis(),
        )
        .await?;
    Ok(PendingBan {
        store,
        scope,
        user_id: user_id as i64,
        operation_id,
    })
}

impl PendingBan {
    pub async fn finish(self, app: &BotState, outcome: db::guild_data::CaseOutcome) {
        if let Err(error) = self
            .store
            .finish_member_ban_attempt(
                self.scope.clone(),
                self.user_id,
                &self.operation_id,
                outcome,
                chrono::Utc::now().timestamp_millis(),
            )
            .await
        {
            tracing::warn!(
                guild_id = self.scope.guild_id(),
                user_id = self.user_id,
                "ban attempt marker settlement failed: {error:#}"
            );
            mark_gap(
                app,
                &self.store,
                self.scope,
                Some(self.user_id),
                "ban attempt settlement unavailable",
            )
            .await;
        }
    }
}

pub async fn observe_join(app: &BotState, member: &Member, observed_at_ms: i64) {
    let Some((store, scope)) = live_observation(
        app,
        member.guild_id.get(),
        Some(member.user.id.get() as i64),
        "member join persistence unavailable",
    )
    .await
    else {
        return;
    };
    if let Err(error) = store
        .observe_member(scope.clone(), from_member(member, observed_at_ms, true))
        .await
    {
        tracing::warn!(
            guild_id = member.guild_id.get(),
            user_id = member.user.id.get(),
            "member join observation failed: {error:#}"
        );
        mark_gap(
            app,
            &store,
            scope,
            Some(member.user.id.get() as i64),
            "member join persistence unavailable",
        )
        .await;
    }
}

pub async fn observe_update(app: &BotState, event: &GuildMemberUpdateEvent, observed_at_ms: i64) {
    let Some((store, scope)) = live_observation(
        app,
        event.guild_id.get(),
        Some(event.user.id.get() as i64),
        "member update persistence unavailable",
    )
    .await
    else {
        return;
    };
    if let Err(error) = store
        .observe_member(scope.clone(), from_update(event, observed_at_ms))
        .await
    {
        tracing::warn!(
            guild_id = event.guild_id.get(),
            user_id = event.user.id.get(),
            "member name observation failed: {error:#}"
        );
        mark_gap(
            app,
            &store,
            scope,
            Some(event.user.id.get() as i64),
            "member update persistence unavailable",
        )
        .await;
    }
}

pub async fn observe_departure(
    app: &BotState,
    guild_id: u64,
    user: &User,
    departed_at_ms: i64,
    expected_joined_at_ms: Option<i64>,
) -> Option<MemberDepartureDecision> {
    let (store, scope) = live_observation(
        app,
        guild_id,
        Some(user.id.get() as i64),
        "member departure persistence unavailable",
    )
    .await?;
    match store
        .depart_member_checked(
            scope.clone(),
            user.id.get() as i64,
            departed_at_ms,
            expected_joined_at_ms,
        )
        .await
    {
        Ok(MemberDepartureDecision::NeedsVerification) => {
            // The fenced departure transaction queued the targeted check.
            Some(MemberDepartureDecision::NeedsVerification)
        }
        Ok(result) => Some(result),
        Err(error) => {
            tracing::warn!(
                guild_id,
                user_id = user.id.get(),
                "member departure observation failed: {error:#}"
            );
            mark_gap(
                app,
                &store,
                scope,
                Some(user.id.get() as i64),
                "member departure persistence unavailable",
            )
            .await;
            None
        }
    }
}

pub async fn observe_ban(
    app: &BotState,
    guild_id: u64,
    user: &User,
    observed_at_ms: i64,
) -> Option<bool> {
    let (store, scope) = live_observation(
        app,
        guild_id,
        Some(user.id.get() as i64),
        "ban observation persistence unavailable",
    )
    .await?;
    match store
        .observe_ban_with_identity(
            scope.clone(),
            user.id.get() as i64,
            observed_at_ms,
            user.name.clone(),
            user.global_name.clone(),
        )
        .await
    {
        Ok(changed) => Some(changed),
        Err(error) => {
            tracing::warn!(
                guild_id,
                user_id = user.id.get(),
                "member ban observation failed: {error:#}"
            );
            mark_gap(
                app,
                &store,
                scope,
                Some(user.id.get() as i64),
                "ban observation persistence unavailable",
            )
            .await;
            None
        }
    }
}

pub async fn observe_unban(
    app: &BotState,
    guild_id: u64,
    user: &User,
    observed_at_ms: i64,
) -> Option<bool> {
    let (store, scope) = live_observation(
        app,
        guild_id,
        Some(user.id.get() as i64),
        "unban observation persistence unavailable",
    )
    .await?;
    match store
        .observe_unban_with_identity(
            scope.clone(),
            user.id.get() as i64,
            observed_at_ms,
            user.name.clone(),
            user.global_name.clone(),
        )
        .await
    {
        Ok(changed) => Some(changed),
        Err(error) => {
            tracing::warn!(
                guild_id,
                user_id = user.id.get(),
                "member unban observation failed: {error:#}"
            );
            mark_gap(
                app,
                &store,
                scope,
                Some(user.id.get() as i64),
                "unban observation persistence unavailable",
            )
            .await;
            None
        }
    }
}
