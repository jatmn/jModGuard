//! Nickname side effects require a current request and matching Discord membership.
use crate::{
    db,
    enrollment_repair::{MembershipSource, membership_joined_at_matches},
    state::BotState,
};
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
    if !membership_joined_at_matches(job.joined_at_ns, joined) {
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
            record_nickname_failure(
                &app,
                &job,
                &error,
                nickname_outcome_unknown(&app.db, job.guild_id).await,
            )
            .await;
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
    let http_for_read = Arc::clone(&http);
    apply_nickname_edit(
        &app,
        &job,
        || async move {
            guild
                .member(&http_for_read, me)
                .await
                .map(|member| member.nick)
                .map_err(anyhow::Error::from)
        },
        || {
            let nickname = job.nickname.clone();
            async move {
                guild
                    .edit_member(&http, me, EditMember::new().nickname(nickname))
                    .await
                    .map(|_| ())
            }
        },
    )
    .await;
}

/// Apply one nickname edit. A previous attempt cancelled inside Discord is
/// `outcome_unknown`: confirm the name before sending `edit_member` again.
async fn apply_nickname_edit<R, E, RF, EF>(
    app: &Arc<BotState>,
    job: &db::NicknameReconcileJob,
    read_nickname: RF,
    edit_member: EF,
) where
    RF: FnOnce() -> R,
    R: std::future::Future<Output = Result<Option<String>>>,
    EF: FnOnce() -> E,
    E: std::future::Future<Output = serenity::Result<()>>,
{
    if !app.activation.may_record() {
        return;
    }
    if nickname_outcome_unknown(&app.db, job.guild_id).await {
        let current =
            tokio::time::timeout(crate::discord::DISCORD_REST_TIMEOUT, read_nickname()).await;
        match current {
            Ok(Ok(nick)) if nick.as_deref().unwrap_or("") == job.nickname => {
                if !app.activation.may_record() {
                    return;
                }
                if let Err(error) = db::acknowledge_nickname_reconcile(&app.db, job).await {
                    tracing::warn!(
                        guild_id = job.guild_id,
                        "failed to acknowledge nickname reconciliation: {error:#}"
                    );
                }
                return;
            }
            Ok(Ok(_)) => {}
            result => {
                let error = match result {
                    Ok(Err(error)) => error,
                    Err(error) => anyhow::Error::from(error),
                    Ok(Ok(_)) => unreachable!("successful reads handled above"),
                };
                tracing::warn!(
                    guild_id = job.guild_id,
                    "nickname confirmation read failed: {error:#}"
                );
                record_nickname_failure(app, job, &error, true).await;
                return;
            }
        }
        if !app.activation.may_record() {
            return;
        }
    }
    let now = chrono::Utc::now().timestamp();
    match db::mark_nickname_outcome_unknown(&app.db, job, now).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => {
            tracing::error!(
                guild_id = job.guild_id,
                "failed to persist nickname reconciliation outcome: {error:#}"
            );
            return;
        }
    }
    if !app.activation.may_record() {
        return;
    }
    let edit = tokio::time::timeout(
        crate::discord::DISCORD_REST_TIMEOUT,
        crate::discord::race_discord_call(&app.activation, edit_member()),
    )
    .await;
    let edit = match edit {
        Ok(crate::discord::DiscordCall::Cancelled) => return,
        Ok(crate::discord::DiscordCall::Finished(result)) => Ok(result),
        Err(elapsed) => Err(elapsed),
    };
    match edit {
        Err(_) => {
            if !app.activation.may_record() {
                return;
            }
            let now = chrono::Utc::now().timestamp();
            let message = "Discord nickname request timed out";
            if let Err(error) =
                db::schedule_nickname_reconcile_retry(&app.db, job, now, "timeout", message).await
            {
                tracing::error!(
                    guild_id = job.guild_id,
                    "failed to persist nickname reconciliation timeout: {error:#}"
                );
            }
        }
        Ok(Ok(_)) => {
            if !app.activation.may_record() {
                return;
            }
            if let Err(error) = db::acknowledge_nickname_reconcile(&app.db, job).await {
                tracing::warn!(
                    guild_id = job.guild_id,
                    "failed to acknowledge nickname reconciliation: {error:#}"
                );
            }
        }
        Ok(Err(error)) => {
            if !app.activation.may_record() {
                return;
            }
            record_nickname_failure(app, job, &error.into(), false).await;
        }
    }
}

async fn record_nickname_failure(
    app: &BotState,
    job: &db::NicknameReconcileJob,
    error: &anyhow::Error,
    preserve_unknown: bool,
) {
    if !app.activation.may_record() {
        return;
    }
    let now = chrono::Utc::now().timestamp();
    use crate::discord::NicknameFailureClass;
    let (class, mut code) = error.downcast_ref::<serenity::Error>().map_or(
        (NicknameFailureClass::Transient, "read_failed".into()),
        crate::discord::classify_nickname_failure,
    );
    if preserve_unknown && class == NicknameFailureClass::Transient {
        // A failed read cannot establish whether the previous edit took effect.
        code = "outcome_unknown".into();
    }
    let message = error.to_string();
    let result = match class {
        NicknameFailureClass::Transient => {
            db::schedule_nickname_reconcile_retry(&app.db, job, now, &code, &message)
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
            db::finish_nickname_reconcile(&app.db, job, "dormant", &code, &message, now)
                .await
                .map(|()| tracing::warn!(
                    guild_id = job.guild_id,
                    "nickname reconciliation awaits membership reconfirmation or a fresh request: {message}"
                ))
        }
        NicknameFailureClass::Terminal => {
            db::finish_nickname_reconcile(&app.db, job, "terminal", &code, &message, now)
                .await
                .map(|()| tracing::error!(
                    guild_id = job.guild_id,
                    "nickname reconciliation needs operator action: {message}"
                ))
        }
    };
    if let Err(state_error) = result {
        tracing::error!(
            guild_id = job.guild_id,
            "failed to persist nickname reconciliation failure: {state_error:#}"
        );
    }
}

async fn nickname_outcome_unknown(db: &db::Db, guild_id: i64) -> bool {
    matches!(
        db::nickname_reconcile_status(db, guild_id).await,
        Ok(Some(status)) if status.last_error_code.as_deref() == Some("outcome_unknown")
    )
}

#[cfg(test)]
mod tests;
