//! Owner server directory facts captured by the bot at membership time.
//! Display data only: rows never authorize access and stay bounded to one row
//! per guild ever enrolled. Only the bot writes; the API reads for the owner
//! directory. Adder identity comes from a bounded audit-log BOT_ADD lookup.
//!
//! Name/icon/member-count facts describe the guild and are monotonic by
//! observation time. The adder describes one membership: it is stored with the
//! enrollment join time it was captured for and is only ever shown for, or
//! written against, that exact membership, so a rejoin can never inherit (or be
//! overwritten by) another membership's installer.
use super::*;

/// One owner-directory row: an enrollment joined with its display facts when
/// they have been observed. Missing facts stay `None` ("not observed yet").
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OwnerServerRow {
    #[serde(serialize_with = "crate::ids::ser_i64_str")]
    pub guild_id: i64,
    /// The membership's identity token: owner actions are bound to it.
    pub enrollment_id: String,
    #[serde(serialize_with = "crate::ids::ser_i64_str")]
    pub joined_at_ns: i64,
    pub active: bool,
    pub name: Option<String>,
    pub icon_hash: Option<String>,
    pub member_count: Option<i64>,
    pub added_by: Option<String>,
    pub added_by_name: Option<String>,
}

fn bound_text(value: &str, max: usize) -> String {
    value.chars().take(max.max(1)).collect()
}

/// Record the guild facts observed at `observed_at_ns`. A fresher stored
/// observation wins, so a delayed snapshot (a slow REST backfill, a replayed
/// gateway event) cannot overwrite newer name/icon/member-count facts. The
/// adder is not a guild fact and is written only by `record_guild_adder`.
pub async fn record_guild_join_info(
    db: &Db,
    guild_id: i64,
    name: &str,
    icon_hash: Option<&str>,
    member_count: Option<i64>,
    observed_at_ns: i64,
) -> Result<()> {
    anyhow::ensure!(
        guild_id > 0 && observed_at_ns > 0,
        "invalid guild join observation"
    );
    let name = bound_text(name.trim(), 128);
    anyhow::ensure!(!name.is_empty(), "invalid guild name");
    let icon_hash = icon_hash
        .map(|hash| bound_text(hash.trim(), 128))
        .filter(|hash| !hash.is_empty());
    let member_count = member_count.filter(|count| *count >= 0);
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_write()?;
        tx.ensure_mutation_receipt()?;
        tx.execute_rows(
            "INSERT INTO guild_join_info(guild_id,name,icon_hash,member_count,recorded_at_ns)
             VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(guild_id) DO UPDATE SET
               name=excluded.name,
               icon_hash=excluded.icon_hash,
               member_count=excluded.member_count,
               recorded_at_ns=excluded.recorded_at_ns
             WHERE excluded.recorded_at_ns >= guild_join_info.recorded_at_ns",
            params![
                guild_id,
                name.as_str(),
                icon_hash.as_deref(),
                member_count,
                observed_at_ns
            ],
            1,
        )?;
        tx.commit()?;
        Ok(())
    })
    .await
}

/// Owner directory rows, active servers first, then the most recent joins.
/// Every active enrollment is always returned: the bot's active memberships are
/// the directory's purpose and are already bounded by Discord. `departed_limit`
/// bounds only retained departed history, which drops its oldest rows first.
/// An adder is reported only when it was captured for the row's current
/// membership; a rejoin shows "unknown" until its own capture resolves.
pub async fn owner_guild_directory(db: &Db, departed_limit: i64) -> Result<Vec<OwnerServerRow>> {
    anyhow::ensure!(
        (1..=4096).contains(&departed_limit),
        "invalid directory limit"
    );
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let rows = tx
            .prepare(
                "SELECT e.guild_id,e.joined_at_ns,e.active,
                        j.name,j.icon_hash,j.member_count,
                        CASE WHEN j.added_by_joined_at_ns=e.joined_at_ns THEN j.added_by END,
                        CASE WHEN j.added_by_joined_at_ns=e.joined_at_ns THEN j.added_by_name END,
                        e.id
                 FROM guild_enrollments e
                 LEFT JOIN guild_join_info j ON j.guild_id=e.guild_id
                 WHERE e.active=1 OR e.guild_id IN (
                     SELECT guild_id FROM guild_enrollments WHERE active=0
                     ORDER BY joined_at_ns DESC,guild_id ASC LIMIT ?1)
                 ORDER BY e.active DESC,e.joined_at_ns DESC,e.guild_id ASC",
            )?
            .query_map(params![departed_limit], |row| {
                Ok(OwnerServerRow {
                    guild_id: row.get(0)?,
                    joined_at_ns: row.get(1)?,
                    active: row.get(2)?,
                    name: row.get(3)?,
                    icon_hash: row.get(4)?,
                    member_count: row.get(5)?,
                    added_by: row.get(6)?,
                    added_by_name: row.get(7)?,
                    enrollment_id: row.get(8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        tx.finish_read()?;
        Ok(rows)
    })
    .await
}

const BACKFILL_FACTS_NEED_REFRESH: &str =
    "(j.guild_id IS NULL OR j.recorded_at_ns < e.joined_at_ns)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuildJoinInfoBackfillCandidate {
    pub guild_id: i64,
    pub joined_at_ns: i64,
    /// The page saw no facts or an observation from before this membership.
    pub refresh_facts: bool,
}

/// Active enrollments whose current membership has no captured adder (no row,
/// no adder, or an adder captured for an earlier membership), in guild-ID
/// order after `after_guild_id`. Include the join time for the adder recency
/// window and whether this enrollment needs a fresh fact observation.
pub async fn guild_join_info_backfill_candidates(
    db: &Db,
    after_guild_id: i64,
    limit: i64,
) -> Result<Vec<GuildJoinInfoBackfillCandidate>> {
    anyhow::ensure!(after_guild_id >= 0, "invalid backfill cursor");
    anyhow::ensure!((1..=4096).contains(&limit), "invalid backfill limit");
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let rows = tx
            .prepare(&format!(
                "SELECT e.guild_id,e.joined_at_ns,{BACKFILL_FACTS_NEED_REFRESH}
                 FROM guild_enrollments e
                 LEFT JOIN guild_join_info j ON j.guild_id=e.guild_id
                 WHERE e.active=1 AND e.guild_id>?1
                   AND (j.guild_id IS NULL OR j.added_by_joined_at_ns IS NOT e.joined_at_ns)
                 ORDER BY e.guild_id ASC
                 LIMIT ?2"
            ))?
            .query_map(params![after_guild_id, limit], |row| {
                Ok(GuildJoinInfoBackfillCandidate {
                    guild_id: row.get(0)?,
                    joined_at_ns: row.get(1)?,
                    refresh_facts: row.get(2)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, anyhow::Error>>()?;
        tx.finish_read()?;
        Ok(rows)
    })
    .await
}

/// Recheck a queued fact refresh just before the detail request. No row means
/// the captured enrollment is no longer active.
pub async fn guild_join_info_backfill_refresh_needed(
    db: &Db,
    guild_id: i64,
    joined_at_ns: i64,
) -> Result<Option<bool>> {
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let refresh_facts = tx
            .query_row(
                &format!(
                    "SELECT {BACKFILL_FACTS_NEED_REFRESH}
                     FROM guild_enrollments e
                     LEFT JOIN guild_join_info j ON j.guild_id=e.guild_id
                     WHERE e.guild_id=?1 AND e.active=1 AND e.joined_at_ns=?2"
                ),
                params![guild_id, joined_at_ns],
                |row| Ok(row.get(0)?),
            )
            .optional()?;
        tx.finish_read()?;
        Ok(refresh_facts)
    })
    .await
}

/// Store the adder captured for the membership that joined at `joined_at_ns`.
/// The write applies only while that is still the guild's recorded membership,
/// so a lookup that finishes after a rejoin cannot name the wrong installer.
/// The update-only write keeps guild facts untouched; a missing row waits for
/// the bounded startup pass instead of guessing a name.
pub async fn record_guild_adder(
    db: &Db,
    guild_id: i64,
    joined_at_ns: i64,
    added_by: &str,
    added_by_name: &str,
) -> Result<()> {
    anyhow::ensure!(
        guild_id > 0 && joined_at_ns > 0,
        "invalid guild adder observation"
    );
    let added_by = added_by.trim().to_owned();
    anyhow::ensure!(
        !added_by.is_empty()
            && added_by.len() <= 32
            && added_by.bytes().all(|b| b.is_ascii_digit()),
        "invalid adder identity"
    );
    let added_by_name = bound_text(added_by_name.trim(), 64);
    anyhow::ensure!(!added_by_name.is_empty(), "invalid adder name");
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_write()?;
        tx.ensure_mutation_receipt()?;
        tx.execute_rows(
            "UPDATE guild_join_info SET added_by=?1,added_by_name=?2,added_by_joined_at_ns=?3
             WHERE guild_id=?4
               AND EXISTS(SELECT 1 FROM guild_enrollments e
                          WHERE e.guild_id=?4 AND e.joined_at_ns=?3)",
            params![
                added_by.as_str(),
                added_by_name.as_str(),
                joined_at_ns,
                guild_id
            ],
            1,
        )?;
        tx.commit()?;
        Ok(())
    })
    .await
}
