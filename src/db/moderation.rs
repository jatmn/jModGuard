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
        let id = tx.query_row(
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

fn query_events_conn(
    conn: &impl SqlExecutor,
    guild_id: i64,
    event_type: Option<&str>,
    user_id: Option<i64>,
    limit: i64,
) -> Result<Vec<EventRow>> {
    let limit = limit.clamp(1, 200);
    let mut sql = String::from(
        "SELECT id, guild_id, event_type, user_id, actor_id, data, created_at
         FROM events WHERE guild_id = ?1",
    );
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
        let id = tx.query_row(
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
        conn.execute(
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
    #[serde(
        default,
        serialize_with = "crate::ids::ser_opt_i64_str",
        deserialize_with = "crate::ids::de_opt_i64"
    )]
    pub moderator_id: Option<i64>,
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
        conn.execute(
            "INSERT INTO bans (guild_id, user_id, moderator_id, reason, created_at, active)
             VALUES (?1, ?2, ?3, ?4, ?5, 1)",
            params![guild_id, user_id, moderator_id, reason, now],
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
        conn.execute(
            "UPDATE bans SET active = 0 WHERE guild_id = ?1 AND user_id = ?2 AND active = 1",
            params![guild_id, user_id],
        )?;
        Ok(())
    })
    .await
}

pub async fn list_bans(db: &Db, guild_id: i64, limit: i64) -> Result<Vec<BanRow>> {
    db.with_conn(move |conn| list_bans_conn(conn, guild_id, limit))
        .await
}

fn list_bans_conn(conn: &impl SqlExecutor, guild_id: i64, limit: i64) -> Result<Vec<BanRow>> {
    let limit = limit.clamp(1, 200);
    let mut stmt = conn.prepare(
        "SELECT id, guild_id, user_id, moderator_id, reason, created_at, active
             FROM bans WHERE guild_id = ?1 ORDER BY id DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![guild_id, limit], |r| {
        Ok(BanRow {
            id: r.get(0)?,
            guild_id: r.get(1)?,
            user_id: r.get(2)?,
            moderator_id: r.get(3)?,
            reason: r.get(4)?,
            created_at: r.get(5)?,
            active: r.get::<i64>(6)? != 0,
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
pub async fn guild_stats(db: &Db, guild_id: i64) -> Result<Stats> {
    db.with_conn(move |conn| guild_stats_conn(conn, guild_id))
        .await
}

fn guild_stats_conn(conn: &impl SqlExecutor, guild_id: i64) -> Result<Stats> {
    let one = |sql: &str| conn.query_row(sql, params![guild_id], |r| Ok(r.get::<i64>(0)?));
    Ok(Stats {
        events: one("SELECT COUNT(*) FROM events WHERE guild_id = ?1 AND (
                   event_type IN ('mod_action','timeout_add','timeout_remove',
                                  'member_ban','member_unban','member_kick')
                   OR CASE WHEN json_valid(data) THEN
                     (event_type = 'automod'
                      AND json_extract(data, '$.action') IN ('warn','timeout','kick','ban')
                      AND COALESCE(json_extract(data, '$.dry_run'), 0) = 0)
                     OR (event_type = 'plan_executed' AND json_type(data, '$.moderation') = 'true')
                   ELSE 0 END
                 )")?,
        warns: one("SELECT COUNT(*) FROM warns WHERE guild_id = ?1")?,
        bans: one("SELECT COUNT(*) FROM bans WHERE guild_id = ?1")?,
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
        let tx = conn.transaction_primary_read()?;
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
        let id = tx.query_row(
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
        let id = tx.query_row(
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

pub async fn record_enrolled_ban(
    db: &Db,
    enrollment: Option<GuildEnrollment>,
    user_id: i64,
    moderator_id: Option<i64>,
    reason: &str,
) -> Result<()> {
    let (now, reason) = (now_str(), reason.to_string());
    let enrollment = enrollment.ok_or(EnrollmentChanged)?;
    let guild_id = enrollment.guild_id;
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_read()?;
        super::enrollments::verify_enrollment_conn(&tx, &enrollment)?;
        super::guild_stores::refuse_fenced_write(&tx, enrollment.guild_id)?;
        tx.ensure_mutation_receipt()?;
        tx.execute(
            "INSERT INTO bans (guild_id, user_id, moderator_id, reason, created_at, active)
             VALUES (?1, ?2, ?3, ?4, ?5, 1)",
            params![guild_id, user_id, moderator_id, reason, now],
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
        tx.execute(
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
        tx.ensure_mutation_receipt()?;
        let count = tx.execute(
            "DELETE FROM warns WHERE guild_id=?1 AND user_id=?2",
            params![enrollment.guild_id, user_id],
        )?;
        tx.commit()?;
        Ok(count)
    })
    .await
}
