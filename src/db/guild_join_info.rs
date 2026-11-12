//! Owner server directory facts captured by the bot at membership time.
//! Display data only: rows never authorize access and stay bounded to one row
//! per guild ever enrolled. Only the bot writes; the API reads for the owner
//! directory. Adder identity comes from a bounded audit-log BOT_ADD lookup.
use super::*;

/// Display facts for one server the bot observed at membership time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GuildJoinInfo {
    #[serde(serialize_with = "crate::ids::ser_i64_str")]
    pub guild_id: i64,
    pub name: String,
    pub icon_hash: Option<String>,
    pub member_count: Option<i64>,
    pub added_by: Option<String>,
    pub added_by_name: Option<String>,
    #[serde(serialize_with = "crate::ids::ser_i64_str")]
    pub recorded_at_ns: i64,
}

/// One owner-directory row: an enrollment joined with its display facts when
/// they have been observed. Missing facts stay `None` ("not observed yet").
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OwnerServerRow {
    #[serde(serialize_with = "crate::ids::ser_i64_str")]
    pub guild_id: i64,
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

/// Record display facts observed at membership time. Fresh name/icon/member
/// observations overwrite stored ones; a `None` adder never erases a stored
/// adder. Adder identity is captured separately by the bot's audit lookup.
#[allow(clippy::too_many_arguments)]
pub async fn record_guild_join_info(
    db: &Db,
    guild_id: i64,
    name: &str,
    icon_hash: Option<&str>,
    member_count: Option<i64>,
    added_by: Option<&str>,
    added_by_name: Option<&str>,
    recorded_at_ns: i64,
) -> Result<()> {
    anyhow::ensure!(
        guild_id > 0 && recorded_at_ns > 0,
        "invalid guild join observation"
    );
    let name = bound_text(name.trim(), 128);
    anyhow::ensure!(!name.is_empty(), "invalid guild name");
    let icon_hash = icon_hash
        .map(|hash| bound_text(hash.trim(), 128))
        .filter(|hash| !hash.is_empty());
    let member_count = member_count.filter(|count| *count >= 0);
    let added_by = added_by
        .map(|id| bound_text(id.trim(), 32))
        .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()));
    let added_by_name = added_by_name
        .map(|name| bound_text(name.trim(), 64))
        .filter(|name| !name.is_empty());
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_write()?;
        tx.ensure_mutation_receipt()?;
        tx.execute_rows(
            "INSERT INTO guild_join_info(guild_id,name,icon_hash,member_count,added_by,added_by_name,recorded_at_ns)
             VALUES(?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(guild_id) DO UPDATE SET
               name=excluded.name,
               icon_hash=excluded.icon_hash,
               member_count=excluded.member_count,
               added_by=COALESCE(excluded.added_by, guild_join_info.added_by),
               added_by_name=COALESCE(excluded.added_by_name, guild_join_info.added_by_name),
               recorded_at_ns=excluded.recorded_at_ns",
            params![
                guild_id,
                name.as_str(),
                icon_hash.as_deref(),
                member_count,
                added_by.as_deref(),
                added_by_name.as_deref(),
                recorded_at_ns
            ],
            1,
        )?;
        tx.commit()?;
        Ok(())
    })
    .await
}

/// Owner directory rows, active servers first, then the most recent joins.
/// Bounded by `limit`: a deployment beyond it loses the oldest departed
/// history while every active server still sorts into the window.
pub async fn owner_guild_directory(db: &Db, limit: i64) -> Result<Vec<OwnerServerRow>> {
    anyhow::ensure!((1..=4096).contains(&limit), "invalid directory limit");
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let rows = tx
            .prepare(
                "SELECT e.guild_id,e.joined_at_ns,e.active,
                        j.name,j.icon_hash,j.member_count,j.added_by,j.added_by_name
                 FROM guild_enrollments e
                 LEFT JOIN guild_join_info j ON j.guild_id=e.guild_id
                 ORDER BY e.active DESC,e.joined_at_ns DESC,e.guild_id ASC
                 LIMIT ?1",
            )?
            .query_map(params![limit], |row| {
                Ok(OwnerServerRow {
                    guild_id: row.get(0)?,
                    joined_at_ns: row.get(1)?,
                    active: row.get(2)?,
                    name: row.get(3)?,
                    icon_hash: row.get(4)?,
                    member_count: row.get(5)?,
                    added_by: row.get(6)?,
                    added_by_name: row.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        tx.finish_read()?;
        Ok(rows)
    })
    .await
}

/// Active enrollments still missing a stored adder (or their whole row),
/// with each membership's join time for the adder recency window. The bot's
/// startup pass resolves these with one audit request per guild.
pub async fn guild_join_info_backfill_candidates(db: &Db, limit: i64) -> Result<Vec<(i64, i64)>> {
    anyhow::ensure!((1..=4096).contains(&limit), "invalid backfill limit");
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let rows = tx
            .prepare(
                "SELECT e.guild_id,e.joined_at_ns FROM guild_enrollments e
                 LEFT JOIN guild_join_info j ON j.guild_id=e.guild_id
                 WHERE e.active=1 AND (j.guild_id IS NULL OR j.added_by IS NULL)
                 ORDER BY e.guild_id ASC
                 LIMIT ?1",
            )?
            .query_map(params![limit], |row| {
                Ok((row.get::<i64>(0)?, row.get::<i64>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, anyhow::Error>>()?;
        tx.finish_read()?;
        Ok(rows)
    })
    .await
}

/// Store the adder for a guild whose facts were already recorded in this
/// process. The update-only write keeps gateway facts untouched; a missing
/// row waits for the bounded startup pass instead of guessing a name.
pub async fn record_guild_adder(
    db: &Db,
    guild_id: i64,
    added_by: &str,
    added_by_name: &str,
    recorded_at_ns: i64,
) -> Result<()> {
    anyhow::ensure!(
        guild_id > 0 && recorded_at_ns > 0,
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
            "UPDATE guild_join_info SET added_by=?1,added_by_name=?2,recorded_at_ns=?3
             WHERE guild_id=?4",
            params![
                added_by.as_str(),
                added_by_name.as_str(),
                recorded_at_ns,
                guild_id
            ],
            1,
        )?;
        tx.commit()?;
        Ok(())
    })
    .await
}
