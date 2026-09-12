//! Directory-side tenant lifecycle: erasure jobs, backup-copy intents, core wipe.
use super::guild_stores::{GuildPlacement, StoreId};
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuildErasureJob {
    pub guild_id: i64,
    pub enrollment: GuildEnrollment,
    pub placement: GuildPlacement,
}

pub async fn queue_guild_erasure(
    db: &Db,
    enrollment: GuildEnrollment,
    placement: GuildPlacement,
) -> Result<bool> {
    anyhow::ensure!(
        enrollment.guild_id == placement.guild_id() && enrollment.guild_id > 0,
        "invalid guild erasure"
    );
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_read()?;
        let existing: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM guild_erasure_jobs WHERE guild_id=?1)",
            params![enrollment.guild_id],
            |r| Ok(r.get(0)?),
        )?;
        if existing {
            tx.finish_read()?;
            return Ok(false);
        }
        tx.ensure_mutation_receipt()?;
        tx.execute(
            "INSERT INTO guild_erasure_jobs(guild_id,enrollment_id,joined_at_ns,store_id,placement_revision,created_at) VALUES(?1,?2,?3,?4,?5,unixepoch())",
            params![
                enrollment.guild_id,
                enrollment.id.as_str(),
                enrollment.joined_at_ns,
                placement.store_id().as_str(),
                placement.revision()
            ],
        )?;
        tx.commit()?;
        Ok(true)
    })
    .await
}

pub async fn guild_erasure_queued(db: &Db, guild: i64) -> Result<bool> {
    anyhow::ensure!(guild > 0, "invalid guild id");
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_read()?;
        let queued: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM guild_erasure_jobs WHERE guild_id=?1)",
            params![guild],
            |r| Ok(r.get(0)?),
        )?;
        tx.finish_read()?;
        Ok(queued)
    })
    .await
}

pub async fn next_guild_erasure(db: &Db) -> Result<Option<GuildErasureJob>> {
    db.with_conn(|conn| {
        let tx = conn.transaction_primary_read()?;
        let row = tx
            .query_row(
                "SELECT guild_id,enrollment_id,joined_at_ns,store_id,placement_revision FROM guild_erasure_jobs ORDER BY created_at,guild_id LIMIT 1",
                (),
                |r| {
                    Ok((
                        r.get::<i64>(0)?,
                        r.get::<String>(1)?,
                        r.get::<i64>(2)?,
                        r.get::<String>(3)?,
                        r.get::<String>(4)?,
                    ))
                },
            )
            .optional()?;
        tx.finish_read()?;
        let Some((guild_id, id, joined_at_ns, store_id, revision)) = row else {
            return Ok(None);
        };
        Ok(Some(GuildErasureJob {
            guild_id,
            enrollment: GuildEnrollment {
                guild_id,
                id,
                joined_at_ns,
            },
            placement: GuildPlacement::from_parts(guild_id, StoreId::parse(&store_id)?, revision)?,
        }))
    })
    .await
}

pub async fn finish_guild_erasure(db: &Db, job: &GuildErasureJob) -> Result<()> {
    let guild = job.guild_id;
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_read()?;
        tx.ensure_mutation_receipt()?;
        tx.execute(
            "DELETE FROM guild_erasure_jobs WHERE guild_id=?1",
            params![guild],
        )?;
        tx.commit()
    })
    .await
}

pub async fn record_backup_cleanup_intent(
    db: &Db,
    kind: &str,
    guild: i64,
    enrollment_id: Option<&str>,
    case_id: Option<&str>,
    evidence_id: Option<&str>,
    object_key: Option<&str>,
) -> Result<()> {
    let intent = format!("{:032x}", rand::random::<u128>());
    let kind = kind.to_owned();
    let enrollment_id = enrollment_id.map(str::to_owned);
    let case_id = case_id.map(str::to_owned);
    let evidence_id = evidence_id.map(str::to_owned);
    let object_key = object_key.map(str::to_owned);
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_read()?;
        tx.ensure_mutation_receipt()?;
        tx.execute(
            "INSERT INTO backup_cleanup_intents(intent_id,kind,guild_id,enrollment_id,case_id,evidence_id,object_key,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,unixepoch())",
            params![
                intent,
                kind,
                guild,
                enrollment_id,
                case_id,
                evidence_id,
                object_key
            ],
        )?;
        tx.commit()
    })
    .await
}

/// Delete a departed guild's directory-owned configuration and core moderation rows.
///
/// The active-enrollment check and deletes share one primary transaction. A stale
/// erasure job may still remove its closed guild-store scope, but it must not wipe
/// directory data after a newer membership has become active.
pub async fn wipe_departed_directory_guild_data(db: &Db, guild: i64) -> Result<bool> {
    anyhow::ensure!(guild > 0, "invalid guild id");
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_read()?;
        if super::enrollments::active_enrollment_conn(&tx, guild)?.is_some() {
            tx.finish_read()?;
            return Ok(false);
        }
        tx.ensure_mutation_receipt()?;
        for sql in [
            "DELETE FROM rules WHERE guild_id=?1",
            "DELETE FROM commands WHERE guild_id=?1",
            "DELETE FROM templates WHERE guild_id=?1",
            "DELETE FROM log_channels WHERE guild_id=?1",
            "DELETE FROM badwords WHERE guild_id=?1",
            "DELETE FROM guild_settings WHERE guild_id=?1",
            "DELETE FROM events WHERE guild_id=?1",
            "DELETE FROM warns WHERE guild_id=?1",
            "DELETE FROM bans WHERE guild_id=?1",
            "DELETE FROM nickname_reconcile WHERE guild_id=?1",
            "DELETE FROM bot_actions WHERE guild_id=?1",
        ] {
            tx.execute(sql, params![guild])?;
        }
        tx.commit()?;
        Ok(true)
    })
    .await
}

pub async fn evidence_storage_id(db: &Db, configured: &str) -> Result<String> {
    if !configured.is_empty() {
        crate::db::guild_data::CaseId::try_from(configured.to_owned())?;
        return Ok(configured.to_owned());
    }
    db.with_conn(|conn| {
        let mut tx = conn.transaction_primary_read()?;
        if let Some(existing) = tx
            .query_row(
                "SELECT value FROM meta WHERE key='evidence_storage_id'",
                (),
                |r| Ok(r.get::<String>(0)?),
            )
            .optional()?
        {
            tx.finish_read()?;
            return Ok(existing);
        }
        let id = format!("{:032x}", rand::random::<u128>());
        tx.ensure_mutation_receipt()?;
        tx.execute(
            "INSERT INTO meta(key,value) VALUES('evidence_storage_id',?1)",
            params![id.as_str()],
        )?;
        tx.commit()?;
        Ok(id)
    })
    .await
}
