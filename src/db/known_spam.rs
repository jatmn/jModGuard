//! Known spam configuration uses the same captured writes as Badwords.
use super::*;
use crate::known_spam::{Entry, MAX_ENTRIES};

pub(super) struct SpamListChange<T> {
    pub value: T,
    pub previous_revision: String,
    pub revision: String,
}

pub async fn add_enrolled_known_spam(
    db: &Db,
    scope: ConfigurationWriteScope,
    kind: &str,
    value: &str,
    note: Option<&str>,
) -> Result<i64> {
    add_enrolled_known_spam_change(db, scope, kind, value, note)
        .await
        .map(|change| change.value.id)
}

pub(super) async fn add_enrolled_known_spam_change(
    db: &Db,
    scope: ConfigurationWriteScope,
    kind: &str,
    value: &str,
    note: Option<&str>,
) -> Result<SpamListChange<Entry>> {
    let mut entry = Entry::new(kind, value, note)?;
    let key = entry.match_key();
    entry.guild_id = scope.guild_id();
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        scope.verify_conn(&tx)?;
        let (count, previous_revision): (i64, String) = tx.query_row(
            "SELECT count(*), coalesce((SELECT value FROM meta WHERE key='config_revision'), '')
             FROM known_spam WHERE guild_id=?1",
            params![entry.guild_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        anyhow::ensure!(count < MAX_ENTRIES as i64, "known spam list is full");
        let duplicate: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM known_spam WHERE guild_id=?1 AND kind=?2 AND match_key=?3)",
            params![entry.guild_id, entry.kind.as_str(), key.as_str()], |r| Ok(r.get(0)?))?;
        anyhow::ensure!(!duplicate, "known spam entry already exists");
        tx.execute_rows("INSERT INTO known_spam(guild_id,kind,value,match_key,note) VALUES (?1,?2,?3,?4,?5)",
            params![entry.guild_id, entry.kind.as_str(), entry.value.as_str(), key, entry.note.as_deref()], 1)?;
        entry.id = tx.last_insert_rowid();
        let revision = bump_config_version_with_revision_conn(&tx)?;
        tx.commit()?;
        Ok(SpamListChange { value: entry, previous_revision, revision })
    }).await
}

pub async fn delete_enrolled_known_spam(
    db: &Db,
    scope: ConfigurationWriteScope,
    id: i64,
) -> Result<()> {
    delete_enrolled_known_spam_change(db, scope, id)
        .await
        .map(|_| ())
}

pub(super) async fn delete_enrolled_known_spam_change(
    db: &Db,
    scope: ConfigurationWriteScope,
    id: i64,
) -> Result<SpamListChange<i64>> {
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_write()?;
        scope.verify_conn(&tx)?;
        let (exists, previous_revision): (bool, String) = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM known_spam WHERE guild_id=?1 AND id=?2),
              coalesce((SELECT value FROM meta WHERE key='config_revision'), '')",
            params![scope.guild_id(), id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        anyhow::ensure!(exists, "known spam entry not found");
        tx.prepare_reclaim()?;
        let changed = tx.execute_reclaim(
            "DELETE FROM known_spam WHERE guild_id=?1 AND id=?2",
            params![scope.guild_id(), id],
        )?;
        anyhow::ensure!(changed == 1, "known spam entry not found");
        let revision = bump_config_version_reclaim_with_revision_conn(&tx)?;
        tx.commit()?;
        Ok(SpamListChange {
            value: id,
            previous_revision,
            revision,
        })
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
