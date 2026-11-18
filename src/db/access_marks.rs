//! Gateway access-change marks in the existing core `meta` table (#653).
//!
//! The bot records that a guild's roles or owner changed, or that one
//! member's roles changed or they left, under a monotonic `access_epoch`.
//! The API reads the epoch before each Discord guild-list fetch and keeps it
//! with that snapshot. A later mark for the guild, or for the user in that
//! guild, makes the snapshot too old for that guild. No new schema: each
//! mark is one `meta` row, pruned once no unexpired snapshot can predate it.

use super::write_budget::BoundedWrites;
use super::{Db, SqlExecutor, params};
use anyhow::Result;

const EPOCH_KEY: &str = "access_epoch";
const MARK_PREFIX: &str = "access_mark:";
/// `MARK_PREFIX` with its last byte incremented: the key range upper bound.
const MARK_END: &str = "access_mark;";
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

/// Records one batch of marks under the next epoch, and prunes marks at or
/// below `prune_through`. Returns the new epoch. Only the writer calls this.
pub async fn record_access_marks(
    db: &Db,
    guilds: Vec<i64>,
    members: Vec<(i64, i64)>,
    prune_through: Option<i64>,
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
        for key in keys {
            tx.execute_rows(upsert, params![key, epoch.to_string()], 1)?;
        }
        if let Some(through) = prune_through.filter(|through| *through > 0) {
            tx.execute_reclaim(
                "DELETE FROM meta WHERE key >= ?1 AND key < ?2 \
                 AND CAST(value AS INTEGER) <= ?3",
                params![MARK_PREFIX, MARK_END, through.min(epoch - 1)],
            )?;
        }
        tx.commit()?;
        Ok(epoch)
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
    let keys = [guild_key(guild), member_key(guild, user)];
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let mark: i64 = tx.query_row(
            "SELECT COALESCE(MAX(CAST(value AS INTEGER)), 0) FROM meta WHERE key IN (?1, ?2)",
            params![keys[0].clone(), keys[1].clone()],
            |row| Ok(row.get(0)?),
        )?;
        tx.finish_read()?;
        Ok(mark)
    })
    .await
}
