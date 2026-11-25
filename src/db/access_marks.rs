//! Gateway access-change marks in the existing core `meta` table (#653).
//!
//! The bot records that a guild's roles or owner changed, or that one
//! member's roles changed or they left, under a monotonic `access_epoch`.
//! The API reads the epoch before each Discord guild-list fetch and keeps it
//! with that snapshot. A later mark for the guild, or for the user in that
//! guild, makes the snapshot too old for that guild. No new schema: each
//! mark is one `meta` row. Its value is `epoch:expires_at` so the writer can
//! reclaim it after retention even if the bot restarts.

use super::write_budget::BoundedWrites;
use super::{Db, SqlExecutor, params};
use anyhow::Result;

const EPOCH_KEY: &str = "access_epoch";
const MARK_PREFIX: &str = "access_mark:";
/// `MARK_PREFIX` with its last byte incremented: the key range upper bound.
const MARK_END: &str = "access_mark;";
const PRUNE_BATCH: i64 = 256;
const LEGACY_BATCH: i64 = 32;
const RETENTION_SECS: i64 = ACCESS_MARK_RETENTION.as_secs() as i64;
/// Member marks written in one batch. The bot turns any excess for a guild
/// into one guild mark, which invalidates more but never less.
pub const ACCESS_MARK_BATCH_MEMBERS: usize = 32;
/// Guild marks written in one batch; callers keep the rest for the next one.
pub const ACCESS_MARK_BATCH_GUILDS: usize = 32;
/// How long a mark is kept. The API never authorizes from a snapshot older
/// than this, so an older mark cannot postdate any snapshot still in use.
pub const ACCESS_MARK_RETENTION: std::time::Duration = std::time::Duration::from_secs(600);

fn guild_key(guild: i64) -> String {
    format!("{MARK_PREFIX}{guild}")
}

fn member_key(guild: i64, user: i64) -> String {
    format!("{MARK_PREFIX}{guild}:{user}")
}

/// Records one batch of marks under the next epoch and returns that epoch.
/// Only the writer calls this.
pub async fn record_access_marks(
    db: &Db,
    guilds: Vec<i64>,
    members: Vec<(i64, i64)>,
) -> Result<i64> {
    anyhow::ensure!(
        guilds.len() <= ACCESS_MARK_BATCH_GUILDS && members.len() <= ACCESS_MARK_BATCH_MEMBERS,
        "too many access marks in one batch"
    );
    anyhow::ensure!(
        guilds.iter().all(|guild| *guild > 0)
            && members.iter().all(|(guild, user)| *guild > 0 && *user > 0),
        "invalid access mark id"
    );
    let keys = guilds
        .iter()
        .map(|guild| guild_key(*guild))
        .chain(
            members
                .iter()
                .map(|(guild, user)| member_key(*guild, *user)),
        )
        .collect::<Vec<_>>();
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_write()?;
        tx.ensure_mutation_receipt()?;
        let current: i64 = tx.query_row(
            "SELECT COALESCE((SELECT CAST(value AS INTEGER) FROM meta WHERE key=?1), 0)",
            params![EPOCH_KEY],
            |row| Ok(row.get(0)?),
        )?;
        let epoch = current.max(0) + 1;
        let upsert = "INSERT INTO meta (key, value) VALUES (?1, ?2) \
                      ON CONFLICT(key) DO UPDATE SET value=excluded.value";
        tx.execute_rows(upsert, params![EPOCH_KEY, epoch.to_string()], 1)?;
        // Keep the epoch as the numeric prefix so existing API readers can
        // continue comparing it. Round the database clock up to a full second.
        let mark = "INSERT INTO meta (key, value) VALUES (?1, ?2 || ':' || (unixepoch() + ?3 + 1)) \
                    ON CONFLICT(key) DO UPDATE SET value=excluded.value";
        for key in keys {
            tx.execute_rows(mark, params![key, epoch, RETENTION_SECS], 1)?;
        }
        tx.commit()?;
        Ok(epoch)
    })
    .await
}

/// Reclaims one bounded batch of expired marks. Epoch-only marks from an
/// older writer get a full retention window starting when first observed here.
/// Returns true when a full batch was processed so the bot can check again soon.
pub async fn maintain_access_marks(db: &Db) -> Result<bool> {
    db.with_conn(|conn| {
        let expired = "key >= ?1 AND key < ?2 AND instr(value, ':') > 0 \
                       AND CAST(substr(value, instr(value, ':') + 1) AS INTEGER) <= unixepoch()";
        let legacy = "key >= ?1 AND key < ?2 AND instr(value, ':') = 0";
        let pending = format!(
            "SELECT EXISTS(SELECT 1 FROM meta WHERE {expired}), \
                         EXISTS(SELECT 1 FROM meta WHERE {legacy})"
        );
        // Advisory read first: with nothing to reclaim the idle pass takes no
        // writer lock. Replica lag only keeps a mark until a later pass, which
        // invalidates more, never less.
        let (has_expired, has_legacy): (bool, bool) =
            conn.query_row_advisory(&pending, params![MARK_PREFIX, MARK_END], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
        if !has_expired && !has_legacy {
            return Ok(false);
        }
        let mut tx = conn.transaction_primary_write()?;
        let (has_expired, has_legacy): (bool, bool) =
            tx.query_row(&pending, params![MARK_PREFIX, MARK_END], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
        if !has_expired && !has_legacy {
            tx.finish_read()?;
            return Ok(false);
        }
        tx.ensure_mutation_receipt()?;
        let mut batch_full = false;
        if has_expired {
            let pruned = tx.execute_reclaim(
                &format!(
                    "DELETE FROM meta WHERE key IN \
                          (SELECT key FROM meta WHERE {expired} ORDER BY key LIMIT ?3)"
                ),
                params![MARK_PREFIX, MARK_END, PRUNE_BATCH],
            )?;
            batch_full = pruned == PRUNE_BATCH as usize;
        }
        if has_legacy {
            let dated = tx.execute_rows(
                &format!(
                    "UPDATE meta SET value = value || ':' || (unixepoch() + ?4 + 1) \
                          WHERE key IN (SELECT key FROM meta WHERE {legacy} ORDER BY key LIMIT ?3)"
                ),
                params![MARK_PREFIX, MARK_END, LEGACY_BATCH, RETENTION_SECS],
                LEGACY_BATCH as u64,
            )?;
            batch_full |= dated == LEGACY_BATCH as usize;
        }
        tx.commit()?;
        Ok(batch_full)
    })
    .await
}

/// The latest recorded epoch, or 0 before the first mark.
pub async fn access_epoch(db: &Db) -> Result<i64> {
    db.with_conn(|conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let epoch: i64 = tx.query_row(
            "SELECT COALESCE((SELECT CAST(value AS INTEGER) FROM meta WHERE key=?1), 0)",
            params![EPOCH_KEY],
            |row| Ok(row.get(0)?),
        )?;
        tx.finish_read()?;
        Ok(epoch)
    })
    .await
}

/// The newest mark that applies to `user` in `guild`, or 0 when none is kept.
pub async fn access_mark(db: &Db, guild: i64, user: i64) -> Result<i64> {
    access_mark_for_guilds(db, guild, None, user).await
}

/// Reads the newest guild or member mark for either guild in one primary snapshot.
/// An absent second guild contributes no keys to the lookup.
pub async fn access_mark_for_guilds(
    db: &Db,
    guild: i64,
    other_guild: Option<i64>,
    user: i64,
) -> Result<i64> {
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let mark: i64 = tx.query_row(
            "SELECT COALESCE(MAX(CAST(value AS INTEGER)), 0) FROM meta WHERE key IN (?1, ?2, ?3, ?4)",
            params![
                guild_key(guild),
                member_key(guild, user),
                other_guild.map(guild_key),
                other_guild.map(|guild| member_key(guild, user)),
            ],
            |row| Ok(row.get(0)?),
        )?;
        tx.finish_read()?;
        Ok(mark)
    })
    .await
}
