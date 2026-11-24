//! Operational recovery records for confirmed cleanup deletes. No message
//! bodies, evidence, or attachments are recorded here.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CleanupDeleteRecord {
    pub batch: String,
    pub guild_id: i64,
    pub enrollment_id: String,
    pub channel_id: i64,
    pub message_id: i64,
    pub user_id: i64,
    pub rule_id: String,
    pub rule_name: String,
    pub observed_at_ms: i64,
    pub purge: bool,
}

pub async fn save_cleanup_deletes(
    db: &Db,
    owner: String,
    rows: Vec<CleanupDeleteRecord>,
) -> Result<()> {
    let rows = serde_json::to_string(&rows)?;
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_write()?;
        tx.ensure_mutation_receipt()?;
        tx.execute_bounded(
            "INSERT INTO cleanup_delete_journal
                (batch,message_id,owner,guild_id,enrollment_id,channel_id,user_id,
                 rule_id,rule_name,observed_at_ms,purge)
             SELECT j.value->>'batch',j.value->>'message_id',?1,e.guild_id,e.id,
                    j.value->>'channel_id',j.value->>'user_id',j.value->>'rule_id',
                    j.value->>'rule_name',j.value->>'observed_at_ms',j.value->>'purge'
               FROM json_each(?2) j
               JOIN guild_enrollments e
                 ON e.guild_id=j.value->>'guild_id' AND e.id=j.value->>'enrollment_id' AND e.active=1
              WHERE NOT EXISTS (SELECT 1 FROM guild_write_fences f WHERE f.guild_id=e.guild_id)
             ON CONFLICT(batch,message_id) DO UPDATE SET purge=excluded.purge",
            params![owner, rows],
            1024 * 1024,
        )?;
        tx.commit()
    })
    .await
}

pub async fn recovered_cleanup_deletes(
    db: &Db,
    owner: String,
    after: i64,
) -> Result<Vec<(i64, CleanupDeleteRecord)>> {
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let rows = tx.prepare(
            "SELECT d.rowid,d.batch,d.guild_id,d.enrollment_id,d.channel_id,d.message_id,
                    d.user_id,d.rule_id,d.rule_name,d.observed_at_ms,d.purge
               FROM cleanup_delete_journal d
               JOIN guild_enrollments e ON e.guild_id=d.guild_id AND e.id=d.enrollment_id AND e.active=1
              WHERE d.rowid>?1 AND d.owner<>?2 AND d.dispatching=0
                AND NOT EXISTS (SELECT 1 FROM guild_write_fences f WHERE f.guild_id=d.guild_id)
              ORDER BY d.rowid LIMIT 32",
        )?.query_map(params![after, owner], |row| {
            Ok((row.get(0)?, CleanupDeleteRecord {
                batch: row.get(1)?,
                guild_id: row.get(2)?,
                enrollment_id: row.get(3)?,
                channel_id: row.get(4)?,
                message_id: row.get(5)?,
                user_id: row.get(6)?,
                rule_id: row.get(7)?,
                rule_name: row.get(8)?,
                observed_at_ms: row.get(9)?,
                purge: row.get(10)?,
            }))
        })?.collect::<Result<_>>()?;
        tx.finish_read()?;
        Ok(rows)
    })
    .await
}

#[derive(Clone, Copy)]
pub struct CleanupJournalKey<'a> {
    pub guild_id: i64,
    pub enrollment_id: &'a str,
    pub batch: &'a str,
    pub messages: &'a [i64],
}

pub async fn ensure_cleanup_enrollment(db: &Db, guild: i64, enrollment_id: String) -> Result<()> {
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let current = active_enrollment_conn(&tx, guild)?;
        anyhow::ensure!(
            current.is_some_and(|enrollment| enrollment.id == enrollment_id),
            "cleanup enrollment is no longer active"
        );
        super::guild_stores::refuse_fenced_write(&tx, guild)?;
        tx.finish_read()
    })
    .await
}

/// Claim or reopen the exact rows covered by a Discord attempt. If any row
/// is missing, belongs to another enrollment, or changed state, roll back.
pub async fn set_cleanup_dispatching(
    db: &Db,
    key: CleanupJournalKey<'_>,
    expected_dispatching: bool,
    dispatching: bool,
) -> Result<()> {
    let guild = key.guild_id;
    let enrollment = key.enrollment_id.to_owned();
    let count = key.messages.len();
    if count == 0 {
        // Failure-only summaries have no confirmed deletes to journal.
        return ensure_cleanup_enrollment(db, guild, enrollment).await;
    }
    let batch = key.batch.to_owned();
    let messages = serde_json::to_string(key.messages)?;
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_write()?;
        tx.ensure_mutation_receipt()?;
        let changed = tx.execute_bounded(
            "UPDATE cleanup_delete_journal SET dispatching=?6
              WHERE batch=?1 AND message_id IN (SELECT value FROM json_each(?2))
                AND guild_id=?3 AND enrollment_id=?4 AND dispatching=?5
                AND EXISTS (SELECT 1 FROM guild_enrollments e
                            WHERE e.guild_id=?3 AND e.id=?4 AND e.active=1)
                AND NOT EXISTS (SELECT 1 FROM guild_write_fences f WHERE f.guild_id=?3)",
            params![
                batch,
                messages,
                guild,
                enrollment,
                expected_dispatching,
                dispatching
            ],
            0,
        )?;
        anyhow::ensure!(
            changed == count,
            "cleanup dispatch rows are no longer current"
        );
        tx.commit()
    })
    .await
}

pub async fn settle_cleanup_deletes(db: &Db, keys: Vec<(String, i64)>) -> Result<()> {
    let keys = serde_json::to_string(&keys)?;
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_write()?;
        tx.prepare_reclaim()?;
        tx.execute_reclaim(
            "DELETE FROM cleanup_delete_journal WHERE (batch,message_id) IN (
                SELECT value->>0,value->>1 FROM json_each(?1))",
            params![keys],
        )?;
        tx.commit()
    })
    .await
}

pub async fn prune_cleanup_journal(db: &Db, owner: String) -> Result<usize> {
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_write()?;
        tx.prepare_reclaim()?;
        let removed = tx.execute_reclaim(
            "DELETE FROM cleanup_delete_journal WHERE rowid IN (
                 SELECT d.rowid FROM cleanup_delete_journal d
                  WHERE (d.owner<>?1 AND d.dispatching=1)
                     OR NOT EXISTS (SELECT 1 FROM guild_enrollments e
                         WHERE e.guild_id=d.guild_id AND e.id=d.enrollment_id AND e.active=1)
                  ORDER BY d.rowid LIMIT 256)",
            params![owner],
        )?;
        tx.commit()?;
        Ok(removed)
    })
    .await
}
