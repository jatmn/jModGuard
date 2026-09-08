//! Bot membership observations only. These identifiers do not authorize dashboard access.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GuildEnrollment {
    #[serde(serialize_with = "crate::ids::ser_i64_str")]
    pub guild_id: i64,
    pub id: String,
    #[serde(serialize_with = "crate::ids::ser_i64_str")]
    pub joined_at_ns: i64,
}

pub async fn guild_enrollment(db: &Db, guild: i64) -> Result<Option<GuildEnrollment>> {
    anyhow::ensure!(guild > 0, "invalid guild id");
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_read()?;
        let result = active_enrollment_conn(&tx, guild)?;
        tx.finish_read()?;
        Ok(result)
    })
    .await
}

/// Matching reconnects preserve identity; only a newer join replaces a retired join.
pub async fn enroll_guild(
    db: &Db,
    guild: i64,
    joined_at_ns: i64,
) -> Result<Option<GuildEnrollment>> {
    observe_membership(db, guild, joined_at_ns, true).await
}

/// Only use a confirmed departure with that membership's join timestamp.
/// A missing guild/member response or gateway outage is not departure evidence.
pub async fn retire_guild_enrollment(db: &Db, guild: i64, joined_at_ns: i64) -> Result<()> {
    observe_membership(db, guild, joined_at_ns, false).await?;
    Ok(())
}

async fn observe_membership(
    db: &Db,
    guild: i64,
    joined_at_ns: i64,
    present: bool,
) -> Result<Option<GuildEnrollment>> {
    anyhow::ensure!(
        guild > 0 && joined_at_ns > 0,
        "invalid guild membership observation"
    );
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_read()?;
        let current = tx
            .query_row(
                "SELECT id,joined_at_ns,active FROM guild_enrollments WHERE guild_id=?1",
                params![guild],
                |r| Ok((r.get::<String>(0)?, r.get::<i64>(1)?, r.get::<bool>(2)?)),
            )
            .optional()?;
        if let Some((id, joined, active)) = &current {
            if *joined > joined_at_ns || (*joined == joined_at_ns && !active) {
                tx.finish_read()?;
                return Ok(None);
            }
            if *joined == joined_at_ns && present {
                // Healthy reconnect/REST observation writes no row or mutation receipt.
                tx.finish_read()?;
                return Ok(Some(GuildEnrollment {
                    guild_id: guild,
                    id: id.clone(),
                    joined_at_ns,
                }));
            }
        }
        // The primary snapshot already holds the write lock: enabling recovery
        // here preserves the comparison/write atomicity without no-op receipts.
        tx.ensure_mutation_receipt()?;
        // Membership transition and invalidation commit together. Never replay running work.
        tx.execute(
            "UPDATE bot_actions SET state='cancelled',outcome='enrollment_ended',finished_at=unixepoch()
             WHERE guild_id=?1 AND state IN ('pending','claimed')", params![guild],
        )?;
        tx.execute(
            "UPDATE bot_actions SET state='uncertain',outcome='enrollment_ended'
             WHERE guild_id=?1 AND state='running'", params![guild],
        )?;
        let id = format!("{:032x}", rand::random::<u128>());
        tx.execute(
            "INSERT INTO guild_enrollments(guild_id,id,joined_at_ns,active) VALUES(?1,?2,?3,?4)
             ON CONFLICT(guild_id) DO UPDATE SET id=excluded.id,
             joined_at_ns=excluded.joined_at_ns,active=excluded.active",
            params![guild, id.as_str(), joined_at_ns, present],
        )?;
        tx.commit()?;
        Ok(present.then_some(GuildEnrollment {
            guild_id: guild,
            id,
            joined_at_ns,
        }))
    })
    .await
}

pub(super) fn active_enrollment_conn(
    conn: &impl SqlExecutor,
    guild: i64,
) -> Result<Option<GuildEnrollment>> {
    conn.query_row(
        "SELECT id,joined_at_ns FROM guild_enrollments WHERE guild_id=?1 AND active=1",
        params![guild],
        |r| {
            Ok(GuildEnrollment {
                guild_id: guild,
                id: r.get(0)?,
                joined_at_ns: r.get(1)?,
            })
        },
    )
    .optional()
}

/// Verify a captured identity inside the same primary snapshot as its queue operation.
/// It identifies an enrollment; it does not confer user permissions.
pub(super) fn verify_enrollment_conn(
    conn: &impl SqlExecutor,
    expected: &GuildEnrollment,
) -> Result<()> {
    if expected.guild_id <= 0 {
        return Err(EnrollmentChanged.into());
    }
    let current = active_enrollment_conn(conn, expected.guild_id).map_err(|error| {
        tracing::warn!(
            guild_id = expected.guild_id,
            "enrollment verification failed: {error:#}"
        );
        EnrollmentUnverified
    })?;
    if current.as_ref() != Some(expected) {
        return Err(EnrollmentChanged.into());
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("Server membership changed. Reload before making a new request.")]
pub struct EnrollmentChanged;

#[derive(Debug, thiserror::Error)]
#[error("Server access could not be verified. Try again shortly.")]
pub struct EnrollmentUnverified;
