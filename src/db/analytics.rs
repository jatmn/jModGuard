//! Durable analytics aggregates: daily moderation and membership series.
//!
//! The daily series derives from the selected moderation records (warns, bans
//! and counted events) plus the per-guild daily membership counters written by
//! the bot. The counters carry no user IDs; they are not member/user activity
//! collection. Days with no observation are reported as zero, and days before
//! a guild's first observation are simply absent from the durable record —
//! series values never reconstruct history that was not recorded.

use super::write_budget::BoundedWrites;
use super::{Db, SqlExecutor, params};
use crate::ids::ser_i64_str;
use anyhow::Result;
use chrono::Utc;
use serde::Serialize;
use std::collections::HashMap;

/// Largest daily window an analytics read may request.
pub const ANALYTICS_MAX_WINDOW_DAYS: i64 = 90;
/// Default daily window for dashboard reads.
pub const ANALYTICS_DEFAULT_WINDOW_DAYS: i64 = 30;
/// Daily counter rows are pruned beyond this many days by the bot's writes.
pub const ANALYTICS_MEMBER_DAILY_HORIZON_DAYS: i64 = 92;

/// One UTC day of per-guild analytics counts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AnalyticsPoint {
    /// UTC calendar day, `YYYY-MM-DD`.
    pub day: String,
    pub warns: i64,
    pub bans: i64,
    pub automod: i64,
    pub mod_actions: i64,
    pub timeouts: i64,
    pub joins: i64,
    pub leaves: i64,
}

/// Dashboard range choices. Every guild analytics panel follows one of these.
pub const ANALYTICS_RANGES: [i64; 3] = [7, 30, 90];
/// Largest number of rows in each ranked breakdown.
pub const ANALYTICS_TOP_LIMIT: i64 = 8;

/// Totals for the window of equal length just before the requested one.
/// Membership totals are `None` when that window reaches past the daily
/// counter horizon, so the comparison never treats pruned days as zero.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PreviousTotals {
    pub warns: i64,
    pub bans: i64,
    pub automod: i64,
    pub mod_actions: i64,
    pub timeouts: i64,
    pub joins: Option<i64>,
    pub leaves: Option<i64>,
}

/// One ranked breakdown row, e.g. an automod rule ID or a mod-action kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RankedCount {
    pub key: String,
    pub count: i64,
}

/// Recorded mod-action events by acting moderator. IDs stay decimal strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModeratorCount {
    #[serde(serialize_with = "ser_i64_str")]
    pub moderator_id: i64,
    pub count: i64,
}

/// Guild-scoped analytics for one range: the daily series plus every
/// dashboard panel, aggregated in SQL from stored records only.
#[derive(Debug, Clone, Serialize)]
pub struct GuildAnalytics {
    pub window_days: i64,
    pub series: Vec<AnalyticsPoint>,
    pub previous: PreviousTotals,
    /// Counted automod enforcement by rule ID, including dedicated spam bans.
    pub top_rules: Vec<RankedCount>,
    /// Recorded mod-action events by command kind (warn, ban, kick, ...).
    pub mod_action_kinds: Vec<RankedCount>,
    /// Recorded mod-action events by acting moderator.
    pub moderators: Vec<ModeratorCount>,
    /// Sparse `[weekday (0 = Sunday), UTC hour, count]` cells for counted
    /// automod enforcement, mod actions and dedicated spam bans.
    pub heatmap: Vec<[i64; 3]>,
}

/// Per-guild activity totals across one analytics window, with the previous
/// window of equal length. Guild IDs stay decimal strings on the wire. Name
/// and icon are the bot's last observed display facts, `None` when unknown.
#[derive(Debug, Clone, Serialize)]
pub struct GuildActivityTotals {
    #[serde(serialize_with = "ser_i64_str")]
    pub guild_id: i64,
    /// The bot currently has an active membership in this server.
    pub active: bool,
    pub name: Option<String>,
    pub icon_hash: Option<String>,
    pub warns: i64,
    pub bans: i64,
    pub automod: i64,
    pub mod_actions: i64,
    pub timeouts: i64,
    pub joins: i64,
    pub leaves: i64,
    pub previous: PreviousTotals,
}

impl GuildActivityTotals {
    fn zeroed(guild_id: i64) -> Self {
        Self {
            guild_id,
            active: false,
            name: None,
            icon_hash: None,
            warns: 0,
            bans: 0,
            automod: 0,
            mod_actions: 0,
            timeouts: 0,
            joins: 0,
            leaves: 0,
            previous: PreviousTotals {
                joins: Some(0),
                leaves: Some(0),
                ..PreviousTotals::default()
            },
        }
    }
}

/// Owner-scoped platform analytics. Series aggregate every guild; the per-guild
/// breakdown covers the same window. Rows include every active server, even
/// without activity, plus any other server with records in either window.
#[derive(Debug, Clone, Serialize)]
pub struct PlatformAnalytics {
    pub window_days: i64,
    pub series: Vec<AnalyticsPoint>,
    /// Fleet totals for the previous window of equal length.
    pub previous: PreviousTotals,
    /// Active bot enrollments (the durable server count).
    pub servers: i64,
    pub guilds: Vec<GuildActivityTotals>,
}

/// Matches the lifetime EVENT_STATS_SQL definition for automod records: only
/// recorded, non-dry-run enforcement actions with a real punishment action.
const EVENT_BUCKETS_COUNTED_SQL: &str = "(event_type IN ('mod_action','timeout_add') \
     OR (event_type='automod' AND json_valid(data) \
     AND json_extract(data,'$.action') IN ('warn','timeout','kick','ban') \
     AND COALESCE(json_extract(data,'$.dry_run'),0)=0))";

/// Exact-image and repeated-text enforcement never reaches the generic
/// executor, so it writes no `automod` event. Its only core record is the
/// unattributed ban mirror tagged with the dedicated rule ID; count that in the
/// spam bucket. Test mode records nothing, so it stays excluded.
const DEDICATED_SPAM_BANS_SQL: &str = "(moderator_id IS NULL \
     AND (instr(reason, '[automod:attachment_flood] ')=1 \
     OR instr(reason, '[automod:repeat_text] ')=1))";

fn clamp_window(window_days: i64) -> i64 {
    window_days.clamp(1, ANALYTICS_MAX_WINDOW_DAYS)
}

/// UTC calendar day (`YYYY-MM-DD`) for an epoch-millisecond observation.
fn analytics_day_of_ms(at_ms: i64) -> String {
    match chrono::DateTime::from_timestamp_millis(at_ms) {
        Some(at) => at.format("%Y-%m-%d").to_string(),
        // Out-of-range timestamps fall back to the current UTC day; the
        // counter is analytics only and must never fail a runtime write.
        None => Utc::now().format("%Y-%m-%d").to_string(),
    }
}

fn day_for_offset(window_days: i64, offset: i64) -> String {
    (Utc::now().date_naive() - chrono::Duration::days(window_days - 1 - offset))
        .format("%Y-%m-%d")
        .to_string()
}

/// The first day of a window ending today (UTC).
fn window_start_day(window_days: i64) -> String {
    day_for_offset(window_days, 0)
}

/// Zero-fills the window ending today, merging observed days from the map.
fn filled_series(map: HashMap<String, AnalyticsPoint>, window_days: i64) -> Vec<AnalyticsPoint> {
    (0..window_days)
        .map(|offset| {
            let day = day_for_offset(window_days, offset);
            let mut point = map.get(&day).cloned().unwrap_or_default();
            point.day = day;
            point
        })
        .collect()
}

fn apply_day(
    map: &mut HashMap<String, AnalyticsPoint>,
    day: String,
    apply: impl FnOnce(&mut AnalyticsPoint),
) {
    let point = map.entry(day).or_default();
    apply(point);
}

/// Daily-series aggregate columns. Each column is one JSON array so the whole
/// window reads in a single statement: a remote interactive snapshot cannot
/// afford a prepare/query round trip per source table.
fn series_columns(guild_filter: &str, day: &str) -> String {
    series_columns_with_events(
        guild_filter,
        day,
        &format!(
            "SELECT substr(created_at,1,10) AS d, event_type AS b, COUNT(*) AS n FROM events \
             WHERE {guild_filter}created_at>={day} AND {EVENT_BUCKETS_COUNTED_SQL} GROUP BY 1, 2"
        ),
    )
}

/// [`series_columns`] with the counted-event `(d, b, n)` rows supplied by the
/// caller, so a guild summary can reuse its already-parsed events.
fn series_columns_with_events(guild_filter: &str, day: &str, events: &str) -> String {
    format!(
        "(SELECT json_group_array(json_array(d, n)) FROM \
           (SELECT substr(created_at,1,10) AS d, COUNT(*) AS n FROM warns \
            WHERE {guild_filter}created_at>={day} GROUP BY 1)), \
         (SELECT json_group_array(json_array(d, n)) FROM \
           (SELECT substr(created_at,1,10) AS d, COUNT(*) AS n FROM bans \
            WHERE {guild_filter}created_at>={day} GROUP BY 1)), \
         (SELECT json_group_array(json_array(d, n)) FROM \
           (SELECT substr(created_at,1,10) AS d, COUNT(*) AS n FROM bans \
            WHERE {guild_filter}created_at>={day} AND {DEDICATED_SPAM_BANS_SQL} GROUP BY 1)), \
         (SELECT json_group_array(json_array(d, b, n)) FROM ({events})), \
         (SELECT json_group_array(json_array(day, j, l)) FROM \
           (SELECT day, SUM(joins) AS j, SUM(leaves) AS l FROM analytics_member_daily \
            WHERE {guild_filter}day>={day} GROUP BY day))"
    )
}

fn json_rows<T: serde::de::DeserializeOwned>(raw: Option<String>, what: &str) -> Result<Vec<T>> {
    match raw {
        Some(raw) => serde_json::from_str(&raw)
            .map_err(|error| anyhow::anyhow!("corrupt analytics {what} aggregate: {error}")),
        None => Ok(Vec::new()),
    }
}

/// Raw JSON aggregate columns, read before the row closes and parsed after.
type SeriesColumns = [Option<String>; 5];

fn series_from_columns(columns: SeriesColumns) -> Result<HashMap<String, AnalyticsPoint>> {
    let [warns, bans, spam_bans, events, members] = columns;
    let mut map: HashMap<String, AnalyticsPoint> = HashMap::new();
    for (day, n) in json_rows::<(String, i64)>(warns, "warn")? {
        apply_day(&mut map, day, move |point| point.warns += n);
    }
    for (day, n) in json_rows::<(String, i64)>(bans, "ban")? {
        apply_day(&mut map, day, move |point| point.bans += n);
    }
    // The automod bucket keeps the lifetime statistics definition (recorded,
    // non-dry-run enforcement) and also holds the dedicated spam bans.
    for (day, n) in json_rows::<(String, i64)>(spam_bans, "spam ban")? {
        apply_day(&mut map, day, move |point| point.automod += n);
    }
    for (day, bucket, n) in json_rows::<(String, String, i64)>(events, "event")? {
        apply_day(&mut map, day, move |point| match bucket.as_str() {
            "automod" => point.automod += n,
            "mod_action" => point.mod_actions += n,
            "timeout_add" => point.timeouts += n,
            _ => {}
        });
    }
    // Daily membership counters (no user IDs; rows exist only for observed days).
    for (day, joins, leaves) in json_rows::<(String, i64, i64)>(members, "membership")? {
        apply_day(&mut map, day, move |point| {
            point.joins += joins;
            point.leaves += leaves;
        });
    }
    Ok(map)
}

fn series_row(row: &crate::db_compat::Row) -> Result<SeriesColumns> {
    Ok([
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
    ])
}

/// Automod rule ID for a counted automod event, or the dedicated spam rule
/// tag of an unattributed spam ban.
const DEDICATED_SPAM_RULE_SQL: &str = "CASE WHEN instr(reason, '[automod:attachment_flood] ')=1 \
     THEN 'attachment_flood' ELSE 'repeat_text' END";

/// Counted events of both windows, parsed once. `?2` starts the requested
/// window and `?3` the previous window of equal length. Materialized so every
/// panel column below aggregates the same rows without rescanning `events` or
/// re-parsing JSON.
fn guild_events_cte() -> String {
    format!(
        "ev AS MATERIALIZED (SELECT event_type AS b, created_at>=?2 AS cur, actor_id AS actor, \
           CASE WHEN event_type IN ('automod','mod_action') AND json_valid(data) \
             THEN CAST(json_extract(data, CASE event_type WHEN 'automod' THEN '$.rule' \
               ELSE '$.action' END) AS TEXT) END AS k, \
           substr(created_at,1,10) AS d, CAST(substr(created_at,12,2) AS INTEGER) AS hr \
         FROM events WHERE guild_id=?1 AND created_at>=?3 AND {EVENT_BUCKETS_COUNTED_SQL})"
    )
}

/// Panel columns for one guild window; see [`guild_events_cte`] for `?2`/`?3`.
/// Each column is one scalar or JSON array so the whole summary stays one
/// statement.
fn guild_panel_columns() -> String {
    let prev = "guild_id=?1 AND created_at>=?3 AND created_at<?2";
    let cur = "guild_id=?1 AND created_at>=?2";
    format!(
        "(SELECT COUNT(*) FROM warns WHERE {prev}), \
         (SELECT COUNT(*) FROM bans WHERE {prev}), \
         (SELECT COUNT(*) FROM bans WHERE {prev} AND {DEDICATED_SPAM_BANS_SQL}), \
         (SELECT json_group_array(json_array(b, n)) FROM \
           (SELECT b, COUNT(*) AS n FROM ev WHERE NOT cur GROUP BY b)), \
         (SELECT COALESCE(SUM(joins), 0) FROM analytics_member_daily \
            WHERE guild_id=?1 AND day>=?3 AND day<?2), \
         (SELECT COALESCE(SUM(leaves), 0) FROM analytics_member_daily \
            WHERE guild_id=?1 AND day>=?3 AND day<?2), \
         (SELECT json_group_array(json_array(k, n)) FROM \
           (SELECT k, SUM(n) AS n FROM \
             (SELECT k, COUNT(*) AS n FROM ev WHERE cur AND b='automod' GROUP BY k \
              UNION ALL \
              SELECT {DEDICATED_SPAM_RULE_SQL}, COUNT(*) FROM bans \
              WHERE {cur} AND {DEDICATED_SPAM_BANS_SQL} GROUP BY 1) \
            WHERE k IS NOT NULL AND k<>'' GROUP BY k ORDER BY n DESC, k LIMIT ?4)), \
         (SELECT json_group_array(json_array(k, n)) FROM \
           (SELECT k, COUNT(*) AS n FROM ev WHERE cur AND b='mod_action' \
              AND k IS NOT NULL AND k<>'' GROUP BY k ORDER BY n DESC, k LIMIT ?4)), \
         (SELECT json_group_array(json_array(actor, n)) FROM \
           (SELECT actor, COUNT(*) AS n FROM ev WHERE cur AND b='mod_action' AND actor IS NOT NULL \
            GROUP BY actor ORDER BY n DESC, actor LIMIT ?4)), \
         (SELECT json_group_array(json_array(w, hr, n)) FROM \
           (SELECT CAST(strftime('%w', d) AS INTEGER) AS w, hr, SUM(n) AS n FROM \
             (SELECT d, hr, COUNT(*) AS n FROM ev \
              WHERE cur AND b IN ('automod','mod_action') GROUP BY d, hr \
              UNION ALL \
              SELECT substr(created_at,1,10), CAST(substr(created_at,12,2) AS INTEGER), COUNT(*) \
              FROM bans WHERE {cur} AND {DEDICATED_SPAM_BANS_SQL} GROUP BY 1, 2) \
            WHERE hr BETWEEN 0 AND 23 GROUP BY 1, 2 HAVING w BETWEEN 0 AND 6))"
    )
}

/// Panel values read from one guild summary row (columns 5..).
struct GuildPanels {
    previous: PreviousTotals,
    top_rules: Vec<RankedCount>,
    mod_action_kinds: Vec<RankedCount>,
    moderators: Vec<ModeratorCount>,
    heatmap: Vec<[i64; 3]>,
}

/// One guild's daily series and dashboard panels in one statement. The
/// caller zero-fills the window; observed days outside it are harmless.
fn guild_summary_conn(
    conn: &impl SqlExecutor,
    guild_id: i64,
    window_days: i64,
) -> Result<(HashMap<String, AnalyticsPoint>, GuildPanels)> {
    let start_day = window_start_day(window_days);
    let previous_start = window_start_day(window_days * 2);
    let sql = format!(
        "WITH {} SELECT {}, {}",
        guild_events_cte(),
        series_columns_with_events(
            "guild_id=?1 AND ",
            "?2",
            "SELECT d, b, COUNT(*) AS n FROM ev WHERE cur GROUP BY d, b"
        ),
        guild_panel_columns()
    );
    type Raw = (
        SeriesColumns,
        [i64; 3],
        Option<String>,
        [i64; 2],
        [Option<String>; 4],
    );
    let (series, counts, events, members, ranked): Raw = conn.query_row(
        &sql,
        params![guild_id, start_day, previous_start, ANALYTICS_TOP_LIMIT],
        |row| {
            Ok((
                series_row(row)?,
                [row.get(5)?, row.get(6)?, row.get(7)?],
                row.get(8)?,
                [row.get(9)?, row.get(10)?],
                [row.get(11)?, row.get(12)?, row.get(13)?, row.get(14)?],
            ))
        },
    )?;
    let [warns, bans, spam_bans] = counts;
    // The previous membership window is complete only while every day of it
    // is still retained by the daily counter pruning.
    let membership_complete = window_days * 2 - 1 <= ANALYTICS_MEMBER_DAILY_HORIZON_DAYS;
    let mut previous = PreviousTotals {
        warns,
        bans,
        automod: spam_bans,
        joins: membership_complete.then_some(members[0]),
        leaves: membership_complete.then_some(members[1]),
        ..PreviousTotals::default()
    };
    for (bucket, n) in json_rows::<(String, i64)>(events, "previous event")? {
        match bucket.as_str() {
            "automod" => previous.automod += n,
            "mod_action" => previous.mod_actions += n,
            "timeout_add" => previous.timeouts += n,
            _ => {}
        }
    }
    let [rules, kinds, moderators, heatmap] = ranked;
    let ranked = |raw, what| -> Result<Vec<RankedCount>> {
        let mut rows: Vec<RankedCount> = json_rows::<(String, i64)>(raw, what)?
            .into_iter()
            .map(|(key, count)| RankedCount { key, count })
            .collect();
        rows.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.key.cmp(&b.key)));
        Ok(rows)
    };
    let mut moderators: Vec<ModeratorCount> = json_rows::<(i64, i64)>(moderators, "moderator")?
        .into_iter()
        .map(|(moderator_id, count)| ModeratorCount {
            moderator_id,
            count,
        })
        .collect();
    moderators.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.moderator_id.cmp(&b.moderator_id))
    });
    let mut heatmap = json_rows::<[i64; 3]>(heatmap, "heat map")?;
    heatmap.sort_unstable();
    let panels = GuildPanels {
        previous,
        top_rules: ranked(rules, "rule")?,
        mod_action_kinds: ranked(kinds, "mod action kind")?,
        moderators,
        heatmap,
    };
    Ok((series_from_columns(series)?, panels))
}

/// Fleet series, per-guild totals for the requested and previous windows,
/// display facts and the server count in one statement. `?1` starts the
/// requested window and `?2` the previous window of equal length. Counting
/// grows with the retained rows of both windows; the route is owner-only.
fn platform_conn(conn: &impl SqlExecutor, window_days: i64) -> Result<PlatformAnalytics> {
    let start_day = window_start_day(window_days);
    let previous_start = window_start_day(window_days * 2);
    let sql = format!(
        "WITH w AS MATERIALIZED (SELECT guild_id, SUM(created_at>=?1) AS c, SUM(created_at<?1) AS p \
           FROM warns WHERE created_at>=?2 GROUP BY guild_id), \
         b AS MATERIALIZED (SELECT guild_id, SUM(created_at>=?1) AS c, SUM(created_at<?1) AS p \
           FROM bans WHERE created_at>=?2 GROUP BY guild_id), \
         s AS MATERIALIZED (SELECT guild_id, SUM(created_at>=?1) AS c, SUM(created_at<?1) AS p \
           FROM bans WHERE created_at>=?2 AND {DEDICATED_SPAM_BANS_SQL} GROUP BY guild_id), \
         ev AS MATERIALIZED (SELECT guild_id, \
              SUM(event_type='automod' AND created_at>=?1) AS a, \
              SUM(event_type='mod_action' AND created_at>=?1) AS m, \
              SUM(event_type='timeout_add' AND created_at>=?1) AS t, \
              SUM(event_type='automod' AND created_at<?1) AS pa, \
              SUM(event_type='mod_action' AND created_at<?1) AS pm, \
              SUM(event_type='timeout_add' AND created_at<?1) AS pt \
            FROM events WHERE {EVENT_BUCKETS_COUNTED_SQL} AND created_at>=?2 GROUP BY guild_id), \
         m AS MATERIALIZED (SELECT guild_id, \
              SUM(CASE WHEN day>=?1 THEN joins ELSE 0 END) AS j, \
              SUM(CASE WHEN day>=?1 THEN leaves ELSE 0 END) AS l, \
              SUM(CASE WHEN day<?1 THEN joins ELSE 0 END) AS pj, \
              SUM(CASE WHEN day<?1 THEN leaves ELSE 0 END) AS pl \
            FROM analytics_member_daily WHERE day>=?2 GROUP BY guild_id) \
         SELECT {}, \
         (SELECT json_group_array(json_array(guild_id, c, p)) FROM w), \
         (SELECT json_group_array(json_array(guild_id, c, p)) FROM b), \
         (SELECT json_group_array(json_array(guild_id, c, p)) FROM s), \
         (SELECT json_group_array(json_array(guild_id, a, m, t, pa, pm, pt)) FROM ev), \
         (SELECT json_group_array(json_array(guild_id, j, l, pj, pl)) FROM m), \
         (SELECT COUNT(*) FROM guild_enrollments WHERE active=1), \
         (SELECT json_group_array(json_array(e.guild_id, e.active, i.name, i.icon_hash)) \
            FROM guild_enrollments e LEFT JOIN guild_join_info i ON i.guild_id=e.guild_id \
            WHERE e.active=1 OR e.guild_id IN \
              (SELECT guild_id FROM w UNION SELECT guild_id FROM b \
               UNION SELECT guild_id FROM ev UNION SELECT guild_id FROM m))",
        series_columns("", "?1")
    );
    let (series, guild_columns, servers, info) =
        conn.query_row(&sql, params![start_day, previous_start], |row| {
            let guild_columns: [Option<String>; 5] = [
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
            ];
            Ok((
                series_row(row)?,
                guild_columns,
                row.get::<i64>(10)?,
                row.get::<Option<String>>(11)?,
            ))
        })?;
    let series_map = series_from_columns(series)?;
    let [warns, bans, spam_bans, events, members] = guild_columns;
    let mut guilds: HashMap<i64, GuildActivityTotals> = HashMap::new();
    fn entry(
        guilds: &mut HashMap<i64, GuildActivityTotals>,
        guild_id: i64,
    ) -> &mut GuildActivityTotals {
        guilds
            .entry(guild_id)
            .or_insert_with(|| GuildActivityTotals::zeroed(guild_id))
    }
    for (guild_id, n, p) in json_rows::<(i64, i64, i64)>(warns, "guild warn")? {
        let totals = entry(&mut guilds, guild_id);
        totals.warns += n;
        totals.previous.warns += p;
    }
    for (guild_id, n, p) in json_rows::<(i64, i64, i64)>(bans, "guild ban")? {
        let totals = entry(&mut guilds, guild_id);
        totals.bans += n;
        totals.previous.bans += p;
    }
    for (guild_id, n, p) in json_rows::<(i64, i64, i64)>(spam_bans, "guild spam ban")? {
        let totals = entry(&mut guilds, guild_id);
        totals.automod += n;
        totals.previous.automod += p;
    }
    type EventRow = (i64, i64, i64, i64, i64, i64, i64);
    for (guild_id, automod, mod_actions, timeouts, p_automod, p_mod_actions, p_timeouts) in
        json_rows::<EventRow>(events, "guild event")?
    {
        let totals = entry(&mut guilds, guild_id);
        totals.automod += automod;
        totals.mod_actions += mod_actions;
        totals.timeouts += timeouts;
        totals.previous.automod += p_automod;
        totals.previous.mod_actions += p_mod_actions;
        totals.previous.timeouts += p_timeouts;
    }
    for (guild_id, joins, leaves, p_joins, p_leaves) in
        json_rows::<(i64, i64, i64, i64, i64)>(members, "guild membership")?
    {
        let totals = entry(&mut guilds, guild_id);
        totals.joins += joins;
        totals.leaves += leaves;
        totals.previous.joins = Some(totals.previous.joins.unwrap_or(0) + p_joins);
        totals.previous.leaves = Some(totals.previous.leaves.unwrap_or(0) + p_leaves);
    }
    for (guild_id, active, name, icon_hash) in
        json_rows::<(i64, i64, Option<String>, Option<String>)>(info, "guild display")?
    {
        let totals = entry(&mut guilds, guild_id);
        totals.active = active == 1;
        totals.name = name;
        totals.icon_hash = icon_hash;
    }
    // Same rule as the guild view: the previous membership window is complete
    // only while every day of it is still retained by the counter pruning.
    let membership_complete = window_days * 2 - 1 <= ANALYTICS_MEMBER_DAILY_HORIZON_DAYS;
    let mut previous = PreviousTotals {
        joins: membership_complete.then_some(0),
        leaves: membership_complete.then_some(0),
        ..PreviousTotals::default()
    };
    let mut guilds: Vec<GuildActivityTotals> = guilds.into_values().collect();
    for totals in &mut guilds {
        if !membership_complete {
            totals.previous.joins = None;
            totals.previous.leaves = None;
        }
        let p = &totals.previous;
        previous.warns += p.warns;
        previous.bans += p.bans;
        previous.automod += p.automod;
        previous.mod_actions += p.mod_actions;
        previous.timeouts += p.timeouts;
        previous.joins = previous.joins.zip(p.joins).map(|(a, b)| a + b);
        previous.leaves = previous.leaves.zip(p.leaves).map(|(a, b)| a + b);
    }
    guilds.sort_by_key(|totals| totals.guild_id);
    Ok(PlatformAnalytics {
        window_days,
        series: filled_series(series_map, window_days),
        previous,
        servers,
        guilds,
    })
}

/// Guild analytics read in the captured active membership. HTTP callers must
/// still authorize the user; captured identity is not user authorization.
pub async fn enrolled_guild_analytics(
    db: &Db,
    enrollment: super::GuildEnrollment,
    window_days: i64,
) -> Result<GuildAnalytics> {
    let window_days = clamp_window(window_days);
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_snapshot()?;
        super::enrollments::verify_enrollment_conn(&tx, &enrollment)?;
        let (map, panels) = guild_summary_conn(&tx, enrollment.guild_id, window_days)?;
        tx.finish_read()?;
        Ok(GuildAnalytics {
            window_days,
            series: filled_series(map, window_days),
            previous: panels.previous,
            top_rules: panels.top_rules,
            mod_action_kinds: panels.mod_action_kinds,
            moderators: panels.moderators,
            heatmap: panels.heatmap,
        })
    })
    .await
}

/// Owner-scoped platform analytics read. Callers must verify the session is
/// the bot owner; this reads every guild's aggregates.
pub async fn platform_analytics(db: &Db, window_days: i64) -> Result<PlatformAnalytics> {
    let window_days = clamp_window(window_days);
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let result = platform_conn(&tx, window_days)?;
        tx.finish_read()?;
        Ok(result)
    })
    .await
}

/// Bumps the guild's daily membership counters by the given deltas. Runtime
/// write: verifies the captured enrollment, refuses fenced writes, and never
/// touches `config_version`. Rows beyond the horizon are pruned with the write.
pub async fn bump_member_daily(
    db: &Db,
    enrollment: super::GuildEnrollment,
    at_ms: i64,
    joins_delta: i64,
    leaves_delta: i64,
) -> Result<()> {
    anyhow::ensure!(
        joins_delta >= 0 && leaves_delta >= 0,
        "negative analytics counter delta"
    );
    if joins_delta == 0 && leaves_delta == 0 {
        return Ok(());
    }
    let day = analytics_day_of_ms(at_ms);
    let prune_before = (Utc::now().date_naive()
        - chrono::Duration::days(ANALYTICS_MEMBER_DAILY_HORIZON_DAYS))
    .format("%Y-%m-%d")
    .to_string();
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_write()?;
        super::enrollments::verify_enrollment_conn(&tx, &enrollment)?;
        super::guild_stores::refuse_fenced_write(&tx, enrollment.guild_id)?;
        tx.ensure_mutation_receipt()?;
        tx.execute_rows(
            "INSERT INTO analytics_member_daily (guild_id, day, joins, leaves) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(guild_id, day) DO UPDATE \
             SET joins=joins+excluded.joins, leaves=leaves+excluded.leaves",
            params![enrollment.guild_id, day, joins_delta, leaves_delta],
            1,
        )?;
        tx.execute_reclaim(
            "DELETE FROM analytics_member_daily WHERE guild_id=?1 AND day<?2",
            params![enrollment.guild_id, prune_before],
        )?;
        tx.commit()?;
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests;
