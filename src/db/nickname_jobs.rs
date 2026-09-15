use super::enrollments::active_enrollment_conn;
use super::*;

pub(super) fn enqueue_nickname_reconcile_conn(
    conn: &impl SqlExecutor,
    guild_id: i64,
) -> Result<()> {
    let enrollment = active_enrollment_conn(conn, guild_id)?;
    let defaults = terminal_defaults_conn(conn)?;
    let nickname = effective_settings_conn(conn, guild_id, &defaults)?.bot_name;
    let job_id = format!("{:032x}", rand::random::<u128>());
    conn.execute(
        "INSERT INTO nickname_reconcile
             (guild_id, generation, state, attempt_count, next_attempt_at,
              last_error_code, last_error, last_error_at, job_id, enrollment_id, nickname)
         VALUES (?1, 1, ?2, 0, 0, ?3, NULL, NULL, ?4, ?5, ?6)
         ON CONFLICT(guild_id) DO UPDATE SET
             generation = generation + 1,
             state = excluded.state, attempt_count = 0, next_attempt_at = 0,
             last_error_code = excluded.last_error_code, last_error = NULL, last_error_at = NULL,
             job_id = excluded.job_id, enrollment_id = excluded.enrollment_id,
             nickname = excluded.nickname",
        params![
            guild_id,
            if enrollment.is_some() {
                "pending"
            } else {
                "dormant"
            },
            if enrollment.is_some() {
                None
            } else {
                Some("enrollment_required")
            },
            job_id,
            enrollment.map(|e| e.id),
            nickname
        ],
    )?;
    Ok(())
}

/// Queue an explicit re-sync of the current stored nickname and active membership.
/// Settings writes capture their replacement nickname in their own transaction.
pub async fn request_nickname_reconcile(db: &Db, guild_id: i64) -> Result<()> {
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        super::guild_stores::refuse_fenced_write(&tx, guild_id)?;
        enqueue_nickname_reconcile_conn(&tx, guild_id)?;
        tx.commit()?;
        Ok(())
    })
    .await
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NicknameReconcileJob {
    pub guild_id: i64,
    pub job_id: String,
    pub enrollment_id: String,
    pub joined_at_ns: i64,
    pub generation: i64,
    pub nickname: String,
    pub attempt_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NicknameReconcileStatus {
    pub state: String,
    pub attempt_count: u32,
    pub next_attempt_at: i64,
    pub last_error_code: Option<String>,
    pub last_error: Option<String>,
    pub last_error_at: Option<i64>,
}

pub const MAX_NICKNAME_RECONCILE_ATTEMPTS: u32 = 8;

pub(super) fn validate_discord_nickname(nickname: &str) -> Result<()> {
    anyhow::ensure!(
        nickname.chars().count() <= 32,
        "Discord nickname must be at most 32 characters"
    );
    anyhow::ensure!(
        !nickname.chars().any(char::is_control),
        "Discord nickname cannot contain control characters"
    );
    Ok(())
}

/// Primary-routed due nickname work. Persisted scheduling prevents a dead row
/// from consuming Discord calls or delaying later jobs on every poll.
pub async fn pending_nickname_reconciles(
    db: &Db,
    now: i64,
    limit: u32,
) -> Result<Vec<NicknameReconcileJob>> {
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_read()?;
        let mut stmt = tx.prepare(
            "SELECT n.guild_id,n.generation,n.attempt_count,n.nickname,n.job_id,n.enrollment_id,e.joined_at_ns
             FROM nickname_reconcile n JOIN guild_enrollments e
                 ON e.guild_id=n.guild_id AND e.id=n.enrollment_id AND e.active=1
             WHERE n.state='pending' AND n.next_attempt_at <= ?1 AND n.job_id IS NOT NULL
                 AND n.nickname IS NOT NULL
                 AND NOT EXISTS(SELECT 1 FROM guild_write_fences f WHERE f.guild_id=n.guild_id)
             ORDER BY n.next_attempt_at,n.guild_id LIMIT ?2",
        )?;
        let out = stmt.query_map(params![now, i64::from(limit.max(1))], |row| {
            Ok(NicknameReconcileJob {
                guild_id: row.get(0)?, generation: row.get(1)?,
                attempt_count: row.get::<i64>(2)? as u32, nickname: row.get(3)?,
                job_id: row.get(4)?, enrollment_id: row.get(5)?, joined_at_ns: row.get(6)?,
            })
        })?.collect::<Result<Vec<_>>>()?;
        tx.finish_read()?;
        Ok(out)
    })
    .await
}

pub async fn nickname_reconcile_status(
    db: &Db,
    guild_id: i64,
) -> Result<Option<NicknameReconcileStatus>> {
    db.with_conn(move |conn| nickname_reconcile_status_conn(conn, guild_id))
        .await
}

pub(super) fn nickname_reconcile_status_conn(
    conn: &impl SqlExecutor,
    guild_id: i64,
) -> Result<Option<NicknameReconcileStatus>> {
    conn.query_row(
        "SELECT state, attempt_count, next_attempt_at,
                    last_error_code, last_error, last_error_at
             FROM nickname_reconcile WHERE guild_id = ?1",
        params![guild_id],
        |row| {
            Ok(NicknameReconcileStatus {
                state: row.get(0)?,
                attempt_count: row.get::<i64>(1)? as u32,
                next_attempt_at: row.get(2)?,
                last_error_code: row.get(3)?,
                last_error: row.get(4)?,
                last_error_at: row.get(5)?,
            })
        },
    )
    .optional()
}

fn nickname_job_matches_conn(conn: &impl SqlExecutor, job: &NicknameReconcileJob) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM nickname_reconcile n JOIN guild_enrollments e
            ON e.guild_id=n.guild_id AND e.id=n.enrollment_id AND e.active=1
         WHERE n.guild_id=?1 AND n.job_id=?2 AND n.enrollment_id=?3 AND e.joined_at_ns=?4
            AND n.generation=?5 AND n.nickname=?6 AND n.attempt_count=?7 AND n.state='pending')",
        params![
            job.guild_id,
            job.job_id.as_str(),
            job.enrollment_id.as_str(),
            job.joined_at_ns,
            job.generation,
            job.nickname.as_str(),
            i64::from(job.attempt_count)
        ],
        |r| Ok(r.get(0)?),
    )
}

/// Recheck the exact pending request on the primary immediately before Discord dispatch.
/// This does not lock Discord or provide exclusive ownership across bot replicas.
pub async fn nickname_reconcile_can_dispatch(db: &Db, job: &NicknameReconcileJob) -> Result<bool> {
    let job = job.clone();
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_read()?;
        let matches = nickname_job_matches_conn(&tx, &job)?
            && !super::guild_stores::guild_write_fenced(&tx, job.guild_id)?;
        tx.finish_read()?;
        Ok(matches)
    })
    .await
}

fn nickname_retry_delay_secs(job: &NicknameReconcileJob) -> i64 {
    let exponent = job.attempt_count.min(9);
    let base = (5_i64 * (1_i64 << exponent)).min(3600);
    let jitter_window = (base / 4).max(1);
    let seed = job.guild_id.unsigned_abs()
        ^ (job.generation as u64).rotate_left(17)
        ^ u64::from(job.attempt_count).rotate_left(33);
    base + (seed % jitter_window as u64) as i64
}

pub async fn schedule_nickname_reconcile_retry(
    db: &Db,
    job: &NicknameReconcileJob,
    now: i64,
    code: &str,
    error: &str,
) -> Result<bool> {
    let attempts = job.attempt_count.saturating_add(1);
    let exhausted = attempts >= MAX_NICKNAME_RECONCILE_ATTEMPTS;
    let next_attempt_at = now + nickname_retry_delay_secs(job);
    let (guild_id, generation) = (job.guild_id, job.generation);
    let job = job.clone();
    let (code, error) = (code.to_string(), error.to_string());
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        if !nickname_job_matches_conn(&tx, &job)? {
            tx.finish_read()?;
            return Ok(false);
        }
        tx.execute(
            "UPDATE nickname_reconcile SET
                 state = ?1, attempt_count = ?2, next_attempt_at = ?3,
                 last_error_code = ?4, last_error = ?5, last_error_at = ?6
             WHERE guild_id = ?7 AND generation = ?8 AND state = 'pending'",
            params![
                if exhausted { "terminal" } else { "pending" },
                i64::from(attempts),
                next_attempt_at,
                code,
                error,
                now,
                guild_id,
                generation
            ],
        )?;
        tx.commit()?;
        Ok(exhausted)
    })
    .await
}

pub async fn finish_nickname_reconcile(
    db: &Db,
    job: &NicknameReconcileJob,
    state: &str,
    code: &str,
    error: &str,
    now: i64,
) -> Result<()> {
    anyhow::ensure!(matches!(state, "terminal" | "dormant"));
    let (guild_id, generation) = (job.guild_id, job.generation);
    let job = job.clone();
    let (state, code, error) = (state.to_string(), code.to_string(), error.to_string());
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        if !nickname_job_matches_conn(&tx, &job)? {
            tx.finish_read()?;
            return Ok(());
        }
        tx.execute(
            "UPDATE nickname_reconcile SET state = ?1, attempt_count = attempt_count + 1,
                 next_attempt_at = 0, last_error_code = ?2,
                 last_error = ?3, last_error_at = ?4
             WHERE guild_id = ?5 AND generation = ?6 AND state = 'pending'",
            params![state, code, error, now, guild_id, generation],
        )?;
        tx.commit()?;
        Ok(())
    })
    .await
}

pub async fn acknowledge_nickname_reconcile(db: &Db, job: &NicknameReconcileJob) -> Result<()> {
    let job = job.clone();
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        if !nickname_job_matches_conn(&tx, &job)? {
            tx.finish_read()?;
            return Ok(());
        }
        tx.execute(
            "DELETE FROM nickname_reconcile WHERE guild_id=?1",
            params![job.guild_id],
        )?;
        tx.commit()?;
        Ok(())
    })
    .await
}
