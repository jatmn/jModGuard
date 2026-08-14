use super::*;

// ---------------------------------------------------------------------------
// members
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemberRow {
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
    pub username: Option<String>,
    pub first_joined_at: Option<String>,
    pub last_joined_at: Option<String>,
    pub last_left_at: Option<String>,
    pub join_count: i64,
    pub message_count: i64,
    pub last_seen_at: Option<String>,
    pub currently_in: bool,
}

pub async fn member_joined(db: &Db, guild_id: i64, user_id: i64, username: &str) -> Result<()> {
    let (now, username) = (now_str(), username.to_string());
    db.with_conn(move |conn| {
        conn.execute(
            "INSERT INTO members (guild_id, user_id, username, first_joined_at, last_joined_at, last_left_at, join_count, currently_in)
             VALUES (?1, ?2, ?3, ?4, ?4, NULL, 1, 1)
             ON CONFLICT(guild_id, user_id) DO UPDATE SET
               username = excluded.username,
               last_joined_at = excluded.last_joined_at,
               first_joined_at = COALESCE(members.first_joined_at, excluded.first_joined_at),
               join_count = members.join_count + 1,
               currently_in = 1",
            params![guild_id, user_id, username, now],
        )?;
        Ok(())
    })
    .await
}

pub async fn member_left(db: &Db, guild_id: i64, user_id: i64) -> Result<()> {
    let now = now_str();
    db.with_conn(move |conn| {
        conn.execute(
            "UPDATE members SET last_left_at = ?3, currently_in = 0 WHERE guild_id = ?1 AND user_id = ?2",
            params![guild_id, user_id, now],
        )?;
        Ok(())
    })
    .await
}

pub async fn bump_message(db: &Db, guild_id: i64, user_id: i64, username: &str) -> Result<()> {
    let (now, username) = (now_str(), username.to_string());
    db.with_conn(move |conn| {
        conn.execute(
            "INSERT INTO members (guild_id, user_id, username, join_count, message_count, last_seen_at, currently_in)
             VALUES (?1, ?2, ?3, 0, 1, ?4, 1)
             ON CONFLICT(guild_id, user_id) DO UPDATE SET
               username = excluded.username,
               message_count = members.message_count + 1,
               last_seen_at = excluded.last_seen_at",
            params![guild_id, user_id, username, now],
        )?;
        Ok(())
    })
    .await
}

fn row_to_member(r: &Row) -> Result<MemberRow> {
    Ok(MemberRow {
        guild_id: r.get(0)?,
        user_id: r.get(1)?,
        username: r.get(2)?,
        first_joined_at: r.get(3)?,
        last_joined_at: r.get(4)?,
        last_left_at: r.get(5)?,
        join_count: r.get(6)?,
        message_count: r.get(7)?,
        last_seen_at: r.get(8)?,
        currently_in: r.get::<i64>(9)? != 0,
    })
}

const MEMBER_COLS: &str = "guild_id, user_id, username, first_joined_at, last_joined_at, last_left_at, join_count, message_count, last_seen_at, currently_in";

pub async fn member_row(db: &Db, guild_id: i64, user_id: i64) -> Result<Option<MemberRow>> {
    db.with_conn(move |conn| {
        conn.query_row(
            &format!("SELECT {MEMBER_COLS} FROM members WHERE guild_id = ?1 AND user_id = ?2"),
            params![guild_id, user_id],
            row_to_member,
        )
        .optional()
    })
    .await
}

pub async fn search_members(db: &Db, guild_id: i64, q: &str, limit: i64) -> Result<Vec<MemberRow>> {
    let limit = limit.clamp(1, 200);
    let (like, q, limit) = (format!("%{}%", q.to_lowercase()), q.to_string(), limit);
    db.with_conn(move |conn| {
        let mut stmt = conn.prepare(&format!(
            "SELECT {MEMBER_COLS} FROM members WHERE guild_id = ?1 AND (LOWER(COALESCE(username,'')) LIKE ?2 OR CAST(user_id AS TEXT) LIKE ?2)
             ORDER BY last_seen_at DESC LIMIT ?3"
        ))?;
        let rows = stmt.query_map(params![guild_id, like, limit], row_to_member)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        let _ = q;
        Ok(out)
    })
    .await
}

pub async fn backfill_member(db: &Db, guild_id: i64, user_id: i64, username: &str) -> Result<()> {
    let (now, username) = (now_str(), username.to_string());
    db.with_conn(move |conn| {
        conn.execute(
            "INSERT INTO members (guild_id, user_id, username, first_joined_at, last_joined_at, join_count, currently_in)
             VALUES (?1, ?2, ?3, ?4, ?4, 1, 1)
             ON CONFLICT(guild_id, user_id) DO UPDATE SET
               username = excluded.username, currently_in = 1,
               first_joined_at = COALESCE(members.first_joined_at, excluded.first_joined_at)",
            params![guild_id, user_id, username, now],
        )?;
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------
// messages
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub async fn insert_message(
    db: &Db,
    message_id: i64,
    guild_id: i64,
    channel_id: i64,
    author_id: i64,
    content: &str,
    attachments: &str,
    created_at: &str,
) -> Result<()> {
    let (content, attachments, created_at) = (
        content.to_string(),
        attachments.to_string(),
        created_at.to_string(),
    );
    db.with_conn(move |conn| {
        conn.execute(
            "INSERT OR REPLACE INTO messages (message_id, guild_id, channel_id, author_id, content, attachments, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![message_id, guild_id, channel_id, author_id, content, attachments, created_at],
        )?;
        Ok(())
    })
    .await
}

/// Persists one message's member counter, message row, and user cache in one
/// transaction. The gateway runs moderation before awaiting this best-effort
/// tracking operation, so a remote outage cannot delay enforcement by three
/// independent database deadlines.
#[allow(clippy::too_many_arguments)]
pub async fn record_message_activity(
    db: &Db,
    message_id: i64,
    guild_id: i64,
    channel_id: i64,
    author_id: i64,
    username: &str,
    global_name: Option<&str>,
    content: &str,
    attachments: &str,
    created_at: &str,
) -> Result<()> {
    let now = now_str();
    let (username, global_name, content, attachments, created_at) = (
        username.to_string(),
        global_name.map(str::to_string),
        content.to_string(),
        attachments.to_string(),
        created_at.to_string(),
    );
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        tx.execute(
            "INSERT INTO members (guild_id, user_id, username, join_count, message_count, last_seen_at, currently_in)
             VALUES (?1, ?2, ?3, 0, 1, ?4, 1)
             ON CONFLICT(guild_id, user_id) DO UPDATE SET
               username = excluded.username,
               message_count = members.message_count + 1,
               last_seen_at = excluded.last_seen_at",
            params![guild_id, author_id, username.clone(), now.clone()],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO messages (message_id, guild_id, channel_id, author_id, content, attachments, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![message_id, guild_id, channel_id, author_id, content, attachments, created_at],
        )?;
        tx.execute(
            "INSERT INTO users (user_id, username, global_name, avatar, updated_at)
             VALUES (?1, ?2, ?3, NULL, ?4)
             ON CONFLICT(user_id) DO UPDATE SET
               username = excluded.username,
               global_name = excluded.global_name,
               updated_at = excluded.updated_at
             WHERE users.username IS NOT excluded.username
                OR users.global_name IS NOT excluded.global_name",
            params![author_id, username, global_name, now],
        )?;
        tx.commit()
    })
    .await
}

pub async fn mark_edited_with_previous(
    db: &Db,
    message_id: i64,
    content: &str,
) -> Result<Option<(String, String)>> {
    let (now, content) = (now_str(), content.to_string());
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        let previous = tx
            .query_row(
                "SELECT content, attachments FROM messages WHERE message_id = ?1",
                params![message_id],
                |r| Ok((r.get::<String>(0)?, r.get::<String>(1)?)),
            )
            .optional()?;
        if previous.is_some() {
            tx.execute(
                "UPDATE messages SET content = ?1, edited_at = ?2 WHERE message_id = ?3",
                params![content, now, message_id],
            )?;
        }
        tx.commit()?;
        Ok(previous)
    })
    .await
}

pub async fn mark_deleted_with_previous(
    db: &Db,
    message_id: i64,
) -> Result<Option<(String, String)>> {
    let now = now_str();
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        let previous = tx
            .query_row(
                "SELECT content, attachments FROM messages WHERE message_id = ?1",
                params![message_id],
                |r| Ok((r.get::<String>(0)?, r.get::<String>(1)?)),
            )
            .optional()?;
        if previous.is_some() {
            tx.execute(
                "UPDATE messages SET deleted_at = ?1 WHERE message_id = ?2 AND deleted_at IS NULL",
                params![now, message_id],
            )?;
        }
        tx.commit()?;
        Ok(previous)
    })
    .await
}

pub async fn mark_deleted(db: &Db, message_id: i64) -> Result<()> {
    let now = now_str();
    db.with_conn(move |conn| {
        conn.execute(
            "UPDATE messages SET deleted_at = ?1 WHERE message_id = ?2 AND deleted_at IS NULL",
            params![now, message_id],
        )?;
        Ok(())
    })
    .await
}

pub async fn purge_old_messages(db: &Db) -> Result<usize> {
    db.with_conn(move |conn| {
        let guild_ids: Vec<i64> = {
            let mut stmt = conn.prepare("SELECT DISTINCT guild_id FROM messages")?;
            let rows = stmt.query_map((), |r| Ok(r.get::<i64>(0)?))?;
            let mut v = Vec::new();
            for row in rows {
                v.push(row?);
            }
            v
        };

        let mut total = 0usize;
        for guild_id in guild_ids {
            // One bounded primary transaction per guild makes policy + delete
            // atomic without holding the global writer lock across every
            // guild and hundreds of remote round trips.
            let tx = conn.transaction_immediate()?;
            let defaults = terminal_defaults_conn(&tx)?;
            let days = retention_days_for_guild(&tx, guild_id, &defaults)?;
            // 0 = keep forever (UI allows min 0).
            if days == 0 {
                tx.finish_read()?;
                continue;
            }
            let Some(duration) = chrono::Duration::try_days(days as i64) else {
                tracing::warn!(
                    "skipping invalid retention_days={days} for guild {guild_id}; \
                     set it to at most {MAX_RETENTION_DAYS}"
                );
                tx.finish_read()?;
                continue;
            };
            let cutoff = (Utc::now() - duration).to_rfc3339();
            let n = tx.execute(
                "DELETE FROM messages WHERE guild_id = ?1 AND created_at < ?2",
                params![guild_id, cutoff],
            )?;
            total += n;
            tx.commit()?;
        }
        Ok(total)
    })
    .await
}

/// Effective retention for a guild: field-level guild → global → config defaults.
fn retention_days_for_guild(
    conn: &impl SqlExecutor,
    guild_id: i64,
    defaults: &GuildSettings,
) -> Result<u32> {
    let guild = read_settings_override_conn(conn, guild_id)?;
    let global = read_settings_override_conn(conn, GLOBAL_SCOPE)?;
    Ok(resolve_settings(&guild, &global, defaults).retention_days)
}

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
    let limit = limit.clamp(1, 200);
    let event_type = event_type.map(|s| s.to_string());
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

    db.with_conn(move |conn| {
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
    })
    .await
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
    let limit = limit.clamp(1, 200);
    db.with_conn(move |conn| {
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
    })
    .await
}

// ---------------------------------------------------------------------------
// users
// ---------------------------------------------------------------------------

pub async fn upsert_user(
    db: &Db,
    user_id: i64,
    username: &str,
    global_name: Option<&str>,
    avatar: Option<&str>,
) -> Result<()> {
    let (now, username) = (now_str(), username.to_string());
    let (global_name, avatar) = (
        global_name.map(|s| s.to_string()),
        avatar.map(|s| s.to_string()),
    );
    db.with_conn(move |conn| {
        conn.execute(
            "INSERT INTO users (user_id, username, global_name, avatar, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(user_id) DO UPDATE SET
               username = excluded.username,
               global_name = excluded.global_name,
               avatar = excluded.avatar,
               updated_at = excluded.updated_at
             WHERE users.username IS NOT excluded.username
                OR users.global_name IS NOT excluded.global_name
                OR users.avatar IS NOT excluded.avatar",
            params![user_id, username, global_name, avatar, now],
        )?;
        Ok(())
    })
    .await
}

#[allow(dead_code)]
pub async fn get_user(db: &Db, user_id: i64) -> Result<Option<(String, Option<String>)>> {
    db.with_conn(move |conn| {
        conn.query_row(
            "SELECT username, avatar FROM users WHERE user_id = ?1",
            params![user_id],
            |r| Ok((r.get::<String>(0)?, r.get::<Option<String>>(1)?)),
        )
        .optional()
    })
    .await
}

// ---------------------------------------------------------------------------
// stats
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Stats {
    pub messages: i64,
    pub events: i64,
    pub members: i64,
    pub warns: i64,
    pub bans: i64,
    pub guilds: i64,
}

/// Process-wide aggregates for bot housekeeping logs only.
/// Do **not** expose this to Discord commands or the WebUI — use [`guild_stats`].
pub async fn stats(db: &Db) -> Result<Stats> {
    db.with_conn(|conn| {
        let one = |sql: &str| conn.query_row(sql, (), |r| Ok(r.get::<i64>(0)?));
        Ok(Stats {
            messages: one("SELECT COUNT(*) FROM messages")?,
            events: one("SELECT COUNT(*) FROM events")?,
            members: one("SELECT COUNT(*) FROM members")?,
            warns: one("SELECT COUNT(*) FROM warns")?,
            bans: one("SELECT COUNT(*) FROM bans")?,
            guilds: one("SELECT COUNT(DISTINCT guild_id) FROM members")?,
        })
    })
    .await
}

/// Per-guild aggregates for the WebUI (avoids leaking other servers' data).
pub async fn guild_stats(db: &Db, guild_id: i64) -> Result<Stats> {
    db.with_conn(move |conn| {
        let one = |sql: &str| conn.query_row(sql, params![guild_id], |r| Ok(r.get::<i64>(0)?));
        Ok(Stats {
            messages: one("SELECT COUNT(*) FROM messages WHERE guild_id = ?1")?,
            events: one("SELECT COUNT(*) FROM events WHERE guild_id = ?1")?,
            members: one("SELECT COUNT(*) FROM members WHERE guild_id = ?1")?,
            warns: one("SELECT COUNT(*) FROM warns WHERE guild_id = ?1")?,
            bans: one("SELECT COUNT(*) FROM bans WHERE guild_id = ?1")?,
            guilds: 1,
        })
    })
    .await
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
