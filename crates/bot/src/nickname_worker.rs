//! Nickname side effects require a current request and matching Discord membership.
use crate::{db, enrollment_repair::MembershipSource, state::BotState};
use anyhow::{Context as _, Result};
use serenity::builder::EditMember;
use std::sync::Arc;

async fn dispatch_ready(
    db: &db::Db,
    job: &db::NicknameReconcileJob,
    source: &impl MembershipSource,
) -> Result<bool> {
    anyhow::ensure!(job.guild_id > 0, "invalid nickname guild id");
    if !db::nickname_reconcile_can_dispatch(db, job).await? {
        return Ok(false);
    }
    let joined = tokio::time::timeout(
        crate::discord::DISCORD_REST_TIMEOUT,
        source.joined_at_ns(job.guild_id as u64),
    )
    .await
    .context("nickname membership verification timed out")??;
    anyhow::ensure!(joined > 0, "unsupported bot join timestamp");
    if joined != job.joined_at_ns {
        db::finish_nickname_reconcile(
            db,
            job,
            "dormant",
            "membership_changed",
            "bot membership changed; fresh nickname request required",
            chrono::Utc::now().timestamp(),
        )
        .await?;
        return Ok(false);
    }
    // Permission/membership HTTP can race retirement or a newer settings request.
    db::nickname_reconcile_can_dispatch(db, job).await
}

pub async fn reconcile_nickname_job(
    app: Arc<BotState>,
    http: Arc<serenity::http::Http>,
    me: serenity::model::id::UserId,
    job: db::NicknameReconcileJob,
) {
    let source = crate::enrollment_repair::DiscordMembership {
        http: &http,
        bot_user_id: me,
    };
    match dispatch_ready(&app.db, &job, &source).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => {
            tracing::warn!(
                guild_id = job.guild_id,
                "nickname membership verification deferred: {error:#}"
            );
            if let Err(error) = db::schedule_nickname_reconcile_retry(
                &app.db,
                &job,
                chrono::Utc::now().timestamp(),
                "membership_unverified",
                "could not verify current bot membership",
            )
            .await
            {
                tracing::warn!(
                    guild_id = job.guild_id,
                    "nickname verification retry unavailable: {error:#}"
                );
            }
            return;
        }
    }
    let Ok(guild_id_u64) = u64::try_from(job.guild_id) else {
        tracing::error!(
            guild_id = job.guild_id,
            "invalid guild id in nickname outbox"
        );
        return;
    };
    let guild = serenity::model::id::GuildId::new(guild_id_u64);
    let edit = tokio::time::timeout(
        crate::discord::DISCORD_REST_TIMEOUT,
        guild.edit_member(&http, me, EditMember::new().nickname(job.nickname.clone())),
    )
    .await;
    match edit {
        Err(_) => {
            let now = chrono::Utc::now().timestamp();
            let message = "Discord nickname request timed out";
            if let Err(error) =
                db::schedule_nickname_reconcile_retry(&app.db, &job, now, "timeout", message).await
            {
                tracing::error!(
                    guild_id = job.guild_id,
                    "failed to persist nickname reconciliation timeout: {error:#}"
                );
            }
        }
        Ok(Ok(_)) => {
            if let Err(error) = db::acknowledge_nickname_reconcile(&app.db, &job).await {
                tracing::warn!(
                    guild_id = job.guild_id,
                    "failed to acknowledge nickname reconciliation: {error:#}"
                );
            }
        }
        Ok(Err(error)) => {
            let now = chrono::Utc::now().timestamp();
            use crate::discord::NicknameFailureClass;
            let (class, code) = crate::discord::classify_nickname_failure(&error);
            let message = error.to_string();
            let result = match class {
                NicknameFailureClass::Transient => {
                    db::schedule_nickname_reconcile_retry(&app.db, &job, now, &code, &message)
                        .await
                        .map(|exhausted| {
                            if exhausted {
                                tracing::error!(
                                    guild_id = job.guild_id,
                                    attempts = job.attempt_count + 1,
                                    "nickname reconciliation exhausted its retry budget: {message}"
                                );
                            } else {
                                tracing::warn!(
                                    guild_id = job.guild_id,
                                    attempt = job.attempt_count + 1,
                                    "nickname reconciliation scheduled for retry: {message}"
                                );
                            }
                        })
                }
                NicknameFailureClass::Dormant => {
                    db::finish_nickname_reconcile(&app.db, &job, "dormant", &code, &message, now)
                        .await
                        .map(|()| {
                            tracing::warn!(
                                guild_id = job.guild_id,
                                "nickname reconciliation awaits membership reconfirmation or a fresh request: {message}"
                            )
                        })
                }
                NicknameFailureClass::Terminal => {
                    db::finish_nickname_reconcile(&app.db, &job, "terminal", &code, &message, now)
                        .await
                        .map(|()| {
                            tracing::error!(
                                guild_id = job.guild_id,
                                "nickname reconciliation needs operator action: {message}"
                            )
                        })
                }
            };
            if let Err(state_error) = result {
                tracing::error!(
                    guild_id = job.guild_id,
                    "failed to persist nickname reconciliation failure: {state_error:#}"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests;
