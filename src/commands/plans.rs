//! Action-plan executor for custom commands.
//!
//! Steps run sequentially; each failing step is traced and execution
//! continues. All steps are audited via the `plan_executed` log event.

use anyhow::Result;
use serenity::model::id::RoleId;

use super::{BehaviorOutcome, CmdEnv};
use crate::types::PlanStep;
use crate::{db, logging};

pub async fn run(env: &CmdEnv<'_>, steps: &[PlanStep]) -> Result<BehaviorOutcome> {
    let mut vars = super::behaviors::behaviors_base_vars(env);
    let mut responses: Vec<String> = Vec::new();

    for (idx, step) in steps.iter().enumerate() {
        let res = run_step(env, step, &mut vars, &mut responses).await;
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

async fn run_step(
    env: &CmdEnv<'_>,
    step: &PlanStep,
    vars: &mut std::collections::BTreeMap<String, String>,
    responses: &mut Vec<String>,
) -> Result<()> {
    let guild_id = env.guild_id;
    let target_opt = vars.get("target").cloned();
    match step {
        PlanStep::Reply { template } => {
            let text = crate::templates::render(template, vars);
            responses.push(text);
        }
        PlanStep::Dm { template } => {
            let text = crate::templates::render(template, vars);
            let user_id: u64 = target_opt
                .as_deref()
                .and_then(|t| t.parse().ok())
                .unwrap_or(env.actor.get());
            let channel = env
                .ctx
                .http
                .create_private_channel(&serde_json::json!({"recipient_id": user_id}))
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
            let member = env
                .ctx
                .cache
                .guild(guild_id)
                .and_then(|g| g.members.get(&env.actor).cloned());
            if let Some(m) = member {
                let _ = m.add_role(&env.ctx.http, RoleId::new(*role_id)).await;
            }
        }
        PlanStep::RemoveRole { role_id } => {
            let member = env
                .ctx
                .cache
                .guild(guild_id)
                .and_then(|g| g.members.get(&env.actor).cloned());
            if let Some(m) = member {
                let _ = m.remove_role(&env.ctx.http, RoleId::new(*role_id)).await;
            }
        }
        PlanStep::Timeout { minutes } => {
            let member = env
                .ctx
                .cache
                .guild(guild_id)
                .and_then(|g| g.members.get(&env.actor).cloned());
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
            if let Some(member) = env
                .ctx
                .cache
                .guild(guild_id)
                .and_then(|g| g.members.get(&env.actor).cloned())
            {
                let _ = member
                    .kick_with_reason(&env.ctx.http, "custom command")
                    .await;
            }
        }
        PlanStep::Ban { delete_days } => {
            let _ = env
                .guild_id
                .ban_with_reason(&env.ctx.http, env.actor, *delete_days, "custom command")
                .await;
            let _ = db::record_ban(
                &env.app.db,
                guild_id.get() as i64,
                env.actor.get() as i64,
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
