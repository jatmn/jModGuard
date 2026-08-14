use super::*;

pub(super) fn enqueue_nickname_reconcile_conn(
    conn: &impl SqlExecutor,
    guild_id: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO nickname_reconcile
             (guild_id, generation, state, attempt_count, next_attempt_at,
              last_error_code, last_error, last_error_at)
         VALUES (?1, 1, 'pending', 0, 0, NULL, NULL, NULL)
         ON CONFLICT(guild_id) DO UPDATE SET
             generation = generation + 1,
             state = 'pending', attempt_count = 0, next_attempt_at = 0,
             last_error_code = NULL, last_error = NULL, last_error_at = NULL",
        params![guild_id],
    )?;
    Ok(())
}

/// Persists reconciliation work before any Discord nickname side effect. If
/// the following settings transaction rolls back (including an indeterminate
/// remote commit), the bot restores the still-authoritative database value.
pub async fn request_nickname_reconcile(db: &Db, guild_id: i64) -> Result<()> {
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        enqueue_nickname_reconcile_conn(&tx, guild_id)?;
        tx.commit()?;
        Ok(())
    })
    .await
}

/// Applies only guild-lifecycle transitions. Reconnects must not reset a
/// terminal operator error; a real join may create missing work, while any
/// guild-create event can reactivate work made dormant by a prior departure.
pub async fn activate_nickname_reconcile_for_guild_create(
    db: &Db,
    guild_id: i64,
    is_new: bool,
) -> Result<()> {
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        let reactivated = tx.execute(
            "UPDATE nickname_reconcile SET
                 generation = generation + 1, state = 'pending',
                 attempt_count = 0, next_attempt_at = 0,
                 last_error_code = NULL, last_error = NULL, last_error_at = NULL
             WHERE guild_id = ?1 AND state = 'dormant'",
            params![guild_id],
        )?;
        if reactivated == 0 && is_new {
            tx.execute(
                "INSERT OR IGNORE INTO nickname_reconcile
                     (guild_id, generation, state, attempt_count, next_attempt_at,
                      last_error_code, last_error, last_error_at)
                 VALUES (?1, 1, 'pending', 0, 0, NULL, NULL, NULL)",
                params![guild_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    })
    .await
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NicknameReconcileJob {
    pub guild_id: i64,
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
        let defaults = terminal_defaults_conn(&tx)?;
        let mut stmt = tx.prepare(
            "SELECT guild_id, generation, attempt_count
             FROM nickname_reconcile
             WHERE state = 'pending' AND next_attempt_at <= ?1
             ORDER BY next_attempt_at, guild_id LIMIT ?2",
        )?;
        let pending = stmt.query_map(params![now, i64::from(limit.max(1))], |row| {
            Ok((
                row.get::<i64>(0)?,
                row.get::<i64>(1)?,
                row.get::<i64>(2)? as u32,
            ))
        })?;
        let mut out = Vec::new();
        for row in pending {
            let (guild_id, generation, attempt_count) = row?;
            let nickname = effective_settings_conn(&tx, guild_id, &defaults)?.bot_name;
            out.push(NicknameReconcileJob {
                guild_id,
                generation,
                nickname,
                attempt_count,
            });
        }
        tx.finish_read()?;
        Ok(out)
    })
    .await
}

pub async fn nickname_reconcile_status(
    db: &Db,
    guild_id: i64,
) -> Result<Option<NicknameReconcileStatus>> {
    db.with_conn(move |conn| {
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
    let (code, error) = (code.to_string(), error.to_string());
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
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
    let (state, code, error) = (state.to_string(), code.to_string(), error.to_string());
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
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

pub async fn dormancy_nickname_reconcile(db: &Db, guild_id: i64, reason: &str) -> Result<()> {
    let reason = reason.to_string();
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        tx.execute(
            "UPDATE nickname_reconcile SET generation = generation + 1,
                 state = 'dormant', next_attempt_at = 0,
                 last_error_code = 'guild_departed', last_error = ?1,
                 last_error_at = unixepoch() WHERE guild_id = ?2",
            params![reason, guild_id],
        )?;
        tx.commit()?;
        Ok(())
    })
    .await
}

pub async fn acknowledge_nickname_reconcile(db: &Db, guild_id: i64, generation: i64) -> Result<()> {
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        tx.execute(
            "DELETE FROM nickname_reconcile
             WHERE guild_id = ?1 AND generation = ?2 AND state = 'pending'",
            params![guild_id, generation],
        )?;
        tx.commit()?;
        Ok(())
    })
    .await
}
