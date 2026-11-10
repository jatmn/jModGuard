//! Known spam configuration uses the same captured writes as Badwords.
use super::*;
use crate::known_spam::{Entry, MAX_ENTRIES};

pub async fn add_enrolled_known_spam(
    db: &Db,
    scope: ConfigurationWriteScope,
    kind: &str,
    value: &str,
    note: Option<&str>,
) -> Result<i64> {
    let entry = Entry::new(kind, value, note)?;
    let key = entry.match_key();
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        scope.verify_conn(&tx)?;
        let count: i64 = tx.query_row("SELECT count(*) FROM known_spam WHERE guild_id=?1",
            params![scope.guild_id()], |r| Ok(r.get(0)?))?;
        anyhow::ensure!(count < MAX_ENTRIES as i64, "known spam list is full");
        let duplicate: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM known_spam WHERE guild_id=?1 AND kind=?2 AND match_key=?3)",
            params![scope.guild_id(), entry.kind.as_str(), key.as_str()], |r| Ok(r.get(0)?))?;
        anyhow::ensure!(!duplicate, "known spam entry already exists");
        tx.execute_rows("INSERT INTO known_spam(guild_id,kind,value,match_key,note) VALUES (?1,?2,?3,?4,?5)",
            params![scope.guild_id(), entry.kind, entry.value, key, entry.note], 1)?;
        let id = tx.last_insert_rowid();
        bump_config_version_conn(&tx)?;
        tx.commit()?;
        Ok(id)
    }).await
}

pub async fn delete_enrolled_known_spam(
    db: &Db,
    scope: ConfigurationWriteScope,
    id: i64,
) -> Result<()> {
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_write()?;
        scope.verify_conn(&tx)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM known_spam WHERE guild_id=?1 AND id=?2)",
            params![scope.guild_id(), id],
            |r| Ok(r.get(0)?),
        )?;
        anyhow::ensure!(exists, "known spam entry not found");
        tx.prepare_reclaim()?;
        let changed = tx.execute_reclaim(
            "DELETE FROM known_spam WHERE guild_id=?1 AND id=?2",
            params![scope.guild_id(), id],
        )?;
        anyhow::ensure!(changed == 1, "known spam entry not found");
        bump_config_version_reclaim_conn(&tx)?;
        tx.commit()?;
        Ok(())
    })
    .await
}

pub async fn get_known_spam_primary(db: &Db, id: i64) -> Result<Option<Entry>> {
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let entry = tx
            .query_row(
                "SELECT id,guild_id,kind,value,note FROM known_spam WHERE id=?1",
                params![id],
                |r| {
                    Ok(Entry {
                        id: r.get(0)?,
                        guild_id: r.get(1)?,
                        kind: r.get(2)?,
                        value: r.get(3)?,
                        note: r.get(4)?,
                    })
                },
            )
            .optional()?;
        tx.finish_read()?;
        Ok(entry)
    })
    .await
}
