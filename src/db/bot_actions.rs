//! Durable queue storage only. No current HTTP admission or Discord worker uses it.
use super::*;
use crate::bot_actions::{Action, Limits};

#[derive(Debug, Clone, Serialize)]
pub struct BotAction {
    pub id: String,
    #[serde(serialize_with = "crate::ids::ser_i64_str")]
    pub guild_id: i64,
    #[serde(serialize_with = "crate::ids::ser_i64_str")]
    pub actor_id: i64,
    pub action: Action,
    pub state: String,
    pub outcome: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
    #[serde(skip)]
    pub(crate) claim: Option<String>,
}
impl BotAction {
    /// Claim identity stays private to the data layer and is never serialized.
    pub fn is_claimed(&self) -> bool {
        self.state == "claimed" && self.claim.is_some()
    }
}
const COLS: &str = "id,guild_id,actor_id,payload,state,outcome,created_at,expires_at,claim";

fn row_action(r: &Row) -> Result<BotAction> {
    Ok(BotAction {
        id: r.get(0)?,
        guild_id: r.get(1)?,
        actor_id: r.get(2)?,
        action: serde_json::from_str(&r.get::<String>(3)?)?,
        state: r.get(4)?,
        outcome: r.get(5)?,
        created_at: r.get(6)?,
        expires_at: r.get(7)?,
        claim: r.get(8)?,
    })
}
fn key() -> String {
    format!("{:032x}", rand::random::<u128>())
}

/// Unknown remote commit outcomes must be retried with the same request key.
pub async fn enqueue_bot_action(
    db: &Db,
    guild: i64,
    actor: i64,
    request: &str,
    action: Action,
    now: i64,
    limits: Limits,
) -> Result<BotAction> {
    limits.validate()?;
    action.validate()?;
    anyhow::ensure!(guild > 0 && actor > 0, "invalid guild or actor");
    anyhow::ensure!(
        !request.is_empty()
            && request.len() <= 64
            && request
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        crate::bot_actions::RequestKeyError::Invalid
    );
    let (request, payload, resource, id) = (
        request.to_owned(),
        serde_json::to_string(&action)?,
        action.resource(),
        key(),
    );
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        let existing = tx
            .query_row(
                &format!("SELECT {COLS} FROM bot_actions WHERE guild_id=?1 AND request_key=?2"),
                params![guild, request.as_str()],
                row_action,
            )
            .optional()?;
        if let Some(existing) = existing {
            anyhow::ensure!(
                existing.actor_id == actor && existing.action == action,
                "request key conflicts with another action"
            );
            tx.finish_read()?;
            return Ok(existing);
        }
        let request_time = crate::bot_actions::request_key_time(&request, now)?;
        let floor = action_request_floor(&tx)?.max(now.saturating_sub(i64::from(limits.history_retention_secs)));
        if request_time <= floor {
            return Err(crate::bot_actions::RequestKeyError::Expired.into());
        }
        let count: i64 = tx.query_row(
            "SELECT count(*) FROM bot_actions WHERE guild_id=?1
             AND state IN ('pending','claimed','running','uncertain')",
            params![guild],
            |r| Ok(r.get(0)?),
        )?;
        anyhow::ensure!(
            count < i64::from(limits.pending_per_guild),
            "guild action queue is full"
        );
        let active: i64 = tx.query_row(
            "SELECT count(*) FROM bot_actions WHERE guild_id=?1 AND resource=?2
             AND state IN ('pending','claimed','running','uncertain')",
            params![guild, resource.as_str()],
            |r| Ok(r.get(0)?),
        )?;
        anyhow::ensure!(
            active == 0,
            "an action for this target is still pending or uncertain"
        );
        let expires = now
            .checked_add(i64::from(limits.max_age_secs))
            .context("action expiry overflow")?;
        tx.execute(
            "INSERT INTO bot_actions
             (id,guild_id,actor_id,request_key,resource,payload,state,created_at,expires_at,request_created_at)
             VALUES(?1,?2,?3,?4,?5,?6,'pending',?7,?8,?9)",
            params![
                id.as_str(),
                guild,
                actor,
                request,
                resource,
                payload,
                now,
                expires,
                request_time
            ],
        )?;
        let result = tx.query_row(
            &format!("SELECT {COLS} FROM bot_actions WHERE id=?1"),
            params![id],
            row_action,
        )?;
        tx.commit()?;
        Ok(result)
    })
    .await
}

pub async fn bot_action(db: &Db, guild: i64, id: &str) -> Result<Option<BotAction>> {
    let id = id.to_owned();
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_read()?;
        let job = tx
            .query_row(
                &format!("SELECT {COLS} FROM bot_actions WHERE guild_id=?1 AND id=?2"),
                params![guild, id],
                row_action,
            )
            .optional()?;
        tx.finish_read()?;
        Ok(job)
    })
    .await
}

pub async fn recent_bot_actions(db: &Db, guild: i64) -> Result<Vec<BotAction>> {
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_read()?;
        let mut stmt = tx.prepare(&format!(
            "SELECT {COLS} FROM bot_actions WHERE guild_id=?1
             ORDER BY created_at DESC,id DESC LIMIT 50"
        ))?;
        let rows = stmt.query_map(params![guild], row_action)?;
        let result = rows.collect::<Result<Vec<_>>>()?;
        drop(stmt);
        tx.finish_read()?;
        Ok(result)
    })
    .await
}

/// Claim one job. Expired running work is never claimed again, even after restart.
pub async fn claim_bot_action(db: &Db, now: i64) -> Result<Option<BotAction>> {
    db.with_conn(move |conn| {
        // An advisory read avoids idle write transactions/receipts. Replica lag
        // can defer a poll but never authorizes dispatch: claim below is primary.
        let due: i64 = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM bot_actions WHERE state='pending'
             OR (state IN ('claimed','running') AND expires_at<=?1))",
            params![now],
            |r| Ok(r.get(0)?),
        )?;
        if due == 0 {
            return Ok(None);
        }
        let tx = conn.transaction_immediate()?;
        tx.execute(
            "UPDATE bot_actions SET state='expired',outcome='expired_before_execution',finished_at=?1
             WHERE id IN (SELECT id FROM bot_actions
             WHERE state IN ('pending','claimed') AND expires_at<=?1 LIMIT 64)",
            params![now],
        )?;
        tx.execute(
            "UPDATE bot_actions SET state='uncertain',outcome='worker_outcome_unknown'
             WHERE id IN (SELECT id FROM bot_actions
             WHERE state='running' AND expires_at<=?1 LIMIT 64)",
            params![now],
        )?;
        let job = tx.query_row(
            &format!("SELECT {COLS} FROM bot_actions WHERE state='pending' AND expires_at>?1
                      ORDER BY created_at,id LIMIT 1"),
            params![now],
            row_action,
        )
        .optional()?;
        let Some(mut job) = job else {
            tx.commit()?;
            return Ok(None);
        };
        job.action.validate()?;
        let claim = key();
        tx.execute(
            "UPDATE bot_actions SET state='claimed',claim=?1,claimed_at=?2
             WHERE id=?3 AND state='pending'",
            params![claim.as_str(), now, job.id.as_str()],
        )?;
        job.claim = Some(claim);
        job.state = "claimed".into();
        tx.commit()?;
        Ok(Some(job))
    })
    .await
}

/// Consume the claim before any dispatch. Reusing a copied job cannot send twice.
pub async fn begin_bot_action(db: &Db, job: &BotAction, now: i64) -> Result<bool> {
    let job = job.clone();
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        if !claimed_identity_matches(&tx, &job)? {
            tx.finish_read()?;
            return Ok(false);
        }
        let n = tx.execute(
            "UPDATE bot_actions SET state='running'
             WHERE id=?1 AND claim=?2 AND state='claimed' AND expires_at>?3",
            params![job.id, job.claim, now],
        )?;
        tx.commit()?;
        Ok(n == 1)
    })
    .await
}

/// Completion is claim-bound and atomic with the local effect/audit. A failed
/// receipt must never cause another Discord mutation; running becomes uncertain.
pub async fn finish_bot_action(
    db: &Db,
    job: &BotAction,
    state: &str,
    outcome: &str,
) -> Result<bool> {
    anyhow::ensure!(
        matches!(state, "succeeded" | "failed" | "uncertain"),
        "invalid action outcome"
    );
    let (job, state, outcome) = (job.clone(), state.to_owned(), outcome.to_owned());
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        if !claimed_identity_matches(&tx, &job)? {
            tx.finish_read()?;
            return Ok(false);
        }
        let changed = tx.execute(
            "UPDATE bot_actions SET state=?1,outcome=?2,finished_at=unixepoch()
             WHERE id=?3 AND claim=?4 AND state IN ('running','uncertain')",
            params![
                state.as_str(),
                outcome.as_str(),
                job.id.as_str(),
                job.claim.as_deref()
            ],
        )?;
        if changed == 0 {
            tx.finish_read()?;
            return Ok(false);
        }
        if state == "succeeded" {
            match &job.action {
                Action::Unban { user_id, reason } => {
                    tx.execute(
                        "UPDATE bans SET active=0 WHERE guild_id=?1 AND user_id=?2",
                        params![job.guild_id, user_id],
                    )?;
                    let data = serde_json::json!({
                        "reason":reason, "source":"webui", "action_id":job.id, "outcome":outcome,
                    })
                    .to_string();
                    tx.execute(
                        "INSERT INTO events(guild_id,event_type,user_id,actor_id,data,created_at)
                         VALUES(?1,'member_unban',?2,?3,?4,?5)",
                        params![job.guild_id, user_id, job.actor_id, data, now_str()],
                    )?;
                }
                Action::SetLogChannel { event, channel_id } => {
                    write_log_channel_conn(&tx, job.guild_id, event, *channel_id)?;
                }
            }
        }
        tx.commit()?;
        Ok(true)
    })
    .await
}

pub async fn cancel_bot_action(db: &Db, guild: i64, id: &str) -> Result<bool> {
    let id = id.to_owned();
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        let n = tx.execute(
            "UPDATE bot_actions SET state='cancelled',outcome='cancelled_before_execution',
             finished_at=unixepoch() WHERE guild_id=?1 AND id=?2 AND state='pending'",
            params![guild, id],
        )?;
        tx.commit()?;
        Ok(n == 1)
    })
    .await
}

/// Direct bot/config writers supersede queued log changes in the same transaction.
pub(super) fn supersede_log_action(conn: &impl SqlExecutor, guild: i64, event: &str) -> Result<()> {
    conn.execute(
        "UPDATE bot_actions SET state='cancelled',outcome='superseded_by_config_write',
         finished_at=unixepoch() WHERE guild_id=?1 AND resource=?2
         AND state IN ('pending','claimed','running','uncertain')",
        params![guild, format!("log:{event}")],
    )?;
    Ok(())
}

fn action_request_floor(conn: &impl SqlExecutor) -> Result<i64> {
    let value: String = conn.query_row(
        "SELECT value FROM meta WHERE key='action_request_floor'",
        (),
        |r| Ok(r.get(0)?),
    )?;
    let floor = value
        .parse::<i64>()
        .context("invalid action request floor")?;
    anyhow::ensure!(floor >= 0, "invalid action request floor");
    Ok(floor)
}

/// Remove one bounded batch of resolved operational history. The rejection floor
/// and deletions commit together, so an old request cannot become new work after
/// cleanup, a restart, or a later increase in retention. Moderation audits are separate.
pub async fn prune_bot_action_history(db: &Db, now: i64, limits: Limits) -> Result<usize> {
    limits.validate()?;
    let cutoff = now
        .saturating_sub(i64::from(limits.history_retention_secs))
        .max(0);
    db.with_conn(move |conn| {
        const DUE: &str = "SELECT id FROM bot_actions INDEXED BY bot_actions_finished
            WHERE state IN ('succeeded','failed','cancelled','expired') AND request_created_at>0
            AND max(finished_at,request_created_at)<=?1
            ORDER BY max(finished_at,request_created_at),id LIMIT ?2";
        // A replica can delay housekeeping, never authorize its primary deletion.
        let due: i64 =
            conn.query_row(&format!("SELECT EXISTS({DUE})"), params![cutoff, 1], |r| {
                Ok(r.get(0)?)
            })?;
        if due == 0 {
            return Ok(0);
        }
        let tx = conn.transaction_immediate()?;
        let floor = action_request_floor(&tx)?.max(cutoff);
        let deleted = tx.execute(
            &format!("DELETE FROM bot_actions WHERE id IN ({DUE})"),
            params![cutoff, limits.cleanup_batch_size],
        )?;
        if deleted == 0 {
            tx.finish_read()?;
            return Ok(0);
        }
        tx.execute(
            "UPDATE meta SET value=?1 WHERE key='action_request_floor'",
            params![floor.to_string()],
        )?;
        tx.commit()?;
        Ok(deleted)
    })
    .await
}

fn claimed_identity_matches(conn: &impl SqlExecutor, job: &BotAction) -> Result<bool> {
    let stored = conn
        .query_row(
            &format!("SELECT {COLS} FROM bot_actions WHERE id=?1 AND claim=?2"),
            params![job.id.as_str(), job.claim.as_deref()],
            row_action,
        )
        .optional()?;
    Ok(stored.is_some_and(|stored| {
        stored.guild_id == job.guild_id
            && stored.actor_id == job.actor_id
            && stored.action == job.action
            && stored.expires_at == job.expires_at
    }))
}
