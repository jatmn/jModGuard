//! Owner server directory facts: capture at membership time plus a bounded
//! startup backfill. Display data only — these rows never authorize access.
//! The adder identity comes from one action-filtered audit-log request, so a
//! missing VIEW_AUDIT_LOG permission leaves the adder unknown without retries
//! beyond the next startup pass.
use crate::db;
use crate::state::BotState;
use serenity::model::guild::audit_log::{Action, AuditLogs, MemberAction};
use serenity::model::id::{GuildId, UserId};
use std::sync::Arc;
use std::time::Duration;

/// Bounded per pass: the busiest bot platform cannot exceed this many
/// unresolved servers per startup, and failures simply wait for a later pass.
const BACKFILL_PASS_LIMIT: i64 = 100;
/// One paced audit request per guild keeps the pass far below Discord's
/// per-route and global rate limits.
const BACKFILL_GUILD_PACE: Duration = Duration::from_secs(2);
/// An adder entry must belong to the current membership: Discord indexes new
/// audit entries asynchronously, so a lookup right after a quick rejoin can
/// still see the previous add. Entries older than this tolerance below the
/// membership's join time are ignored, and the row waits for a later pass.
const ADDER_ENTRY_TOLERANCE_NS: i64 = 5 * 60 * 1_000_000_000;

pub(crate) fn now_ns() -> i64 {
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis().max(1))
}

/// Newest BOT_ADD entry for this bot user that belongs to the membership
/// that joined at `joined_at_ns`, as (adder id, adder display name). Entries
/// arrive most-recent first; the first match for the bot's own identity that
/// is not from an earlier membership is the latest addition of this bot.
pub(crate) fn bot_adder_from_logs(
    logs: &AuditLogs,
    bot_user_id: u64,
    joined_at_ns: i64,
) -> Option<(String, String)> {
    let oldest_allowed = joined_at_ns.saturating_sub(ADDER_ENTRY_TOLERANCE_NS);
    let entry = logs.entries.iter().find(|entry| {
        matches!(entry.action, Action::Member(MemberAction::BotAdd))
            && entry
                .target_id
                .is_some_and(|target| target.get() == bot_user_id)
            && entry
                .id
                .created_at()
                .timestamp_nanos_opt()
                .is_some_and(|created_at_ns| created_at_ns >= oldest_allowed)
    })?;
    let added_by = entry.user_id.get().to_string();
    let added_by_name = logs
        .users
        .get(&entry.user_id)
        .map(|user| {
            user.global_name
                .clone()
                .unwrap_or_else(|| user.name.clone())
        })
        .unwrap_or_else(|| added_by.clone());
    Some((added_by, added_by_name))
}

/// Single bounded audit lookup: Discord filters server-side, so one request
/// returns the most recent BOT_ADD entries for this guild.
pub(crate) async fn capture_bot_adder(
    http: &serenity::http::Http,
    guild_id: i64,
    bot_user_id: u64,
    joined_at_ns: i64,
) -> anyhow::Result<Option<(String, String)>> {
    anyhow::ensure!(guild_id > 0 && joined_at_ns > 0, "invalid guild id");
    let logs = http
        .get_audit_logs(
            GuildId::new(guild_id as u64),
            Some(Action::Member(MemberAction::BotAdd)),
            None,
            None,
            Some(50),
        )
        .await?;
    Ok(bot_adder_from_logs(&logs, bot_user_id, joined_at_ns))
}

/// Record the display facts the gateway already carries for this guild.
/// Adder capture is a separate bounded lookup; a `None` adder never erases
/// a stored one.
pub(crate) async fn record_guild_observation(
    app: &Arc<BotState>,
    guild_id: u64,
    name: &str,
    icon_hash: Option<&str>,
    member_count: u64,
    recorded_at_ns: i64,
) {
    let Ok(guild) = i64::try_from(guild_id) else {
        tracing::warn!(guild_id, "guild directory observation has an invalid id");
        return;
    };
    let member_count = i64::try_from(member_count).ok();
    if let Err(error) = db::record_guild_join_info(
        &app.db,
        guild,
        name,
        icon_hash,
        member_count,
        None,
        None,
        recorded_at_ns,
    )
    .await
    {
        tracing::warn!(guild, "guild directory observation not recorded: {error:#}");
    }
}

/// Capture the adder for a fresh join and store it with the guild facts.
/// The observation row is written first; a missing row waits for the startup
/// pass instead of guessing a name here.
pub(crate) async fn capture_fresh_join_adder(
    app: &Arc<BotState>,
    http: &serenity::http::Http,
    guild_id: u64,
    bot_user_id: u64,
    joined_at_ns: i64,
) {
    let Ok(guild) = i64::try_from(guild_id) else {
        return;
    };
    match capture_bot_adder(http, guild, bot_user_id, joined_at_ns).await {
        Ok(Some((added_by, added_by_name))) => {
            if let Err(error) =
                db::record_guild_adder(&app.db, guild, &added_by, &added_by_name, now_ns()).await
            {
                tracing::warn!(guild, "guild adder observation not recorded: {error:#}");
            }
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(guild, "guild adder lookup deferred: {error:#}");
        }
    }
}

/// One bounded startup pass: resolve servers still missing their adder (or
/// their whole row) with one paced REST lookup per guild. Failures are logged
/// and left for a later pass; shutdown ends the pass cleanly.
pub async fn run_startup_backfill(
    app: Arc<BotState>,
    http: Arc<serenity::http::Http>,
    bot_user_id: UserId,
) {
    let candidates =
        match db::guild_join_info_backfill_candidates(&app.db, BACKFILL_PASS_LIMIT).await {
            Ok(candidates) => candidates,
            Err(error) => {
                tracing::warn!("guild directory backfill unavailable: {error:#}");
                return;
            }
        };
    for (index, (guild_id, joined_at_ns)) in candidates.iter().enumerate() {
        if index > 0 {
            tokio::select! {
                _ = crate::shutdown_signal() => return,
                _ = tokio::time::sleep(BACKFILL_GUILD_PACE) => {}
            }
        }
        backfill_guild(&app, &http, bot_user_id, *guild_id, *joined_at_ns).await;
    }
}

async fn backfill_guild(
    app: &Arc<BotState>,
    http: &serenity::http::Http,
    bot_user_id: UserId,
    guild_id: i64,
    joined_at_ns: i64,
) {
    let guild = match http
        .get_guild_with_counts(GuildId::new(guild_id as u64))
        .await
    {
        Ok(guild) => guild,
        Err(error) => {
            tracing::warn!(guild_id, "guild directory lookup deferred: {error:#}");
            return;
        }
    };
    let adder = match capture_bot_adder(http, guild_id, bot_user_id.get(), joined_at_ns).await {
        Ok(adder) => adder,
        Err(error) => {
            tracing::warn!(guild_id, "guild adder lookup deferred: {error:#}");
            None
        }
    };
    let (added_by, added_by_name) = adder
        .map(|(id, name)| (Some(id), Some(name)))
        .unwrap_or((None, None));
    let icon_hash = guild.icon.as_ref().map(|hash| hash.to_string());
    if let Err(error) = db::record_guild_join_info(
        &app.db,
        guild_id,
        &guild.name,
        icon_hash.as_deref(),
        guild
            .approximate_member_count
            .and_then(|count| i64::try_from(count).ok()),
        added_by.as_deref(),
        added_by_name.as_deref(),
        now_ns(),
    )
    .await
    {
        tracing::warn!(guild_id, "guild directory facts not recorded: {error:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bot_add_fixture(entries: serde_json::Value, users: serde_json::Value) -> AuditLogs {
        serde_json::from_value(serde_json::json!({
            "audit_log_entries": entries,
            "users": users,
            "application_commands": [], "auto_moderation_rules": [],
            "guild_scheduled_events": [], "integrations": [], "threads": [],
            "webhooks": []
        }))
        .unwrap()
    }

    const BOT_ADD: u8 = MemberAction::BotAdd as u8;
    const SNOWFLAKE_EPOCH_MS: i64 = 1_420_070_400_000;

    fn entry_id(created_ms: i64) -> String {
        ((created_ms - SNOWFLAKE_EPOCH_MS) << 22).to_string()
    }

    #[test]
    fn newest_bot_add_entry_for_this_bot_wins() {
        let newest_ms = 1_790_000_000_000;
        let joined_at_ns = newest_ms * 1_000_000;
        let logs = bot_add_fixture(
            serde_json::json!([
                // Discord returns entries most-recent first. Another bot's
                // addition is newest here but must never match.
                {"id":entry_id(newest_ms + 60_000),"target_id":"999","user_id":"333","action_type":BOT_ADD},
                // Newest add of this bot: added by admin 111 with a global name.
                {"id":entry_id(newest_ms),"target_id":"42","user_id":"111","action_type":BOT_ADD},
                // An older add by a different admin, from an earlier membership.
                {"id":entry_id(newest_ms - 3_600_000),"target_id":"42","user_id":"222","action_type":BOT_ADD}
            ]),
            serde_json::json!([
                {"id":"111","username":"adminuser","global_name":"Admin","discriminator":"0","avatar":null},
                {"id":"222","username":"older","discriminator":"0","avatar":null},
                {"id":"333","username":"other","discriminator":"0","avatar":null}
            ]),
        );
        let (added_by, added_by_name) = bot_adder_from_logs(&logs, 42, joined_at_ns).unwrap();
        assert_eq!(added_by, "111");
        assert_eq!(added_by_name, "Admin");
    }

    #[test]
    fn an_add_from_an_earlier_membership_is_ignored() {
        let joined_ms = 1_790_000_000_000;
        let joined_at_ns = joined_ms * 1_000_000;
        // The only BOT_ADD entry for this bot predates the current membership
        // by two hours: a quick rejoin whose new entry Discord has not indexed
        // yet. Recording its admin would name the wrong person.
        let logs = bot_add_fixture(
            serde_json::json!([
                {"id":entry_id(joined_ms - 7_200_000),"target_id":"42","user_id":"222","action_type":BOT_ADD}
            ]),
            serde_json::json!([
                {"id":"222","username":"older","discriminator":"0","avatar":null}
            ]),
        );
        assert!(bot_adder_from_logs(&logs, 42, joined_at_ns).is_none());
        // An entry inside the tolerance window stays acceptable (clock skew):
        // the same entry against a membership joined four minutes later.
        let (added_by, _) =
            bot_adder_from_logs(&logs, 42, (joined_ms - 7_200_000 + 240_000) * 1_000_000).unwrap();
        assert_eq!(added_by, "222");
    }

    #[test]
    fn foreign_bot_add_and_wrong_action_never_match() {
        let now_ms = 1_790_000_000_000;
        let logs = bot_add_fixture(
            serde_json::json!([
                {"id":entry_id(now_ms),"target_id":"999","user_id":"222","action_type":BOT_ADD},
                {"id":entry_id(now_ms - 1_000),"target_id":"42","user_id":"222","action_type":20}
            ]),
            serde_json::json!([
                {"id":"222","username":"someone","discriminator":"0","avatar":null}
            ]),
        );
        assert!(bot_adder_from_logs(&logs, 42, now_ms * 1_000_000).is_none());
    }

    #[test]
    fn missing_user_profile_keeps_the_adder_identity() {
        let now_ms = 1_790_000_000_000;
        let logs = bot_add_fixture(
            serde_json::json!([
                {"id":entry_id(now_ms),"target_id":"7","user_id":"123456789012345678","action_type":BOT_ADD}
            ]),
            serde_json::json!([]),
        );
        let (added_by, added_by_name) = bot_adder_from_logs(&logs, 7, now_ms * 1_000_000).unwrap();
        assert_eq!(added_by, "123456789012345678");
        assert_eq!(added_by_name, "123456789012345678");
    }
}
