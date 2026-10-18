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
    /// An active row provisionally covers this delivery. Id-less observations
    /// remain available for late audit reconciliation; attribution is unchanged.
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
/// insert. An active row whose `operation_id` is this attempt is the gateway echo of
/// this same ban; the command fills that row instead of inserting another. An older
/// active row with any other id, including none, is ended and kept as history. `Some`
/// means a row was inserted or filled and the caller may publish. `None` means do not
/// publish.
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
    operation_id: Option<&str>,
) -> Result<Option<i64>> {
    let (now, reason) = (
        occurred_at.map(str::to_owned).unwrap_or_else(now_str),
        reason.to_string(),
    );
    let user_name = user_name.map(str::to_owned);
    let user_display_name = user_display_name.map(str::to_owned);
    let moderator_name = moderator_name.map(str::to_owned);
    let operation_id = operation_id.filter(|id| !id.is_empty()).map(str::to_owned);
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
        let same_attempt: Option<i64> = match operation_id.as_deref() {
            Some(operation_id) => tx
                .query_row(
                    "SELECT id FROM bans
                     WHERE guild_id=?1 AND user_id=?2 AND active=1
                       AND operation_id = ?3
                     ORDER BY id LIMIT 1",
                    params![guild_id, user_id, operation_id],
                    |row| Ok(row.get(0)?),
                )
                .optional()?,
            None => None,
        };
        tx.ensure_mutation_receipt()?;
        if let Some(row_id) = same_attempt {
            let updated = tx.execute_matching(
                "UPDATE bans SET moderator_id=?1, moderator_name=?2, reason=?3,
                    user_name=COALESCE(?4, user_name),
                    user_display_name=COALESCE(?5, user_display_name),
                    created_at=?6
                 WHERE id=?7 AND guild_id=?8 AND user_id=?9 AND active=1 AND operation_id = ?10",
                params![
                    moderator_id,
                    moderator_name,
                    reason,
                    user_name,
                    user_display_name,
                    now,
                    row_id,
                    guild_id,
                    user_id,
                    operation_id
                ],
            )?;
            anyhow::ensure!(updated == 1, "gateway ban echo was no longer this attempt");
            tx.commit()?;
            return Ok(Some(row_id));
        }
        tx.execute_matching(
            "UPDATE bans SET active=0 WHERE guild_id=?1 AND user_id=?2 AND active=1",
            params![guild_id, user_id],
        )?;
        tx.execute_rows(
            "INSERT INTO bans (guild_id, user_id, moderator_id, reason, created_at, active, user_name, user_display_name, moderator_name, operation_id)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?8, ?9)",
            params![guild_id, user_id, moderator_id, reason, now, user_name, user_display_name, moderator_name, operation_id],
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
/// `member_ban` event. `operation_id` is saved on that row so only the same ban
/// attempt can fill it. A manual delivery inserts one active row and one
/// `member_ban` event in the same transaction, with no operation id. A repeat
/// delivery for the same ban returns the existing row. A stale active mirror from
/// an earlier ban cycle is ended and replaced when `member_was_banned` is false,
/// an intervening unban was observed, the row is already attributed to a prior
/// manual ban, or a bot echo belongs to a different attempt.
#[allow(clippy::too_many_arguments)]
pub async fn record_gateway_ban(
    db: &Db,
    enrollment: Option<GuildEnrollment>,
    user_id: i64,
    user_name: Option<&str>,
    user_display_name: Option<&str>,
    occurred_at: Option<&str>,
    owned_by_bot: bool,
    operation_id: Option<&str>,
    member_was_banned: bool,
) -> Result<GatewayBanWrite> {
    record_gateway_ban_with_audit(
        db,
        enrollment,
        user_id,
        user_name,
        user_display_name,
        occurred_at,
        owned_by_bot,
        operation_id,
        member_was_banned,
        None,
    )
    .await
}

/// Known identities are supplied by callers that already own the action match.
/// Pending audit deliveries may distinguish a newer ban, but are attached only
/// by reconciliation against persisted observations.
pub enum GatewayBanAuditEntries {
    Known(Option<i64>),
    Pending(Vec<i64>),
    /// The bot sampled this fence while this callback was registered in flight.
    Observed {
        pending: Vec<i64>,
        audit_after_ms: i64,
    },
}

impl From<Option<i64>> for GatewayBanAuditEntries {
    fn from(entry: Option<i64>) -> Self {
        Self::Known(entry)
    }
}

/// Like [`record_gateway_ban`], but accepts the Discord audit-log entry id for this delivery when known.
#[allow(clippy::too_many_arguments)]
pub async fn record_gateway_ban_with_audit(
    db: &Db,
    enrollment: Option<GuildEnrollment>,
    user_id: i64,
    user_name: Option<&str>,
    user_display_name: Option<&str>,
    occurred_at: Option<&str>,
    owned_by_bot: bool,
    operation_id: Option<&str>,
    member_was_banned: bool,
    audit_entries: impl Into<GatewayBanAuditEntries>,
) -> Result<GatewayBanWrite> {
    let (gateway_audit_entry_id, pending_audit_entries, audit_after_ms) = match audit_entries.into()
    {
        GatewayBanAuditEntries::Known(entry) => (entry, Vec::new(), 0),
        GatewayBanAuditEntries::Pending(entries) => (None, entries, 0),
        GatewayBanAuditEntries::Observed {
            pending,
            audit_after_ms,
        } => (None, pending, audit_after_ms),
    };
    let now = occurred_at.map(str::to_owned).unwrap_or_else(now_str);
    let reason = GATEWAY_BAN_REASON.to_owned();
    let user_name = user_name.map(str::to_owned);
    let user_display_name = user_display_name.map(str::to_owned);
    let operation_id = if owned_by_bot {
        operation_id.filter(|id| !id.is_empty()).map(str::to_owned)
    } else {
        None
    };
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
            operation_id,
            member_was_banned,
            gateway_audit_entry_id,
            pending_audit_entries,
            audit_after_ms,
        )
    })
    .await
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveBanMirror {
    id: i64,
    reason: Option<String>,
    /// Gateway placeholder text in storage with no moderator yet; distinct from a
    /// moderator who literally chose the same display string.
    gateway_reason_unset: bool,
    moderator_id: Option<i64>,
    operation_id: Option<String>,
    created_at: String,
}

fn load_active_ban_mirror(
    conn: &impl SqlExecutor,
    guild_id: i64,
    user_id: i64,
) -> Result<Option<ActiveBanMirror>> {
    conn.query_row(
        "SELECT id, reason, moderator_id, operation_id, created_at
         FROM bans WHERE guild_id=?1 AND user_id=?2 AND active=1 ORDER BY id LIMIT 1",
        params![guild_id, user_id],
        |row| {
            let reason: Option<String> = row.get(1)?;
            let moderator_id: Option<i64> = row.get(2)?;
            let operation_id: Option<String> = row.get(3)?;
            let gateway_reason_unset =
                reason.as_deref() == Some(GATEWAY_BAN_REASON) && moderator_id.is_none();
            Ok(ActiveBanMirror {
                id: row.get(0)?,
                reason,
                gateway_reason_unset,
                moderator_id,
                operation_id,
                created_at: row.get(4)?,
            })
        },
    )
    .optional()
}

fn unban_observed_after_mirror(
    conn: &impl SqlExecutor,
    guild_id: i64,
    user_id: i64,
    mirror_created_at: &str,
) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM events
            WHERE guild_id=?1 AND user_id=?2 AND event_type='member_unban'
              AND created_at > ?3
         )",
        params![guild_id, user_id, mirror_created_at],
        |row| Ok(row.get::<i64>(0)? != 0),
    )
}

fn manual_member_ban_event_exists_for_mirror(
    conn: &impl SqlExecutor,
    guild_id: i64,
    user_id: i64,
    row_id: i64,
) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM events
            WHERE guild_id=?1 AND user_id=?2 AND event_type='member_ban'
              AND json_valid(data) AND json_extract(data, '$.ban_id') = ?3
         )",
        params![guild_id, user_id, row_id],
        |row| Ok(row.get::<i64>(0)? != 0),
    )
}

fn gateway_placeholder_mirror(mirror: &ActiveBanMirror) -> bool {
    mirror.gateway_reason_unset && mirror.operation_id.is_none()
}

fn bot_owned_mirror_without_manual_event(
    conn: &impl SqlExecutor,
    guild_id: i64,
    user_id: i64,
    mirror: &ActiveBanMirror,
) -> Result<bool> {
    if mirror.operation_id.is_none()
        || mirror.moderator_id.is_some()
        || !mirror.gateway_reason_unset
    {
        return Ok(false);
    }
    Ok(!manual_member_ban_event_exists_for_mirror(
        conn, guild_id, user_id, mirror.id,
    )?)
}

fn bot_echo_matches_mirror(
    conn: &impl SqlExecutor,
    guild_id: i64,
    user_id: i64,
    mirror: &ActiveBanMirror,
    operation_id: &Option<String>,
    observed_at: &str,
) -> Result<bool> {
    match operation_id {
        Some(operation_id) if mirror.operation_id.as_deref() == Some(operation_id.as_str()) => {
            Ok(true)
        }
        Some(_) => {
            if manual_member_ban_event_exists_for_mirror(conn, guild_id, user_id, mirror.id)? {
                return Ok(true);
            }
            if completed_bot_gateway_mirror(mirror) {
                return Ok(true);
            }
            if mirror.operation_id.is_some() && mirror.created_at.as_str() > observed_at {
                return Ok(true);
            }
            if mirror.created_at.as_str() >= observed_at {
                return Ok(true);
            }
            Ok(mirror.operation_id.is_none()
                && mirror.moderator_id.is_some()
                && !mirror.gateway_reason_unset)
        }
        None if gateway_placeholder_mirror(mirror) => Ok(true),
        None => Ok(mirror.operation_id.is_some() && mirror.moderator_id.is_some()),
    }
}

fn parse_gateway_audit_entry_id_text(raw: Option<String>) -> Result<Option<i64>> {
    let Some(raw) = raw.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let digits = raw.trim().trim_matches('"');
    if digits.is_empty() {
        return Ok(None);
    }
    let parsed = digits
        .parse::<u64>()
        .map_err(|error| anyhow::anyhow!("audit_entry_id: {error}"))?;
    Ok(Some(i64::try_from(parsed).map_err(|_| {
        anyhow::anyhow!("audit_entry_id out of range")
    })?))
}

fn gateway_audit_entry_id_for_mirror(
    conn: &impl SqlExecutor,
    guild_id: i64,
    user_id: i64,
    row_id: i64,
) -> Result<Option<i64>> {
    let raw = conn
        .query_row(
            "SELECT CAST(json_extract(data, '$.audit_entry_id') AS TEXT)
         FROM events
         WHERE guild_id=?1 AND user_id=?2 AND event_type='member_ban'
           AND json_valid(data) AND json_extract(data, '$.ban_id') = ?3
         ORDER BY id DESC LIMIT 1",
            params![guild_id, user_id, row_id],
            |row| Ok(row.get::<Option<String>>(0)?),
        )
        .optional()?;
    parse_gateway_audit_entry_id_text(raw.flatten())
}

/// Gateway ban redeliveries for the same Discord ban usually arrive soon after the row.
const GATEWAY_MANUAL_REDELIVERY_WINDOW_SECS: i64 = 60;

fn gateway_manual_redelivery_within_window(
    mirror_created_at: &str,
    incoming_at: &str,
) -> Result<bool> {
    let mirror = chrono::DateTime::parse_from_rfc3339(mirror_created_at)
        .map_err(|error| anyhow::anyhow!("mirror created_at: {error}"))?;
    let incoming = chrono::DateTime::parse_from_rfc3339(incoming_at)
        .map_err(|error| anyhow::anyhow!("incoming gateway time: {error}"))?;
    Ok(incoming <= mirror + chrono::Duration::seconds(GATEWAY_MANUAL_REDELIVERY_WINDOW_SECS))
}

fn gateway_ban_row_for_audit_entry(
    conn: &impl SqlExecutor,
    guild_id: i64,
    user_id: i64,
    audit_entry_id: i64,
) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT CAST(json_extract(data, '$.ban_id') AS INTEGER)
         FROM events
         WHERE guild_id=?1 AND user_id=?2 AND event_type='member_ban'
           AND json_valid(data)
           AND CAST(json_extract(data, '$.audit_entry_id') AS TEXT) = CAST(?3 AS TEXT)
         ORDER BY id DESC LIMIT 1",
        params![guild_id, user_id, audit_entry_id],
        |row| Ok(row.get(0)?),
    )
    .optional()
}

fn gateway_audit_entry_id_known_for_user(
    conn: &impl SqlExecutor,
    guild_id: i64,
    user_id: i64,
    audit_entry_id: i64,
) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM events
            WHERE guild_id=?1 AND user_id=?2 AND event_type='member_ban'
              AND json_valid(data)
              AND CAST(json_extract(data, '$.audit_entry_id') AS TEXT) = CAST(?3 AS TEXT)
         )",
        params![guild_id, user_id, audit_entry_id],
        |row| Ok(row.get::<i64>(0)? != 0),
    )
}

/// Whether two manual gateway deliveries are the same Discord ban action.
///
/// Identity comes from the audit-log entry id persisted on the mirror's `member_ban` event.
/// When the incoming delivery lacks an id, a bounded delivery-time window is used
/// as a fallback even if the mirror has already received its audit identity.
/// A known audit id that already has a `member_ban` event is treated as a replay even when
/// the active mirror belongs to a later ban.
fn gateway_deliveries_are_same(
    conn: &impl SqlExecutor,
    guild_id: i64,
    user_id: i64,
    mirror: &ActiveBanMirror,
    incoming_at: &str,
    incoming_audit_entry_id: Option<i64>,
) -> Result<bool> {
    let mirror_audit_entry_id =
        gateway_audit_entry_id_for_mirror(conn, guild_id, user_id, mirror.id)?;
    match (incoming_audit_entry_id, mirror_audit_entry_id) {
        (Some(incoming), Some(stored)) if incoming == stored => Ok(true),
        (Some(incoming), Some(_stored)) => Ok(gateway_audit_entry_id_known_for_user(
            conn, guild_id, user_id, incoming,
        )?),
        (None, _) => gateway_manual_redelivery_within_window(&mirror.created_at, incoming_at),
        (Some(incoming), None) => {
            if gateway_audit_entry_id_known_for_user(conn, guild_id, user_id, incoming)? {
                Ok(true)
            } else {
                Ok(false)
            }
        }
    }
}

fn completed_bot_gateway_mirror(mirror: &ActiveBanMirror) -> bool {
    mirror.operation_id.is_some() && mirror.moderator_id.is_some()
}

fn same_gateway_delivery_as_mirror(
    conn: &impl SqlExecutor,
    mirror: &ActiveBanMirror,
    guild_id: i64,
    user_id: i64,
    incoming_at: &str,
    incoming_audit_entry_id: Option<i64>,
) -> Result<bool> {
    let manual = manual_member_ban_event_exists_for_mirror(conn, guild_id, user_id, mirror.id)?;
    let completed_bot = completed_bot_gateway_mirror(mirror);
    if !manual && !completed_bot {
        return Ok(false);
    }
    gateway_deliveries_are_same(
        conn,
        guild_id,
        user_id,
        mirror,
        incoming_at,
        incoming_audit_entry_id,
    )
}

fn replace_manual_gateway_mirror(
    conn: &impl SqlExecutor,
    mirror: &ActiveBanMirror,
    guild_id: i64,
    user_id: i64,
    member_was_banned: bool,
    incoming_at: &str,
    incoming_audit_entry_id: Option<i64>,
) -> Result<bool> {
    if bot_owned_mirror_without_manual_event(conn, guild_id, user_id, mirror)? {
        if mirror.created_at.as_str() > incoming_at {
            return Ok(false);
        }
        return Ok(true);
    }
    if gateway_placeholder_mirror(mirror) {
        if unban_observed_after_mirror(conn, guild_id, user_id, &mirror.created_at)? {
            return Ok(true);
        }
        if manual_member_ban_event_exists_for_mirror(conn, guild_id, user_id, mirror.id)? {
            if gateway_deliveries_are_same(
                conn,
                guild_id,
                user_id,
                mirror,
                incoming_at,
                incoming_audit_entry_id,
            )? {
                return Ok(false);
            }
            return Ok(true);
        }
        if !member_was_banned {
            return Ok(true);
        }
        return Ok(false);
    }
    if unban_observed_after_mirror(conn, guild_id, user_id, &mirror.created_at)? {
        return Ok(true);
    }
    if same_gateway_delivery_as_mirror(
        conn,
        mirror,
        guild_id,
        user_id,
        incoming_at,
        incoming_audit_entry_id,
    )? {
        return Ok(false);
    }
    let manual = manual_member_ban_event_exists_for_mirror(conn, guild_id, user_id, mirror.id)?;
    if manual
        && let (Some(incoming), Some(stored)) = (
            incoming_audit_entry_id,
            gateway_audit_entry_id_for_mirror(conn, guild_id, user_id, mirror.id)?,
        )
        && incoming != stored
    {
        return Ok(true);
    }
    if manual && member_was_banned {
        return Ok(true);
    }
    Ok(!member_was_banned)
}

fn member_ban_event_json_lacks_audit_entry_id(data: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
        return false;
    };
    match value.get("audit_entry_id") {
        None | Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::String(text)) => text.trim().is_empty(),
        Some(serde_json::Value::Number(number)) => number.as_i64().is_none_or(|id| id <= 0),
        _ => false,
    }
}

/// Context delivered with the exact manual-ban audit identity.
#[derive(Clone)]
pub struct GatewayBanAuditAttribution {
    pub moderator_id: i64,
    pub moderator_name: Option<String>,
    pub reason: String,
}

fn attribute_gateway_ban_audit_conn(
    tx: &mut crate::db_compat::Transaction,
    guild_id: i64,
    user_id: i64,
    audit_entry_id: i64,
    attribution: Option<&GatewayBanAuditAttribution>,
) -> Result<()> {
    let Some(attribution) = attribution else {
        return Ok(());
    };
    tx.ensure_mutation_receipt()?;
    tx.execute_matching(
        "UPDATE bans SET moderator_id=?1,
           moderator_name=CASE WHEN moderator_id=?1 THEN COALESCE(?2, moderator_name) ELSE ?2 END,
           reason=?3
         WHERE guild_id=?4 AND user_id=?5 AND operation_id IS NULL
           AND id IN (SELECT CAST(json_extract(data, '$.ban_id') AS INTEGER)
             FROM events WHERE guild_id=?4 AND user_id=?5 AND event_type='member_ban'
               AND actor_id IS NULL AND json_valid(data)
               AND json_extract(data, '$.source')='discord'
               AND CAST(json_extract(data, '$.audit_entry_id') AS TEXT)=CAST(?6 AS TEXT))",
        params![
            attribution.moderator_id,
            attribution.moderator_name.clone(),
            attribution.reason.clone(),
            guild_id,
            user_id,
            audit_entry_id
        ],
    )?;
    // HTTP lookups leave the event actor unresolved. Store the exact actor
    // atomically with the ban attribution so later replays cannot replace it.
    tx.execute_matching(
        "UPDATE events SET actor_id=?1
         WHERE guild_id=?2 AND user_id=?3 AND event_type='member_ban'
           AND actor_id IS NULL AND json_valid(data)
           AND json_extract(data, '$.source')='discord'
           AND CAST(json_extract(data, '$.audit_entry_id') AS TEXT)=CAST(?4 AS TEXT)
           AND CAST(json_extract(data, '$.ban_id') AS INTEGER) IN (
             SELECT id FROM bans WHERE guild_id=?2 AND user_id=?3
               AND operation_id IS NULL AND moderator_id=?1)",
        params![attribution.moderator_id, guild_id, user_id, audit_entry_id],
    )?;
    Ok(())
}

/// Persist a user's failed-observation fence before its in-memory history is
/// released. This only raises ambiguity cutoffs; it never invents a saved ban or
/// changes confirmed audit facts. Repeating after an uncertain commit is safe.
/// The caller keeps the live fence until commit and waits for earlier writes and
/// the five-second matching window to finish before retiring it.
pub async fn persist_gateway_ban_failure_fence(
    db: &Db,
    enrollment: GuildEnrollment,
    user_id: i64,
    cutoff_ms: i64,
) -> Result<()> {
    const PENDING: &str = "guild_id=?1 AND user_id=?2 AND event_type='member_ban'
        AND json_valid(data) AND json_extract(data, '$.ban_id') IS NOT NULL
        AND (COALESCE(json_extract(data, '$.audit_after_ms'), 0) < ?3
          OR EXISTS (SELECT 1 FROM json_each(events.data, '$.gateway_observations')
                     WHERE COALESCE(json_extract(value, '$.audit_after_ms'), 0) < ?3))";
    loop {
        let enrollment = enrollment.clone();
        let changed = db.with_conn(move |conn| {
            let mut tx = conn.transaction_primary_read()?;
            super::enrollments::verify_enrollment_conn(&tx, &enrollment)?;
            super::guild_stores::refuse_fenced_write(&tx, enrollment.guild_id)?;
            let pending: bool = tx.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM events INDEXED BY idx_events_user WHERE {PENDING})"),
                params![enrollment.guild_id, user_id, cutoff_ms],
                |row| Ok(row.get(0)?),
            )?;
            if !pending {
                tx.finish_read()?;
                return Ok(0);
            }
            tx.ensure_mutation_receipt()?;
            // Bound both the growth reservation and remote transaction lifetime.
            // A per-event Hrana request loop cannot flush a long user history.
            // Confirmed parents can still contain unresolved split observations.
            let changed = tx.execute_matching(
                &format!("UPDATE events SET data=json_set(
                    CASE WHEN json_type(data, '$.gateway_observations')='array' THEN
                        json_set(data, '$.gateway_observations',
                            (SELECT json_group_array(json_set(value, '$.audit_after_ms',
                                MAX(COALESCE(json_extract(value, '$.audit_after_ms'), 0), ?3)))
                             FROM json_each(events.data, '$.gateway_observations')))
                    ELSE data END,
                    '$.audit_after_ms', MAX(COALESCE(json_extract(data, '$.audit_after_ms'), 0), ?3))
                    WHERE id IN (SELECT id FROM events INDEXED BY idx_events_user
                                 WHERE {PENDING} LIMIT 32)"),
                params![enrollment.guild_id, user_id, cutoff_ms],
            )?;
            tx.commit()?;
            Ok(changed)
        }).await?;
        if changed < 32 {
            return Ok(());
        }
    }
}

/// Reconcile inside an existing DB worker after the bot has closed the audit's
/// match window and checked its live observation fence.
pub fn reconcile_gateway_manual_ban_audit_entry_conn(
    conn: &mut Connection,
    enrollment: &GuildEnrollment,
    guild_id: i64,
    user_id: i64,
    audit_entry_id: i64,
    attribution: Option<&GatewayBanAuditAttribution>,
) -> Result<bool> {
    if enrollment.guild_id != guild_id {
        return Err(EnrollmentChanged.into());
    }
    if audit_entry_id <= 0 {
        return Ok(false);
    }
    let mut tx = conn.transaction_primary_read()?;
    super::enrollments::verify_enrollment_conn(&tx, enrollment)?;
    super::guild_stores::refuse_fenced_write(&tx, enrollment.guild_id)?;
    if gateway_audit_entry_id_known_for_user(&tx, guild_id, user_id, audit_entry_id)? {
        if attribution.is_some() {
            attribute_gateway_ban_audit_conn(
                &mut tx,
                guild_id,
                user_id,
                audit_entry_id,
                attribution,
            )?;
            tx.commit()?;
        } else {
            tx.finish_read()?;
        }
        return Ok(true);
    }
    // A subject can have several id-less ban cycles. The snowflake gives the
    // audit action's time, not its delivery time. Only reconcile one nearby
    // gateway observation after that action; ambiguity (including an attributed neighbor)
    // must leave the identity unresolved rather than select the oldest row.
    let audit_at_ms = (audit_entry_id >> 22) + 1_420_070_400_000;
    let pending = {
        let mut statement = tx.prepare(
            "SELECT id, data, created_at FROM events
             WHERE guild_id=?1 AND user_id=?2 AND event_type='member_ban'
               AND json_valid(data)
               AND json_extract(data, '$.ban_id') IS NOT NULL
               AND (CAST(ROUND((julianday(created_at) - 2440587.5) * 86400000) AS INTEGER)
                   BETWEEN ?3 AND ?3 + 5000
                 OR EXISTS (SELECT 1 FROM json_each(data, '$.gateway_observations') AS observation
                    WHERE json_extract(observation.value, '$.at_ms') BETWEEN ?3 AND ?3 + 5000)
                 OR json_extract(data, '$.observations_lost_through') >= ?3)
             LIMIT 2",
        )?;
        statement
            .query_map(params![guild_id, user_id, audit_at_ms], |row| {
                Ok((
                    row.get::<i64>(0)?,
                    row.get::<String>(1)?,
                    row.get::<String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>>>()?
    };
    let [(event_id, data, created_at)] = pending.as_slice() else {
        tx.finish_read()?;
        return Ok(false);
    };
    let mut event: serde_json::Value = serde_json::from_str(data)?;
    if event["observations_lost_through"]
        .as_i64()
        .is_some_and(|at| at >= audit_at_ms)
    {
        tx.finish_read()?;
        return Ok(false);
    }
    let original_at = chrono::DateTime::parse_from_rfc3339(created_at)?.timestamp_millis();
    let original_matches = (0..=5000).contains(&(original_at - audit_at_ms));
    let mut observations = event["gateway_observations"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let matching: Vec<_> = observations
        .iter()
        .enumerate()
        .filter(|(_, observation)| {
            observation["at_ms"]
                .as_i64()
                .is_some_and(|at| (0..=5000).contains(&(at - audit_at_ms)))
        })
        .map(|(index, _)| index)
        .collect();
    if usize::from(original_matches) + matching.len() != 1 {
        tx.finish_read()?;
        return Ok(false);
    }
    let candidate = matching
        .first()
        .map_or(&event, |index| &observations[*index]);
    if candidate["audit_after_ms"]
        .as_i64()
        .is_some_and(|after| audit_at_ms <= after)
    {
        tx.finish_read()?;
        return Ok(false);
    }
    if let Some(index) = matching.first() {
        let at_ms = observations.remove(*index)["at_ms"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("invalid ban observation"))?;
        let at = chrono::DateTime::from_timestamp_millis(at_ms)
            .ok_or_else(|| anyhow::anyhow!("invalid ban observation time"))?
            .to_rfc3339();
        let source_row = event["ban_id"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("invalid ban mirror"))?;
        // Split observations share their original mirror identity. Advance an
        // active split only in observation order, even if audits arrive out of
        // order. An unban or an independently newer ban must still win.
        let origin = event["gateway_observation_origin"]
            .as_i64()
            .unwrap_or(source_row);
        let active_row = tx
            .query_row(
                "SELECT bans.id, bans.created_at FROM bans JOIN events
               ON events.guild_id=bans.guild_id AND events.user_id=bans.user_id
               AND events.event_type='member_ban' AND json_valid(events.data)
               AND json_extract(events.data, '$.ban_id')=bans.id
             WHERE bans.guild_id=?1 AND bans.user_id=?2 AND bans.active=1
               AND COALESCE(json_extract(events.data, '$.gateway_observation_origin'), bans.id)=?3
             LIMIT 1",
                params![guild_id, user_id, origin],
                |row| Ok((row.get::<i64>(0)?, row.get::<String>(1)?)),
            )
            .optional()?;
        let replace = match active_row {
            Some((id, created))
                if chrono::DateTime::parse_from_rfc3339(&created)?.timestamp_millis() < at_ms =>
            {
                Some(id)
            }
            _ => None,
        };
        let active = replace.is_some();
        tx.ensure_mutation_receipt()?;
        if let Some(id) = replace {
            tx.execute_matching("UPDATE bans SET active=0 WHERE id=?1", params![id])?;
        }
        tx.execute_rows(
            "INSERT INTO bans (guild_id, user_id, reason, created_at, active, user_name, user_display_name)
             SELECT guild_id, user_id, ?1, ?2, ?3, user_name, user_display_name FROM bans WHERE id=?4",
            params![GATEWAY_BAN_REASON, at.clone(), active, source_row], 1)?;
        let row_id = tx.last_insert_rowid();
        let new_event = crate::ids::stringify_js_unsafe_integers(&serde_json::json!({
            "reason": GATEWAY_BAN_REASON, "source": "discord", "ban_id": row_id, "audit_entry_id": audit_entry_id,
            "gateway_observation_origin": origin
        })).to_string();
        tx.query_row_write(
            "INSERT INTO events (guild_id, event_type, user_id, actor_id, data, created_at)
            VALUES (?1, 'member_ban', ?2, NULL, ?3, ?4) RETURNING id",
            params![guild_id, user_id, new_event, at],
            |row| Ok(row.get::<i64>(0)?),
        )?;
        event["gateway_observations"] = serde_json::json!(observations);
    } else {
        if !member_ban_event_json_lacks_audit_entry_id(data) {
            tx.finish_read()?;
            return Ok(false);
        }
        event["audit_entry_id"] = serde_json::json!(audit_entry_id);
        tx.ensure_mutation_receipt()?;
    }
    let data = crate::ids::stringify_js_unsafe_integers(&event).to_string();
    tx.execute_matching(
        "UPDATE events SET data=?1 WHERE id=?2",
        params![data, event_id],
    )?;
    // Attribute the exact identity in the same commit, including a split from
    // an earlier callback. No later lookup or still-running handler is needed.
    attribute_gateway_ban_audit_conn(&mut tx, guild_id, user_id, audit_entry_id, attribution)?;
    tx.commit()?;
    Ok(true)
}

/// Reconcile a manual-ban audit ID with a unique gateway ban for the same guild
/// and target in the five seconds following its snowflake timestamp. Ambiguous or old
/// observations stay unresolved. A retained Existing observation may become a
/// distinct ban. Returns true for an already known or reconciled ID.
pub async fn reconcile_gateway_manual_ban_audit_entry(
    db: &Db,
    enrollment: Option<GuildEnrollment>,
    guild_id: i64,
    user_id: i64,
    audit_entry_id: i64,
) -> Result<bool> {
    let enrollment = enrollment.ok_or(EnrollmentChanged)?;
    db.with_conn(move |conn| {
        reconcile_gateway_manual_ban_audit_entry_conn(
            conn,
            &enrollment,
            guild_id,
            user_id,
            audit_entry_id,
            None,
        )
    })
    .await
}

/// Find a ban split from this callback's original mirror and observation.
/// Ordinary redeliveries and unrelated newer bans are not attribution targets.
pub async fn gateway_ban_split_for_observation(
    db: &Db,
    enrollment: Option<GuildEnrollment>,
    user_id: i64,
    original_row_id: i64,
    observed_at_ms: i64,
) -> Result<Option<i64>> {
    let enrollment = enrollment.ok_or(EnrollmentChanged)?;
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_read()?;
        super::enrollments::verify_enrollment_conn(&tx, &enrollment)?;
        super::guild_stores::refuse_fenced_write(&tx, enrollment.guild_id)?;
        let matches = {
            let mut statement = tx.prepare(
                "SELECT CAST(json_extract(data, '$.ban_id') AS INTEGER) FROM events
                 WHERE guild_id=?1 AND user_id=?2 AND event_type='member_ban'
                   AND json_valid(data)
                   AND json_extract(data, '$.ban_id') != ?3
                   AND json_extract(data, '$.gateway_observation_origin') = (
                     SELECT COALESCE(json_extract(data, '$.gateway_observation_origin'), ?3)
                     FROM events WHERE guild_id=?1 AND user_id=?2 AND event_type='member_ban'
                       AND json_valid(data) AND json_extract(data, '$.ban_id')=?3
                     ORDER BY id DESC LIMIT 1)
                   AND CAST(ROUND((julianday(created_at) - 2440587.5) * 86400000) AS INTEGER)=?4
                 LIMIT 2",
            )?;
            statement
                .query_map(
                    params![
                        enrollment.guild_id,
                        user_id,
                        original_row_id,
                        observed_at_ms
                    ],
                    |row| Ok(row.get::<i64>(0)?),
                )?
                .collect::<Result<Vec<_>>>()?
        };
        tx.finish_read()?;
        Ok(match matches.as_slice() {
            [row_id] => Some(*row_id),
            _ => None,
        })
    })
    .await
}

/// Whether this audit entry id already belongs to a persisted gateway `member_ban` event.
pub async fn gateway_manual_ban_audit_entry_is_known(
    db: &Db,
    enrollment: Option<GuildEnrollment>,
    guild_id: i64,
    user_id: i64,
    audit_entry_id: i64,
) -> Result<bool> {
    let enrollment = enrollment.ok_or(EnrollmentChanged)?;
    if enrollment.guild_id != guild_id {
        return Err(EnrollmentChanged.into());
    }
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_read()?;
        super::enrollments::verify_enrollment_conn(&tx, &enrollment)?;
        super::guild_stores::refuse_fenced_write(&tx, enrollment.guild_id)?;
        let known = gateway_audit_entry_id_known_for_user(&tx, guild_id, user_id, audit_entry_id)?;
        tx.finish_read()?;
        Ok(known)
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
    operation_id: Option<String>,
    member_was_banned: bool,
    gateway_audit_entry_id: Option<i64>,
    pending_audit_entries: Vec<i64>,
    audit_after_ms: i64,
) -> Result<GatewayBanWrite> {
    let mut tx = conn.transaction_primary_read()?;
    super::enrollments::verify_enrollment_conn(&tx, enrollment)?;
    super::guild_stores::refuse_fenced_write(&tx, enrollment.guild_id)?;
    if !owned_by_bot
        && let Some(incoming) = gateway_audit_entry_id.filter(|id| *id > 0)
        && gateway_audit_entry_id_known_for_user(&tx, guild_id, user_id, incoming)?
        && load_active_ban_mirror(&tx, guild_id, user_id)?.is_none()
    {
        let row_id = gateway_ban_row_for_audit_entry(&tx, guild_id, user_id, incoming)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "known audit entry {incoming} has no ban row for guild {guild_id} user {user_id}"
                )
            })?;
        tx.finish_read()?;
        return Ok(GatewayBanWrite::Existing { row_id });
    }
    if let Some(mirror) = load_active_ban_mirror(&tx, guild_id, user_id)? {
        let keep = if owned_by_bot {
            bot_echo_matches_mirror(&tx, guild_id, user_id, &mirror, &operation_id, &now)?
        } else {
            // A new audit action after the saved observation defeats the
            // id-less redelivery fallback, even if the older ban never got its
            // audit. Do not attach this pending ID until after the row exists.
            let mut newer_audit = false;
            if !pending_audit_entries.is_empty() {
                let mirror_at =
                    chrono::DateTime::parse_from_rfc3339(&mirror.created_at)?.timestamp_millis();
                let observed_at = chrono::DateTime::parse_from_rfc3339(&now)?.timestamp_millis();
                for entry in &pending_audit_entries {
                    let audit_at = (*entry >> 22) + 1_420_070_400_000;
                    if *entry > 0
                        && audit_at > mirror_at
                        && (0..=5000).contains(&(observed_at - audit_at))
                        && !gateway_audit_entry_id_known_for_user(&tx, guild_id, user_id, *entry)?
                    {
                        newer_audit = true;
                        break;
                    }
                }
            }
            !newer_audit
                && !replace_manual_gateway_mirror(
                    &tx,
                    &mirror,
                    guild_id,
                    user_id,
                    member_was_banned,
                    &now,
                    gateway_audit_entry_id,
                )?
        };
        if keep {
            // Existing is a redelivery guess, not an action identity. Retain a
            // bounded observation on the existing event for a later audit.
            if !owned_by_bot && gateway_audit_entry_id.is_none() {
                let saved = tx
                    .query_row(
                        "SELECT id, data FROM events WHERE guild_id=?1 AND user_id=?2
                     AND event_type='member_ban' AND json_valid(data)
                     AND json_extract(data, '$.ban_id')=?3 ORDER BY id DESC LIMIT 1",
                        params![guild_id, user_id, mirror.id],
                        |row| Ok((row.get::<i64>(0)?, row.get::<String>(1)?)),
                    )
                    .optional()?;
                if let Some((id, data)) = saved {
                    let mut event: serde_json::Value = serde_json::from_str(&data)?;
                    let at = chrono::DateTime::parse_from_rfc3339(&now)?.timestamp_millis();
                    let original = chrono::DateTime::parse_from_rfc3339(&mirror.created_at)?
                        .timestamp_millis();
                    let mut observations = event["gateway_observations"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                    if at != original
                        && !observations.iter().any(|o| o["at_ms"].as_i64() == Some(at))
                    {
                        observations.push(
                            serde_json::json!({"at_ms": at, "audit_after_ms": audit_after_ms}),
                        );
                        if observations.len() > 8 {
                            let lost = observations.remove(0)["at_ms"].as_i64().unwrap_or(at);
                            event["observations_lost_through"] = serde_json::json!(
                                lost.max(event["observations_lost_through"].as_i64().unwrap_or(0))
                            );
                        }
                        event["gateway_observations"] = serde_json::json!(observations);
                        tx.ensure_mutation_receipt()?;
                        tx.execute_matching(
                            "UPDATE events SET data=?1 WHERE id=?2",
                            params![
                                crate::ids::stringify_js_unsafe_integers(&event).to_string(),
                                id
                            ],
                        )?;
                        tx.commit()?;
                        return Ok(GatewayBanWrite::Existing { row_id: mirror.id });
                    }
                    let candidate = if at == original {
                        &mut event
                    } else {
                        let index = observations
                            .iter()
                            .position(|o| o["at_ms"].as_i64() == Some(at))
                            .expect("existing observation");
                        &mut event["gateway_observations"][index]
                    };
                    if audit_after_ms > candidate["audit_after_ms"].as_i64().unwrap_or(0) {
                        candidate["audit_after_ms"] = serde_json::json!(audit_after_ms);
                        tx.ensure_mutation_receipt()?;
                        tx.execute_matching(
                            "UPDATE events SET data=?1 WHERE id=?2",
                            params![
                                crate::ids::stringify_js_unsafe_integers(&event).to_string(),
                                id
                            ],
                        )?;
                        tx.commit()?;
                        return Ok(GatewayBanWrite::Existing { row_id: mirror.id });
                    }
                }
            }
            tx.finish_read()?;
            return Ok(GatewayBanWrite::Existing { row_id: mirror.id });
        }
        tx.ensure_mutation_receipt()?;
        tx.execute_matching(
            "UPDATE bans SET active=0 WHERE guild_id=?1 AND user_id=?2 AND active=1",
            params![guild_id, user_id],
        )?;
    } else {
        tx.ensure_mutation_receipt()?;
    }
    match tx.execute_rows(
        "INSERT INTO bans (guild_id, user_id, moderator_id, reason, created_at, active, user_name, user_display_name, moderator_name, operation_id)
         VALUES (?1, ?2, NULL, ?3, ?4, 1, ?5, ?6, NULL, ?7)",
        params![
            guild_id,
            user_id,
            reason.clone(),
            now.clone(),
            user_name,
            user_display_name,
            operation_id
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
    let mut event = serde_json::json!({
        "reason": reason,
        "source": "discord",
        "ban_id": row_id,
        "audit_after_ms": audit_after_ms,
    });
    if let Some(entry_id) = gateway_audit_entry_id.filter(|id| *id > 0) {
        event["audit_entry_id"] = serde_json::json!(entry_id);
    }
    let data = crate::ids::stringify_js_unsafe_integers(&event).to_string();
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
            "UPDATE bans SET
               moderator_name = CASE WHEN moderator_id IS NULL THEN ?2
                                     ELSE COALESCE(moderator_name, ?2) END,
               reason = CASE WHEN moderator_id IS NULL THEN ?3 ELSE reason END,
               moderator_id = ?1
             WHERE id = ?4 AND guild_id = ?5 AND user_id = ?6
               AND (moderator_id IS NULL OR (moderator_id = ?1 AND moderator_name IS NULL))",
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

/// Records a gateway-observed unban in the moderation event log.
pub async fn record_gateway_unban_event(
    db: &Db,
    enrollment: Option<GuildEnrollment>,
    user_id: i64,
    occurred_at: Option<&str>,
) -> Result<()> {
    let now = occurred_at.map(str::to_owned).unwrap_or_else(now_str);
    let enrollment = enrollment.ok_or(EnrollmentChanged)?;
    let guild_id = enrollment.guild_id;
    let data = crate::ids::stringify_js_unsafe_integers(&serde_json::json!({
        "source": "discord",
    }))
    .to_string();
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_read()?;
        super::enrollments::verify_enrollment_conn(&tx, &enrollment)?;
        super::guild_stores::refuse_fenced_write(&tx, enrollment.guild_id)?;
        tx.ensure_mutation_receipt()?;
        tx.query_row_write(
            "INSERT INTO events (guild_id, event_type, user_id, actor_id, data, created_at)
             VALUES (?1, 'member_unban', ?2, NULL, ?3, ?4)
             RETURNING id",
            params![guild_id, user_id, data, now],
            |row| Ok(row.get::<i64>(0)?),
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
