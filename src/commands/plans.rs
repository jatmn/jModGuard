//! Action-plan executor for custom commands.
//!
//! Steps run sequentially; each failing step is traced and execution
//! continues. All steps are audited via the `plan_executed` log event.

use anyhow::{Result, anyhow};
use serenity::model::id::{RoleId, UserId};

use super::{BehaviorOutcome, CmdEnv};
use crate::types::PlanStep;
use crate::{db, logging};

pub async fn run(env: &CmdEnv<'_>, steps: &[PlanStep]) -> Result<BehaviorOutcome> {
    let mut vars = super::behaviors::behaviors_base_vars(env);
    let target = resolve_target(env).await?;
    if plan_requires_target(steps) && target.is_none() {
        return Err(anyhow!(
            "this action plan requires a target user for timeout, kick, or ban steps"
        ));
    }
    if let Some(target) = target {
        vars.insert("target_id".into(), target.to_string());
        vars.insert(
            "target".into(),
            env.ctx
                .cache
                .user(target)
                .map(|user| user.name.clone())
                .unwrap_or_else(|| target.to_string()),
        );
    }
    let mut responses: Vec<String> = Vec::new();

    for (idx, step) in steps.iter().enumerate() {
        let res = run_step(env, step, target, &mut vars, &mut responses).await;
        if let Err(e) = res {
            tracing::warn!("plan step {idx} of `{}` failed: {e:#}", env.cmd.name);
        }
    }

    let content = if responses.is_empty() {
        String::new()
    } else {
        responses.join("\n")
    };
    vars.insert("content".into(), content.clone());

    // audit the plan run
    let _ = db::insert_event(
        &env.app.db,
        env.guild_id.get() as i64,
        "plan_executed",
        Some(env.actor.get() as i64),
        Some(env.actor.get() as i64),
        &serde_json::json!({"command": env.cmd.name, "steps": steps.len()}),
    )
    .await;
    logging::emit(
        env.app,
        env.ctx,
        env.guild_id,
        "plan_executed",
        vars.clone(),
    )
    .await;

    Ok(BehaviorOutcome {
        response: if content.is_empty() {
            None
        } else {
            Some(content)
        },
        vars,
    })
}

fn plan_requires_target(steps: &[PlanStep]) -> bool {
    steps.iter().any(|step| {
        matches!(
            step,
            PlanStep::Timeout { .. } | PlanStep::Kick | PlanStep::Ban { .. }
        )
    })
}

async fn resolve_target(env: &CmdEnv<'_>) -> Result<Option<UserId>> {
    let Some(token) = env.args.clone().next_arg() else {
        return Ok(None);
    };
    if token.is_empty() {
        return Ok(None);
    }
    super::resolve_user(env.ctx, env.guild_id, &token)
        .await
        .map(Some)
        .ok_or_else(|| anyhow!("could not resolve user `{token}`"))
}

fn target_member(env: &CmdEnv<'_>, target: UserId) -> Option<serenity::model::guild::Member> {
    env.ctx
        .cache
        .guild(env.guild_id)
        .and_then(|guild| guild.members.get(&target).cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_moderation_steps_require_a_target() {
        assert!(!plan_requires_target(&[
            PlanStep::Reply {
                template: "hello".into(),
            },
            PlanStep::AddRole { role_id: 1 },
        ]));
        assert!(plan_requires_target(&[PlanStep::Timeout { minutes: 5 }]));
        assert!(plan_requires_target(&[PlanStep::Kick]));
        assert!(plan_requires_target(&[PlanStep::Ban { delete_days: 0 }]));
    }
}

async fn run_step(
    env: &CmdEnv<'_>,
    step: &PlanStep,
    target: Option<UserId>,
    vars: &mut std::collections::BTreeMap<String, String>,
    responses: &mut Vec<String>,
) -> Result<()> {
    let guild_id = env.guild_id;
    match step {
        PlanStep::Reply { template } => {
            let text = crate::templates::render(template, vars);
            responses.push(text);
        }
        PlanStep::Dm { template } => {
            let text = crate::templates::render(template, vars);
            let user_id = target.unwrap_or(env.actor);
            let channel = env
                .ctx
                .http
                .create_private_channel(&serde_json::json!({"recipient_id": user_id.get()}))
                .await?;
            let _ = channel.say(&env.ctx.http, &text).await;
        }
        PlanStep::Log { event, template } => {
            let text = crate::templates::render(template, vars);
            let mut v = vars.clone();
            v.insert("content".into(), text);
            logging::emit(env.app, env.ctx, guild_id, event, v).await;
        }
        PlanStep::AddRole { role_id } => {
            let member = target_member(env, target.unwrap_or(env.actor));
            if let Some(m) = member {
                let _ = m.add_role(&env.ctx.http, RoleId::new(*role_id)).await;
            }
        }
        PlanStep::RemoveRole { role_id } => {
            let member = target_member(env, target.unwrap_or(env.actor));
            if let Some(m) = member {
                let _ = m.remove_role(&env.ctx.http, RoleId::new(*role_id)).await;
            }
        }
        PlanStep::Timeout { minutes } => {
            let member = target_member(
                env,
                target.expect("target-required plan steps are validated before execution"),
            );
            if let Some(mut m) = member {
                let until = serenity::model::Timestamp::from_unix_timestamp(
                    chrono::Utc::now().timestamp() + (*minutes as i64) * 60,
                )
                .ok();
                if let Some(until) = until {
                    let _ = m
                        .disable_communication_until_datetime(&env.ctx.http, until)
                        .await;
                }
            }
        }
        PlanStep::Kick => {
            if let Some(member) = target_member(
                env,
                target.expect("target-required plan steps are validated before execution"),
            ) {
                let _ = member
                    .kick_with_reason(&env.ctx.http, "custom command")
                    .await;
            }
        }
        PlanStep::Ban { delete_days } => {
            let target = target.expect("target-required plan steps are validated before execution");
            let _ = env
                .guild_id
                .ban_with_reason(&env.ctx.http, target, *delete_days, "custom command")
                .await;
            let _ = db::record_ban(
                &env.app.db,
                guild_id.get() as i64,
                target.get() as i64,
                Some(env.actor.get() as i64),
                "custom command",
            )
            .await;
        }
        PlanStep::DeleteCommandMessage => {
            if let Some(id) = env.source_message {
                let _ = env.channel_id.delete_message(&env.ctx.http, id).await;
            }
        }
        PlanStep::React { emoji } => {
            if let Some(id) = env.source_message {
                let _ = env
                    .ctx
                    .http
                    .create_reaction(
                        env.channel_id,
                        id,
                        &serenity::model::channel::ReactionType::Unicode(emoji.clone()),
                    )
                    .await;
            }
        }
        PlanStep::Wait { secs } => {
            tokio::time::sleep(std::time::Duration::from_secs(*secs)).await;
        }
    }
    Ok(())
}
