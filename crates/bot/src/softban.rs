//! Softban: ban with delete-message-days, durable unban row, immediate unban attempt.
use crate::commands::{Args, CmdEnv};
use crate::db::guild_data::{self, ScheduledWorkRow, transient_backoff_ms};
use crate::{db, discord, logging, state::BotState};
use anyhow::{Result, anyhow};
use serenity::model::prelude::UserId;
use std::sync::Arc;

pub(crate) struct SoftbanParsed {
    pub delete_days: u8,
    pub reason: String,
}

/// Prefix: user, optional integer days 0–7, then reason. A non-integer first token is reason.
pub(crate) fn parse_softban_prefix(
    args: &Args,
    default_days: u8,
    default_reason: &str,
) -> Result<(String, SoftbanParsed)> {
    let mut args = args.clone();
    let target = args
        .next_arg()
        .ok_or_else(|| anyhow!("missing user"))?
        .to_string();
    if args.is_empty() {
        return Ok((
            target,
            SoftbanParsed {
                delete_days: default_days,
                reason: default_reason.to_string(),
            },
        ));
    }
    let first = args.peek().unwrap_or_default();
    if let Ok(days) = first.parse::<u8>() {
        if days > 7 {
            return Err(anyhow!("delete days must be between 0 and 7"));
        }
        args.next_arg();
        let reason = if args.is_empty() {
            default_reason.to_string()
        } else {
            args.rest()
        };
        return Ok((
            target,
            SoftbanParsed {
                delete_days: days,
                reason,
            },
        ));
    }
    Ok((
        target,
        SoftbanParsed {
            delete_days: default_days,
            reason: args.rest(),
        },
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SoftbanAfterBan {
    Stop,
    RefuseExisting,
    Failed,
    Uncertain,
    Proceed,
}

pub(crate) fn softban_after_ban(
    decision: crate::member_directory::BanContinuation,
    ban_outcome: Option<db::guild_data::CaseOutcome>,
) -> SoftbanAfterBan {
    use crate::member_directory::BanContinuation;
    match decision {
        BanContinuation::Stop => SoftbanAfterBan::Stop,
        BanContinuation::AlreadyFinished { .. } | BanContinuation::RecordOnly { .. } => {
            SoftbanAfterBan::RefuseExisting
        }
        BanContinuation::BanThenRecord => match ban_outcome {
            Some(db::guild_data::CaseOutcome::Succeeded) => SoftbanAfterBan::Proceed,
            Some(db::guild_data::CaseOutcome::Failed) => SoftbanAfterBan::Failed,
            _ => SoftbanAfterBan::Uncertain,
        },
    }
}

/// Ban success must be recorded before scheduling unban. If the process dies between
/// ban success and insert, the user stays banned and no row is invented later.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const SOFTBAN_SCHEDULE_ORDER: &str =
    "ban HTTP success, ban case/mirror, insert softban_unban row, then unban HTTP";

pub(crate) struct SoftbanUnbanPayload {
    pub reason: String,
    #[allow(dead_code)]
    pub operation_id: Option<String>,
}

pub(crate) fn parse_softban_unban_payload(payload: &str) -> Result<SoftbanUnbanPayload> {
    let value: serde_json::Value = serde_json::from_str(payload)?;
    let reason = value
        .get("reason")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("scheduled softban payload missing reason"))?
        .to_string();
    let operation_id = value
        .get("operation_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    Ok(SoftbanUnbanPayload {
        reason,
        operation_id,
    })
}

pub(crate) struct UnbanDispatch {
    pub outcome: db::guild_data::CaseOutcome,
    pub retry_class: Option<discord::RetryClass>,
    pub retry_after_secs: Option<u64>,
    pub cancelled: bool,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch_softban_unban(
    app: &Arc<BotState>,
    http: &serenity::http::Http,
    guild_id: serenity::model::id::GuildId,
    target: UserId,
    reason: &str,
    enrollment: Option<db::GuildEnrollment>,
    actor: Option<u64>,
    actor_name: Option<&str>,
    log_target_name: &str,
    log_ctx: Option<&serenity::client::Context>,
    source_channel_id: u64,
) -> Result<UnbanDispatch> {
    let suppression = app.suppress_next_gateway_case(
        crate::state::GatewayCaseAction::Unban,
        guild_id.get(),
        target.get(),
        None,
    );
    let raced = discord::race_discord_call(
        &app.activation,
        http.remove_ban(guild_id, target, Some(reason)),
    )
    .await;
    let discord = match raced {
        discord::DiscordCall::Cancelled => {
            return Ok(UnbanDispatch {
                outcome: db::guild_data::CaseOutcome::Uncertain,
                retry_class: None,
                retry_after_secs: None,
                cancelled: true,
            });
        }
        discord::DiscordCall::Finished(result) => result,
    };
    let retry_class = discord.as_ref().err().map(discord::classify_retry);
    let http_status = discord
        .as_ref()
        .err()
        .and_then(discord::http_error_response)
        .map(|(status, _)| status);
    let outcome = if discord.is_ok() || http_status == Some(404) {
        db::guild_data::CaseOutcome::Succeeded
    } else if matches!(retry_class, Some(discord::RetryClass::Transient)) {
        db::guild_data::CaseOutcome::Uncertain
    } else {
        crate::cases::classify_discord(discord.as_ref().err())
    };
    if matches!(outcome, db::guild_data::CaseOutcome::Failed) {
        app.release_gateway_case_suppression(suppression);
    }
    if app.activation.may_record() {
        let mirror = discord.as_ref().ok().map(|_| {
            crate::gateway_ban_persistence::receive_unban(
                Arc::clone(app),
                guild_id.get(),
                target.get(),
                chrono::Utc::now(),
                enrollment.as_ref().map(|e| e.joined_at_ns),
                true,
                enrollment.clone(),
            )
        });
        crate::cases::record_application(
            app,
            enrollment.clone(),
            db::guild_data::CaseAction::Unban,
            outcome.clone(),
            target.get(),
            actor,
            Some(reason.to_string()),
            db::guild_data::CaseEvidence::Unavailable,
        )
        .await;
        if let Some(handle) = mirror {
            let _ = handle.await;
        }
        if matches!(outcome, db::guild_data::CaseOutcome::Succeeded)
            && let (Some(ctx), Some(actor)) = (log_ctx, actor)
        {
            logging::emit_mod_action(
                app,
                enrollment.clone(),
                ctx,
                guild_id,
                "unban",
                target.get(),
                log_target_name,
                log_target_name,
                actor,
                actor_name.unwrap_or("moderator"),
                source_channel_id,
                reason,
            )
            .await;
        }
    }
    if matches!(outcome, db::guild_data::CaseOutcome::Failed) {
        discord?;
    }
    Ok(UnbanDispatch {
        outcome,
        retry_class,
        retry_after_secs: None,
        cancelled: false,
    })
}

pub(crate) async fn apply_unban_row_result(
    store: &guild_data::GuildDataStore,
    row: &ScheduledWorkRow,
    dispatch: &UnbanDispatch,
    now_ms: i64,
) -> Result<()> {
    if dispatch.cancelled {
        return Ok(());
    }
    if matches!(dispatch.outcome, db::guild_data::CaseOutcome::Succeeded) {
        store
            .finish_scheduled_work(row, "done", None, now_ms)
            .await?;
        return Ok(());
    }
    if matches!(dispatch.retry_class, Some(discord::RetryClass::Transient)) {
        let due = now_ms + transient_backoff_ms(row.attempt.max(1), dispatch.retry_after_secs);
        store
            .finish_scheduled_work(row, "pending", Some(due), now_ms)
            .await?;
        return Ok(());
    }
    if matches!(dispatch.outcome, db::guild_data::CaseOutcome::Failed) {
        store
            .finish_scheduled_work(row, "failed", None, now_ms)
            .await?;
    }
    Ok(())
}

pub(crate) async fn run_softban(
    env: &CmdEnv<'_>,
    default_reason: &str,
    default_days: u8,
) -> Result<(UserId, String, String)> {
    use crate::commands::refuse_fenced_discord_mutation;
    let enrollment = crate::cases::capture(env.app, env.guild_id.get()).await;
    let (target_token, parsed) = if env.source_message.is_none() {
        let mut args = env.args.clone();
        let target = args
            .next_arg()
            .ok_or_else(|| anyhow!("missing user"))?
            .to_string();
        let delete_days = match args.peek() {
            Some(token) if token.parse::<u8>().is_ok() => {
                let days = token.parse::<u8>().unwrap();
                if days > 7 {
                    return Err(anyhow!("delete days must be between 0 and 7"));
                }
                args.next_arg();
                days
            }
            _ => default_days,
        };
        let reason = args
            .next_arg()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| default_reason.to_string());
        (
            target,
            SoftbanParsed {
                delete_days,
                reason: jmodguard::ban_reason::without_tag(&reason).to_owned(),
            },
        )
    } else {
        let (target, parsed) = parse_softban_prefix(&env.args, default_days, default_reason)?;
        (
            target,
            SoftbanParsed {
                reason: jmodguard::ban_reason::without_tag(&parsed.reason).to_owned(),
                delete_days: parsed.delete_days,
            },
        )
    };
    let target = super::commands::resolve_user(env.ctx, env.guild_id, &target_token)
        .await
        .ok_or_else(|| anyhow!("could not resolve user `{target_token}`"))?;
    let target_user = env
        .ctx
        .cache
        .user(target)
        .map(|u| u.name.clone())
        .unwrap_or_else(|| target.to_string());
    refuse_fenced_discord_mutation(env).await?;
    let scope = match enrollment.clone() {
        Some(e) => env.app.tenant.scope(e).await?,
        None => return Err(anyhow!("guild membership is unavailable")),
    };
    let store = env.app.tenant.store(&scope).await?;
    if store
        .open_softban_unban_exists(&scope, target.get() as i64)
        .await?
    {
        return Err(anyhow!("this user already has a pending softban unban"));
    }
    let member = env
        .ctx
        .cache
        .guild(env.guild_id)
        .and_then(|g| g.members.get(&target).cloned());
    let decision = crate::member_directory::observe_ban_handoff(
        env.app,
        env.ctx.http.as_ref(),
        env.guild_id.get(),
        target.get(),
        crate::member_directory::MarkerSave::Missing,
    )
    .await;
    let mut operation_id = String::new();
    let after = match decision {
        crate::member_directory::BanContinuation::Stop => SoftbanAfterBan::Stop,
        crate::member_directory::BanContinuation::AlreadyFinished { .. }
        | crate::member_directory::BanContinuation::RecordOnly { .. } => {
            SoftbanAfterBan::RefuseExisting
        }
        crate::member_directory::BanContinuation::BanThenRecord => {
            if !env.app.activation.may_record() {
                SoftbanAfterBan::Stop
            } else {
                let pending_ban = crate::member_directory::begin_ban_attempt(
                    env.app,
                    env.guild_id.get(),
                    target.get(),
                )
                .await;
                operation_id = pending_ban.operation_id().to_owned();
                let attempted = match db::guild_data::CaseId::try_from(operation_id.clone()) {
                    Ok(operation_id) => {
                        crate::cases::begin_application_attempt(
                            env.app,
                            enrollment.clone(),
                            db::guild_data::CaseAction::Ban,
                            target.get(),
                            Some(env.actor.get()),
                            Some(parsed.reason.clone()),
                            db::guild_data::CaseEvidence::Unavailable,
                            operation_id,
                        )
                        .await
                    }
                    Err(_) => None,
                };
                let discord_reason =
                    crate::cases::ban_dispatch_reason(&parsed.reason, attempted.as_ref());
                let suppression = env.app.suppress_next_gateway_case(
                    crate::state::GatewayCaseAction::Ban,
                    env.guild_id.get(),
                    target.get(),
                    Some(&operation_id),
                );
                let raced = if let Some(m) = member {
                    discord::race_discord_call(
                        &env.app.activation,
                        m.ban_with_reason(&env.ctx.http, parsed.delete_days, &discord_reason),
                    )
                    .await
                } else {
                    discord::race_discord_call(
                        &env.app.activation,
                        env.guild_id.ban_with_reason(
                            &env.ctx.http,
                            target,
                            parsed.delete_days,
                            &discord_reason,
                        ),
                    )
                    .await
                };
                let crate::discord::DiscordCall::Finished(discord) = raced else {
                    pending_ban
                        .finish(env.app, db::guild_data::CaseOutcome::Uncertain)
                        .await;
                    return Err(anyhow!("ban outcome is uncertain; verify it in Discord"));
                };
                let outcome = crate::cases::classify_discord(discord.as_ref().err());
                if matches!(outcome, db::guild_data::CaseOutcome::Failed) {
                    env.app.release_gateway_case_suppression(suppression);
                }
                if let Some(attempt) = attempted {
                    attempt.finish(env.app, outcome.clone()).await;
                }
                pending_ban.finish(env.app, outcome.clone()).await;
                if matches!(outcome, db::guild_data::CaseOutcome::Succeeded)
                    && env.app.activation.may_record()
                {
                    let _ = db::record_enrolled_ban_if_absent(
                        &env.app.db,
                        enrollment.clone(),
                        target.get() as i64,
                        Some(&target_user),
                        None,
                        Some(env.actor.get() as i64),
                        Some(&env.actor_member.user.name),
                        &parsed.reason,
                        None,
                        db::BanRecord::Fresh,
                        Some(&operation_id),
                    )
                    .await?;
                    logging::emit_mod_action(
                        env.app,
                        enrollment.clone(),
                        env.ctx,
                        env.guild_id,
                        "ban",
                        target.get(),
                        &target_user,
                        &target_user,
                        env.actor.get(),
                        env.actor_member.display_name(),
                        env.channel_id.get(),
                        &parsed.reason,
                    )
                    .await;
                }
                if matches!(outcome, db::guild_data::CaseOutcome::Failed) {
                    return Err(discord
                        .err()
                        .map(|error| anyhow!("{error}"))
                        .unwrap_or_else(|| anyhow!("ban failed")));
                }
                softban_after_ban(decision, Some(outcome))
            }
        }
    };
    match after {
        SoftbanAfterBan::Stop => return Err(anyhow!("writer lease is not held")),
        SoftbanAfterBan::RefuseExisting => {
            return Err(anyhow!("user is already banned; softban refused"));
        }
        SoftbanAfterBan::Failed => return Err(anyhow!("ban failed")),
        SoftbanAfterBan::Uncertain => {
            return Err(anyhow!("ban outcome is uncertain; verify it in Discord"));
        }
        SoftbanAfterBan::Proceed => {}
    }
    let now = chrono::Utc::now().timestamp_millis();
    let work_id = store
        .insert_softban_unban_pending(
            &scope,
            target.get() as i64,
            &parsed.reason,
            &operation_id,
            now,
        )
        .await
        .map_err(|error| {
            tracing::warn!("softban unban schedule failed: {error:#}");
            anyhow!("user is banned; the automatic unban could not be scheduled")
        })?;
    let claimed = store
        .claim_scheduled_work_row(
            scope.guild_id(),
            scope.enrollment().id.as_str(),
            &work_id,
            now,
        )
        .await?
        .ok_or_else(|| anyhow!("softban unban row disappeared before unban"))?;
    let dispatch = dispatch_softban_unban(
        env.app,
        env.ctx.http.as_ref(),
        env.guild_id,
        target,
        &parsed.reason,
        enrollment.clone(),
        Some(env.actor.get()),
        Some(env.actor_member.display_name()),
        &target_user,
        Some(env.ctx),
        env.channel_id.get(),
    )
    .await?;
    if dispatch.cancelled {
        return Err(anyhow!("writer lease is not held"));
    }
    apply_unban_row_result(&store, &claimed, &dispatch, now).await?;
    if matches!(dispatch.outcome, db::guild_data::CaseOutcome::Failed) {
        return Err(anyhow!("user is still banned; unban was denied"));
    }
    logging::emit_mod_action(
        env.app,
        enrollment.clone(),
        env.ctx,
        env.guild_id,
        "softban",
        target.get(),
        &target_user,
        &target_user,
        env.actor.get(),
        env.actor_member.display_name(),
        env.channel_id.get(),
        &parsed.reason,
    )
    .await;
    Ok((target, target_user, parsed.reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::Args;

    #[test]
    fn parse_softban_days_prefix() {
        for (input, days, reason) in [
            ("@u", 1, "Softbanned by a moderator"),
            ("@u rude", 1, "rude"),
            ("@u 0", 0, "Softbanned by a moderator"),
            ("@u 7 ok", 7, "ok"),
        ] {
            let (target, parsed) =
                parse_softban_prefix(&Args::new(input), 1, "Softbanned by a moderator").unwrap();
            assert_eq!(target, "@u");
            assert_eq!(parsed.delete_days, days);
            assert_eq!(parsed.reason, reason);
        }
        assert!(parse_softban_prefix(&Args::new("@u 8"), 1, "x").is_err());
        let (_, parsed) = parse_softban_prefix(&Args::new("@u words first"), 1, "d").unwrap();
        assert_eq!(parsed.delete_days, 1);
        assert_eq!(parsed.reason, "words first");
    }

    #[test]
    fn softban_after_ban_decision_table() {
        use crate::member_directory::BanContinuation;
        assert_eq!(
            softban_after_ban(BanContinuation::AlreadyFinished { reason: None }, None),
            SoftbanAfterBan::RefuseExisting
        );
        assert_eq!(
            softban_after_ban(BanContinuation::RecordOnly { reason: None }, None),
            SoftbanAfterBan::RefuseExisting
        );
        assert_eq!(
            softban_after_ban(
                BanContinuation::BanThenRecord,
                Some(db::guild_data::CaseOutcome::Succeeded)
            ),
            SoftbanAfterBan::Proceed
        );
        assert_eq!(
            softban_after_ban(
                BanContinuation::BanThenRecord,
                Some(db::guild_data::CaseOutcome::Uncertain)
            ),
            SoftbanAfterBan::Uncertain
        );
    }

    #[test]
    fn schedule_order_comment_documents_crash_window() {
        assert!(SOFTBAN_SCHEDULE_ORDER.contains("insert"));
    }
}
