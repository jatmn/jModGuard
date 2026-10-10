const BAN_STATS_SQL: &str = "SELECT COUNT(*) FROM bans WHERE guild_id = ?1";
const WARN_STATS_SQL: &str = "SELECT COUNT(*) FROM warns WHERE guild_id = ?1";
const EVENT_STATS_SQL: &str = "SELECT COUNT(*) FROM events WHERE guild_id = ?1 AND (
                   event_type IN ('mod_action','timeout_add','timeout_remove',
                                  'member_ban','member_unban','member_kick')
                   OR CASE WHEN json_valid(data) THEN
                     (event_type = 'automod'
                      AND json_extract(data, '$.action') IN ('warn','timeout','kick','ban')
                      AND COALESCE(json_extract(data, '$.dry_run'), 0) = 0)
                     OR (event_type = 'plan_executed' AND json_type(data, '$.moderation') = 'true')
                   ELSE 0 END
                 )";
const BAN_PAGE_SQL: &str = "SELECT id, guild_id, user_id, moderator_id, reason, created_at, active, user_name, user_display_name, moderator_name
             FROM bans WHERE guild_id = ?1 ORDER BY id DESC LIMIT ?2";
use super::*;

// ---------------------------------------------------------------------------
// events (audit log)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EventRow {
    pub id: i64,
    #[serde(
        serialize_with = "crate::ids::ser_i64_str",
        deserialize_with = "crate::ids::de_i64"
    )]
    pub guild_id: i64,
    pub event_type: String,
    #[serde(
        default,
        serialize_with = "crate::ids::ser_opt_i64_str",
        deserialize_with = "crate::ids::de_opt_i64"
    )]
    pub user_id: Option<i64>,
    #[serde(
        default,
        serialize_with = "crate::ids::ser_opt_i64_str",
        deserialize_with = "crate::ids::de_opt_i64"
    )]
    pub actor_id: Option<i64>,
    pub data: Option<String>,
    pub created_at: String,
}

pub async fn insert_event(
    db: &Db,
    guild_id: i64,
    event_type: &str,
    user_id: Option<i64>,
    actor_id: Option<i64>,
    data: &serde_json::Value,
) -> Result<i64> {
    let (now, event_type) = (now_str(), event_type.to_string());
    // Persist WebUI-safe JSON: snowflakes above JS MAX_SAFE_INTEGER as strings.
    let data = if data.is_null() {
        None
    } else {
        Some(crate::ids::stringify_js_unsafe_integers(data).to_string())
    };
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        let id = tx.query_row_write(
            "INSERT INTO events (guild_id, event_type, user_id, actor_id, data, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             RETURNING id",
            params![guild_id, event_type, user_id, actor_id, data, now],
            |row| Ok(row.get::<i64>(0)?),
        )?;
        tx.commit()?;
        Ok(id)
    })
    .await
}

pub async fn query_events(
    db: &Db,
    guild_id: i64,
    event_type: Option<&str>,
    user_id: Option<i64>,
    limit: i64,
) -> Result<Vec<EventRow>> {
    let event_type = event_type.map(str::to_owned);
    db.with_conn(move |conn| {
        query_events_conn(conn, guild_id, event_type.as_deref(), user_id, limit)
    })
    .await
}

fn event_query(
    guild_id: i64,
    event_type: Option<&str>,
    user_id: Option<i64>,
    limit: i64,
) -> (String, Vec<Value>) {
    let limit = limit.clamp(1, 200);
    let mut sql = String::from(
        "SELECT id, guild_id, event_type, user_id, actor_id, data, created_at
         FROM events",
    );
    // Keep sparse/absent-user lookups selective instead of walking the guild's
    // ordered history and applying user_id as a residual filter.
    if user_id.is_some() {
        sql.push_str(" INDEXED BY idx_events_user");
    }
    sql.push_str(" WHERE guild_id = ?1");
    let mut args: Vec<Value> = vec![guild_id.into()];
    if let Some(et) = &event_type {
        sql.push_str(" AND event_type = ?");
        args.push(et.to_string().into());
    }
    if let Some(uid) = user_id {
        sql.push_str(" AND user_id = ?");
        args.push(uid.into());
    }
    sql.push_str(" ORDER BY id DESC LIMIT ?");
    args.push(limit.into());

    (sql, args)
}

fn query_events_conn(
    conn: &impl SqlExecutor,
    guild_id: i64,
    event_type: Option<&str>,
    user_id: Option<i64>,
    limit: i64,
) -> Result<Vec<EventRow>> {
    let (sql, args) = event_query(guild_id, event_type, user_id, limit);

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(args), |r| {
        Ok(EventRow {
            id: r.get(0)?,
            guild_id: r.get(1)?,
            event_type: r.get(2)?,
            user_id: r.get(3)?,
            actor_id: r.get(4)?,
            data: r.get(5)?,
            created_at: r.get(6)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        let mut row = row?;
        // Heal legacy rows that stored snowflakes as JSON numbers.
        if let Some(raw) = row.data.take() {
            row.data = Some(crate::ids::wire_safe_event_data_json(&raw));
        }
        out.push(row);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// warns / bans
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WarnRow {
    pub id: i64,
    #[serde(
        serialize_with = "crate::ids::ser_i64_str",
        deserialize_with = "crate::ids::de_i64"
    )]
    pub guild_id: i64,
    #[serde(
        serialize_with = "crate::ids::ser_i64_str",
        deserialize_with = "crate::ids::de_i64"
    )]
    pub user_id: i64,
    #[serde(
        serialize_with = "crate::ids::ser_i64_str",
        deserialize_with = "crate::ids::de_i64"
    )]
    pub moderator_id: i64,
    pub reason: Option<String>,
    pub created_at: String,
}

pub async fn add_warn(
    db: &Db,
    guild_id: i64,
    user_id: i64,
    moderator_id: i64,
    reason: &str,
) -> Result<i64> {
    let (now, reason) = (now_str(), reason.to_string());
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        let id = tx.query_row_write(
            "INSERT INTO warns (guild_id, user_id, moderator_id, reason, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             RETURNING id",
            params![guild_id, user_id, moderator_id, reason, now],
            |row| Ok(row.get::<i64>(0)?),
        )?;
        tx.commit()?;
        Ok(id)
    })
    .await
}

pub async fn list_warns(db: &Db, guild_id: i64, user_id: i64) -> Result<Vec<WarnRow>> {
    db.with_conn(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, guild_id, user_id, moderator_id, reason, created_at
             FROM warns WHERE guild_id = ?1 AND user_id = ?2 ORDER BY id DESC",
        )?;
        let rows = stmt.query_map(params![guild_id, user_id], |r| {
            Ok(WarnRow {
                id: r.get(0)?,
                guild_id: r.get(1)?,
                user_id: r.get(2)?,
                moderator_id: r.get(3)?,
                reason: r.get(4)?,
                created_at: r.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
    .await
}

pub async fn clear_warns(db: &Db, guild_id: i64, user_id: i64) -> Result<usize> {
    db.with_conn(move |conn| {
        conn.execute_reclaim(
            "DELETE FROM warns WHERE guild_id = ?1 AND user_id = ?2",
            params![guild_id, user_id],
        )
    })
    .await
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BanRow {
    pub id: i64,
    #[serde(
        serialize_with = "crate::ids::ser_i64_str",
        deserialize_with = "crate::ids::de_i64"
    )]
    pub guild_id: i64,
    #[serde(
        serialize_with = "crate::ids::ser_i64_str",
        deserialize_with = "crate::ids::de_i64"
    )]
    pub user_id: i64,
    pub user_name: Option<String>,
    pub user_display_name: Option<String>,
    #[serde(
        default,
        serialize_with = "crate::ids::ser_opt_i64_str",
        deserialize_with = "crate::ids::de_opt_i64"
    )]
    pub moderator_id: Option<i64>,
    pub moderator_name: Option<String>,
    pub reason: Option<String>,
    pub created_at: String,
    pub active: bool,
}

pub async fn record_ban(
    db: &Db,
    guild_id: i64,
    user_id: i64,
    moderator_id: Option<i64>,
    reason: &str,
) -> Result<()> {
    let (now, reason) = (now_str(), reason.to_string());
    db.with_conn(move |conn| {
        conn.execute_rows(
            "INSERT INTO bans (guild_id, user_id, moderator_id, reason, created_at, active)
             VALUES (?1, ?2, ?3, ?4, ?5, 1)",
            params![guild_id, user_id, moderator_id, reason, now],
            1,
        )?;
        Ok(())
    })
    .await
}

pub async fn mark_unbanned(db: &Db, guild_id: i64, user_id: i64) -> Result<()> {
    // Mirror of Discord ban state. Callers that already succeeded at Discord
    // `remove_ban` should treat errors here as non-fatal (log + continue);
    // `GUILD_BAN_REMOVE` on the bot also calls this to heal drift.
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        tx.execute_matching(
            "UPDATE bans SET active = 0 WHERE guild_id = ?1 AND user_id = ?2 AND active = 1",
            params![guild_id, user_id],
        )?;
        tx.commit()
    })
    .await
}

pub async fn list_bans(db: &Db, guild_id: i64, limit: i64) -> Result<Vec<BanRow>> {
    db.with_conn(move |conn| list_bans_conn(conn, guild_id, limit))
        .await
}

fn list_bans_conn(conn: &impl SqlExecutor, guild_id: i64, limit: i64) -> Result<Vec<BanRow>> {
    let limit = limit.clamp(1, 200);
    let mut stmt = conn.prepare(BAN_PAGE_SQL)?;
    let rows = stmt.query_map(params![guild_id, limit], |r| {
        Ok(BanRow {
            id: r.get(0)?,
            guild_id: r.get(1)?,
            user_id: r.get(2)?,
            moderator_id: r.get(3)?,
            reason: r.get(4)?,
            created_at: r.get(5)?,
            active: r.get::<i64>(6)? != 0,
            user_name: r.get(7)?,
            user_display_name: r.get(8)?,
            moderator_name: r.get(9)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// stats
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Stats {
    pub events: i64,
    pub warns: i64,
    pub bans: i64,
}

/// Per-guild identified moderation aggregates. Legacy unclassified plan summaries
/// remain available through query_events, but cannot truthfully count as moderation.
/// Autocommit existence check used before a ban handoff. No transaction.
pub async fn active_ban_recorded(db: &Db, guild_id: i64, user_id: i64) -> Result<bool> {
    db.with_conn(move |conn| {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM bans WHERE guild_id=?1 AND user_id=?2 AND active=1)",
            params![guild_id, user_id],
            |row| Ok(row.get::<i64>(0)? != 0),
        )
    })
    .await
}

pub async fn guild_stats(db: &Db, guild_id: i64) -> Result<Stats> {
    db.with_conn(move |conn| guild_stats_conn(conn, guild_id))
        .await
}

fn guild_stats_conn(conn: &impl SqlExecutor, guild_id: i64) -> Result<Stats> {
    let one = |sql: &str| conn.query_row(sql, params![guild_id], |r| Ok(r.get::<i64>(0)?));
    Ok(Stats {
        events: one(EVENT_STATS_SQL)?,
        warns: one(WARN_STATS_SQL)?,
        bans: one(BAN_STATS_SQL)?,
    })
}

/// Parses an RFC3339 timestamp into a friendly relative duration string.
pub fn time_ago(ts: &Option<String>) -> String {
    let Some(ts) = ts else { return "never".into() };
    let Ok(dt) = DateTime::parse_from_rfc3339(ts) else {
        return ts.clone();
    };
    let diff = Utc::now() - dt.with_timezone(&Utc);
    let secs = diff.num_seconds();
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// Membership and moderation data share one primary read transaction. Captured
/// identity is not user authorization; HTTP callers must still check permissions.
async fn enrolled_moderation_read<T: Send + 'static>(
    db: &Db,
    enrollment: GuildEnrollment,
    read: impl FnOnce(&crate::db_compat::Transaction, i64) -> Result<T> + Send + 'static,
) -> Result<T> {
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_snapshot()?;
        super::enrollments::verify_enrollment_conn(&tx, &enrollment)?;
        let result = read(&tx, enrollment.guild_id)?;
        tx.finish_read()?;
        Ok(result)
    })
    .await
}

/// Read audit rows in the captured active membership, retaining filters/order/limits.
pub async fn query_enrolled_events(
    db: &Db,
    enrollment: GuildEnrollment,
    event_type: Option<&str>,
    user_id: Option<i64>,
    limit: i64,
) -> Result<Vec<EventRow>> {
    let event_type = event_type.map(str::to_owned);
    enrolled_moderation_read(db, enrollment, move |tx, guild| {
        query_events_conn(tx, guild, event_type.as_deref(), user_id, limit)
    })
    .await
}

/// Read ban history in the captured active membership.
pub async fn list_enrolled_bans(
    db: &Db,
    enrollment: GuildEnrollment,
    limit: i64,
) -> Result<Vec<BanRow>> {
    enrolled_moderation_read(db, enrollment, move |tx, guild| {
        list_bans_conn(tx, guild, limit)
    })
    .await
}

/// Read all moderation counters in the same captured primary snapshot.
pub async fn enrolled_guild_stats(db: &Db, enrollment: GuildEnrollment) -> Result<Stats> {
    enrolled_moderation_read(db, enrollment, guild_stats_conn).await
}

// Runtime producers retain their admission identity through asynchronous effects.
pub async fn insert_enrolled_event(
    db: &Db,
    enrollment: Option<GuildEnrollment>,
    event_type: &str,
    user_id: Option<i64>,
    actor_id: Option<i64>,
    data: &serde_json::Value,
) -> Result<i64> {
    let (now, event_type) = (now_str(), event_type.to_string());
    // Persist WebUI-safe JSON: snowflakes above JS MAX_SAFE_INTEGER as strings.
    let data = if data.is_null() {
        None
    } else {
        Some(crate::ids::stringify_js_unsafe_integers(data).to_string())
    };
    let enrollment = enrollment.ok_or(EnrollmentChanged)?;
    let guild_id = enrollment.guild_id;
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_read()?;
        super::enrollments::verify_enrollment_conn(&tx, &enrollment)?;
        super::guild_stores::refuse_fenced_write(&tx, enrollment.guild_id)?;
        tx.ensure_mutation_receipt()?;
        let id = tx.query_row_write(
            "INSERT INTO events (guild_id, event_type, user_id, actor_id, data, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             RETURNING id",
            params![guild_id, event_type, user_id, actor_id, data, now],
            |row| Ok(row.get::<i64>(0)?),
        )?;
        tx.commit()?;
        Ok(id)
    })
    .await
}

pub async fn add_enrolled_warn(
    db: &Db,
    enrollment: Option<GuildEnrollment>,
    user_id: i64,
    moderator_id: i64,
    reason: &str,
) -> Result<i64> {
    let (now, reason) = (now_str(), reason.to_string());
    let enrollment = enrollment.ok_or(EnrollmentChanged)?;
    let guild_id = enrollment.guild_id;
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_read()?;
        super::enrollments::verify_enrollment_conn(&tx, &enrollment)?;
        super::guild_stores::refuse_fenced_write(&tx, enrollment.guild_id)?;
        tx.ensure_mutation_receipt()?;
        let id = tx.query_row_write(
            "INSERT INTO warns (guild_id, user_id, moderator_id, reason, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             RETURNING id",
            params![guild_id, user_id, moderator_id, reason, now],
            |row| Ok(row.get::<i64>(0)?),
        )?;
        tx.commit()?;
        Ok(id)
    })
    .await
}

/// How a ban row relates to a previous active mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BanRecord {
    /// Discord already shows this ban. A matching active row is the same dispatch.
    Retry,
    /// Discord showed the user was not banned. Replace a stale active mirror.
    Fresh,
}

/// Reason stored on a gateway ban until an audit entry attributes it.
/// Migration 0018 uses this same text when choosing which duplicate active row to keep.
pub const GATEWAY_BAN_REASON: &str = "Reason unavailable";

/// How a gateway ban delivery relates to the active mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayBanWrite {
    /// The bot's echo stored the missing active row. No `member_ban` event.
    OwnedByBot { row_id: i64 },
    /// An active row is already this ban. Leave its actor and reason unchanged.
    Existing { row_id: i64 },
    /// This delivery inserted the only active row and one `member_ban` event.
    Recorded { row_id: i64 },
}

/// Insert an active ban row for this dispatch.
///
/// [`BanRecord::Retry`] returns `None` and writes nothing when an active row already has
/// the same guild, user, reason, and moderator, even when `occurred_at` is `None`.
/// [`BanRecord::Fresh`] replaces that stale active mirror and inserts a new row.
/// `created_at` stays the timestamp of the insert and is not the dispatch identity. A
/// different reason replaces the stale active row. An ended row does not block the
/// insert. An active row that still has no moderator and [`GATEWAY_BAN_REASON`] is the
/// gateway echo of this same ban; the command fills that row instead of inserting
/// another. `Some` means a row was inserted or filled and the caller may publish.
/// `None` means do not publish.
#[allow(clippy::too_many_arguments)]
pub async fn record_enrolled_ban_if_absent(
    db: &Db,
    enrollment: Option<GuildEnrollment>,
    user_id: i64,
    user_name: Option<&str>,
    user_display_name: Option<&str>,
    moderator_id: Option<i64>,
    moderator_name: Option<&str>,
    reason: &str,
    occurred_at: Option<&str>,
    kind: BanRecord,
) -> Result<Option<i64>> {
    let (now, reason) = (
        occurred_at.map(str::to_owned).unwrap_or_else(now_str),
        reason.to_string(),
    );
    let user_name = user_name.map(str::to_owned);
    let user_display_name = user_display_name.map(str::to_owned);
    let moderator_name = moderator_name.map(str::to_owned);
    let enrollment = enrollment.ok_or(EnrollmentChanged)?;
    let guild_id = enrollment.guild_id;
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_read()?;
        super::enrollments::verify_enrollment_conn(&tx, &enrollment)?;
        super::guild_stores::refuse_fenced_write(&tx, enrollment.guild_id)?;
        let same_dispatch: Option<i64> = tx
            .query_row(
                "SELECT id FROM bans
                 WHERE guild_id=?1 AND user_id=?2 AND active=1
                   AND reason IS ?3 AND moderator_id IS ?4
                 ORDER BY id DESC LIMIT 1",
                params![guild_id, user_id, reason.clone(), moderator_id],
                |row| Ok(row.get(0)?),
            )
            .optional()?;
        if kind == BanRecord::Retry && same_dispatch.is_some() {
            tx.finish_read()?;
            return Ok(None);
        }
        let placeholder: Option<i64> = tx
            .query_row(
                "SELECT id FROM bans
                 WHERE guild_id=?1 AND user_id=?2 AND active=1
                   AND moderator_id IS NULL AND reason IS ?3
                 ORDER BY id LIMIT 1",
                params![guild_id, user_id, GATEWAY_BAN_REASON],
                |row| Ok(row.get(0)?),
            )
            .optional()?;
        tx.ensure_mutation_receipt()?;
        if let Some(row_id) = placeholder {
            let updated = tx.execute_matching(
                "UPDATE bans SET moderator_id=?1, moderator_name=?2, reason=?3,
                    user_name=COALESCE(?4, user_name),
                    user_display_name=COALESCE(?5, user_display_name),
                    created_at=?6
                 WHERE id=?7 AND guild_id=?8 AND user_id=?9 AND active=1 AND moderator_id IS NULL",
                params![
                    moderator_id,
                    moderator_name,
                    reason,
                    user_name,
                    user_display_name,
                    now,
                    row_id,
                    guild_id,
                    user_id
                ],
            )?;
            anyhow::ensure!(updated == 1, "gateway ban placeholder was no longer active");
            tx.commit()?;
            return Ok(Some(row_id));
        }
        tx.execute_matching(
            "UPDATE bans SET active=0 WHERE guild_id=?1 AND user_id=?2 AND active=1",
            params![guild_id, user_id],
        )?;
        tx.execute_rows(
            "INSERT INTO bans (guild_id, user_id, moderator_id, reason, created_at, active, user_name, user_display_name, moderator_name)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?8)",
            params![guild_id, user_id, moderator_id, reason, now, user_name, user_display_name, moderator_name],
            1,
        )?;
        let row_id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(Some(row_id))
    })
    .await
}

pub async fn record_enrolled_ban(
    db: &Db,
    enrollment: Option<GuildEnrollment>,
    user_id: i64,
    moderator_id: Option<i64>,
    reason: &str,
) -> Result<()> {
    record_enrolled_ban_with_identity(
        db,
        enrollment,
        user_id,
        None,
        None,
        moderator_id,
        None,
        reason,
        None,
    )
    .await
    .map(|_| ())
}

#[allow(clippy::too_many_arguments)]
pub async fn record_enrolled_ban_with_identity(
    db: &Db,
    enrollment: Option<GuildEnrollment>,
    user_id: i64,
    user_name: Option<&str>,
    user_display_name: Option<&str>,
    moderator_id: Option<i64>,
    moderator_name: Option<&str>,
    reason: &str,
    occurred_at: Option<&str>,
) -> Result<i64> {
    let (now, reason) = (
        occurred_at.map(str::to_owned).unwrap_or_else(now_str),
        reason.to_string(),
    );
    let user_name = user_name.map(str::to_owned);
    let user_display_name = user_display_name.map(str::to_owned);
    let moderator_name = moderator_name.map(str::to_owned);
    let enrollment = enrollment.ok_or(EnrollmentChanged)?;
    let guild_id = enrollment.guild_id;
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_read()?;
        super::enrollments::verify_enrollment_conn(&tx, &enrollment)?;
        super::guild_stores::refuse_fenced_write(&tx, enrollment.guild_id)?;
        tx.ensure_mutation_receipt()?;
        tx.execute_rows(
            "INSERT INTO bans (guild_id, user_id, moderator_id, reason, created_at, active, user_name, user_display_name, moderator_name)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?8)",
            params![guild_id, user_id, moderator_id, reason, now, user_name, user_display_name, moderator_name], 1,
        )?;
        let row_id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(row_id)
    })
    .await
}

/// Record a gateway ban without creating a second active row.
///
/// A bot echo inserts the one active row when none exists, and does not insert a
/// `member_ban` event. A manual delivery inserts one active row and one
/// `member_ban` event in the same transaction. A later delivery for an active
/// ban returns the existing row and does not change its actor or reason.
#[allow(clippy::too_many_arguments)]
pub async fn record_gateway_ban(
    db: &Db,
    enrollment: Option<GuildEnrollment>,
    user_id: i64,
    user_name: Option<&str>,
    user_display_name: Option<&str>,
    occurred_at: Option<&str>,
    owned_by_bot: bool,
) -> Result<GatewayBanWrite> {
    let now = occurred_at.map(str::to_owned).unwrap_or_else(now_str);
    let reason = GATEWAY_BAN_REASON.to_owned();
    let user_name = user_name.map(str::to_owned);
    let user_display_name = user_display_name.map(str::to_owned);
    let enrollment = enrollment.ok_or(EnrollmentChanged)?;
    let guild_id = enrollment.guild_id;
    db.with_conn(move |conn| {
        record_gateway_ban_conn(
            conn,
            &enrollment,
            guild_id,
            user_id,
            user_name,
            user_display_name,
            reason,
            now,
            owned_by_bot,
        )
    })
    .await
}

#[allow(clippy::too_many_arguments)]
fn record_gateway_ban_conn(
    conn: &mut Connection,
    enrollment: &GuildEnrollment,
    guild_id: i64,
    user_id: i64,
    user_name: Option<String>,
    user_display_name: Option<String>,
    reason: String,
    now: String,
    owned_by_bot: bool,
) -> Result<GatewayBanWrite> {
    let mut tx = conn.transaction_primary_read()?;
    super::enrollments::verify_enrollment_conn(&tx, enrollment)?;
    super::guild_stores::refuse_fenced_write(&tx, enrollment.guild_id)?;
    if let Some(row_id) = active_ban_id(&tx, guild_id, user_id)? {
        tx.finish_read()?;
        return Ok(GatewayBanWrite::Existing { row_id });
    }
    tx.ensure_mutation_receipt()?;
    match tx.execute_rows(
        "INSERT INTO bans (guild_id, user_id, moderator_id, reason, created_at, active, user_name, user_display_name, moderator_name)
         VALUES (?1, ?2, NULL, ?3, ?4, 1, ?5, ?6, NULL)",
        params![
            guild_id,
            user_id,
            reason.clone(),
            now.clone(),
            user_name,
            user_display_name
        ],
        1,
    ) {
        Ok(_) => {}
        Err(error) if active_ban_conflict(&error) => {
            drop(tx);
            return existing_active_ban(conn, enrollment, guild_id, user_id);
        }
        Err(error) => return Err(error),
    }
    let row_id = tx.last_insert_rowid();
    if owned_by_bot {
        tx.commit()?;
        return Ok(GatewayBanWrite::OwnedByBot { row_id });
    }
    let data = crate::ids::stringify_js_unsafe_integers(&serde_json::json!({
        "reason": reason,
        "source": "discord",
        "ban_id": row_id,
    }))
    .to_string();
    tx.query_row_write(
        "INSERT INTO events (guild_id, event_type, user_id, actor_id, data, created_at)
         VALUES (?1, 'member_ban', ?2, NULL, ?3, ?4)
         RETURNING id",
        params![guild_id, user_id, data, now],
        |row| Ok(row.get::<i64>(0)?),
    )?;
    tx.commit()?;
    Ok(GatewayBanWrite::Recorded { row_id })
}

fn active_ban_id(conn: &impl SqlExecutor, guild_id: i64, user_id: i64) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM bans WHERE guild_id=?1 AND user_id=?2 AND active=1 ORDER BY id LIMIT 1",
        params![guild_id, user_id],
        |row| Ok(row.get(0)?),
    )
    .optional()
}

fn existing_active_ban(
    conn: &mut Connection,
    enrollment: &GuildEnrollment,
    guild_id: i64,
    user_id: i64,
) -> Result<GatewayBanWrite> {
    let tx = conn.transaction_primary_read()?;
    super::enrollments::verify_enrollment_conn(&tx, enrollment)?;
    super::guild_stores::refuse_fenced_write(&tx, enrollment.guild_id)?;
    let row_id = active_ban_id(&tx, guild_id, user_id)?.ok_or_else(|| {
        anyhow::anyhow!("active ban conflict left no row for guild {guild_id} user {user_id}")
    })?;
    tx.finish_read()?;
    Ok(GatewayBanWrite::Existing { row_id })
}

fn active_ban_conflict(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        if let Some(libsql::Error::SqliteFailure(code, message)) = cause.downcast_ref() {
            return *code == 2067
                || message.contains("idx_bans_one_active")
                || message.contains("UNIQUE constraint failed");
        }
        let text = cause.to_string();
        text.contains("idx_bans_one_active") || text.contains("UNIQUE constraint failed")
    })
}

#[allow(clippy::too_many_arguments)]
pub async fn attribute_enrolled_ban(
    db: &Db,
    enrollment: Option<GuildEnrollment>,
    row_id: i64,
    user_id: i64,
    moderator_id: i64,
    moderator_name: Option<&str>,
    reason: &str,
) -> Result<()> {
    let enrollment = enrollment.ok_or(EnrollmentChanged)?;
    let moderator_name = moderator_name.map(str::to_owned);
    let reason = reason.to_owned();
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_read()?;
        super::enrollments::verify_enrollment_conn(&tx, &enrollment)?;
        super::guild_stores::refuse_fenced_write(&tx, enrollment.guild_id)?;
        tx.ensure_mutation_receipt()?;
        tx.execute_matching(
            "UPDATE bans SET moderator_id = ?1, moderator_name = ?2, reason = ?3
             WHERE id = ?4 AND guild_id = ?5 AND user_id = ?6 AND moderator_id IS NULL",
            params![
                moderator_id,
                moderator_name,
                reason,
                row_id,
                enrollment.guild_id,
                user_id
            ],
        )?;
        tx.commit()?;
        Ok(())
    })
    .await
}

pub async fn mark_enrolled_unbanned(
    db: &Db,
    enrollment: Option<GuildEnrollment>,
    user_id: i64,
) -> Result<()> {
    // Mirror of Discord ban state. Callers that already succeeded at Discord
    // `remove_ban` should treat errors here as non-fatal (log + continue);
    // `GUILD_BAN_REMOVE` on the bot also calls this to heal drift.
    let enrollment = enrollment.ok_or(EnrollmentChanged)?;
    let guild_id = enrollment.guild_id;
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_read()?;
        super::enrollments::verify_enrollment_conn(&tx, &enrollment)?;
        super::guild_stores::refuse_fenced_write(&tx, enrollment.guild_id)?;
        tx.ensure_mutation_receipt()?;
        tx.execute_matching(
            "UPDATE bans SET active = 0 WHERE guild_id = ?1 AND user_id = ?2 AND active = 1",
            params![guild_id, user_id],
        )?;
        tx.commit()?;
        Ok(())
    })
    .await
}

/// Warning deletion uses the same captured runtime identity as other mirror mutations.
pub async fn clear_enrolled_warns(
    db: &Db,
    enrollment: Option<GuildEnrollment>,
    user_id: i64,
) -> Result<usize> {
    let enrollment = enrollment.ok_or(EnrollmentChanged)?;
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_read()?;
        super::enrollments::verify_enrollment_conn(&tx, &enrollment)?;
        super::guild_stores::refuse_fenced_write(&tx, enrollment.guild_id)?;
        tx.prepare_reclaim()?;
        let count = tx.execute_reclaim(
            "DELETE FROM warns WHERE guild_id=?1 AND user_id=?2",
            params![enrollment.guild_id, user_id],
        )?;
        tx.commit()?;
        Ok(count)
    })
    .await
}

#[cfg(test)]
mod query_plan_tests {
    use super::*;

    #[test]
    fn status_dashboard_and_pages_use_bounded_index_searches() {
        let path =
            std::env::temp_dir().join(format!("hot_plans_{:032x}.db", rand::random::<u128>()));
        let db = Db::open(&path, 2).unwrap();
        db.with_conn_sync(|conn| {
            // Both !stats/!status and WebUI stats call guild_stats_conn.
            for sql in [EVENT_STATS_SQL, WARN_STATS_SQL, BAN_STATS_SQL] {
                crate::db::tests::assert_index_plan(conn, sql, params![42], false);
            }
            for event_type in [None, Some("automod")] {
                for user_id in [None, Some(99)] {
                    let (sql, args) = event_query(42, event_type, user_id, 50);
                    crate::db::tests::assert_index_plan(
                        conn,
                        &sql,
                        params_from_iter(args),
                        user_id.is_none(),
                    );
                    if user_id.is_some() {
                        let (_, args) = event_query(42, event_type, user_id, 50);
                        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
                        let plan = stmt
                            .query_map(params_from_iter(args), |r| Ok(r.get::<String>(3)?))?
                            .collect::<Result<Vec<_>>>()?;
                        assert!(
                            plan.iter()
                                .any(|line| line.contains("SEARCH events")
                                    && line.contains("user_id=?")),
                            "user filter must constrain the index search: {plan:?}"
                        );
                    }
                }
            }
            crate::db::tests::assert_index_plan(conn, BAN_PAGE_SQL, params![42, 50], true);
            Ok(())
        })
        .unwrap();
        drop(db);
        let _ = std::fs::remove_file(path);
    }
}
