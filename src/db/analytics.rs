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

/// Guild-scoped daily analytics series.
#[derive(Debug, Clone, Serialize)]
pub struct GuildAnalytics {
    pub window_days: i64,
    pub series: Vec<AnalyticsPoint>,
}

/// Per-guild activity totals across one analytics window. Guild IDs stay
/// decimal strings on the wire.
#[derive(Debug, Clone, Serialize)]
pub struct GuildActivityTotals {
    #[serde(serialize_with = "ser_i64_str")]
    pub guild_id: i64,
    pub warns: i64,
    pub bans: i64,
    pub automod: i64,
    pub mod_actions: i64,
    pub timeouts: i64,
    pub joins: i64,
    pub leaves: i64,
}

impl GuildActivityTotals {
    fn zeroed(guild_id: i64) -> Self {
        Self {
            guild_id,
            warns: 0,
            bans: 0,
            automod: 0,
            mod_actions: 0,
            timeouts: 0,
            joins: 0,
            leaves: 0,
        }
    }
}

/// Owner-scoped platform analytics. Series aggregate every guild; the per-guild
/// breakdown covers the same window.
#[derive(Debug, Clone, Serialize)]
pub struct PlatformAnalytics {
    pub window_days: i64,
    pub series: Vec<AnalyticsPoint>,
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

fn collect_pairs(
    conn: &impl SqlExecutor,
    sql_guild: &str,
    sql_all: &str,
    guild_id: Option<i64>,
    start_day: &str,
) -> Result<Vec<(String, i64)>> {
    let mut rows = Vec::new();
    if let Some(guild_id) = guild_id {
        let mut stmt = conn.prepare(sql_guild)?;
        let mapped = stmt.query_map(params![guild_id, start_day], |row| {
            Ok((row.get::<String>(0)?, row.get::<i64>(1)?))
        })?;
        for row in mapped {
            rows.push(row?);
        }
    } else {
        let mut stmt = conn.prepare(sql_all)?;
        let mapped = stmt.query_map(params![start_day], |row| {
            Ok((row.get::<String>(0)?, row.get::<i64>(1)?))
        })?;
        for row in mapped {
            rows.push(row?);
        }
    }
    Ok(rows)
}

/// Accumulates the daily series from every counted source. The caller
/// zero-fills the window; observed days outside it are harmless.
fn analytics_series_conn(
    conn: &impl SqlExecutor,
    guild_id: Option<i64>,
    start_day: &str,
) -> Result<HashMap<String, AnalyticsPoint>> {
    let mut map: HashMap<String, AnalyticsPoint> = HashMap::new();
    for (day, n) in collect_pairs(
        conn,
        "SELECT substr(created_at,1,10), COUNT(*) FROM warns \
         WHERE guild_id=?1 AND created_at>=?2 GROUP BY 1",
        "SELECT substr(created_at,1,10), COUNT(*) FROM warns \
         WHERE created_at>=?1 GROUP BY 1",
        guild_id,
        start_day,
    )? {
        apply_day(&mut map, day, move |point| point.warns += n);
    }
    for (day, n) in collect_pairs(
        conn,
        "SELECT substr(created_at,1,10), COUNT(*) FROM bans \
         WHERE guild_id=?1 AND created_at>=?2 GROUP BY 1",
        "SELECT substr(created_at,1,10), COUNT(*) FROM bans \
         WHERE created_at>=?1 GROUP BY 1",
        guild_id,
        start_day,
    )? {
        apply_day(&mut map, day, move |point| point.bans += n);
    }
    let spam_bans_guild = format!(
        "SELECT substr(created_at,1,10), COUNT(*) FROM bans \
         WHERE guild_id=?1 AND created_at>=?2 AND {DEDICATED_SPAM_BANS_SQL} GROUP BY 1"
    );
    let spam_bans_all = format!(
        "SELECT substr(created_at,1,10), COUNT(*) FROM bans \
         WHERE created_at>=?1 AND {DEDICATED_SPAM_BANS_SQL} GROUP BY 1"
    );
    for (day, n) in collect_pairs(conn, &spam_bans_guild, &spam_bans_all, guild_id, start_day)? {
        apply_day(&mut map, day, move |point| point.automod += n);
    }
    // Counted events land in one of three buckets; the automod bucket keeps
    // the lifetime statistics definition (recorded, non-dry-run enforcement)
    // and also holds the dedicated spam bans counted above.
    let events_guild = format!(
        "SELECT substr(created_at,1,10), \
         CASE WHEN event_type='automod' THEN 'automod' ELSE event_type END, COUNT(*) \
         FROM events WHERE guild_id=?1 AND created_at>=?2 AND {EVENT_BUCKETS_COUNTED_SQL} \
         GROUP BY 1, 2"
    );
    let events_all = format!(
        "SELECT substr(created_at,1,10), \
         CASE WHEN event_type='automod' THEN 'automod' ELSE event_type END, COUNT(*) \
         FROM events WHERE created_at>=?1 AND {EVENT_BUCKETS_COUNTED_SQL} \
         GROUP BY 1, 2"
    );
    let mut event_rows = Vec::new();
    if let Some(guild_id) = guild_id {
        let mut stmt = conn.prepare(&events_guild)?;
        let mapped = stmt.query_map(params![guild_id, start_day], |row| {
            Ok((
                row.get::<String>(0)?,
                row.get::<String>(1)?,
                row.get::<i64>(2)?,
            ))
        })?;
        for row in mapped {
            event_rows.push(row?);
        }
    } else {
        let mut stmt = conn.prepare(&events_all)?;
        let mapped = stmt.query_map(params![start_day], |row| {
            Ok((
                row.get::<String>(0)?,
                row.get::<String>(1)?,
                row.get::<i64>(2)?,
            ))
        })?;
        for row in mapped {
            event_rows.push(row?);
        }
    }
    for (day, bucket, n) in event_rows {
        apply_day(&mut map, day, move |point| match bucket.as_str() {
            "automod" => point.automod += n,
            "mod_action" => point.mod_actions += n,
            "timeout_add" => point.timeouts += n,
            _ => {}
        });
    }
    // Daily membership counters (no user IDs; rows exist only for observed days).
    if let Some(guild_id) = guild_id {
        let mut stmt = conn.prepare(
            "SELECT day, joins, leaves FROM analytics_member_daily \
             WHERE guild_id=?1 AND day>=?2",
        )?;
        let mapped = stmt.query_map(params![guild_id, start_day], |row| {
            Ok((
                row.get::<String>(0)?,
                row.get::<i64>(1)?,
                row.get::<i64>(2)?,
            ))
        })?;
        for row in mapped {
            let (day, joins, leaves) = row?;
            apply_day(&mut map, day, move |point| {
                point.joins += joins;
                point.leaves += leaves;
            });
        }
    } else {
        let mut stmt = conn.prepare(
            "SELECT day, SUM(joins), SUM(leaves) FROM analytics_member_daily \
             WHERE day>=?1 GROUP BY day",
        )?;
        let mapped = stmt.query_map(params![start_day], |row| {
            Ok((
                row.get::<String>(0)?,
                row.get::<i64>(1)?,
                row.get::<i64>(2)?,
            ))
        })?;
        for row in mapped {
            let (day, joins, leaves) = row?;
            apply_day(&mut map, day, move |point| {
                point.joins += joins;
                point.leaves += leaves;
            });
        }
    }
    Ok(map)
}

/// Per-guild window totals for the platform breakdown. Counting grows with the
/// window's retained rows; the route is owner-only.
fn platform_conn(conn: &impl SqlExecutor, window_days: i64) -> Result<PlatformAnalytics> {
    let start_day = window_start_day(window_days);
    let series_map = analytics_series_conn(conn, None, &start_day)?;
    let mut guilds: HashMap<i64, GuildActivityTotals> = HashMap::new();
    let mut stmt = conn
        .prepare("SELECT guild_id, COUNT(*) FROM warns WHERE created_at>=?1 GROUP BY guild_id")?;
    for row in stmt.query_map(params![start_day.clone()], |row| {
        Ok((row.get::<i64>(0)?, row.get::<i64>(1)?))
    })? {
        let (guild_id, n) = row?;
        guilds
            .entry(guild_id)
            .or_insert_with(|| GuildActivityTotals::zeroed(guild_id))
            .warns += n;
    }
    let mut stmt =
        conn.prepare("SELECT guild_id, COUNT(*) FROM bans WHERE created_at>=?1 GROUP BY guild_id")?;
    for row in stmt.query_map(params![start_day.clone()], |row| {
        Ok((row.get::<i64>(0)?, row.get::<i64>(1)?))
    })? {
        let (guild_id, n) = row?;
        guilds
            .entry(guild_id)
            .or_insert_with(|| GuildActivityTotals::zeroed(guild_id))
            .bans += n;
    }
    let spam_bans_sql = format!(
        "SELECT guild_id, COUNT(*) FROM bans \
         WHERE created_at>=?1 AND {DEDICATED_SPAM_BANS_SQL} GROUP BY guild_id"
    );
    let mut stmt = conn.prepare(&spam_bans_sql)?;
    for row in stmt.query_map(params![start_day.clone()], |row| {
        Ok((row.get::<i64>(0)?, row.get::<i64>(1)?))
    })? {
        let (guild_id, n) = row?;
        guilds
            .entry(guild_id)
            .or_insert_with(|| GuildActivityTotals::zeroed(guild_id))
            .automod += n;
    }
    let events_sql = format!(
        "SELECT guild_id, \
         SUM(CASE WHEN event_type='automod' THEN 1 ELSE 0 END), \
         SUM(CASE WHEN event_type='mod_action' THEN 1 ELSE 0 END), \
         SUM(CASE WHEN event_type='timeout_add' THEN 1 ELSE 0 END) \
         FROM events WHERE {EVENT_BUCKETS_COUNTED_SQL} AND created_at>=?1 GROUP BY guild_id"
    );
    let mut stmt = conn.prepare(&events_sql)?;
    for row in stmt.query_map(params![start_day.clone()], |row| {
        Ok((
            row.get::<i64>(0)?,
            row.get::<i64>(1)?,
            row.get::<i64>(2)?,
            row.get::<i64>(3)?,
        ))
    })? {
        let (guild_id, automod, mod_actions, timeouts) = row?;
        let entry = guilds
            .entry(guild_id)
            .or_insert_with(|| GuildActivityTotals::zeroed(guild_id));
        entry.automod += automod;
        entry.mod_actions += mod_actions;
        entry.timeouts += timeouts;
    }
    let mut stmt = conn.prepare(
        "SELECT guild_id, SUM(joins), SUM(leaves) FROM analytics_member_daily \
         WHERE day>=?1 GROUP BY guild_id",
    )?;
    for row in stmt.query_map(params![start_day.clone()], |row| {
        Ok((row.get::<i64>(0)?, row.get::<i64>(1)?, row.get::<i64>(2)?))
    })? {
        let (guild_id, joins, leaves) = row?;
        let entry = guilds
            .entry(guild_id)
            .or_insert_with(|| GuildActivityTotals::zeroed(guild_id));
        entry.joins += joins;
        entry.leaves += leaves;
    }
    let servers: i64 = conn.query_row(
        "SELECT COUNT(*) FROM guild_enrollments WHERE active=1",
        (),
        |row| Ok(row.get(0)?),
    )?;
    let mut guilds: Vec<GuildActivityTotals> = guilds.into_values().collect();
    guilds.sort_by_key(|totals| totals.guild_id);
    Ok(PlatformAnalytics {
        window_days,
        series: filled_series(series_map, window_days),
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
    let start_day = window_start_day(window_days);
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_snapshot()?;
        super::enrollments::verify_enrollment_conn(&tx, &enrollment)?;
        let map = analytics_series_conn(&tx, Some(enrollment.guild_id), &start_day)?;
        tx.finish_read()?;
        Ok(GuildAnalytics {
            window_days,
            series: filled_series(map, window_days),
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
