//! Owner server directory facts: capture at membership time plus a paced
//! backfill that covers every membership observed at startup. Display data only — these rows never authorize access.
//! The adder identity comes from one action-filtered audit-log request, so a
//! missing VIEW_AUDIT_LOG permission leaves the adder unknown without retries
//! beyond the next startup pass. An adder is bound to the membership (the
//! enrollment join time) it was captured for.
use crate::db;
use crate::state::BotState;
use serenity::model::guild::audit_log::{Action, AuditLogs, MemberAction};
use serenity::model::id::{GuildId, UserId};
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

/// Unresolved servers are read in pages this large. The startup pass visits
/// each unresolved server once, in id order, so servers whose lookup keeps
/// failing cannot starve later ones; failures wait for the next startup.
const BACKFILL_PAGE_SIZE: i64 = 100;
/// One paced lookup per guild keeps the pass far below Discord's per-route and
/// global rate limits, however many servers are unresolved.
const BACKFILL_GUILD_PACE: Duration = Duration::from_secs(2);
/// An adder entry must belong to the current membership. Discord stamps the
/// add entry and the bot's join within moments of each other, so only a small
/// allowance below the membership's join time absorbs that skew. A lookup right
/// after a quick rejoin can still see the previous add, which is far older than
/// this allowance unless the earlier membership itself lasted only seconds;
/// older entries are ignored and the row waits for a later pass.
const ADDER_ENTRY_TOLERANCE_NS: i64 = 10 * 1_000_000_000;

pub(crate) fn now_ns() -> i64 {
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis().max(1))
}

/// Newest BOT_ADD entry for this bot user that belongs to the membership
/// that joined at `joined_at_ns`, as (adder id, adder display name). Entries
/// arrive most-recent first; the first match for the bot's own identity that
/// is not from an earlier membership is the latest addition of this bot.
/// An entry more than `ADDER_ENTRY_TOLERANCE_NS` before the join belongs to an
/// earlier membership.
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
/// Adder capture is a separate bounded lookup bound to the membership.
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
                db::record_guild_adder(&app.db, guild, joined_at_ns, &added_by, &added_by_name)
                    .await
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

/// Coordinates the paced backfill. Startup memberships are enrolled by
/// independent Gateway handlers, so the Ready-time scan can finish before (or
/// pass the id of) an enrollment that commits later. Each such enrollment asks
/// for another scan instead: a request while a pass runs makes that pass scan
/// again, and a request while idle starts a new pass.
#[derive(Default)]
pub struct Backfill {
    state: parking_lot::Mutex<BackfillState>,
}

#[derive(Default)]
struct BackfillState {
    running: bool,
    rescan: bool,
    /// (guild, membership join time) pairs this process already looked up. A
    /// failed or empty lookup waits for the next startup rather than repeating.
    attempted: HashSet<(i64, i64)>,
}

impl Backfill {
    /// True when the caller must run a pass; otherwise the pass already running
    /// is told to scan again before it ends.
    fn begin(&self) -> bool {
        let mut state = self.state.lock();
        if state.running {
            state.rescan = true;
            false
        } else {
            state.running = true;
            state.rescan = false;
            true
        }
    }

    /// True when another scan was requested while this one ran; otherwise the
    /// pass is over and a later request starts a new one.
    fn finish_pass(&self) -> bool {
        let mut state = self.state.lock();
        if std::mem::take(&mut state.rescan) {
            true
        } else {
            state.running = false;
            false
        }
    }

    fn stop(&self) {
        self.state.lock().running = false;
    }

    fn first_attempt(&self, guild_id: i64, joined_at_ns: i64) -> bool {
        self.state.lock().attempted.insert((guild_id, joined_at_ns))
    }
}

/// Resolve each missing adder, refreshing missing or earlier-membership facts
/// as needed. One visit per guild, paced. Called at Ready and after each startup
/// enrollment (a membership recorded without a fresh-join capture).
pub(crate) fn request_backfill(
    app: &Arc<BotState>,
    http: &Arc<serenity::http::Http>,
    bot_user_id: UserId,
) {
    if !app.directory_backfill.begin() {
        return;
    }
    let app = Arc::clone(app);
    let http = Arc::clone(http);
    tokio::spawn(async move {
        let resolver_db = app.db.clone();
        run_backfill(
            &app.db,
            &app.directory_backfill,
            BACKFILL_GUILD_PACE,
            move |candidate| {
                let db = resolver_db.clone();
                let http = Arc::clone(&http);
                async move { backfill_guild(&db, &http, bot_user_id, candidate).await }
            },
        )
        .await;
    });
}

/// Paced passes over the unresolved servers in id order, repeated while a
/// request arrived mid-pass. Failures are logged and left for the next
/// startup; shutdown ends the run cleanly.
async fn run_backfill<F, Fut>(db: &db::Db, backfill: &Backfill, pace: Duration, mut resolve: F)
where
    F: FnMut(db::GuildJoinInfoBackfillCandidate) -> Fut,
    Fut: Future<Output = ()>,
{
    let mut visited = false;
    loop {
        let mut after_guild_id = 0;
        loop {
            let candidates = match db::guild_join_info_backfill_candidates(
                db,
                after_guild_id,
                BACKFILL_PAGE_SIZE,
            )
            .await
            {
                Ok(candidates) => candidates,
                Err(error) => {
                    tracing::warn!("guild directory backfill unavailable: {error:#}");
                    break;
                }
            };
            let Some(last_guild_id) = candidates.last().map(|candidate| candidate.guild_id) else {
                break;
            };
            for candidate in candidates {
                if !backfill.first_attempt(candidate.guild_id, candidate.joined_at_ns) {
                    continue;
                }
                if visited {
                    tokio::select! {
                        _ = crate::shutdown_signal() => {
                            backfill.stop();
                            return;
                        }
                        _ = tokio::time::sleep(pace) => {}
                    }
                }
                visited = true;
                resolve(candidate).await;
            }
            after_guild_id = last_guild_id;
        }
        if !backfill.finish_pass() {
            return;
        }
    }
}

async fn backfill_guild(
    db: &db::Db,
    http: &serenity::http::Http,
    bot_user_id: UserId,
    db::GuildJoinInfoBackfillCandidate {
        guild_id,
        joined_at_ns,
        refresh_facts,
    }: db::GuildJoinInfoBackfillCandidate,
) {
    let refresh_at = if refresh_facts {
        // Stamp the refresh before checking current facts so a later Gateway
        // observation takes precedence even if it arrives before the REST call.
        let observed_at_ns = now_ns();
        match db::guild_join_info_backfill_refresh_needed(db, guild_id, joined_at_ns).await {
            Ok(Some(true)) => Some(observed_at_ns),
            Ok(Some(false)) => None,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(
                    guild_id,
                    "guild directory freshness check deferred: {error:#}"
                );
                return;
            }
        }
    } else {
        None
    };
    if let Some(observed_at_ns) = refresh_at {
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
        let icon_hash = guild.icon.as_ref().map(|hash| hash.to_string());
        if let Err(error) = db::record_guild_join_info(
            db,
            guild_id,
            &guild.name,
            icon_hash.as_deref(),
            guild
                .approximate_member_count
                .and_then(|count| i64::try_from(count).ok()),
            observed_at_ns,
        )
        .await
        {
            tracing::warn!(guild_id, "guild directory facts not recorded: {error:#}");
            return;
        }
    }
    match capture_bot_adder(http, guild_id, bot_user_id.get(), joined_at_ns).await {
        Ok(Some((added_by, added_by_name))) => {
            if let Err(error) =
                db::record_guild_adder(db, guild_id, joined_at_ns, &added_by, &added_by_name).await
            {
                tracing::warn!(guild_id, "guild adder observation not recorded: {error:#}");
            }
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(guild_id, "guild adder lookup deferred: {error:#}");
        }
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
        // A rejoin a minute after a short earlier membership still ignores the
        // earlier add: only moment-scale skew is tolerated.
        assert!(
            bot_adder_from_logs(&logs, 42, (joined_ms - 7_200_000 + 60_000) * 1_000_000).is_none()
        );
        // An entry inside the skew allowance stays acceptable: the same entry
        // against a membership that joined five seconds later.
        let (added_by, _) =
            bot_adder_from_logs(&logs, 42, (joined_ms - 7_200_000 + 5_000) * 1_000_000).unwrap();
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

    fn backfill_db() -> db::Db {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_directory_backfill_{:032x}.db",
            rand::random::<u128>()
        ));
        db::Db::open(&path, 2).unwrap()
    }

    /// Runs the backfill with a recording resolver; `during` runs inside the
    /// first lookup, standing in for startup handlers that commit meanwhile.
    async fn visits<D, Fut>(db: &db::Db, backfill: &Arc<Backfill>, during: D) -> Vec<(i64, i64)>
    where
        D: Fn() -> Fut,
        Fut: Future<Output = ()>,
    {
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        run_backfill(db, backfill, Duration::ZERO, |candidate| {
            let first = {
                let mut seen = recorded.lock();
                seen.push((candidate.guild_id, candidate.joined_at_ns));
                seen.len() == 1
            };
            let hook = if first { Some(during()) } else { None };
            async move {
                if let Some(hook) = hook {
                    hook.await;
                }
            }
        })
        .await;
        seen.lock().clone()
    }

    #[tokio::test]
    async fn enrollments_committed_during_a_pass_are_visited_in_the_same_process() {
        let db = backfill_db();
        let backfill = Arc::new(Backfill::default());
        db::enroll_guild(&db, 20, 100).await.unwrap();
        assert!(backfill.begin());
        let seen = visits(&db, &backfill, || {
            let db = db.clone();
            let backfill = Arc::clone(&backfill);
            async move {
                // Startup handlers enroll one id the cursor already passed and
                // one beyond it, each requesting another scan once committed.
                db::enroll_guild(&db, 10, 100).await.unwrap();
                assert!(!backfill.begin(), "a running pass is asked to scan again");
                db::enroll_guild(&db, 30, 100).await.unwrap();
                assert!(!backfill.begin());
            }
        })
        .await;
        assert_eq!(
            seen,
            vec![(20, 100), (30, 100), (10, 100)],
            "the id beyond the cursor joins this pass and the id behind it the \
             requested rescan; no lookup repeats"
        );
        assert!(backfill.begin(), "a finished run accepts the next request");
    }

    #[tokio::test]
    async fn an_enrollment_after_the_scan_exited_starts_a_new_pass_once() {
        let db = backfill_db();
        let backfill = Arc::new(Backfill::default());
        assert!(backfill.begin());
        // The Ready-time scan finds nothing and exits before any enrollment.
        assert!(visits(&db, &backfill, || async {}).await.is_empty());
        db::enroll_guild(&db, 10, 100).await.unwrap();
        assert!(
            backfill.begin(),
            "the exited pass is restarted by the request"
        );
        assert_eq!(visits(&db, &backfill, || async {}).await, vec![(10, 100)]);
        // The lookup left the server unresolved: this process does not repeat it.
        assert!(backfill.begin());
        assert!(visits(&db, &backfill, || async {}).await.is_empty());
    }

    #[tokio::test]
    async fn startup_backfill_refreshes_only_missing_or_earlier_membership_facts() {
        use db::SqlExecutor as _;
        use hyper::service::{make_service_fn, service_fn};
        use hyper::{Body, Response, Server};
        use serde_json::json;
        use std::convert::Infallible;

        let db = backfill_db();
        let joined_ms = 1_790_000_000_000;
        let joined = joined_ms * 1_000_000;
        for guild in [10, 20, 30, 40, 50] {
            db::enroll_guild(&db, guild, joined).await.unwrap();
        }
        for (guild, recorded) in [(10, joined), (30, joined - 1), (40, joined + 1)] {
            db::record_guild_join_info(&db, guild, "Gateway", None, Some(7), recorded)
                .await
                .unwrap();
        }
        db.with_conn(|conn| {
            conn.execute_batch_bounded(
                "CREATE TABLE backfill_fact_updates(guild_id INTEGER);
                 CREATE TRIGGER count_backfill_fact_update
                 AFTER UPDATE OF name,icon_hash,member_count,recorded_at_ns ON guild_join_info
                 BEGIN
                   INSERT INTO backfill_fact_updates VALUES(NEW.guild_id);
                 END;",
                4096,
            )
        })
        .await
        .unwrap();

        let calls = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let server = Server::bind(&([127, 0, 0, 1], 0).into());
        let http = serenity::http::HttpBuilder::new("fixture")
            .proxy(format!("http://{}", server.local_addr()))
            .ratelimiter_disabled(true)
            .build();
        let server = tokio::spawn(server.serve(make_service_fn({
            let calls = calls.clone();
            let db = db.clone();
            move |_| {
                let (calls, db) = (calls.clone(), db.clone());
                async move {
                    Ok::<_, Infallible>(service_fn(move |request: hyper::Request<Body>| {
                        let (calls, db) = (calls.clone(), db.clone());
                        async move {
                            let mut parts = request.uri().path().split("/guilds/").nth(1).unwrap().split('/');
                            let guild: i64 = parts.next().unwrap().parse().unwrap();
                            let audit = parts.next() == Some("audit-logs");
                            calls.lock().push((guild, audit));

                            if audit && guild == 10 {
                                // The paged scan has already queued this guild with missing facts.
                                db::record_guild_join_info(&db, 50, "Late Gateway", None, Some(5), joined + 1)
                                    .await
                                    .unwrap();
                            }

                            if audit && guild == 40 {
                                // Change membership while the earlier audit request is outstanding.
                                db::retire_guild_enrollment(&db, guild, joined).await.unwrap();
                                db::enroll_guild(&db, guild, joined + 2).await.unwrap();
                                db::record_guild_join_info(&db, guild, "Rejoined", None, Some(9), joined + 3)
                                    .await
                                    .unwrap();
                            }

                            let body = if audit {
                                json!({
                                    "audit_log_entries": [
                                        {"id":entry_id(joined_ms),"target_id":"42","user_id":"99","action_type":BOT_ADD}
                                    ],
                                    "users": [], "application_commands": [], "auto_moderation_rules": [],
                                    "guild_scheduled_events": [], "integrations": [], "threads": [], "webhooks": []
                                })
                            } else {
                                assert!(request.uri().query().unwrap_or_default().split('&')
                                    .any(|pair| pair == "with_counts=true"));
                                json!({
                                    "id":guild.to_string(), "name":"REST", "owner_id":"11", "verification_level":0,
                                    "default_message_notifications":0, "explicit_content_filter":0, "roles":[],
                                    "emojis":[], "features":[], "mfa_level":0, "system_channel_flags":0,
                                    "premium_tier":0, "preferred_locale":"en-US", "nsfw_level":0,
                                    "stickers":[], "premium_progress_bar_enabled":false, "approximate_member_count":8
                                })
                            };
                            Ok::<_, Infallible>(Response::new(Body::from(body.to_string())))
                        }
                    }))
                }
            }
        })));

        let backfill = Backfill::default();
        assert!(backfill.begin());
        run_backfill(&db, &backfill, Duration::ZERO, |candidate| {
            backfill_guild(&db, &http, UserId::new(42), candidate)
        })
        .await;
        server.abort();
        let _ = server.await;

        assert_eq!(
            *calls.lock(),
            vec![
                (10, true),
                (20, false),
                (20, true),
                (30, false),
                (30, true),
                (40, true),
                (50, true)
            ],
            "only missing and earlier-membership facts require guild-detail requests"
        );
        let fresh_writes = db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM backfill_fact_updates WHERE guild_id IN (10, 50)",
                    (),
                    |row| Ok(row.get::<i64>(0)?),
                )
            })
            .await
            .unwrap();
        assert_eq!(fresh_writes, 0);

        let rows = db::owner_guild_directory(&db, 10).await.unwrap();
        for (guild, name, count) in [
            (10, "Gateway", 7),
            (20, "REST", 8),
            (30, "REST", 8),
            (50, "Late Gateway", 5),
        ] {
            let row = rows.iter().find(|row| row.guild_id == guild).unwrap();
            assert_eq!(row.name.as_deref(), Some(name));
            assert_eq!(row.member_count, Some(count));
            assert_eq!(row.added_by.as_deref(), Some("99"));
        }
        let row = rows.iter().find(|row| row.guild_id == 40).unwrap();
        assert_eq!(row.joined_at_ns, joined + 2);
        assert_eq!(row.name.as_deref(), Some("Rejoined"));
        assert_eq!(row.member_count, Some(9));
        assert_eq!(
            row.added_by, None,
            "the earlier membership's adder is ignored"
        );
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
