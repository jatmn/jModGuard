//! Action-plan executor for custom commands.
//!
//! Steps run sequentially; each failing step is traced and execution
//! continues. Plans report through the `plan_executed` Discord event; plans with
//! moderation steps also retain their existing durable execution summary.

use anyhow::{Result, anyhow};
use serenity::model::id::{RoleId, UserId};

use super::{BehaviorOutcome, CmdEnv, PermLevel, refuse_fenced_discord_mutation};
use crate::types::PlanStep;
use crate::{db, logging};

pub async fn run(env: &CmdEnv<'_>, steps: &[PlanStep]) -> Result<BehaviorOutcome> {
    let enrollment = crate::cases::capture(env.app, env.guild_id.get()).await;
    let mut vars = super::behaviors::behaviors_base_vars(env);
    // A plan's first argument is a target only when one of its steps can act
    // on another user. Reply/log-only plans must remain free to accept normal
    // command text without trying to parse it as a Discord user.
    let target = if plan_accepts_target(steps) {
        resolve_target(env).await?
    } else {
        None
    };
    if plan_requires_target(steps) && target.is_none() {
        return Err(anyhow!(
            "this action plan requires a target user for timeout, kick, or ban steps"
        ));
    }
    // Public plans are self-service by design. Supplying a target turns a
    // state-changing plan into a moderation operation, so never let an
    // `everyone` command grant roles, DM, or moderate another member.
    if target.is_some() && PermLevel::parse(&env.cmd.permission) < PermLevel::Mod {
        return Err(anyhow!(
            "targeted action plans require the command permission to be mod or higher"
        ));
    }
    if let Some(target) = target {
        vars.insert("target_id".into(), target.to_string());
        if let Some(member) = env
            .ctx
            .cache
            .guild(env.guild_id)
            .and_then(|guild| guild.members.get(&target).cloned())
        {
            remember_target(&mut vars, &member);
        } else if let Some(user) = env.ctx.cache.user(target) {
            vars.insert("target".into(), user.name.clone());
            vars.insert(
                "target_name".into(),
                user.global_name
                    .clone()
                    .unwrap_or_else(|| user.name.clone()),
            );
            if let Some(global_name) = &user.global_name {
                vars.insert("target_global_name".into(), global_name.clone());
            }
        } else {
            vars.insert("target".into(), target.to_string());
        }
    }
    let mut responses: Vec<String> = Vec::new();

    for (idx, step) in steps.iter().enumerate() {
        let res = run_step(
            env,
            enrollment.clone(),
            step,
            target,
            &mut vars,
            &mut responses,
        )
        .await;
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

    let _ = record_moderation_plan(
        &env.app.db,
        enrollment,
        env.actor.get() as i64,
        &env.cmd.name,
        steps,
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
        suppress_response: false,
    })
}

async fn record_moderation_plan(
    db: &db::Db,
    enrollment: Option<db::GuildEnrollment>,
    actor: i64,
    command: &str,
    steps: &[PlanStep],
) -> Result<()> {
    if plan_requires_target(steps) {
        // Preserve the existing summary, not a claim that every step succeeded.
        // Selected moderation steps record their own outcomes separately.
        db::insert_enrolled_event(
            db,
            enrollment,
            "plan_executed",
            Some(actor),
            Some(actor),
            &serde_json::json!({"command": command, "steps": steps.len(), "moderation": true}),
        )
        .await?;
    }
    Ok(())
}

fn plan_requires_target(steps: &[PlanStep]) -> bool {
    steps.iter().any(|step| {
        matches!(
            step,
            PlanStep::Timeout { .. } | PlanStep::Kick | PlanStep::Ban { .. }
        )
    })
}

pub(crate) fn plan_accepts_target(steps: &[PlanStep]) -> bool {
    steps.iter().any(|step| {
        matches!(
            step,
            PlanStep::Dm { .. }
                | PlanStep::AddRole { .. }
                | PlanStep::RemoveRole { .. }
                | PlanStep::Timeout { .. }
                | PlanStep::Kick
                | PlanStep::Ban { .. }
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

async fn target_member(env: &CmdEnv<'_>, target: UserId) -> Result<serenity::model::guild::Member> {
    if let Some(member) = env
        .ctx
        .cache
        .guild(env.guild_id)
        .and_then(|guild| guild.members.get(&target).cloned())
    {
        return Ok(member);
    }
    env.guild_id
        .member(&env.ctx.http, target)
        .await
        .map_err(|e| anyhow!("target is not available in this server: {e}"))
}

fn remember_target(
    vars: &mut std::collections::BTreeMap<String, String>,
    member: &serenity::model::guild::Member,
) {
    vars.insert("target_id".into(), member.user.id.to_string());
    vars.insert("target".into(), member.user.name.clone());
    vars.insert("target_name".into(), member.display_name().to_string());
    if let Some(global_name) = &member.user.global_name {
        vars.insert("target_global_name".into(), global_name.clone());
    }
}

async fn run_step(
    env: &CmdEnv<'_>,
    enrollment: Option<db::GuildEnrollment>,
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
            refuse_fenced_discord_mutation(env).await?;
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
            let member = target_member(env, target.unwrap_or(env.actor)).await?;
            if target.is_some() {
                remember_target(vars, &member);
            }
            refuse_fenced_discord_mutation(env).await?;
            member
                .add_role(&env.ctx.http, RoleId::new(*role_id))
                .await?;
        }
        PlanStep::RemoveRole { role_id } => {
            let member = target_member(env, target.unwrap_or(env.actor)).await?;
            if target.is_some() {
                remember_target(vars, &member);
            }
            refuse_fenced_discord_mutation(env).await?;
            member
                .remove_role(&env.ctx.http, RoleId::new(*role_id))
                .await?;
        }
        PlanStep::Timeout { minutes } => {
            let target = target.expect("target-required plan steps are validated before execution");
            let mut member = target_member(env, target).await?;
            remember_target(vars, &member);
            let until = serenity::model::Timestamp::from_unix_timestamp(
                chrono::Utc::now().timestamp() + (*minutes as i64) * 60,
            )
            .map_err(|e| anyhow!("invalid timeout: {e}"))?;
            refuse_fenced_discord_mutation(env).await?;
            let result = member
                .disable_communication_until_datetime(&env.ctx.http, until)
                .await;
            record_step_outcome(
                env.app,
                env.actor,
                enrollment.clone(),
                crate::db::guild_data::CaseAction::Timeout,
                target,
                &result,
            )
            .await;
            result?;
        }
        PlanStep::Kick => {
            let target = target.expect("target-required plan steps are validated before execution");
            let member = target_member(env, target).await?;
            remember_target(vars, &member);
            refuse_fenced_discord_mutation(env).await?;
            let result = member
                .kick_with_reason(&env.ctx.http, "custom command")
                .await;
            record_step_outcome(
                env.app,
                env.actor,
                enrollment.clone(),
                crate::db::guild_data::CaseAction::Kick,
                target,
                &result,
            )
            .await;
            result?;
        }
        PlanStep::Ban { delete_days } => {
            let target = target.expect("target-required plan steps are validated before execution");
            refuse_fenced_discord_mutation(env).await?;
            let suppression = env.app.suppress_next_gateway_case(
                crate::state::GatewayCaseAction::Ban,
                env.guild_id.get(),
                target.get(),
            );
            let result = env
                .guild_id
                .ban_with_reason(&env.ctx.http, target, *delete_days, "custom command")
                .await;
            if matches!(
                crate::cases::classify_discord(result.as_ref().err()),
                crate::db::guild_data::CaseOutcome::Failed
            ) {
                env.app.release_gateway_case_suppression(suppression);
            }
            record_step_outcome(
                env.app,
                env.actor,
                enrollment.clone(),
                crate::db::guild_data::CaseAction::Ban,
                target,
                &result,
            )
            .await;
            result?;
            db::record_enrolled_ban(
                &env.app.db,
                enrollment.clone(),
                target.get() as i64,
                Some(env.actor.get() as i64),
                "custom command",
            )
            .await?;
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

async fn record_step_outcome(
    app: &crate::state::BotState,
    actor: UserId,
    enrollment: Option<db::GuildEnrollment>,
    action: crate::db::guild_data::CaseAction,
    target: UserId,
    result: &serenity::Result<()>,
) {
    let outcome = crate::cases::classify_discord(result.as_ref().err());
    if matches!(
        action,
        crate::db::guild_data::CaseAction::Ban | crate::db::guild_data::CaseAction::Kick
    ) {
        crate::cases::record_application(
            app,
            enrollment,
            action,
            outcome,
            target.get(),
            Some(actor.get()),
            Some("custom command".into()),
            crate::db::guild_data::CaseEvidence::Unavailable,
        )
        .await;
        return;
    }
    crate::cases::record(
        app,
        enrollment,
        action,
        outcome,
        target.get(),
        Some(actor.get()),
        Some("custom command".into()),
        crate::db::guild_data::CaseEvidence::Unavailable,
        crate::cases::application_source(),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn selected_plan_results_keep_actor_outcome_and_original_membership() {
        use crate::db::guild_data::{CaseAction, CaseOutcome, CaseSource};
        let path =
            std::env::temp_dir().join(format!("plan_cases_{:032x}.db", rand::random::<u128>()));
        let app = crate::state::BotState::new(Default::default(), db::Db::open(&path, 2).unwrap())
            .unwrap();
        let old = db::enroll_guild(&app.db, 10, 100).await.unwrap().unwrap();
        app.tenant.activate(old.clone()).await.unwrap();
        for action in [CaseAction::Timeout, CaseAction::Kick, CaseAction::Ban] {
            for success in [true, false] {
                let result = if success {
                    Ok(())
                } else {
                    Err(serenity::Error::Other("interrupted response"))
                };
                record_step_outcome(
                    &app,
                    UserId::new(42),
                    Some(old.clone()),
                    action.clone(),
                    UserId::new(99),
                    &result,
                )
                .await;
            }
        }
        let (store, scope) = app.tenant.live(old.clone()).await.unwrap();
        let ids = store
            .case_ids(scope.clone(), None, 10, crate::tenant::case_limits())
            .await
            .unwrap();
        assert_eq!(ids.len(), 6);
        let mut succeeded = 0;
        let mut uncertain = 0;
        for id in ids {
            let data = store
                .case(scope.clone(), id, crate::tenant::case_limits())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(data.actor_id, Some(42));
            assert_eq!(data.subject_id, 99);
            assert_eq!(data.reason.as_deref(), Some("custom command"));
            assert!(matches!(data.source, CaseSource::Application { .. }));
            match data.outcome {
                CaseOutcome::Succeeded => succeeded += 1,
                CaseOutcome::Uncertain => uncertain += 1,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!((succeeded, uncertain), (3, 3));
        db::retire_guild_enrollment(&app.db, 10, 100).await.unwrap();
        app.tenant.drive_erasure().await.unwrap();
        let current = db::enroll_guild(&app.db, 10, 101).await.unwrap().unwrap();
        app.tenant.activate(current.clone()).await.unwrap();
        record_step_outcome(
            &app,
            UserId::new(42),
            Some(old),
            CaseAction::Ban,
            UserId::new(99),
            &Ok(()),
        )
        .await;
        let (store, scope) = app.tenant.live(current).await.unwrap();
        assert_eq!(
            store
                .case_count(scope, crate::tenant::case_limits())
                .await
                .unwrap(),
            0
        );
        drop(store);
        drop(app);
        crate::tenant::remove_sidecars(&path);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn routine_plans_leave_no_sql_summary_but_moderation_plans_keep_theirs() {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_plan_audit_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = db::Db::open(&path, 2).unwrap();
        let baseline = db::get_config_version(&db).await.unwrap();
        for steps in [
            vec![PlanStep::Reply {
                template: "hello".into(),
            }],
            vec![PlanStep::Log {
                event: "system".into(),
                template: "hello".into(),
            }],
        ] {
            record_moderation_plan(
                &db,
                db::enroll_guild(&db, 10, 100).await.unwrap(),
                20,
                "routine",
                &steps,
            )
            .await
            .unwrap();
        }
        assert!(
            db::query_events(&db, 10, None, None, 10)
                .await
                .unwrap()
                .is_empty()
        );
        for step in [
            PlanStep::Timeout { minutes: 5 },
            PlanStep::Kick,
            PlanStep::Ban { delete_days: 0 },
        ] {
            record_moderation_plan(
                &db,
                db::enroll_guild(&db, 10, 100).await.unwrap(),
                20,
                "moderate",
                &[step],
            )
            .await
            .unwrap();
        }
        let rows = db::query_events(&db, 10, Some("plan_executed"), None, 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 3);
        for row in rows {
            assert_eq!(row.actor_id, Some(20));
            let data: serde_json::Value =
                serde_json::from_str(row.data.as_deref().unwrap()).unwrap();
            assert_eq!(
                data,
                serde_json::json!({"command":"moderate", "steps":1, "moderation":true})
            );
        }
        assert_eq!(db::guild_stats(&db, 10).await.unwrap().events, 3);
        assert!(
            db::query_events(&db, 11, None, None, 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(db::get_config_version(&db).await.unwrap(), baseline);
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

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

    #[test]
    fn only_target_capable_steps_consume_the_first_argument() {
        assert!(!plan_accepts_target(&[PlanStep::Reply {
            template: "hello".into(),
        }]));
        assert!(plan_accepts_target(&[PlanStep::AddRole { role_id: 1 }]));
        assert!(plan_accepts_target(&[PlanStep::Timeout { minutes: 5 }]));
    }
}
