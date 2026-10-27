//! Optional moderator reason DM before kick or ban.

use anyhow::{Result, anyhow};
use serenity::model::id::UserId;
use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;

use super::CmdEnv;
use crate::types::Behavior;

pub(crate) const MOD_REASON_DM_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModReasonDmOutcome {
    Sent,
    NotSent,
    Cancelled,
}

pub(crate) fn take_prefix_dm_flag(parts: &mut Vec<String>) -> bool {
    let mut dm = false;
    parts.retain(|part| {
        if part == "--dm" {
            dm = true;
            false
        } else {
            true
        }
    });
    dm
}

pub(crate) fn slash_dm_flag(options: &[(String, String)]) -> bool {
    options
        .iter()
        .any(|(name, value)| name == "dm" && value == "true")
}

pub(crate) fn slash_option_excluded_from_args(name: &str) -> bool {
    name == "dm"
}

pub(crate) fn command_supports_reason_dm(behavior: &Behavior) -> bool {
    matches!(behavior, Behavior::Kick { .. } | Behavior::Ban { .. })
}

pub(crate) async fn bounded_mod_reason_dm_future<F>(future: F) -> ModReasonDmOutcome
where
    F: Future<Output = ModReasonDmOutcome>,
{
    match tokio::time::timeout(MOD_REASON_DM_TIMEOUT, future).await {
        Ok(outcome) => outcome,
        Err(_) => ModReasonDmOutcome::NotSent,
    }
}

pub(crate) fn mod_reason_dm_vars(
    guild: &str,
    action: &str,
    reason: &str,
    actor: &str,
) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    vars.insert("guild".into(), guild.into());
    vars.insert("action".into(), action.into());
    vars.insert("reason".into(), reason.into());
    vars.insert("actor".into(), actor.into());
    vars
}

pub(crate) fn render_mod_reason_dm_body(vars: &BTreeMap<String, String>) -> String {
    jmodguard::templates::render_bounded(jmodguard::defaults::MOD_REASON_DM_TEMPLATE, vars, 2000)
}

pub(crate) fn render_mod_reason_dm_clause(outcome: ModReasonDmOutcome) -> String {
    let status = match outcome {
        ModReasonDmOutcome::Sent => jmodguard::defaults::MOD_REASON_DM_STATUS_SENT,
        ModReasonDmOutcome::NotSent => jmodguard::defaults::MOD_REASON_DM_STATUS_NOT_SENT,
        ModReasonDmOutcome::Cancelled => return String::new(),
    };
    let mut vars = BTreeMap::new();
    vars.insert("dm_status".into(), status.into());
    jmodguard::templates::render(jmodguard::defaults::MOD_REASON_DM_CLAUSE_TEMPLATE, &vars)
}

pub(crate) fn dm_outcome_allows_moderation(outcome: ModReasonDmOutcome) -> Result<()> {
    if outcome == ModReasonDmOutcome::Cancelled {
        return Err(anyhow!("writer lease is not held"));
    }
    Ok(())
}

pub(crate) async fn send_mod_reason_dm(
    env: &CmdEnv<'_>,
    target: UserId,
    action: &str,
    reason: &str,
) -> ModReasonDmOutcome {
    bounded_mod_reason_dm_future(send_mod_reason_dm_inner(env, target, action, reason)).await
}

async fn send_mod_reason_dm_inner(
    env: &CmdEnv<'_>,
    target: UserId,
    action: &str,
    reason: &str,
) -> ModReasonDmOutcome {
    let guild = env
        .ctx
        .cache
        .guild(env.guild_id)
        .map(|g| g.name.clone())
        .unwrap_or_else(|| env.guild_id.to_string());
    let actor = format!("{} (<@{}>)", env.actor_member.display_name(), env.actor);
    let vars = mod_reason_dm_vars(&guild, action, reason, &actor);
    let text = render_mod_reason_dm_body(&vars);

    let channel = match crate::discord::race_discord_call(
        &env.app.activation,
        env.ctx
            .http
            .create_private_channel(&serde_json::json!({"recipient_id": target.get()})),
    )
    .await
    {
        crate::discord::DiscordCall::Cancelled => return ModReasonDmOutcome::Cancelled,
        crate::discord::DiscordCall::Finished(Err(_)) => return ModReasonDmOutcome::NotSent,
        crate::discord::DiscordCall::Finished(Ok(channel)) => channel,
    };

    match crate::discord::race_discord_call(&env.app.activation, channel.say(&env.ctx.http, &text))
        .await
    {
        crate::discord::DiscordCall::Cancelled => ModReasonDmOutcome::Cancelled,
        crate::discord::DiscordCall::Finished(Ok(_)) => ModReasonDmOutcome::Sent,
        crate::discord::DiscordCall::Finished(Err(_)) => ModReasonDmOutcome::NotSent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::Args;

    #[test]
    fn prefix_dm_flag_strips_whole_argument_only() {
        let mut parts = vec!["123".into(), "--dm".into(), "because".into()];
        assert!(take_prefix_dm_flag(&mut parts));
        assert_eq!(parts, vec!["123", "because"]);

        let mut reason_dm = vec!["123".into(), "dm".into()];
        assert!(!take_prefix_dm_flag(&mut reason_dm));
        assert_eq!(reason_dm, vec!["123", "dm"]);

        let mut reason_true = vec!["123".into(), "true".into()];
        assert!(!take_prefix_dm_flag(&mut reason_true));

        let mut embedded = vec!["123".into(), "foo--dm".into(), "bar".into()];
        assert!(!take_prefix_dm_flag(&mut embedded));
        assert_eq!(embedded, vec!["123", "foo--dm", "bar"]);
    }

    #[test]
    fn args_strip_dm_flag_leaves_reason_words() {
        let mut args = Args::new("123 dm");
        assert!(!args.strip_dm_flag());
        assert_eq!(args.next_arg().as_deref(), Some("123"));
        assert_eq!(args.rest(), "dm");

        let mut flagged = Args::new("123 --dm spam");
        assert!(flagged.strip_dm_flag());
        assert_eq!(flagged.next_arg().as_deref(), Some("123"));
        assert_eq!(flagged.rest(), "spam");
    }

    #[test]
    fn slash_dm_flag_is_true_only_for_named_true() {
        assert!(slash_dm_flag(&[("dm".into(), "true".into())]));
        assert!(!slash_dm_flag(&[("dm".into(), "false".into())]));
        assert!(!slash_dm_flag(&[]));
        assert!(!slash_dm_flag(&[("reason".into(), "true".into())]));
    }

    #[test]
    fn without_tag_strips_attempt_marker_before_dm_render() {
        let vars = mod_reason_dm_vars(
            "Guild",
            "ban",
            jmodguard::ban_reason::without_tag("[jmg:0123456789abcdef0123] secret"),
            "Mod",
        );
        let body = render_mod_reason_dm_body(&vars);
        assert!(!body.contains("[jmg:"));
        assert!(body.contains("secret"));
    }

    fn try_moderation(dm: ModReasonDmOutcome, ban_calls: &mut u32) -> Result<()> {
        dm_outcome_allows_moderation(dm)?;
        *ban_calls += 1;
        Ok(())
    }

    #[test]
    fn dm_outcome_decision_table() {
        let mut ban_calls = 0u32;
        assert!(try_moderation(ModReasonDmOutcome::Cancelled, &mut ban_calls).is_err());
        assert_eq!(ban_calls, 0);

        for outcome in [ModReasonDmOutcome::NotSent, ModReasonDmOutcome::Sent] {
            ban_calls = 0;
            assert!(try_moderation(outcome, &mut ban_calls).is_ok());
            assert_eq!(ban_calls, 1);
            ban_calls = 0;
            assert!(try_moderation(outcome, &mut ban_calls).is_ok());
            assert_eq!(ban_calls, 1, "second command is separate, not a retry");
        }
    }

    #[test]
    fn dm_clause_renders_sent_and_not_sent() {
        assert!(
            render_mod_reason_dm_clause(ModReasonDmOutcome::Sent)
                .contains(jmodguard::defaults::MOD_REASON_DM_STATUS_SENT,)
        );
        assert!(
            render_mod_reason_dm_clause(ModReasonDmOutcome::NotSent)
                .contains(jmodguard::defaults::MOD_REASON_DM_STATUS_NOT_SENT,)
        );
        assert!(render_mod_reason_dm_clause(ModReasonDmOutcome::Cancelled).is_empty());
    }

    #[test]
    fn reason_dm_flag_applies_only_to_kick_and_ban() {
        let kick = crate::defaults::default_commands()
            .into_iter()
            .find(|cmd| cmd.name == "kick")
            .unwrap();
        let warn = crate::defaults::default_commands()
            .into_iter()
            .find(|cmd| cmd.name == "warn")
            .unwrap();
        assert!(command_supports_reason_dm(&kick.behavior));
        assert!(!command_supports_reason_dm(&warn.behavior));

        let mut warn_args = Args::new("42 --dm rule broken");
        let dm = if command_supports_reason_dm(&warn.behavior) {
            warn_args.strip_dm_flag()
        } else {
            false
        };
        assert!(!dm);
        assert_eq!(warn_args.next_arg().as_deref(), Some("42"));
        assert_eq!(warn_args.rest(), "--dm rule broken");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn slow_dm_future_times_out_as_not_sent() {
        let outcome = bounded_mod_reason_dm_future(async {
            tokio::time::sleep(MOD_REASON_DM_TIMEOUT + Duration::from_millis(250)).await;
            ModReasonDmOutcome::Sent
        })
        .await;
        assert_eq!(outcome, ModReasonDmOutcome::NotSent);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn fast_dm_future_keeps_outcome() {
        let outcome = bounded_mod_reason_dm_future(async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            ModReasonDmOutcome::Sent
        })
        .await;
        assert_eq!(outcome, ModReasonDmOutcome::Sent);
        tokio::time::advance(MOD_REASON_DM_TIMEOUT).await;
    }
}
