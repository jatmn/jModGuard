//! Durable guild-scheduled work rows (schema 17).
use super::tenant_logs::verify_scope;
use super::*;

pub const CLAIM_LEASE_MS: i64 = 60_000;
const CLEANUP_AGE_MS: i64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone)]
pub struct ScheduledWorkRow {
    pub guild_id: i64,
    pub enrollment_id: String,
    pub work_id: String,
    pub kind: String,
    pub subject_id: i64,
    pub payload: String,
    pub attempt: i64,
}

fn random_work_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl GuildDataStore {
    pub async fn open_softban_unban_exists(
        &self,
        scope: &GuildDataScope,
        subject_id: i64,
    ) -> Result<bool> {
        self.check_target(scope.placement())?;
        let expected = self.expected.clone();
        let scope = scope.clone();
        let guild_id = scope.guild_id();
        let enrollment_id = scope.enrollment().id.clone();
        self.db
            .with_conn(move |conn| {
                let tx = conn.transaction_primary_read()?;
                verify_scope(&tx, &expected, &scope, true)?;
                let exists: bool = tx.query_row(
                    "SELECT EXISTS(
                        SELECT 1 FROM guild_scheduled_work
                        WHERE guild_id=?1 AND enrollment_id=?2 AND kind='softban_unban'
                          AND subject_id=?3 AND state IN ('pending','claimed')
                    )",
                    params![guild_id, enrollment_id.as_str(), subject_id],
                    |r| Ok(r.get(0)?),
                )?;
                tx.finish_read()?;
                Ok(exists)
            })
            .await
    }

    pub async fn insert_softban_unban_pending(
        &self,
        scope: &GuildDataScope,
        subject_id: i64,
        reason: &str,
        operation_id: &str,
        due_at_ms: i64,
    ) -> Result<String> {
        self.check_target(scope.placement())?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        let scope = scope.clone();
        let guild_id = scope.guild_id();
        let enrollment_id = scope.enrollment().id.clone();
        let work_id = random_work_id();
        let payload = serde_json::json!({
            "reason": reason,
            "operation_id": operation_id,
        })
        .to_string();
        let created = now_ms();
        self.db
            .with_conn(move |conn| {
                let mut tx = conn.transaction_primary_read()?;
                verify_scope(&tx, &expected, &scope, true)?;
                tx.ensure_mutation_receipt()?;
                tx.execute_rows(
                    "INSERT INTO guild_scheduled_work(
                        guild_id,enrollment_id,work_id,kind,subject_id,due_at_ms,payload,
                        state,attempt,claim_until_ms,created_at_ms,updated_at_ms
                     ) VALUES(?1,?2,?3,'softban_unban',?4,?5,?6,'pending',0,NULL,?7,?7)",
                    params![
                        guild_id,
                        enrollment_id.as_str(),
                        work_id.as_str(),
                        subject_id,
                        due_at_ms,
                        payload,
                        created,
                    ],
                    1,
                )?;
                tx.commit()?;
                Ok(work_id)
            })
            .await
    }

    pub async fn release_expired_scheduled_claims(&self, now_ms: i64) -> Result<u32> {
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let mut tx = conn.transaction_primary_read()?;
                verify_identity(&tx, &expected)?;
                tx.ensure_mutation_receipt()?;
                let changed = tx.execute_matching(
                    "UPDATE guild_scheduled_work
                     SET state='pending', claim_until_ms=NULL, updated_at_ms=?1
                     WHERE state='claimed' AND claim_until_ms IS NOT NULL AND claim_until_ms<=?1
                       AND kind!='user_timezone'",
                    params![now_ms],
                )?;
                tx.commit()?;
                Ok(changed as u32)
            })
            .await
    }

    pub async fn claim_due_scheduled_work(
        &self,
        kinds: &[&str],
        now_ms: i64,
        limit: usize,
    ) -> Result<Vec<ScheduledWorkRow>> {
        if kinds.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let expected = self.expected.clone();
        let kind_list = kinds
            .iter()
            .map(|k| format!("'{}'", k.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(",");
        let select_sql = format!(
            "SELECT guild_id,enrollment_id,work_id,kind,subject_id,payload,attempt
             FROM guild_scheduled_work
             WHERE state='pending' AND kind IN ({kind_list}) AND kind!='user_timezone'
               AND due_at_ms<=?1 AND attempt<8
             ORDER BY due_at_ms,created_at_ms
             LIMIT ?2"
        );
        self.db
            .with_conn(move |conn| {
                let mut tx = conn.transaction_primary_read()?;
                verify_identity(&tx, &expected)?;
                let mut stmt = tx.prepare(&select_sql)?;
                let candidates = stmt
                    .query_map(params![now_ms, limit as i64], |row| {
                        Ok(ScheduledWorkRow {
                            guild_id: row.get(0)?,
                            enrollment_id: row.get(1)?,
                            work_id: row.get(2)?,
                            kind: row.get(3)?,
                            subject_id: row.get(4)?,
                            payload: row.get(5)?,
                            attempt: row.get(6)?,
                        })
                    })?
                    .collect::<Result<Vec<_>>>()?;
                drop(stmt);
                let mut claimed = Vec::new();
                tx.ensure_mutation_receipt()?;
                for row in candidates {
                    let claim_until = now_ms + CLAIM_LEASE_MS;
                    let changed = tx.execute_matching(
                        "UPDATE guild_scheduled_work
                         SET state='claimed', attempt=attempt+1, claim_until_ms=?1, updated_at_ms=?2
                         WHERE guild_id=?3 AND enrollment_id=?4 AND work_id=?5
                           AND state='pending' AND attempt<8",
                        params![
                            claim_until,
                            now_ms,
                            row.guild_id,
                            row.enrollment_id.as_str(),
                            row.work_id.as_str(),
                        ],
                    )?;
                    if changed == 1 {
                        claimed.push(ScheduledWorkRow {
                            attempt: row.attempt + 1,
                            ..row
                        });
                    }
                }
                tx.commit()?;
                Ok(claimed)
            })
            .await
    }

    pub async fn scheduled_work_membership_active(
        &self,
        guild_id: i64,
        enrollment_id: &str,
        joined_at_ns: i64,
    ) -> Result<bool> {
        let expected = self.expected.clone();
        let enrollment_id = enrollment_id.to_owned();
        self.db
            .with_conn(move |conn| {
                let tx = conn.transaction_primary_read()?;
                verify_identity(&tx, &expected)?;
                let active: bool = tx
                    .query_row(
                        "SELECT EXISTS(
                            SELECT 1 FROM guild_memberships
                            WHERE guild_id=?1 AND enrollment_id=?2 AND active=1 AND joined_at_ns=?3
                        )",
                        params![guild_id, enrollment_id.as_str(), joined_at_ns],
                        |r| Ok(r.get(0)?),
                    )
                    .unwrap_or(false);
                tx.finish_read()?;
                Ok(active)
            })
            .await
    }

    pub async fn claim_scheduled_work_row(
        &self,
        guild_id: i64,
        enrollment_id: &str,
        work_id: &str,
        now_ms: i64,
    ) -> Result<Option<ScheduledWorkRow>> {
        let expected = self.expected.clone();
        let enrollment_id = enrollment_id.to_owned();
        let work_id = work_id.to_owned();
        self.db
            .with_conn(move |conn| {
                let mut tx = conn.transaction_primary_read()?;
                verify_identity(&tx, &expected)?;
                let row = tx
                    .query_row(
                        "SELECT guild_id,enrollment_id,work_id,kind,subject_id,payload,attempt
                         FROM guild_scheduled_work
                         WHERE guild_id=?1 AND enrollment_id=?2 AND work_id=?3 AND state='pending'",
                        params![guild_id, enrollment_id.as_str(), work_id.as_str()],
                        |r| {
                            Ok(ScheduledWorkRow {
                                guild_id: r.get(0)?,
                                enrollment_id: r.get(1)?,
                                work_id: r.get(2)?,
                                kind: r.get(3)?,
                                subject_id: r.get(4)?,
                                payload: r.get(5)?,
                                attempt: r.get(6)?,
                            })
                        },
                    )
                    .optional()?;
                let Some(row) = row else {
                    tx.finish_read()?;
                    return Ok(None);
                };
                tx.ensure_mutation_receipt()?;
                let claim_until = now_ms + CLAIM_LEASE_MS;
                let changed = tx.execute_matching(
                    "UPDATE guild_scheduled_work
                     SET state='claimed', attempt=attempt+1, claim_until_ms=?1, updated_at_ms=?2
                     WHERE guild_id=?3 AND enrollment_id=?4 AND work_id=?5 AND state='pending'",
                    params![
                        claim_until,
                        now_ms,
                        guild_id,
                        enrollment_id.as_str(),
                        work_id.as_str(),
                    ],
                )?;
                if changed != 1 {
                    tx.finish_read()?;
                    return Ok(None);
                }
                tx.commit()?;
                Ok(Some(ScheduledWorkRow {
                    attempt: row.attempt + 1,
                    ..row
                }))
            })
            .await
    }

    pub async fn finish_scheduled_work(
        &self,
        row: &ScheduledWorkRow,
        state: &str,
        due_at_ms: Option<i64>,
        now_ms: i64,
    ) -> Result<bool> {
        anyhow::ensure!(matches!(state, "pending" | "done" | "failed" | "claimed"));
        let expected = self.expected.clone();
        let row = row.clone();
        let state = state.to_owned();
        self.db
            .with_conn(move |conn| {
                let mut tx = conn.transaction_primary_read()?;
                verify_identity(&tx, &expected)?;
                tx.ensure_mutation_receipt()?;
                let changed = if state.as_str() == "pending" {
                    let due = due_at_ms.unwrap_or(now_ms);
                    tx.execute_matching(
                        "UPDATE guild_scheduled_work
                         SET state='pending', claim_until_ms=NULL, due_at_ms=?1, updated_at_ms=?2
                         WHERE guild_id=?3 AND enrollment_id=?4 AND work_id=?5 AND state='claimed'",
                        params![
                            due,
                            now_ms,
                            row.guild_id,
                            row.enrollment_id.as_str(),
                            row.work_id.as_str(),
                        ],
                    )?
                } else {
                    tx.execute_matching(
                        "UPDATE guild_scheduled_work
                         SET state=?1, claim_until_ms=NULL, updated_at_ms=?2
                         WHERE guild_id=?3 AND enrollment_id=?4 AND work_id=?5 AND state='claimed'",
                        params![
                            state.as_str(),
                            now_ms,
                            row.guild_id,
                            row.enrollment_id.as_str(),
                            row.work_id.as_str(),
                        ],
                    )?
                };
                tx.commit()?;
                Ok(changed == 1)
            })
            .await
    }

    pub async fn prune_finished_scheduled_work(&self, now_ms: i64) -> Result<u32> {
        let cutoff = now_ms.saturating_sub(CLEANUP_AGE_MS);
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let mut tx = conn.transaction_primary_read()?;
                verify_identity(&tx, &expected)?;
                tx.ensure_mutation_receipt()?;
                let removed = tx.execute_matching(
                    "DELETE FROM guild_scheduled_work
                     WHERE rowid IN (
                        SELECT rowid FROM guild_scheduled_work
                        WHERE state IN ('done','failed') AND updated_at_ms<?1
                        LIMIT 100
                     )",
                    params![cutoff],
                )?;
                tx.commit()?;
                Ok(removed as u32)
            })
            .await
    }
}

pub fn transient_backoff_ms(attempt: i64, retry_after_secs: Option<u64>) -> i64 {
    if let Some(secs) = retry_after_secs.filter(|s| *s <= 3600) {
        return (secs as i64) * 1_000;
    }
    let seconds = 2_i64.pow(attempt.min(31) as u32).min(15 * 60);
    seconds * 1_000
}
