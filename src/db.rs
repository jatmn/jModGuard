//! SQLite data layer: connection pool, migrations, and all persistence.
//!
//! Every async function runs its work on a pooled connection via
//! `spawn_blocking`; parameters are moved (owned) into the closures so no
//! borrowed data ever crosses the async boundary.

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, Connection, OptionalExtension};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::Path;
use std::sync::Arc;

use crate::types::{CommandDef, GuildSettings, RuleDef, GLOBAL_SCOPE};

const MIGRATIONS: &[&str] = &[include_str!("../migrations/0001_init.sql")];

pub type Pool = r2d2::Pool<SqliteConnectionManager>;

#[derive(Clone)]
pub struct Db {
    pool: Arc<Pool>,
}

impl Db {
    pub fn open(path: &Path, pool_size: u32) -> Result<Self> {
        // Migrate + seed on a dedicated connection first, so no pool
        // connections are racing the DDL at startup.
        {
            let mut migrator = Connection::open(path)
                .with_context(|| format!("failed to open database {}", path.display()))?;
            migrator.execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=NORMAL;
                 PRAGMA busy_timeout=5000;",
            )?;
            migrate_conn(&mut migrator)?;
            seed_globals_conn(&mut migrator)?;
        }

        let manager = SqliteConnectionManager::file(path).with_init(|c| {
            c.execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=NORMAL;
                 PRAGMA busy_timeout=5000;
                 PRAGMA foreign_keys=ON;",
            )
        });
        let pool = r2d2::Pool::builder()
            .max_size(pool_size.max(2))
            .build(manager)
            .context("failed to create database pool")?;

        Ok(Self { pool: Arc::new(pool) })
    }

    /// Runs a blocking closure on a pooled connection off the async runtime.
    pub async fn with_conn<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().context("failed to get db connection")?;
            f(&mut conn)
        })
        .await
        .context("db task panicked")?
    }

    /// Runs a closure synchronously (tests only).
    #[allow(dead_code)]
    pub fn with_conn_sync<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T>,
    {
        let mut conn = self.pool.get().context("failed to get db connection")?;
        f(&mut conn)
    }
}

fn migrate_conn(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap_or(0);
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let target = (i + 1) as i64;
        if version < target {
            let tx = conn.transaction()?;
            tx.execute_batch(sql).with_context(|| format!("migration {target}"))?;
            tx.pragma_update(None, "user_version", target)?;
            tx.commit()?;
            tracing::info!("applied migration {target}");
        }
    }
    Ok(())
}

fn seed_globals_conn(conn: &mut Connection) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO guild_settings (guild_id, settings) VALUES (?1, ?2)",
        params![GLOBAL_SCOPE, serde_json::to_string(&GuildSettings::default())?],
    )?;
    let rules = crate::defaults::default_rules();
    for (i, rule) in rules.iter().enumerate() {
        conn.execute(
            "INSERT OR IGNORE INTO rules (guild_id, rule_id, data) VALUES (?1, ?2, ?3)",
            params![GLOBAL_SCOPE, crate::types::RULE_IDS[i], serde_json::to_string(rule)?],
        )?;
    }
    for cmd in crate::defaults::default_commands() {
        conn.execute(
            "INSERT OR IGNORE INTO commands (guild_id, name, data) VALUES (?1, ?2, ?3)",
            params![GLOBAL_SCOPE, cmd.name.to_lowercase(), serde_json::to_string(&cmd)?],
        )?;
    }
    for (event, template) in crate::defaults::default_templates() {
        conn.execute(
            "INSERT OR IGNORE INTO templates (guild_id, event_type, template) VALUES (?1, ?2, ?3)",
            params![GLOBAL_SCOPE, event, template],
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn now_str() -> String {
    Utc::now().to_rfc3339()
}

fn read_json<T: DeserializeOwned>(data: &str, what: &str) -> Result<T> {
    serde_json::from_str(data).with_context(|| format!("corrupt JSON in {what}"))
}

/// Resolves a per-guild row against the global row, falling back to `default`.
pub(crate) fn resolve<T: Clone>(guild_row: &Option<T>, global_row: &Option<T>, default: &T) -> T {
    guild_row.clone().or_else(|| global_row.clone()).unwrap_or_else(|| default.clone())
}

// ---------------------------------------------------------------------------
// generic scope row helpers
// ---------------------------------------------------------------------------

pub(crate) async fn load_rows<T: DeserializeOwned + Send + 'static>(
    db: &Db,
    guild_id: i64,
    table: &'static str,
    key_col: &'static str,
    key_value: Option<&str>,
) -> Result<Vec<(String, T)>> {
    let key_value = key_value.map(|s| s.to_string());
    db.with_conn(move |conn| {
        let sql = if key_value.is_some() {
            format!("SELECT {key_col}, data FROM {table} WHERE guild_id = ?1 AND {key_col} = ?2")
        } else {
            format!("SELECT {key_col}, data FROM {table} WHERE guild_id = ?1")
        };
        let mut stmt = conn.prepare(&sql)?;
        let mut args: Vec<rusqlite::types::Value> = vec![guild_id.into()];
        if let Some(kv) = &key_value {
            args.push(kv.to_string().into());
        }
        let rows = stmt.query_map(rusqlite::params_from_iter(args), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (k, v) = row?;
            out.push((k, read_json(&v, table)?));
        }
        Ok(out)
    })
    .await
}

async fn upsert_row<T: Serialize + Send + 'static>(
    db: &Db,
    table: &'static str,
    key_cols: &'static str,
    key_params: Vec<rusqlite::types::Value>,
    data_col: &'static str,
    data: T,
) -> Result<()> {
    let json = serde_json::to_string(&data)?;
    db.with_conn(move |conn| {
        let keys = key_params
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        let n = key_params.len() + 1;
        conn.execute(
            &format!(
                "INSERT INTO {table} ({key_cols}, {data_col}) VALUES ({keys}, ?{n})
                 ON CONFLICT({key_cols}) DO UPDATE SET {data_col} = excluded.{data_col}"
            ),
            rusqlite::params_from_iter(key_params.into_iter().chain(std::iter::once(json.into()))),
        )?;
        Ok(())
    })
    .await
}

async fn delete_row(
    db: &Db,
    table: &'static str,
    key_cols: &'static str,
    key_params: Vec<rusqlite::types::Value>,
) -> Result<()> {
    db.with_conn(move |conn| {
        let keys = key_params
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        conn.execute(
            &format!("DELETE FROM {table} WHERE ({key_cols}) = ({keys})"),
            rusqlite::params_from_iter(key_params),
        )?;
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------
// settings
// ---------------------------------------------------------------------------

pub async fn get_settings(db: &Db, guild_id: i64) -> Result<GuildSettings> {
    db.with_conn(move |conn| {
        let guild: Option<String> = conn
            .query_row(
                "SELECT settings FROM guild_settings WHERE guild_id = ?1",
                params![guild_id],
                |r| r.get(0),
            )
            .optional()?;
        let global: Option<String> = conn
            .query_row(
                "SELECT settings FROM guild_settings WHERE guild_id = ?1",
                params![GLOBAL_SCOPE],
                |r| r.get(0),
            )
            .optional()?;
        let g = guild.as_deref().map(|s| read_json(s, "guild_settings")).transpose()?;
        let gl = global.as_deref().map(|s| read_json(s, "guild_settings")).transpose()?;
        Ok(resolve(&g, &gl, &GuildSettings::default()))
    })
    .await
}

pub async fn upsert_settings(db: &Db, guild_id: i64, settings: &GuildSettings) -> Result<()> {
    let json = serde_json::to_string(settings)?;
    db.with_conn(move |conn| {
        conn.execute(
            "INSERT INTO guild_settings (guild_id, settings) VALUES (?1, ?2)
             ON CONFLICT(guild_id) DO UPDATE SET settings = excluded.settings",
            params![guild_id, json],
        )?;
        Ok(())
    })
    .await
}

pub async fn ensure_guild(db: &Db, guild_id: i64) -> Result<()> {
    let json = serde_json::to_string(&GuildSettings::default())?;
    db.with_conn(move |conn| {
        conn.execute(
            "INSERT OR IGNORE INTO guild_settings (guild_id, settings) VALUES (?1, ?2)",
            params![guild_id, json],
        )?;
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------
// rules
// ---------------------------------------------------------------------------

pub async fn list_rules(db: &Db, guild_id: i64) -> Result<Vec<(String, RuleDef)>> {
    load_rows(db, guild_id, "rules", "rule_id", None).await
}

pub async fn get_rule(db: &Db, guild_id: i64, rule_id: &str) -> Result<Option<RuleDef>> {
    let rows: Vec<(String, RuleDef)> =
        load_rows(db, guild_id, "rules", "rule_id", Some(rule_id)).await?;
    Ok(rows.into_iter().next().map(|(_, r)| r))
}

pub async fn upsert_rule(db: &Db, guild_id: i64, rule_id: &str, rule: &RuleDef) -> Result<()> {
    upsert_row(
        db,
        "rules",
        "guild_id, rule_id",
        vec![guild_id.into(), rule_id.to_string().into()],
        "data",
        rule.clone(),
    )
    .await
}

pub async fn delete_rule(db: &Db, guild_id: i64, rule_id: &str) -> Result<()> {
    delete_row(
        db,
        "rules",
        "guild_id, rule_id",
        vec![guild_id.into(), rule_id.to_string().into()],
    )
    .await
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

pub async fn list_commands(db: &Db, guild_id: i64) -> Result<Vec<(String, CommandDef)>> {
    load_rows(db, guild_id, "commands", "name", None).await
}

pub async fn get_command_row(db: &Db, guild_id: i64, name: &str) -> Result<Option<CommandDef>> {
    let rows: Vec<(String, CommandDef)> =
        load_rows(db, guild_id, "commands", "name", Some(name)).await?;
    Ok(rows.into_iter().next().map(|(_, c)| c))
}

pub async fn upsert_command(db: &Db, guild_id: i64, name: &str, cmd: &CommandDef) -> Result<()> {
    upsert_row(
        db,
        "commands",
        "guild_id, name",
        vec![guild_id.into(), name.to_string().into()],
        "data",
        cmd.clone(),
    )
    .await
}

pub async fn delete_command(db: &Db, guild_id: i64, name: &str) -> Result<()> {
    delete_row(
        db,
        "commands",
        "guild_id, name",
        vec![guild_id.into(), name.to_string().into()],
    )
    .await
}

// ---------------------------------------------------------------------------
// templates
// ---------------------------------------------------------------------------

pub async fn list_templates(db: &Db, guild_id: i64) -> Result<Vec<(String, String)>> {
    db.with_conn(move |conn| {
        let mut stmt =
            conn.prepare("SELECT event_type, template FROM templates WHERE guild_id = ?1")?;
        let rows = stmt.query_map(params![guild_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
    .await
}

pub async fn upsert_template(db: &Db, guild_id: i64, event: &str, template: &str) -> Result<()> {
    let (event, template) = (event.to_string(), template.to_string());
    db.with_conn(move |conn| {
        conn.execute(
            "INSERT INTO templates (guild_id, event_type, template) VALUES (?1, ?2, ?3)
             ON CONFLICT(guild_id, event_type) DO UPDATE SET template = excluded.template",
            params![guild_id, event, template],
        )?;
        Ok(())
    })
    .await
}

pub async fn delete_template(db: &Db, guild_id: i64, event: &str) -> Result<()> {
    delete_row(
        db,
        "templates",
        "guild_id, event_type",
        vec![guild_id.into(), event.to_string().into()],
    )
    .await
}

pub async fn get_template_row(db: &Db, guild_id: i64, event: &str) -> Result<Option<String>> {
    let (guild_id, event) = (guild_id, event.to_string());
    db.with_conn(move |conn| {
        Ok(conn
            .query_row(
                "SELECT template FROM templates WHERE guild_id = ?1 AND event_type = ?2",
                params![guild_id, event],
                |r| r.get(0),
            )
            .optional()?)
    })
    .await
}

// ---------------------------------------------------------------------------
// log channels
// ---------------------------------------------------------------------------

pub async fn list_log_channels(db: &Db, guild_id: i64) -> Result<Vec<(String, i64)>> {
    db.with_conn(move |conn| {
        let mut stmt =
            conn.prepare("SELECT event_type, channel_id FROM log_channels WHERE guild_id = ?1")?;
        let rows = stmt.query_map(params![guild_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
    .await
}

pub async fn upsert_log_channel(db: &Db, guild_id: i64, event: &str, channel_id: i64) -> Result<()> {
    let event = event.to_string();
    db.with_conn(move |conn| {
        conn.execute(
            "INSERT INTO log_channels (guild_id, event_type, channel_id) VALUES (?1, ?2, ?3)
             ON CONFLICT(guild_id, event_type) DO UPDATE SET channel_id = excluded.channel_id",
            params![guild_id, event, channel_id],
        )?;
        Ok(())
    })
    .await
}

pub async fn delete_log_channel(db: &Db, guild_id: i64, event: &str) -> Result<()> {
    delete_row(
        db,
        "log_channels",
        "guild_id, event_type",
        vec![guild_id.into(), event.to_string().into()],
    )
    .await
}

// ---------------------------------------------------------------------------
// badwords
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Badword {
    pub id: i64,
    pub guild_id: i64,
    pub pattern: String,
    pub note: Option<String>,
}

pub async fn list_badwords(db: &Db, guild_id: i64) -> Result<Vec<Badword>> {
    db.with_conn(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, guild_id, pattern, note FROM badwords
             WHERE guild_id = ?1 OR guild_id = ?2 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![guild_id, GLOBAL_SCOPE], |r| {
            Ok(Badword {
                id: r.get(0)?,
                guild_id: r.get(1)?,
                pattern: r.get(2)?,
                note: r.get(3)?,
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

pub async fn add_badword(db: &Db, guild_id: i64, pattern: &str, note: Option<&str>) -> Result<i64> {
    let (pattern, note) = (pattern.to_string(), note.map(|s| s.to_string()));
    db.with_conn(move |conn| {
        conn.execute(
            "INSERT INTO badwords (guild_id, pattern, note) VALUES (?1, ?2, ?3)",
            params![guild_id, pattern, note],
        )?;
        Ok(conn.last_insert_rowid())
    })
    .await
}

pub async fn delete_badword(db: &Db, id: i64) -> Result<()> {
    db.with_conn(move |conn| {
        conn.execute("DELETE FROM badwords WHERE id = ?1", params![id])?;
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------
// members
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemberRow {
    pub guild_id: i64,
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

fn row_to_member(r: &rusqlite::Row<'_>) -> rusqlite::Result<MemberRow> {
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
        currently_in: r.get::<_, i64>(9)? != 0,
    })
}

const MEMBER_COLS: &str = "guild_id, user_id, username, first_joined_at, last_joined_at, last_left_at, join_count, message_count, last_seen_at, currently_in";

pub async fn member_row(db: &Db, guild_id: i64, user_id: i64) -> Result<Option<MemberRow>> {
    db.with_conn(move |conn| {
        Ok(conn
            .query_row(
                &format!(
                    "SELECT {MEMBER_COLS} FROM members WHERE guild_id = ?1 AND user_id = ?2"
                ),
                params![guild_id, user_id],
                row_to_member,
            )
            .optional()?)
    })
    .await
}

pub async fn search_members(db: &Db, guild_id: i64, q: &str, limit: i64) -> Result<Vec<MemberRow>> {
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

pub async fn get_message(db: &Db, message_id: i64) -> Result<Option<(String, String)>> {
    db.with_conn(move |conn| {
        Ok(conn
            .query_row(
                "SELECT content, attachments FROM messages WHERE message_id = ?1",
                params![message_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?)
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

pub async fn mark_edited(db: &Db, message_id: i64, content: &str) -> Result<()> {
    let (now, content) = (now_str(), content.to_string());
    db.with_conn(move |conn| {
        conn.execute(
            "UPDATE messages SET content = ?1, edited_at = ?2 WHERE message_id = ?3",
            params![content, now, message_id],
        )?;
        Ok(())
    })
    .await
}

pub async fn purge_old_messages(db: &Db, retention_days: u32) -> Result<usize> {
    db.with_conn(move |conn| {
        let cutoff = (Utc::now() - chrono::Duration::days(retention_days as i64)).to_rfc3339();
        let n = conn.execute(
            "DELETE FROM messages WHERE created_at < ?1",
            params![cutoff],
        )?;
        Ok(n)
    })
    .await
}

// ---------------------------------------------------------------------------
// events (audit log)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EventRow {
    pub id: i64,
    pub guild_id: i64,
    pub event_type: String,
    pub user_id: Option<i64>,
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
    let data = if data.is_null() { None } else { Some(data.to_string()) };
    db.with_conn(move |conn| {
        conn.execute(
            "INSERT INTO events (guild_id, event_type, user_id, actor_id, data, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![guild_id, event_type, user_id, actor_id, data, now],
        )?;
        Ok(conn.last_insert_rowid())
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
    let mut args: Vec<rusqlite::types::Value> = vec![guild_id.into()];
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
        let rows = stmt.query_map(rusqlite::params_from_iter(args), |r| {
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
            out.push(row?);
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
    pub guild_id: i64,
    pub user_id: i64,
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
        conn.execute(
            "INSERT INTO warns (guild_id, user_id, moderator_id, reason, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![guild_id, user_id, moderator_id, reason, now],
        )?;
        Ok(conn.last_insert_rowid())
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
        Ok(conn.execute(
            "DELETE FROM warns WHERE guild_id = ?1 AND user_id = ?2",
            params![guild_id, user_id],
        )?)
    })
    .await
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BanRow {
    pub id: i64,
    pub guild_id: i64,
    pub user_id: i64,
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
                active: r.get::<_, i64>(6)? != 0,
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
        Ok(conn
            .query_row(
                "SELECT username, avatar FROM users WHERE user_id = ?1",
                params![user_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .optional()?)
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

pub async fn stats(db: &Db) -> Result<Stats> {
    db.with_conn(|conn| {
        let one = |sql: &str| -> Result<i64> {
            Ok(conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_or(0))
        };
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

/// Parses an RFC3339 timestamp into a friendly relative duration string.
pub fn time_ago(ts: &Option<String>) -> String {
    let Some(ts) = ts else { return "never".into() };
    let Ok(dt) = DateTime::parse_from_rfc3339(ts) else { return ts.clone() };
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

// ---------------------------------------------------------------------------
// full guild config (all scopes resolved) — used by the in-memory config cache
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ConfigParts {
    pub settings: GuildSettings,
    pub rules: std::collections::HashMap<String, RuleDef>,
    pub commands: std::collections::HashMap<String, CommandDef>,
    pub templates: std::collections::HashMap<String, String>,
    pub log_channels: std::collections::HashMap<String, i64>,
    pub badwords: Vec<String>,
}

/// Loads `(key, value)` rows where the value column holds JSON-serialized
/// config (rules, commands). The value column name varies per table.
fn rows_map_json<T: DeserializeOwned>(
    conn: &Connection,
    table: &str,
    key_col: &str,
    value_col: &str,
    guild_id: i64,
) -> Result<std::collections::HashMap<String, T>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {key_col}, {value_col} FROM {table} WHERE guild_id = ?1"
    ))?;
    let rows = stmt.query_map(params![guild_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out = std::collections::HashMap::new();
    for row in rows {
        let (k, v) = row?;
        out.insert(k, read_json(&v, table)?);
    }
    Ok(out)
}

/// Loads `(key, value)` rows where the value column is plain text
/// (templates) or an integer (log_channels).
fn rows_map_plain<T: rusqlite::types::FromSql + Send + 'static>(
    conn: &Connection,
    table: &str,
    key_col: &str,
    value_col: &str,
    guild_id: i64,
) -> Result<std::collections::HashMap<String, T>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {key_col}, {value_col} FROM {table} WHERE guild_id = ?1"
    ))?;
    let rows = stmt.query_map(params![guild_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, T>(1)?))
    })?;
    let mut out = std::collections::HashMap::new();
    for row in rows {
        let (k, v) = row?;
        out.insert(k, v);
    }
    Ok(out)
}

pub async fn load_config_parts(db: &Db, guild_id: i64) -> Result<ConfigParts> {
    db.with_conn(move |conn| {
        let one = |conn: &Connection, table: &str, col: &str, id: i64| -> Result<Option<String>> {
            conn.query_row(
                &format!("SELECT {col} FROM {table} WHERE guild_id = ?1"),
                params![id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
        };

        // settings (effective: guild ?? global ?? default)
        let g_settings: Option<GuildSettings> = one(conn, "guild_settings", "settings", guild_id)?
            .map(|s| read_json(&s, "guild_settings"))
            .transpose()?;
        let gl_settings: Option<GuildSettings> =
            one(conn, "guild_settings", "settings", GLOBAL_SCOPE)?
                .map(|s| read_json(&s, "guild_settings"))
                .transpose()?;
        let settings = resolve(&g_settings, &gl_settings, &GuildSettings::default());

        // rules: map rule_id -> effective def
        let g_rules: std::collections::HashMap<String, RuleDef> =
            rows_map_json(conn, "rules", "rule_id", "data", guild_id)?;
        let gl_rules: std::collections::HashMap<String, RuleDef> =
            rows_map_json(conn, "rules", "rule_id", "data", GLOBAL_SCOPE)?;
        let default_rules = crate::defaults::default_rules();
        let mut rules = std::collections::HashMap::new();
        for (i, id) in crate::types::RULE_IDS.iter().enumerate() {
            let def = default_rules[i].clone();
            rules.insert(
                id.to_string(),
                resolve(&g_rules.get(*id).cloned(), &gl_rules.get(*id).cloned(), &def),
            );
        }

        // commands: name(lowercase) -> effective def (guild ?? global)
        let g_cmds: std::collections::HashMap<String, CommandDef> =
            rows_map_json(conn, "commands", "name", "data", guild_id)?;
        let gl_cmds: std::collections::HashMap<String, CommandDef> =
            rows_map_json(conn, "commands", "name", "data", GLOBAL_SCOPE)?;
        let mut commands: std::collections::HashMap<String, CommandDef> = gl_cmds.clone();
        for (name, def) in g_cmds {
            commands.insert(name, def);
        }

        // templates: effective per event (plain text column `template`)
        let g_tpl: std::collections::HashMap<String, String> =
            rows_map_plain(conn, "templates", "event_type", "template", guild_id)?;
        let gl_tpl: std::collections::HashMap<String, String> =
            rows_map_plain(conn, "templates", "event_type", "template", GLOBAL_SCOPE)?;
        let mut templates = std::collections::HashMap::new();
        for (event, def) in crate::defaults::default_templates() {
            let resolved = resolve(
                &g_tpl.get(event).cloned(),
                &gl_tpl.get(event).cloned(),
                &def.to_string(),
            );
            templates.insert(event.to_string(), resolved);
        }
        for (event, t) in &g_tpl {
            templates.insert(event.clone(), t.clone());
        }
        for (event, t) in &gl_tpl {
            templates.entry(event.clone()).or_insert_with(|| t.clone());
        }

        // log channels: effective per event (integer column `channel_id`)
        let g_lc: std::collections::HashMap<String, i64> =
            rows_map_plain(conn, "log_channels", "event_type", "channel_id", guild_id)?;
        let gl_lc: std::collections::HashMap<String, i64> =
            rows_map_plain(conn, "log_channels", "event_type", "channel_id", GLOBAL_SCOPE)?;
        let mut log_channels = std::collections::HashMap::new();
        for event in crate::types::EVENT_TYPES {
            let resolved = resolve(&g_lc.get(event).cloned(), &gl_lc.get(event).cloned(), &0_i64);
            if resolved != 0 {
                log_channels.insert(event.to_string(), resolved);
            }
        }
        for (event, c) in &gl_lc {
            log_channels.insert(event.clone(), *c);
        }
        for (event, c) in &g_lc {
            log_channels.insert(event.clone(), *c);
        }

        // badwords: effective = global + guild (union)
        let mut badwords = Vec::new();
        for scope in [GLOBAL_SCOPE, guild_id] {
            let mut stmt = conn.prepare("SELECT pattern FROM badwords WHERE guild_id = ?1")?;
            let rows = stmt.query_map(params![scope], |r| r.get::<_, String>(0))?;
            for row in rows {
                badwords.push(row?);
            }
        }

        Ok(ConfigParts { settings, rules, commands, templates, log_channels, badwords })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_globals_in_memory() {
        let tmp = std::env::temp_dir().join(format!("jmodguard_test_{}.db", std::process::id()));
        let db = Db::open(&tmp, 2).unwrap();
        let parts = db.with_conn_sync(|conn| {
            // directly inspect rows
            let rules: i64 = conn
                .query_row("SELECT COUNT(*) FROM rules WHERE guild_id = ?1", params![GLOBAL_SCOPE], |r| r.get(0))
                .unwrap();
            let commands: i64 = conn
                .query_row("SELECT COUNT(*) FROM commands WHERE guild_id = ?1", params![GLOBAL_SCOPE], |r| r.get(0))
                .unwrap();
            let templates: i64 = conn
                .query_row("SELECT COUNT(*) FROM templates WHERE guild_id = ?1", params![GLOBAL_SCOPE], |r| r.get(0))
                .unwrap();
            let badwords: i64 = conn
                .query_row("SELECT COUNT(*) FROM badwords", [], |r| r.get(0))
                .unwrap();
            Ok((rules, commands, templates, badwords))
        }).unwrap();
        assert_eq!(parts.0, crate::types::RULE_IDS.len() as i64, "all 7 rules seeded");
        assert!(parts.1 >= 20, "built-in commands seeded");
        assert_eq!(parts.2, crate::types::EVENT_TYPES.len() as i64, "all event templates seeded");
        assert_eq!(parts.3, 0, "no badwords by default");
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(format!("{}-wal", tmp.display()));
        let _ = std::fs::remove_file(format!("{}-shm", tmp.display()));
    }

    #[test]
    fn config_parts_loads_every_section() {
        // Regression: load_config_parts must succeed for a real guild and
        // return rules, commands, templates AND log channels (these tables
        // use non-`data` value columns).
        let tmp = std::env::temp_dir().join(format!("jmodguard_test3_{}.db", std::process::id()));
        let db = Db::open(&tmp, 2).unwrap();
        let guild = 271083998816632834i64;
        crate::db::ensure_guild(&db, guild);
        let parts = tokio_test_block_on(crate::db::load_config_parts(&db, guild));
        let parts = parts.unwrap();
        assert_eq!(parts.rules.len(), crate::types::RULE_IDS.len(), "all rules");
        assert!(parts.commands.len() >= 20, "commands present: {}", parts.commands.len());
        assert_eq!(parts.templates.len(), crate::types::EVENT_TYPES.len(), "all templates");
        assert!(parts.templates.contains_key("automod"));
        assert!(parts.log_channels.is_empty(), "no log channels by default");
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(format!("{}-wal", tmp.display()));
        let _ = std::fs::remove_file(format!("{}-shm", tmp.display()));
    }

    fn tokio_test_block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn config_parts_loads_effective_rules() {
        let tmp = std::env::temp_dir().join(format!("jmodguard_test2_{}.db", std::process::id()));
        let db = Db::open(&tmp, 2).unwrap();
        let parts = db.with_conn_sync(|_| Ok(())).unwrap();
        let _ = parts;
        // rule id keys must exist and match RULE_IDS
        let ids: Vec<String> = db
            .with_conn_sync(|conn| {
                let mut stmt = conn.prepare("SELECT rule_id FROM rules WHERE guild_id = 0 ORDER BY rule_id")?;
                let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
                let mut v = Vec::new();
                for r in rows {
                    v.push(r?);
                }
                Ok(v)
            })
            .unwrap();
        let mut expected: Vec<String> = crate::types::RULE_IDS.iter().map(|s| s.to_string()).collect();
        let mut got = ids;
        got.sort();
        expected.sort();
        assert_eq!(got, expected);
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(format!("{}-wal", tmp.display()));
        let _ = std::fs::remove_file(format!("{}-shm", tmp.display()));
    }
}
