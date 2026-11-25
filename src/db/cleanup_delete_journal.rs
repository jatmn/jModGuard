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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CleanupDeleteSave {
    Saved,
    WriteFenced,
    Retired,
}

/// Results follow the order of the input observations.
pub async fn save_cleanup_deletes(
    db: &Db,
    owner: String,
    rows: Vec<CleanupDeleteRecord>,
) -> Result<Vec<CleanupDeleteSave>> {
    let rows = serde_json::to_string(&rows)?;
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_write()?;
        let outcomes = tx.prepare(
            "SELECT e.id IS NOT NULL, f.guild_id IS NOT NULL
               FROM json_each(?1) j
               LEFT JOIN guild_enrollments e
                 ON e.guild_id=j.value->>'guild_id' AND e.id=j.value->>'enrollment_id' AND e.active=1
               LEFT JOIN guild_write_fences f ON f.guild_id=e.guild_id
              ORDER BY j.key",
        )?.query_map(params![rows.clone()], |row| {
            Ok(if !row.get::<bool>(0)? {
                CleanupDeleteSave::Retired
            } else if row.get::<bool>(1)? {
                CleanupDeleteSave::WriteFenced
            } else {
                CleanupDeleteSave::Saved
            })
        })?.collect::<Result<Vec<_>>>()?;
        let expected = outcomes.iter().filter(|&&status| status == CleanupDeleteSave::Saved).count();
        if expected == 0 {
            tx.finish_read()?;
            return Ok(outcomes);
        }
        tx.ensure_mutation_receipt()?;
        let changed = tx.execute_bounded(
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
             ON CONFLICT(batch,message_id) DO UPDATE SET purge=excluded.purge
              WHERE cleanup_delete_journal.guild_id=excluded.guild_id
                AND cleanup_delete_journal.enrollment_id=excluded.enrollment_id
                AND cleanup_delete_journal.owner=excluded.owner",
            params![owner, rows],
            1024 * 1024,
        )?;
        anyhow::ensure!(
            changed == expected,
            "cleanup delete journal row belongs to different work"
        );
        tx.commit()?;
        Ok(outcomes)
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
              WHERE d.rowid>?1 AND d.owner<>?2 AND d.dispatching<=0
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
    pub recovery: Option<(&'a str, &'a CleanupRecovery)>,
}

fn recovery_owner(instance: &str) -> String {
    // Released recovery rows must remain visible to scans that exclude the
    // current process's live incidents.
    format!("recovery:{instance}")
}

/// Keeps recovery open while a detached arm can still make a row replayable.
#[derive(Default)]
pub struct CleanupRecovery {
    pending: Arc<()>,
}

impl CleanupRecovery {
    pub fn has_pending_claims(&self) -> bool {
        Arc::strong_count(&self.pending) > 1
    }
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

/// The existing journal column holds zero for a ready row, a negative token
/// for a recoverable claim, and a positive token for an armed send. Legacy 1
/// remains possibly sent. Tokens prevent a delayed cancellation or rejection
/// from clearing another attempt's dispatch protection.
#[derive(Clone)]
pub struct CleanupDispatchClaim {
    db: Db,
    guild: i64,
    enrollment: String,
    batch: String,
    messages: String,
    count: usize,
    token: i64,
    _recovery: Option<Arc<()>>,
}

impl CleanupDispatchClaim {
    pub async fn reserve(db: &Db, key: CleanupJournalKey<'_>) -> Result<Self> {
        anyhow::ensure!(!key.messages.is_empty(), "cleanup claim has no messages");
        let claim = Self {
            db: db.clone(),
            guild: key.guild_id,
            enrollment: key.enrollment_id.to_owned(),
            batch: key.batch.to_owned(),
            messages: serde_json::to_string(key.messages)?,
            count: key.messages.len(),
            token: rand::random_range(2..=i64::MAX),
            _recovery: key.recovery.map(|(_, scope)| Arc::clone(&scope.pending)),
        };
        let owner = key.recovery.map(|(instance, _)| recovery_owner(instance));
        claim.transition(None, -claim.token, owner).await?;
        #[cfg(feature = "test-support")]
        if let Ok(committed) = TEST_CLEANUP_CLAIM_COMMITTED.try_with(Arc::clone) {
            committed.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(claim)
    }

    /// Keep the final journal write owned until the caller accepts the send
    /// boundary. Cancellation or a failed commit acknowledgement must release
    /// this token if the database did arm it.
    pub async fn arm_for_send(&self) -> Result<()> {
        let (ready, armed) = tokio::sync::oneshot::channel();
        let (start, started) = tokio::sync::oneshot::channel();
        let claim = self.clone();
        #[cfg(feature = "test-support")]
        let probe = TEST_CLEANUP_ARM_COMMITTED.try_with(Clone::clone).ok();
        tokio::spawn(async move {
            if ready.is_closed() {
                return;
            }
            let result = claim
                .transition(Some(-claim.token), claim.token, None)
                .await;
            let armed = result.is_ok();
            #[cfg(feature = "test-support")]
            if armed && let Some((committed, resume)) = probe {
                committed.notify_one();
                resume.notified().await;
            }
            if ready.send(result).is_ok() && armed && started.await.is_ok() {
                return;
            }
            // Keep the proof that no request started through a primary outage.
            // Releases are token-scoped, so even a lost release reply is safe to retry.
            loop {
                match claim.release().await {
                    Ok(()) => break,
                    Err(error)
                        if is_primary_outage(&error)
                            || is_database_operation_timeout(&error)
                            || is_database_commit_indeterminate(&error)
                            || is_remote_interactive_timeout(&error)
                            || crate::db_compat::remote_unavailable_operation(&error).is_some() =>
                    {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "could not reopen an unstarted cleanup dispatch");
                        break;
                    }
                }
            }
        });
        armed.await??;
        start
            .send(())
            .map_err(|()| anyhow::anyhow!("cleanup dispatch handoff was lost"))
    }

    /// Recheck an already armed bulk report before sending another part.
    pub async fn verify_armed(&self) -> Result<()> {
        self.transition(Some(self.token), self.token, None).await
    }

    /// Reopen only this attempt after cancellation before handoff or a
    /// definitive rejection. A failed arm can leave the recoverable claim as-is.
    pub async fn release(&self) -> Result<()> {
        self.transition(Some(self.token), 0, None).await
    }

    async fn transition(
        &self,
        expected: Option<i64>,
        next: i64,
        owner: Option<String>,
    ) -> Result<()> {
        let Self {
            db,
            guild,
            enrollment,
            batch,
            messages,
            count,
            ..
        } = self.clone();
        db.with_conn(move |conn| {
            let mut tx = conn.transaction_primary_write()?;
            tx.ensure_mutation_receipt()?;
            // Claim tokens can grow a row compared with the zero default.
            let growth = if expected.is_none() {
                (count as u64)
                    .saturating_mul(tx.capacity_page_size()?)
                    .saturating_mul(4)
            } else {
                0
            };
            // Releasing this token cannot authorize another send across a fence.
            let changed = tx.execute_bounded(
                "UPDATE cleanup_delete_journal SET dispatching=?6, owner=coalesce(?7, owner)
                  WHERE batch=?1 AND message_id IN (SELECT value FROM json_each(?2))
                    AND guild_id=?3 AND enrollment_id=?4
                    AND ((?5 IS NULL AND dispatching<=0) OR dispatching=?5)
                    AND EXISTS (SELECT 1 FROM guild_enrollments e
                                WHERE e.guild_id=?3 AND e.id=?4 AND e.active=1)
                    AND (?6=0 OR NOT EXISTS (SELECT 1 FROM guild_write_fences f WHERE f.guild_id=?3))",
                params![batch, messages, guild, enrollment, expected, next, owner],
                growth,
            )?;
            if next != 0 {
                anyhow::ensure!(
                    changed == count,
                    "cleanup dispatch rows are no longer current"
                );
            }
            tx.commit()
        })
        .await
    }
}

#[cfg(feature = "test-support")]
tokio::task_local! {
    // Hold the result after the real transaction commits to simulate a lost reply.
    pub static TEST_CLEANUP_CLAIM_COMMITTED: Arc<tokio::sync::Notify>;
    pub static TEST_CLEANUP_ARM_COMMITTED: (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>);
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

pub async fn prune_cleanup_journal(
    db: &Db,
    owner: String,
    recovery: &CleanupRecovery,
) -> Result<usize> {
    // Once the detached work drains, an armed recovery row left behind by a
    // failed settlement is subject to the ordinary possibly-sent rule.
    let recovering = recovery
        .has_pending_claims()
        .then(|| recovery_owner(&owner));
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_write()?;
        tx.prepare_reclaim()?;
        let removed = tx.execute_reclaim(
            "DELETE FROM cleanup_delete_journal WHERE rowid IN (
                 SELECT d.rowid FROM cleanup_delete_journal d
                  WHERE (d.owner<>?1 AND (?2 IS NULL OR d.owner<>?2) AND d.dispatching>0)
                     OR NOT EXISTS (SELECT 1 FROM guild_enrollments e
                         WHERE e.guild_id=d.guild_id AND e.id=d.enrollment_id AND e.active=1)
                  ORDER BY d.rowid LIMIT 256)",
            params![owner, recovering],
        )?;
        tx.commit()?;
        Ok(removed)
    })
    .await
}
