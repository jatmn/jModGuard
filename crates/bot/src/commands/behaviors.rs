//! Built-in command behavior executors, plus the slash-option spec used to
//! mirror commands to Discord.

use anyhow::{Result, anyhow};
use serenity::builder::EditChannel;
use serenity::model::application::CommandOptionType;
use serenity::model::prelude::{ChannelId, Timestamp};

use super::{Args, BehaviorOutcome, CmdEnv, refuse_fenced_discord_mutation};

fn lease_held(env: &CmdEnv<'_>) -> Result<()> {
    if env.app.activation.may_record() {
        Ok(())
    } else {
        Err(anyhow!("writer lease is not held"))
    }
}
use crate::types::Behavior;
use crate::{db, logging};

/// Runs a command behavior. Returns an optional extra response plus vars
/// used to render the command's response template.
pub async fn run(env: &CmdEnv<'_>) -> Result<BehaviorOutcome> {
    let mut vars = behaviors_base_vars(env);
    let enrollment = crate::cases::capture(env.app, env.guild_id.get()).await;

    match &env.cmd.behavior {
        Behavior::Ban {
            default_reason,
            delete_days,
        } => {
            let (target, reason) = target_and_reason(env, default_reason).await?;
            let reason = jmodguard::ban_reason::without_tag(&reason).to_owned();
            let target_user = user_lookup(env, target).await?;
            let member = env
                .ctx
                .cache
                .guild(env.guild_id)
                .and_then(|g| g.members.get(&target).cloned());
            refuse_fenced_discord_mutation(env).await?;
            let decision = crate::member_directory::observe_ban_handoff(
                env.app,
                env.ctx.http.as_ref(),
                env.guild_id.get(),
                target.get(),
                crate::member_directory::MarkerSave::Missing,
            )
            .await;
            match decision {
                crate::member_directory::BanContinuation::Stop => {
                    return Err(anyhow!("writer lease is not held"));
                }
                crate::member_directory::BanContinuation::AlreadyFinished { reason } => {
                    crate::cases::settle_present_application_ban(
                        env.app,
                        enrollment.clone(),
                        target.get(),
                        reason.as_deref(),
                    )
                    .await;
                }
                crate::member_directory::BanContinuation::RecordOnly {
                    reason: discord_reason,
                } => {
                    if continue_present_command_ban(
                        env.app,
                        enrollment.clone(),
                        target.get() as i64,
                        target_user.known_name.as_deref(),
                        target_user.global_name.as_deref(),
                        env.actor.get() as i64,
                        &env.actor_member.user.name,
                        &reason,
                        discord_reason.as_deref(),
                    )
                    .await?
                    {
                        log_mod(
                            env,
                            enrollment.clone(),
                            "ban",
                            target,
                            &target_user.name,
                            &reason,
                        )
                        .await;
                    }
                }
                crate::member_directory::BanContinuation::BanThenRecord => {
                    if !env.app.activation.may_record() {
                        return Err(anyhow!("writer lease is not held"));
                    }
                    let pending_ban = crate::member_directory::begin_ban_attempt(
                        env.app,
                        env.guild_id.get(),
                        target.get(),
                    )
                    .await;
                    if !env.app.activation.may_record() {
                        return Err(anyhow!("writer lease is not held"));
                    }
                    let attempted = match crate::db::guild_data::CaseId::try_from(
                        pending_ban.operation_id().to_owned(),
                    ) {
                        Ok(operation_id) => {
                            crate::cases::begin_application_attempt(
                                env.app,
                                enrollment.clone(),
                                crate::db::guild_data::CaseAction::Ban,
                                target.get(),
                                Some(env.actor.get()),
                                Some(reason.clone()),
                                crate::db::guild_data::CaseEvidence::Unavailable,
                                operation_id,
                            )
                            .await
                        }
                        Err(_) => None,
                    };
                    let discord_reason =
                        crate::cases::ban_dispatch_reason(&reason, attempted.as_ref());
                    let operation_id = pending_ban.operation_id().to_owned();
                    let suppression = env.app.suppress_next_gateway_case(
                        crate::state::GatewayCaseAction::Ban,
                        env.guild_id.get(),
                        target.get(),
                        Some(&operation_id),
                    );
                    let raced = if let Some(m) = member {
                        crate::discord::race_discord_call(
                            &env.app.activation,
                            m.ban_with_reason(&env.ctx.http, *delete_days, &discord_reason),
                        )
                        .await
                    } else {
                        crate::discord::race_discord_call(
                            &env.app.activation,
                            env.guild_id.ban_with_reason(
                                &env.ctx.http,
                                target,
                                *delete_days,
                                &discord_reason,
                            ),
                        )
                        .await
                    };
                    let crate::discord::DiscordCall::Finished(discord) = raced else {
                        pending_ban
                            .finish(env.app, crate::db::guild_data::CaseOutcome::Uncertain)
                            .await;
                        return Err(anyhow!("ban outcome is uncertain; verify it in Discord"));
                    };
                    let outcome = crate::cases::classify_discord(discord.as_ref().err());
                    if matches!(outcome, crate::db::guild_data::CaseOutcome::Failed) {
                        env.app.release_gateway_case_suppression(suppression);
                    }
                    if !env.app.activation.may_record() {
                        return Err(anyhow!("writer lease is not held"));
                    }
                    if let Some(attempt) = attempted {
                        attempt.finish(env.app, outcome.clone()).await;
                    }
                    pending_ban.finish(env.app, outcome.clone()).await;
                    match outcome {
                        crate::db::guild_data::CaseOutcome::Succeeded => {
                            if env.app.activation.may_record()
                                && record_ban_once(
                                    env,
                                    enrollment.clone(),
                                    target,
                                    &target_user,
                                    &reason,
                                    db::BanRecord::Fresh,
                                    Some(&operation_id),
                                )
                                .await?
                            {
                                log_mod(
                                    env,
                                    enrollment.clone(),
                                    "ban",
                                    target,
                                    &target_user.name,
                                    &reason,
                                )
                                .await;
                            }
                        }
                        crate::db::guild_data::CaseOutcome::Failed => {
                            return Err(discord
                                .err()
                                .map(|error| anyhow!("{error}"))
                                .unwrap_or_else(|| anyhow!("ban failed")));
                        }
                        _ => {
                            return Err(anyhow!("ban outcome is uncertain; verify it in Discord"));
                        }
                    }
                }
            }
            vars.insert("target".into(), target_user.name.clone());
            vars.insert("target_id".into(), target.to_string());
            vars.insert("reason".into(), reason);
            Ok(BehaviorOutcome {
                response: None,
                vars,
                suppress_response: false,
            })
        }
        Behavior::Unban { default_reason } => {
            let (target, reason) = target_and_reason(env, default_reason).await?;
            let target_user = user_lookup(env, target).await?;
            // Discord is authoritative; local bans table is a mirror healed by
            // GUILD_BAN_REMOVE. Do not fail the command after Discord succeeds.
            refuse_fenced_discord_mutation(env).await?;
            lease_held(env)?;
            let suppression = env.app.suppress_next_gateway_case(
                crate::state::GatewayCaseAction::Unban,
                env.guild_id.get(),
                target.get(),
                None,
            );
            let discord = match crate::discord::race_discord_call(
                &env.app.activation,
                env.ctx.http.remove_ban(env.guild_id, target, Some(&reason)),
            )
            .await
            {
                crate::discord::DiscordCall::Cancelled => {
                    crate::cases::record_application(
                        env.app,
                        enrollment.clone(),
                        crate::db::guild_data::CaseAction::Unban,
                        crate::db::guild_data::CaseOutcome::Uncertain,
                        target.get(),
                        Some(env.actor.get()),
                        Some(reason.clone()),
                        crate::db::guild_data::CaseEvidence::Unavailable,
                    )
                    .await;
                    return Err(anyhow!("ban outcome is uncertain; verify it in Discord"));
                }
                crate::discord::DiscordCall::Finished(result) => result,
            };
            let outcome = crate::cases::classify_discord(discord.as_ref().err());
            if matches!(outcome, crate::db::guild_data::CaseOutcome::Failed) {
                env.app.release_gateway_case_suppression(suppression);
            }
            lease_held(env)?;
            crate::cases::record_application(
                env.app,
                enrollment.clone(),
                crate::db::guild_data::CaseAction::Unban,
                outcome,
                target.get(),
                Some(env.actor.get()),
                Some(reason.clone()),
                crate::db::guild_data::CaseEvidence::Unavailable,
            )
            .await;
            discord?;
            if let Err(e) =
                db::mark_enrolled_unbanned(&env.app.db, enrollment.clone(), target.get() as i64)
                    .await
            {
                tracing::warn!(
                    "local ban mirror update failed after Discord unban \
                     (guild={} user={}): {e:#}",
                    env.guild_id,
                    target
                );
            }
            log_mod(
                env,
                enrollment.clone(),
                "unban",
                target,
                &target_user.name,
                &reason,
            )
            .await;
            vars.insert("target".into(), target_user.name.clone());
            vars.insert("target_id".into(), target.to_string());
            vars.insert("reason".into(), reason);
            Ok(BehaviorOutcome {
                response: None,
                vars,
                suppress_response: false,
            })
        }
        Behavior::Kick { default_reason } => {
            let (target, reason) = target_and_reason(env, default_reason).await?;
            let target_user = user_lookup(env, target).await?;
            let member = env
                .ctx
                .cache
                .guild(env.guild_id)
                .and_then(|g| g.members.get(&target).cloned())
                .ok_or_else(|| anyhow!("target is not in this server"))?;
            refuse_fenced_discord_mutation(env).await?;
            lease_held(env)?;
            let discord = match crate::discord::race_discord_call(
                &env.app.activation,
                member.kick_with_reason(&env.ctx.http, &reason),
            )
            .await
            {
                crate::discord::DiscordCall::Cancelled => {
                    Err(crate::discord::cutoff_discord_error())
                }
                crate::discord::DiscordCall::Finished(result) => result,
            };
            let outcome = crate::cases::classify_discord(discord.as_ref().err());
            crate::cases::record_application(
                env.app,
                enrollment.clone(),
                crate::db::guild_data::CaseAction::Kick,
                outcome,
                target.get(),
                Some(env.actor.get()),
                Some(reason.clone()),
                crate::db::guild_data::CaseEvidence::Unavailable,
            )
            .await;
            lease_held(env)?;
            discord?;
            log_mod(
                env,
                enrollment.clone(),
                "kick",
                target,
                &target_user.name,
                &reason,
            )
            .await;
            vars.insert("target".into(), target_user.name.clone());
            vars.insert("target_id".into(), target.to_string());
            vars.insert("reason".into(), reason);
            Ok(BehaviorOutcome {
                response: None,
                vars,
                suppress_response: false,
            })
        }
        Behavior::Timeout { default_minutes } => {
            let mut args = env.args.clone();
            let target =
                resolve_target(env, args.next_arg().ok_or_else(|| anyhow!("missing user"))?)
                    .await?;
            let minutes: u64 = args
                .next_arg()
                .and_then(|m| m.parse().ok())
                .unwrap_or(*default_minutes as u64);
            let reason = if args.is_empty() {
                "timed out".to_string()
            } else {
                args.rest()
            };
            let target_user = user_lookup(env, target).await?;
            let mut member = env
                .ctx
                .cache
                .guild(env.guild_id)
                .and_then(|g| g.members.get(&target).cloned())
                .ok_or_else(|| anyhow!("target is not in this server"))?;
            let until = if minutes == 0 {
                None
            } else {
                Some(
                    Timestamp::from_unix_timestamp(
                        chrono::Utc::now().timestamp() + (minutes as i64) * 60,
                    )
                    .map_err(|e| anyhow!("invalid timeout: {e}"))?,
                )
            };
            refuse_fenced_discord_mutation(env).await?;
            lease_held(env)?;
            let raced = if let Some(until) = until {
                crate::discord::race_discord_call(
                    &env.app.activation,
                    member.disable_communication_until_datetime(&env.ctx.http, until),
                )
                .await
            } else {
                crate::discord::race_discord_call(
                    &env.app.activation,
                    member.enable_communication(&env.ctx.http),
                )
                .await
            };
            let discord = match raced {
                crate::discord::DiscordCall::Cancelled => {
                    Err(crate::discord::cutoff_discord_error())
                }
                crate::discord::DiscordCall::Finished(result) => result,
            };
            let outcome = crate::cases::classify_discord(discord.as_ref().err());
            crate::cases::record(
                env.app,
                enrollment.clone(),
                if minutes == 0 {
                    crate::db::guild_data::CaseAction::TimeoutRemoval
                } else {
                    crate::db::guild_data::CaseAction::Timeout
                },
                outcome,
                target.get(),
                Some(env.actor.get()),
                Some(reason.clone()),
                crate::db::guild_data::CaseEvidence::Unavailable,
                crate::cases::application_source(),
            )
            .await;
            lease_held(env)?;
            discord?;
            vars.insert(
                "duration".into(),
                crate::templates::format_duration(minutes.saturating_mul(60)),
            );
            if minutes == 0 {
                let mut v = vars.clone();
                v.insert("target_id".into(), target.to_string());
                v.insert("reason".into(), reason.clone());
                logging::emit(env.app, env.ctx, env.guild_id, "timeout_remove", v).await;
            } else {
                let mut v = vars.clone();
                v.insert("target_id".into(), target.to_string());
                v.insert("reason".into(), reason.clone());
                logging::emit(env.app, env.ctx, env.guild_id, "timeout_add", v).await;
            }
            log_mod(
                env,
                enrollment.clone(),
                "timeout",
                target,
                &target_user.name,
                &reason,
            )
            .await;
            vars.insert("target".into(), target_user.name.clone());
            vars.insert("target_id".into(), target.to_string());
            vars.insert("minutes".into(), minutes.to_string());
            vars.insert("reason".into(), reason);
            Ok(BehaviorOutcome {
                response: None,
                vars,
                suppress_response: false,
            })
        }
        Behavior::Warn { default_reason } => {
            let mut args = env.args.clone();
            let target =
                resolve_target(env, args.next_arg().ok_or_else(|| anyhow!("missing user"))?)
                    .await?;
            let reason = if args.is_empty() {
                default_reason.clone()
            } else {
                args.rest()
            };
            let target_user = user_lookup(env, target).await?;
            lease_held(env)?;
            let warn_result = db::add_enrolled_warn(
                &env.app.db,
                enrollment.clone(),
                target.get() as i64,
                env.actor.get() as i64,
                &reason,
            )
            .await;
            lease_held(env)?;
            let outcome = crate::cases::classify_db_commit(&warn_result);
            crate::cases::record(
                env.app,
                enrollment.clone(),
                crate::db::guild_data::CaseAction::Warning,
                outcome.clone(),
                target.get(),
                Some(env.actor.get()),
                Some(reason.clone()),
                crate::db::guild_data::CaseEvidence::Unavailable,
                crate::cases::application_source(),
            )
            .await;
            match outcome {
                crate::db::guild_data::CaseOutcome::Succeeded => {}
                crate::db::guild_data::CaseOutcome::Failed => {
                    return Err(warn_result.unwrap_err());
                }
                _ => {
                    return Err(anyhow!(
                        "warn outcome is uncertain; verify it in the database"
                    ));
                }
            }
            log_mod(
                env,
                enrollment.clone(),
                "warn",
                target,
                &target_user.name,
                &reason,
            )
            .await;
            vars.insert("target".into(), target_user.name.clone());
            vars.insert("target_id".into(), target.to_string());
            vars.insert("reason".into(), reason);
            Ok(BehaviorOutcome {
                response: None,
                vars,
                suppress_response: false,
            })
        }
        Behavior::Warns => {
            let mut args = env.args.clone();
            let target =
                resolve_target(env, args.next_arg().ok_or_else(|| anyhow!("missing user"))?)
                    .await?;
            let target_user = user_lookup(env, target).await?;
            vars.insert("target_id".into(), target.to_string());
            let warns =
                db::list_warns(&env.app.db, env.guild_id.get() as i64, target.get() as i64).await?;
            if warns.is_empty() {
                return Ok(BehaviorOutcome {
                    response: Some(format!("**{}** has no warnings.", target_user.name)),
                    vars,
                    suppress_response: false,
                });
            }
            let lines: Vec<String> = warns
                .iter()
                .map(|w| {
                    let reason = w.reason.clone().unwrap_or_default();
                    let ts = db::time_ago(&Some(w.created_at.clone()));
                    format!("`#{}` {reason} — {ts}", w.id)
                })
                .collect();
            let response = format!(
                "**{} warnings for {}:**\n{}",
                warns.len(),
                target_user.name,
                lines.join("\n")
            );
            Ok(BehaviorOutcome {
                response: Some(response),
                vars,
                suppress_response: false,
            })
        }
        Behavior::WarnClear => {
            let mut args = env.args.clone();
            let target =
                resolve_target(env, args.next_arg().ok_or_else(|| anyhow!("missing user"))?)
                    .await?;
            let target_user = user_lookup(env, target).await?;
            let n = db::clear_enrolled_warns(&env.app.db, enrollment.clone(), target.get() as i64)
                .await?;
            vars.insert("target".into(), target_user.name.clone());
            vars.insert("target_id".into(), target.to_string());
            vars.insert("count".into(), n.to_string());
            log_mod(
                env,
                enrollment.clone(),
                "warnclear",
                target,
                &target_user.name,
                &n.to_string(),
            )
            .await;
            Ok(BehaviorOutcome {
                response: Some(format!("Cleared {n} warnings for {}.", target_user.name)),
                vars,
                suppress_response: false,
            })
        }
        Behavior::Banlist => {
            let limit: i64 = env.args.peek().and_then(|s| s.parse().ok()).unwrap_or(25);
            let bans = db::list_bans(&env.app.db, env.guild_id.get() as i64, limit).await?;
            if bans.is_empty() {
                return Ok(BehaviorOutcome {
                    response: Some("No bans recorded.".into()),
                    vars,
                    suppress_response: false,
                });
            }
            let lines: Vec<String> = bans
                .iter()
                .map(|b| {
                    let name = b.user_id.to_string();
                    let reason = b.reason.clone().unwrap_or_default();
                    let ts = db::time_ago(&Some(b.created_at.clone()));
                    let status = if b.active { "active" } else { "unbanned" };
                    format!("`{}` **{name}** ({status}) — {reason} — {ts}", b.user_id)
                })
                .collect();
            let response = format!("**Recent bans ({}):**\n{}", bans.len(), lines.join("\n"));
            Ok(BehaviorOutcome {
                response: Some(response),
                vars,
                suppress_response: false,
            })
        }
        Behavior::Purge { default_count } => {
            let count: u64 = env
                .args
                .peek()
                .and_then(|s| s.parse().ok())
                .unwrap_or(*default_count as u64)
                .clamp(1, 100);
            let msgs = env
                .channel_id
                .messages(
                    &env.ctx.http,
                    serenity::builder::GetMessages::new().limit(count as u8),
                )
                .await?;
            let ids: Vec<serenity::model::id::MessageId> =
                msgs.iter().filter(|m| !m.pinned).map(|m| m.id).collect();
            let n = ids.len();
            refuse_fenced_discord_mutation(env).await?;
            if n > 0 {
                lease_held(env)?;
            }
            if n > 1 {
                let _ = crate::discord::race_discord_call(
                    &env.app.activation,
                    env.channel_id.delete_messages(&env.ctx.http, &ids),
                )
                .await;
            } else if n == 1 {
                let _ = crate::discord::race_discord_call(
                    &env.app.activation,
                    env.channel_id.delete_message(&env.ctx.http, ids[0]),
                )
                .await;
            }
            log_mod(
                env,
                enrollment.clone(),
                "purge",
                env.actor,
                "messages",
                &format!("{n} deleted"),
            )
            .await;
            vars.insert("count".into(), n.to_string());
            Ok(BehaviorOutcome {
                response: None,
                vars,
                suppress_response: false,
            })
        }
        Behavior::Say => {
            let (target, text) = say_target_and_text(&env.args, env.channel_id)?;
            lease_held(env)?;
            match crate::discord::race_discord_call(
                &env.app.activation,
                target.say(&env.ctx.http, &text),
            )
            .await
            {
                crate::discord::DiscordCall::Cancelled => {
                    anyhow::bail!("writer lease is not held");
                }
                crate::discord::DiscordCall::Finished(result) => {
                    result?;
                }
            }
            vars.insert("channel".into(), format!("<#{}>", target));
            Ok(BehaviorOutcome {
                response: None,
                vars,
                // The posted message is already visible in the invocation
                // channel, so a confirmation reply there would be redundant.
                suppress_response: target == env.channel_id,
            })
        }
        Behavior::Slowmode { default_seconds } => {
            let seconds: u64 = env
                .args
                .peek()
                .and_then(|s| s.parse().ok())
                .unwrap_or(*default_seconds);
            refuse_fenced_discord_mutation(env).await?;
            lease_held(env)?;
            match crate::discord::race_discord_call(
                &env.app.activation,
                env.channel_id.edit(
                    &env.ctx.http,
                    EditChannel::new().rate_limit_per_user(seconds as u16),
                ),
            )
            .await
            {
                crate::discord::DiscordCall::Cancelled => {
                    anyhow::bail!("writer lease is not held");
                }
                crate::discord::DiscordCall::Finished(result) => {
                    result?;
                }
            }
            vars.insert("seconds".into(), seconds.to_string());
            let duration = crate::templates::format_duration(seconds);
            vars.insert("duration".into(), duration.clone());
            log_mod(
                env,
                enrollment.clone(),
                "slowmode",
                env.actor,
                "channel",
                &duration,
            )
            .await;
            Ok(BehaviorOutcome {
                response: None,
                vars,
                suppress_response: false,
            })
        }
        Behavior::Setname => {
            let raw = env.args.clone().rest();
            let trimmed = raw.trim();
            // Empty / `clear` / `-` clears the Discord nickname (same as WebUI).
            let clear =
                trimmed.is_empty() || trimmed.eq_ignore_ascii_case("clear") || trimmed == "-";
            let name = if clear { "" } else { trimmed };
            let guild_id = env.guild_id.get();
            // Commit desired state plus a durable outbox row atomically. The
            // bot is the sole Discord side-effect worker, so bot/API writers
            // cannot apply nicknames out of commit order.
            db::patch_settings(
                &env.app.db,
                guild_id as i64,
                None,
                Some(Some(name.to_string())),
                None,
            )
            .await?;
            env.app.invalidate_configs();
            vars.insert(
                "name".into(),
                if clear {
                    "(cleared)".to_string()
                } else {
                    name.to_string()
                },
            );
            Ok(BehaviorOutcome {
                response: None,
                vars,
                suppress_response: false,
            })
        }
        Behavior::SetPrefix => {
            let prefix = env
                .args
                .clone()
                .next_arg()
                .ok_or_else(|| anyhow!("missing prefix"))?;
            // Patch only prefix so other fields keep inheriting from global.
            lease_held(env)?;
            db::patch_settings(
                &env.app.db,
                env.guild_id.get() as i64,
                Some(Some(prefix.clone())),
                None,
                None,
            )
            .await?;
            env.app.invalidate_configs();
            vars.insert("prefix".into(), prefix);
            Ok(BehaviorOutcome {
                response: None,
                vars,
                suppress_response: false,
            })
        }
        Behavior::Template { action } => run_template(env, action).await,
        Behavior::Rule { action } => run_rule(env, action).await,
        Behavior::Logchannel { action } => run_logchannel(env, action).await,
        Behavior::Testregex => {
            let mut args = env.args.clone();
            let pattern = args.next_arg().ok_or_else(|| anyhow!("missing pattern"))?;
            let sample = args.rest();
            if sample.len() > crate::normalize::USER_REGEX_MAX_SAMPLE_BYTES {
                return Err(anyhow!(
                    "sample too long (max {} bytes)",
                    crate::normalize::USER_REGEX_MAX_SAMPLE_BYTES
                ));
            }
            // Same compile budget as WebUI /api/.../testregex and add_badword.
            let re = crate::normalize::compile_user_regex(&pattern).map_err(|e| anyhow!(e))?;
            let normalized = crate::normalize::normalize_lenient(&sample);
            let raw_match = re.is_match(&sample);
            let norm_match = re.is_match(&normalized);
            let response = format!(
                "Pattern `{pattern}`\nRaw match: {raw_match}\nNormalized match: {norm_match}\nNormalized sample: `{normalized}`"
            );
            Ok(BehaviorOutcome {
                response: Some(response),
                vars,
                suppress_response: false,
            })
        }
        Behavior::Help => {
            let cfg = env.app.configs.get(&env.app.db, env.guild_id.get()).await?;
            let mut args = env.args.clone();
            let response = if let Some(name) = args.next_arg() {
                match cfg.find_command(&name) {
                    Some(c) => format!(
                        "**!{}** {} `{}`\nAliases: {}\nUsage: `{}`\nRequires: {}",
                        c.name,
                        c.description,
                        c.category,
                        if c.aliases.is_empty() {
                            "-".to_string()
                        } else {
                            c.aliases.join(", ")
                        },
                        c.usage,
                        c.permission
                    ),
                    None => format!(
                        "Unknown command `{name}`. Use `{}help` for the list.",
                        cfg.settings.prefix
                    ),
                }
            } else {
                let mut by_cat: std::collections::BTreeMap<String, Vec<String>> =
                    Default::default();
                for c in cfg.commands.values() {
                    by_cat
                        .entry(c.category.clone())
                        .or_default()
                        .push(format!("`{}` — {}", c.name, c.description));
                }
                let mut out = format!("**Commands** (prefix `{}`)\n", cfg.settings.prefix);
                for (cat, cmds) in by_cat {
                    out.push_str(&format!("\n**{cat}**\n"));
                    out.push_str(&cmds.join("\n"));
                }
                out.push_str(&format!(
                    "\n\nUse `{}help <command>` for details. Slash equivalents exist too.",
                    cfg.settings.prefix
                ));
                out
            };
            Ok(BehaviorOutcome {
                response: Some(response),
                vars,
                suppress_response: false,
            })
        }
        Behavior::About => {
            let response = format!(
                "**{}** — a data-driven Discord moderation bot.\nVersion {}\nUptime: {}\nConfig via WebUI, `!help` for commands.",
                env.ctx.cache.current_user().name.clone(),
                env!("CARGO_PKG_VERSION"),
                crate::templates::format_duration(env.app.start.elapsed().as_secs())
            );
            Ok(BehaviorOutcome {
                response: Some(response),
                vars,
                suppress_response: false,
            })
        }
        Behavior::Stats => {
            // Guild-scoped only — same contract as WebUI `/api/guilds/{id}/stats`.
            let response = stats_response(
                env.app,
                env.guild_id.get() as i64,
                env.app.start.elapsed().as_secs(),
            )
            .await?;
            Ok(BehaviorOutcome {
                response: Some(response),
                vars,
                suppress_response: false,
            })
        }
        Behavior::Plan { steps } => crate::commands::plans::run(env, steps).await,
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

async fn stats_response(
    app: &crate::state::BotState,
    guild_id: i64,
    uptime_seconds: u64,
) -> Result<String> {
    let api_task = jmodguard::read_link::try_read_link(
        &app.cfg.bot.read_target,
        app.cfg.bot.control_secret.as_str(),
    )
    .ok()
    .flatten()
    .map(|link| tokio::task::spawn_blocking(move || link.fetch_container_use_blocking()));
    // Cached by the background sampler. This does not open cgroup files or wait.
    let bot_use = jmodguard::cgroup::latest_container_sample();
    let s = db::guild_stats(&app.reads, guild_id).await?;
    let updates = db::component_updates_status(&app.reads)
        .await
        .inspect_err(|error| tracing::warn!("release-status lookup failed: {error:#}"))
        .ok();
    let api_reading = match api_task {
        Some(task) => tokio::time::timeout(jmodguard::read_link::STATS_FETCH_TIMEOUT, task)
            .await
            .ok()
            .and_then(|joined| joined.ok().flatten()),
        None => None,
    };
    let (api_sample, api_copy) = api_reading
        .map(|response| (Some(response.sample), Some(response.copy)))
        .unwrap_or((None, None));
    Ok(crate::templates::stats_with_container_use(
        crate::templates::stats_response_with_update_availability(
            uptime_seconds,
            s.events,
            s.warns,
            s.bans,
            updates.as_deref(),
            chrono::Utc::now().timestamp(),
        ),
        &bot_use,
        api_sample.as_ref(),
        api_copy.as_deref(),
    ))
}

/// Standard variables available to every command's response template.
pub fn behaviors_base_vars(env: &CmdEnv<'_>) -> std::collections::BTreeMap<String, String> {
    let mut vars = std::collections::BTreeMap::new();
    vars.insert("user".into(), env.actor_member.display_name().to_string());
    vars.insert(
        "actor".into(),
        format!("{} (<@{}>)", env.actor_member.display_name(), env.actor),
    );
    vars.insert("actor_id".into(), env.actor.to_string());
    vars.insert(
        "guild".into(),
        env.ctx
            .cache
            .guild(env.guild_id)
            .map(|g| g.name.clone())
            .unwrap_or_else(|| env.guild_id.to_string()),
    );
    vars.insert("channel".into(), format!("<#{}>", env.channel_id));
    vars.insert("source_channel_id".into(), env.channel_id.to_string());
    vars
}

async fn resolve_target(env: &CmdEnv<'_>, token: String) -> Result<serenity::model::id::UserId> {
    super::resolve_user(env.ctx, env.guild_id, &token)
        .await
        .ok_or_else(|| anyhow!("could not resolve user `{token}`"))
}

async fn target_and_reason(
    env: &CmdEnv<'_>,
    default_reason: &str,
) -> Result<(serenity::model::id::UserId, String)> {
    let mut args = env.args.clone();
    let target =
        resolve_target(env, args.next_arg().ok_or_else(|| anyhow!("missing user"))?).await?;
    let reason = if args.is_empty() {
        default_reason.to_string()
    } else {
        args.rest()
    };
    Ok((target, reason))
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn continue_present_command_ban(
    app: &crate::state::BotState,
    enrollment: Option<db::GuildEnrollment>,
    target: i64,
    known_name: Option<&str>,
    global_name: Option<&str>,
    actor: i64,
    actor_name: &str,
    reason: &str,
    discord_reason: Option<&str>,
) -> Result<bool> {
    let subject = u64::try_from(target).unwrap_or(0);
    crate::cases::settle_present_application_ban(app, enrollment.clone(), subject, discord_reason)
        .await;
    if !app.activation.may_record() {
        return Ok(false);
    }
    Ok(db::record_enrolled_ban_if_absent(
        &app.db,
        enrollment,
        target,
        known_name,
        global_name,
        Some(actor),
        Some(actor_name),
        reason,
        None,
        db::BanRecord::Retry,
        None,
    )
    .await?
    .is_some())
}

async fn record_ban_once(
    env: &CmdEnv<'_>,
    enrollment: Option<db::GuildEnrollment>,
    target: serenity::model::id::UserId,
    target_user: &UserInfo,
    reason: &str,
    kind: db::BanRecord,
    operation_id: Option<&str>,
) -> Result<bool> {
    if !env.app.activation.may_record() {
        return Ok(false);
    }
    Ok(db::record_enrolled_ban_if_absent(
        &env.app.db,
        enrollment,
        target.get() as i64,
        target_user.known_name.as_deref(),
        target_user.global_name.as_deref(),
        Some(env.actor.get() as i64),
        Some(&env.actor_member.user.name),
        reason,
        None,
        kind,
        operation_id,
    )
    .await?
    .is_some())
}

async fn user_lookup(env: &CmdEnv<'_>, id: serenity::model::id::UserId) -> Result<UserInfo> {
    let known_user = env.ctx.cache.user(id);
    let known_name = known_user.as_ref().map(|u| u.name.clone());
    let global_name = known_user.as_ref().and_then(|u| u.global_name.clone());
    let name = known_name.clone().unwrap_or_else(|| id.to_string());
    Ok(UserInfo {
        name,
        known_name,
        global_name,
    })
}

struct UserInfo {
    name: String,
    known_name: Option<String>,
    global_name: Option<String>,
}

fn parse_channel_mention(token: &str) -> Option<ChannelId> {
    token
        .strip_prefix("<#")
        .and_then(|t| t.strip_suffix('>'))
        .and_then(|t| t.parse::<u64>().ok())
        .map(Into::into)
}

/// Resolves `!say` arguments into the target channel and the text to post.
/// A leading `<#id>` mention redirects the post; without one the invocation
/// channel is the target. Errors when no text remains.
fn say_target_and_text(args: &Args, invocation: ChannelId) -> Result<(ChannelId, String)> {
    let mut args = args.clone();
    let mut text = args.rest();
    let mut target = invocation;
    if let Some(ch) = parse_channel_mention(text.split_whitespace().next().unwrap_or("")) {
        target = ch;
        text = text
            .split_once(char::is_whitespace)
            .map(|x| x.1)
            .unwrap_or("")
            .to_string();
    }
    let text = text.trim();
    if text.is_empty() {
        return Err(anyhow!("nothing to say"));
    }
    Ok((target, text.to_string()))
}

async fn log_mod(
    env: &CmdEnv<'_>,
    enrollment: Option<db::GuildEnrollment>,
    action: &str,
    target: serenity::model::id::UserId,
    target_name: &str,
    reason: &str,
) {
    logging::emit_mod_action(
        env.app,
        enrollment,
        env.ctx,
        env.guild_id,
        action,
        target.get(),
        target_name,
        target_name,
        env.actor.get(),
        env.actor_member.display_name(),
        env.channel_id.get(),
        reason,
    )
    .await;
}

// ---------------------------------------------------------------------------
// template / rule / logchannel subcommands
// ---------------------------------------------------------------------------

async fn run_template(env: &CmdEnv<'_>, default_action: &str) -> Result<BehaviorOutcome> {
    let mut args = env.args.clone();
    let action = args
        .next_arg()
        .unwrap_or_else(|| default_action.to_string());
    let vars = std::collections::BTreeMap::new();
    match action.as_str() {
        "list" => {
            let guild = env.guild_id.get() as i64;
            let mut tpl = db::list_templates(&env.app.db, guild).await?;
            let global = db::list_templates(&env.app.db, 0).await?;
            let existing: std::collections::HashSet<String> =
                tpl.iter().map(|(e, _)| e.clone()).collect();
            tpl.extend(global.into_iter().filter(|(e, _)| !existing.contains(e)));
            let response = tpl
                .iter()
                .map(|(e, t)| {
                    let snippet: String = t.chars().take(60).collect();
                    format!("`{e}` — {snippet}…")
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(BehaviorOutcome {
                response: Some(format!("**Event templates:**\n{response}")),
                vars,
                suppress_response: false,
            })
        }
        "get" => {
            let event = args.next_arg().ok_or_else(|| anyhow!("missing event"))?;
            let cfg = env.app.configs.get(&env.app.db, env.guild_id.get()).await?;
            let t = cfg.templates.get(&event).cloned().unwrap_or_default();
            Ok(BehaviorOutcome {
                response: Some(format!("`{event}`:\n```\n{t}\n```")),
                vars,
                suppress_response: false,
            })
        }
        "set" => {
            let event = args.next_arg().ok_or_else(|| anyhow!("missing event"))?;
            let text = args.rest();
            if text.trim().is_empty() {
                return Err(anyhow!("missing template text"));
            }
            db::upsert_template(&env.app.db, env.guild_id.get() as i64, &event, &text).await?;
            env.app.invalidate_configs();
            Ok(BehaviorOutcome {
                response: Some(format!("Template `{event}` updated for this server.")),
                vars,
                suppress_response: false,
            })
        }
        "reset" => {
            let event = args.next_arg().ok_or_else(|| anyhow!("missing event"))?;
            db::delete_template(&env.app.db, env.guild_id.get() as i64, &event).await?;
            env.app.invalidate_configs();
            Ok(BehaviorOutcome {
                response: Some(format!("Template `{event}` reset to global/default.")),
                vars,
                suppress_response: false,
            })
        }
        other => Err(anyhow!("unknown template action `{other}`")),
    }
}

async fn run_rule(env: &CmdEnv<'_>, default_action: &str) -> Result<BehaviorOutcome> {
    let mut args = env.args.clone();
    let (action, rule_id) = rule_action_and_id(&mut args, default_action);
    let vars = std::collections::BTreeMap::new();
    let guild = env.guild_id.get() as i64;
    match action.as_str() {
        "list" => {
            let cfg = env.app.configs.get(&env.app.db, env.guild_id.get()).await?;
            let mut lines = Vec::new();
            for id in crate::types::RULE_IDS {
                if let Some(r) = cfg.rules.get(id) {
                    let state = if !r.enabled {
                        "disabled"
                    } else if r.dry_run {
                        "dry-run"
                    } else {
                        "enabled"
                    };
                    let actions = r
                        .actions
                        .iter()
                        .map(|a| format!("{}@{}", a.action, a.at))
                        .collect::<Vec<_>>()
                        .join(", ");
                    lines.push(format!("`{id}` — {state} — actions: {actions}"));
                }
            }
            Ok(BehaviorOutcome {
                response: Some(format!("**Automod rules:**\n{}", lines.join("\n"))),
                vars,
                suppress_response: false,
            })
        }
        "toggle" => {
            if rule_id.is_empty() {
                return Err(anyhow!("missing rule"));
            }
            let cfg = env.app.configs.get(&env.app.db, env.guild_id.get()).await?;
            let mut rule = cfg
                .rules
                .get(&rule_id)
                .cloned()
                .ok_or_else(|| anyhow!("unknown rule `{rule_id}`"))?;
            rule.enabled = !rule.enabled;
            db::upsert_rule(&env.app.db, guild, &rule_id, &rule).await?;
            env.app.invalidate_configs();
            Ok(BehaviorOutcome {
                response: Some(format!(
                    "Rule `{rule_id}` is now {}.",
                    if rule.enabled { "enabled" } else { "disabled" }
                )),
                vars,
                suppress_response: false,
            })
        }
        "dryrun" => {
            if rule_id.is_empty() {
                return Err(anyhow!("missing rule"));
            }
            let on = match args.next_arg().as_deref() {
                Some("on") | Some("true") => true,
                Some("off") | Some("false") => false,
                _ => true,
            };
            let cfg = env.app.configs.get(&env.app.db, env.guild_id.get()).await?;
            let mut rule = cfg
                .rules
                .get(&rule_id)
                .cloned()
                .ok_or_else(|| anyhow!("unknown rule `{rule_id}`"))?;
            set_rule_dry_run(&rule_id, &mut rule, on)?;
            db::upsert_rule(&env.app.db, guild, &rule_id, &rule).await?;
            env.app.invalidate_configs();
            Ok(BehaviorOutcome {
                response: Some(format!("Rule `{rule_id}` dry-run is now {on}.")),
                vars,
                suppress_response: false,
            })
        }
        other => Err(anyhow!("unknown rule action `{other}`")),
    }
}

fn rule_action_and_id(args: &mut Args, default_action: &str) -> (String, String) {
    let mut action = args
        .next_arg()
        .unwrap_or_else(|| default_action.to_string());
    let mut rule_id = args.next_arg().unwrap_or_default();
    // Slash options arrive as (name, action); prefix as (action, name).
    if crate::types::RULE_IDS.contains(&action.as_str()) {
        std::mem::swap(&mut action, &mut rule_id);
    }
    (action, rule_id)
}

fn set_rule_dry_run(rule_id: &str, rule: &mut crate::types::RuleDef, on: bool) -> Result<()> {
    if rule_id == "attachment_flood" {
        let mode = rule.params.get("mode").and_then(|value| value.as_str());
        if !matches!(mode, Some("exact_image_test" | "exact_image")) {
            return Err(anyhow!("convert the legacy image rule in the WebUI first"));
        }
    }
    if !matches!(rule_id, "attachment_flood" | "repeat_text") || rule.dry_run == on {
        rule.dry_run = on;
        return Ok(());
    }
    if rule_id == "attachment_flood" {
        rule.params["mode"] = if on {
            "exact_image_test"
        } else {
            "exact_image"
        }
        .into();
    }
    let actions = if on {
        &["log"][..]
    } else {
        &["delete", "ban"][..]
    };
    rule.actions = actions
        .iter()
        .map(|action| crate::types::ActionDef {
            at: 1,
            action: (*action).into(),
            duration_minutes: 0,
            delete_days: 0,
            reason: "spam match".into(),
        })
        .collect();
    rule.dry_run = on;
    Ok(())
}

async fn run_logchannel(env: &CmdEnv<'_>, default_action: &str) -> Result<BehaviorOutcome> {
    let mut args = env.args.clone();
    let action = args
        .next_arg()
        .unwrap_or_else(|| default_action.to_string());
    let vars = std::collections::BTreeMap::new();
    let guild = env.guild_id.get() as i64;
    match action.as_str() {
        "list" => {
            let lc = db::list_log_channels(&env.app.db, guild).await?;
            if lc.is_empty() {
                return Ok(BehaviorOutcome {
                    response: Some(
                        "No log channels configured. Use `!logchannel set <event> <#channel>`."
                            .into(),
                    ),
                    vars,
                    suppress_response: false,
                });
            }
            let response = lc
                .iter()
                .map(|(e, c)| format!("`{e}` -> <#{c}>"))
                .collect::<Vec<_>>()
                .join("\n");
            Ok(BehaviorOutcome {
                response: Some(format!("**Log channels:**\n{response}")),
                vars,
                suppress_response: false,
            })
        }
        "set" => {
            let event = args.next_arg().ok_or_else(|| anyhow!("missing event"))?;
            if !crate::types::is_log_event(&event) {
                return Err(anyhow!("unknown log event `{event}`"));
            }
            let token = args.next_arg().ok_or_else(|| anyhow!("missing channel"))?;
            let channel = parse_channel_mention(&token)
                .or_else(|| token.parse::<u64>().ok().map(Into::into))
                .ok_or_else(|| anyhow!("invalid channel `{token}`"))?;
            crate::discord::validate_log_channel_guild(&env.ctx.http, env.guild_id.get(), channel)
                .await?;
            db::upsert_log_channel(&env.app.db, guild, &event, channel.get() as i64).await?;
            env.app.invalidate_configs();
            Ok(BehaviorOutcome {
                response: Some(format!("`{event}` now logs to <#{channel}>.")),
                vars,
                suppress_response: false,
            })
        }
        "unset" => {
            let event = args.next_arg().ok_or_else(|| anyhow!("missing event"))?;
            db::delete_log_channel(&env.app.db, guild, &event).await?;
            env.app.invalidate_configs();
            Ok(BehaviorOutcome {
                response: Some(format!("`{event}` logging cleared for this server.")),
                vars,
                suppress_response: false,
            })
        }
        other => Err(anyhow!("unknown logchannel action `{other}`")),
    }
}

// ---------------------------------------------------------------------------
// slash option specs
// ---------------------------------------------------------------------------

pub struct OptionSpec {
    pub name: &'static str,
    pub kind: CommandOptionType,
    pub required: bool,
    pub description: &'static str,
}

fn opt(
    name: &'static str,
    kind: CommandOptionType,
    required: bool,
    description: &'static str,
) -> OptionSpec {
    OptionSpec {
        name,
        kind,
        required,
        description,
    }
}

pub fn slash_spec(behavior: &Behavior) -> Vec<OptionSpec> {
    use CommandOptionType::{Channel, Integer, String as Str, User};
    match behavior {
        Behavior::Ban { .. } => vec![
            opt("user", User, true, "User to ban"),
            opt("reason", Str, false, "Ban reason"),
        ],
        Behavior::Unban { .. } => vec![
            opt("user", User, true, "User to unban"),
            opt("reason", Str, false, "Unban reason"),
        ],
        Behavior::Kick { .. } => vec![
            opt("user", User, true, "User to kick"),
            opt("reason", Str, false, "Kick reason"),
        ],
        Behavior::Timeout { .. } => vec![
            opt("user", User, true, "User to timeout"),
            opt(
                "minutes",
                Integer,
                false,
                "Timeout length in minutes (0 = remove)",
            ),
            opt("reason", Str, false, "Timeout reason"),
        ],
        Behavior::Warn { .. } => vec![
            opt("user", User, true, "User to warn"),
            opt("reason", Str, true, "Warning reason"),
        ],
        Behavior::Warns | Behavior::WarnClear => vec![opt("user", User, true, "User")],
        Behavior::Purge { .. } => {
            vec![opt("count", Integer, false, "Number of messages (max 100)")]
        }
        Behavior::Say => vec![opt("text", Str, true, "Text to post")],
        Behavior::Slowmode { .. } => vec![opt("seconds", Integer, true, "Slowmode seconds")],
        // Optional so omitting the option clears (same as prefix `!setname` with no args).
        Behavior::Setname => vec![opt(
            "name",
            Str,
            false,
            "Bot nickname (omit, clear, or - to reset)",
        )],
        Behavior::SetPrefix => vec![opt("prefix", Str, true, "New prefix")],
        Behavior::Template { .. } => vec![
            opt("action", Str, true, "list | get | set | reset"),
            opt("event", Str, false, "Event type"),
            opt("text", Str, false, "Template text"),
        ],
        Behavior::Rule { .. } => vec![
            opt("name", Str, true, "Rule name"),
            opt("action", Str, false, "toggle | dryrun"),
            opt("value", Str, false, "on | off (dryrun)"),
        ],
        Behavior::Logchannel { .. } => vec![
            opt("action", Str, true, "list | set | unset"),
            opt("event", Str, false, "Event type"),
            opt("channel", Channel, false, "Target channel"),
        ],
        // Only advertise a target when a step can consume it. Otherwise a
        // reply/log-only plan has no user parameter at all.
        Behavior::Plan { steps } if crate::commands::plans::plan_accepts_target(steps) => {
            vec![opt("user", User, false, "Target user")]
        }
        Behavior::Plan { .. } => vec![],
        Behavior::Testregex => vec![
            opt("pattern", Str, true, "Regex pattern"),
            opt("sample", Str, true, "Sample text"),
        ],
        Behavior::Help => vec![opt("command", Str, false, "Command name")],
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stats_release_read_failure_is_unavailable() {
        let path =
            std::env::temp_dir().join(format!("stats_release_{:032x}.db", rand::random::<u128>()));
        let db = db::Db::open(&path, 2).unwrap();
        // An empty successful read still means no known release observations.
        db.with_conn(|conn| {
            conn.execute_batch_bounded("DELETE FROM component_updates", 1024 * 1024)?;
            conn.execute_batch_bounded("INSERT INTO warns (guild_id, user_id, moderator_id, reason, created_at) VALUES (10, 42, 99, 'test', '2026-10-05T00:00:00Z')", 1024 * 1024)?;
            Ok(())
        }).await.unwrap();
        let app =
            crate::state::BotState::new(crate::config::BotProcessConfig::default(), db.clone())
                .unwrap();
        let healthy = stats_response(&app, 10, 20).await.unwrap();
        assert!(healthy.contains("**⚠️ Warns:** `1`"), "{healthy}");
        assert!(healthy.contains("**Bot CPU:** `unavailable`"), "{healthy}");
        assert!(
            healthy.contains("**API memory (real program use):** `unavailable`"),
            "{healthy}"
        );
        for component in ["bot", "api", "webui"] {
            assert!(
                healthy.contains(&format!("**{component}:** ❓")),
                "{healthy}"
            );
        }
        db.with_conn(|conn| {
            conn.execute_batch_bounded("DROP TABLE component_updates", 1024 * 1024)?;
            Ok(())
        })
        .await
        .unwrap();
        let unavailable = stats_response(&app, 10, 20).await.unwrap();
        assert!(
            unavailable.contains("**Release status**\nunavailable"),
            "{unavailable}"
        );
        assert!(!unavailable.contains('❓'), "{unavailable}");
        assert!(unavailable.contains("**⚠️ Warns:** `1`"), "{unavailable}");
        assert!(
            unavailable.contains("**⏱️ Uptime:** `20s`"),
            "{unavailable}"
        );
        assert!(
            unavailable.contains("**Bot CPU:** `unavailable`"),
            "{unavailable}"
        );
        assert!(
            unavailable.contains("**Bot memory (real program use):** `unavailable`"),
            "{unavailable}"
        );
        assert!(
            unavailable.contains("**API CPU:** `unavailable`"),
            "{unavailable}"
        );
        assert!(!unavailable.contains("0.0%"), "{unavailable}");
        drop(app);
        let _ = std::fs::remove_file(path);
    }

    // API-read blocking vs the async runtime is covered by
    // `stats_hanging_api_read_returns_within_fetch_timeout`.
    #[tokio::test]
    async fn stats_api_fetch_does_not_block_concurrent_async_work() {
        let path =
            std::env::temp_dir().join(format!("stats_parallel_{:032x}.db", rand::random::<u128>()));
        let db = db::Db::open(&path, 2).unwrap();
        let mut cfg = crate::config::BotProcessConfig::default();
        cfg.bot.read_target = "127.0.0.1:1".into();
        cfg.bot.control_secret = jmodguard::config::ControlSecret::new("x".repeat(32));
        let app = crate::state::BotState::new(cfg, db.clone()).unwrap();
        let progress = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let progress_flag = std::sync::Arc::clone(&progress);
        let watcher = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            progress_flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            stats_response(&app, 10, 5),
        )
        .await;
        watcher.await.unwrap();
        assert!(progress.load(std::sync::atomic::Ordering::SeqCst));
        drop(app);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn stats_invalid_read_target_still_returns_api_rows_unavailable() {
        let path = std::env::temp_dir().join(format!(
            "stats_bad_read_target_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = db::Db::open(&path, 2).unwrap();
        let mut cfg = crate::config::BotProcessConfig::default();
        cfg.bot.read_target = "0.0.0.0:8083".into();
        cfg.bot.control_secret = jmodguard::config::ControlSecret::new("x".repeat(32));
        let app = crate::state::BotState::new(cfg, db.clone()).unwrap();
        let text = stats_response(&app, 10, 5).await.unwrap();
        assert!(text.contains("**API CPU:** `unavailable`"), "{text}");
        assert!(
            text.contains("**API memory (real program use):** `unavailable`"),
            "{text}"
        );
        drop(app);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn stats_hanging_api_read_returns_within_fetch_timeout() {
        use std::io::Read;
        use std::net::TcpListener;
        use std::time::Instant;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut buf = [0u8; 256];
                while stream.read(&mut buf).unwrap_or(0) > 0 {}
            }
        });

        let path = std::env::temp_dir().join(format!(
            "stats_hanging_api_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = db::Db::open(&path, 2).unwrap();
        let mut cfg = crate::config::BotProcessConfig::default();
        cfg.bot.read_target = addr.to_string();
        cfg.bot.control_secret = jmodguard::config::ControlSecret::new("x".repeat(32));
        let app = crate::state::BotState::new(cfg, db.clone()).unwrap();
        let started = Instant::now();
        let text = stats_response(&app, 10, 5).await.unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed
                < jmodguard::read_link::STATS_FETCH_TIMEOUT + std::time::Duration::from_millis(150),
            "stats waited {elapsed:?} for a hung API read"
        );
        assert!(text.contains("**API CPU:** `unavailable`"), "{text}");
        assert!(
            text.contains("**API memory (real program use):** `unavailable`"),
            "{text}"
        );
        drop(app);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn status_does_not_wait_for_a_cpu_sample() {
        let started = std::time::Instant::now();
        let text = crate::templates::stats_with_container_use(
            "stats".into(),
            &jmodguard::cgroup::latest_container_sample(),
            None,
            None,
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(200));
        assert!(text.contains("**Bot CPU:** `unavailable`"), "{text}");
        assert!(
            text.contains("**Bot memory (real program use):** `unavailable`"),
            "{text}"
        );
        assert!(text.contains("**API CPU:** `unavailable`"), "{text}");
        assert!(
            text.contains("**API memory (real program use):** `unavailable`"),
            "{text}"
        );
        assert!(!text.contains("0.0%"), "{text}");
        assert!(!text.contains("0 B"), "{text}");
    }

    #[test]
    fn rule_command_keeps_the_rule_id_for_prefix_and_slash_order() {
        let mut prefix = Args::new("dryrun attachment_flood off");
        assert_eq!(
            rule_action_and_id(&mut prefix, "list"),
            ("dryrun".into(), "attachment_flood".into())
        );
        assert_eq!(prefix.next_arg().as_deref(), Some("off"));

        let command = crate::defaults::default_commands()
            .into_iter()
            .find(|command| matches!(command.behavior, Behavior::Rule { .. }))
            .unwrap();
        let options = [
            ("name".into(), "repeat_text".into()),
            ("action".into(), "dryrun".into()),
            ("value".into(), "off".into()),
        ];
        let mut slash = Args::from_parts(super::super::options_to_args(&command, &options));
        assert_eq!(
            rule_action_and_id(&mut slash, "list"),
            ("dryrun".into(), "repeat_text".into())
        );
        assert_eq!(slash.next_arg().as_deref(), Some("off"));
    }

    #[test]
    fn dedicated_rules_switch_between_dry_run_and_enforcement() {
        for (id, index) in [("attachment_flood", 1), ("repeat_text", 2)] {
            let mut rule = crate::defaults::default_rules().remove(index);
            set_rule_dry_run(id, &mut rule, false).unwrap();
            jmodguard::image_spam::validate_rule(id, &rule).unwrap();
            assert!(!rule.dry_run);
            assert_eq!(
                rule.actions
                    .iter()
                    .map(|a| a.action.as_str())
                    .collect::<Vec<_>>(),
                vec!["delete", "ban"]
            );
            set_rule_dry_run(id, &mut rule, true).unwrap();
            jmodguard::image_spam::validate_rule(id, &rule).unwrap();
            assert!(rule.dry_run);
            assert_eq!(
                rule.actions
                    .iter()
                    .map(|a| a.action.as_str())
                    .collect::<Vec<_>>(),
                vec!["log"]
            );
        }
    }

    #[test]
    fn legacy_image_rule_requires_explicit_conversion() {
        let mut rule = crate::defaults::default_rules().remove(1);
        rule.params = serde_json::json!({"window_secs": 60, "min_channels": 3});
        assert!(set_rule_dry_run("attachment_flood", &mut rule, false).is_err());
        assert!(set_rule_dry_run("attachment_flood", &mut rule, true).is_err());
        assert!(rule.params.get("mode").is_none());
    }
    use crate::types::Behavior;

    #[test]
    fn setname_slash_name_option_is_optional() {
        let specs = slash_spec(&Behavior::Setname);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "name");
        assert!(
            !specs[0].required,
            "optional name lets /setname clear like prefix !setname with no args"
        );
    }

    #[test]
    fn target_capable_plan_slash_user_option_is_optional() {
        let specs = slash_spec(&Behavior::Plan {
            steps: vec![crate::types::PlanStep::AddRole { role_id: 1 }],
        });
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "user");
        assert_eq!(specs[0].kind, CommandOptionType::User);
        assert!(!specs[0].required);
    }

    #[test]
    fn reply_only_plan_has_no_user_option() {
        let specs = slash_spec(&Behavior::Plan {
            steps: vec![crate::types::PlanStep::Reply {
                template: "hello".into(),
            }],
        });
        assert!(specs.is_empty());
    }

    #[test]
    fn say_without_mention_targets_invocation_channel() {
        let (target, text) =
            say_target_and_text(&Args::new("hello world"), ChannelId::new(7)).unwrap();
        assert_eq!(target, ChannelId::new(7));
        assert_eq!(text, "hello world");
    }

    #[test]
    fn say_with_mention_redirects_and_strips_channel() {
        let (target, text) =
            say_target_and_text(&Args::new("<#42> hello there"), ChannelId::new(7)).unwrap();
        assert_eq!(target, ChannelId::new(42));
        assert_eq!(text, "hello there");
    }

    #[test]
    fn say_with_mention_of_invocation_channel_stays_same_channel() {
        let (target, text) = say_target_and_text(&Args::new("<#7> hi"), ChannelId::new(7)).unwrap();
        assert_eq!(target, ChannelId::new(7));
        assert_eq!(text, "hi");
    }

    #[test]
    fn say_without_text_errors() {
        assert!(say_target_and_text(&Args::new(""), ChannelId::new(7)).is_err());
        assert!(say_target_and_text(&Args::new("<#42>"), ChannelId::new(7)).is_err());
        assert!(say_target_and_text(&Args::new("<#42>   "), ChannelId::new(7)).is_err());
    }

    #[test]
    fn say_text_is_trimmed() {
        let (_, text) =
            say_target_and_text(&Args::new("<#42>   padded  "), ChannelId::new(7)).unwrap();
        assert_eq!(text, "padded");
    }
}
