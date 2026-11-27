//! Directory-side tenant lifecycle: erasure jobs and core wipe.
use super::guild_stores::{
    GuildPlacement, GuildWriteFenced, StoreId, clear_erased_placement_conn, refuse_fenced_write,
};
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuildErasureJob {
    pub guild_id: i64,
    pub enrollment: GuildEnrollment,
    pub placement: GuildPlacement,
}

#[derive(Debug, Clone, Copy)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) enum ErasureCoreStep {
    Directory,
    Placement,
}

pub(crate) enum ErasureCoreProgress {
    Advanced,
    Superseded,
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
        let mut tx = conn.transaction_primary_write()?;
        super::guild_stores::refuse_fenced_write(&tx, enrollment.guild_id)?;
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
        tx.execute_rows(
            "INSERT INTO guild_erasure_jobs(guild_id,enrollment_id,joined_at_ns,store_id,placement_revision,created_at) VALUES(?1,?2,?3,?4,?5,unixepoch())",
            params![
                enrollment.guild_id,
                enrollment.id.as_str(),
                enrollment.joined_at_ns,
                placement.store_id().as_str(),
                placement.revision()
            ], 1,
        )?;
        tx.commit()?;
        Ok(true)
    })
    .await
}

pub async fn guild_erasure_queued(db: &Db, guild: i64) -> Result<bool> {
    anyhow::ensure!(guild > 0, "invalid guild id");
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_snapshot()?;
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

/// Returns the first queued erasure whose guild is not write-fenced.
pub async fn next_guild_erasure(db: &Db) -> Result<Option<GuildErasureJob>> {
    next_guild_erasure_after(db, None).await
}

/// One indexed lookup for an unfenced job after the cursor, wrapping once.
/// The caller advances the cursor even when the selected job cannot finish.
pub(crate) async fn next_guild_erasure_after(
    db: &Db,
    after: Option<i64>,
) -> Result<Option<GuildErasureJob>> {
    db.with_conn(move |conn| {
        const RUNNABLE_JOBS: &str = "FROM guild_erasure_jobs AS job
            WHERE NOT EXISTS(SELECT 1 FROM guild_write_fences AS fence WHERE fence.guild_id=job.guild_id)";
        // Keep fenced jobs queued and let the worker sleep when only those
        // jobs remain. A stale positive advisory still needs primary selection.
        let runnable: bool = conn.query_row_advisory(
            &format!("SELECT EXISTS(SELECT 1 {RUNNABLE_JOBS})"),
            (),
            |r| Ok(r.get(0)?),
        )?;
        if !runnable {
            return Ok(None);
        }
        let tx = conn.transaction_primary_snapshot()?;
        let select = format!(
            "SELECT job.guild_id,job.enrollment_id,job.joined_at_ns,job.store_id,job.placement_revision
             {RUNNABLE_JOBS} AND job.guild_id>?1 ORDER BY job.guild_id LIMIT 1"
        );
        let load = |after: i64| {
            tx.query_row(
                &select,
                params![after],
                |r| Ok((r.get::<i64>(0)?, r.get::<String>(1)?, r.get::<i64>(2)?, r.get::<String>(3)?, r.get::<String>(4)?)),
            ).optional()
        };
        let mut row = load(after.unwrap_or(0))?;
        if row.is_none() && after.is_some() {
            row = load(0)?;
        }
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
    let job = job.clone();
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_write()?;
        refuse_fenced_write(&tx, job.guild_id)?;
        finish_guild_erasure_conn(&mut tx, &job)?;
        tx.commit()
    })
    .await
}

fn finish_guild_erasure_conn(
    tx: &mut crate::db_compat::Transaction,
    job: &GuildErasureJob,
) -> Result<()> {
    tx.prepare_reclaim()?;
    let removed = tx.execute_reclaim(
        "DELETE FROM guild_erasure_jobs WHERE guild_id=?1 AND enrollment_id=?2
         AND joined_at_ns=?3 AND store_id=?4 AND placement_revision=?5",
        params![
            job.guild_id,
            job.enrollment.id.as_str(),
            job.enrollment.joined_at_ns,
            job.placement.store_id().as_str(),
            job.placement.revision()
        ],
    )?;
    anyhow::ensure!(removed == 1, GuildWriteFenced);
    Ok(())
}

pub(crate) async fn advance_erasure_core_step(
    db: &Db,
    job: &GuildErasureJob,
    step: ErasureCoreStep,
) -> Result<ErasureCoreProgress> {
    let job = job.clone();
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_write()?;
        refuse_fenced_write(&tx, job.guild_id)?;
        let current: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM guild_erasure_jobs WHERE guild_id=?1 AND enrollment_id=?2
             AND joined_at_ns=?3 AND store_id=?4 AND placement_revision=?5)",
            params![
                job.guild_id,
                job.enrollment.id.as_str(),
                job.enrollment.joined_at_ns,
                job.placement.store_id().as_str(),
                job.placement.revision()
            ],
            |row| Ok(row.get(0)?),
        )?;
        anyhow::ensure!(current, GuildWriteFenced);
        let advanced = match step {
            ErasureCoreStep::Directory => {
                wipe_departed_directory_guild_data_conn(&mut tx, job.guild_id)?
            }
            ErasureCoreStep::Placement => clear_erased_placement_conn(&mut tx, &job.placement)?,
        };
        let progress = if advanced {
            ErasureCoreProgress::Advanced
        } else {
            // A newer enrollment owns the directory, or the placement is already
            // absent. Settle only this job while that proof still holds.
            finish_guild_erasure_conn(&mut tx, &job)?;
            ErasureCoreProgress::Superseded
        };
        tx.commit()?;
        Ok(progress)
    })
    .await
}

/// Delete a departed guild's directory-owned configuration and core moderation rows.
///
/// The write-fence check, active-enrollment check and deletes share one primary
/// transaction. A stale erasure job may still remove its closed guild-store scope,
/// but it must not wipe directory data after a newer membership has become active.
pub async fn wipe_departed_directory_guild_data(db: &Db, guild: i64) -> Result<bool> {
    anyhow::ensure!(guild > 0, "invalid guild id");
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_write()?;
        refuse_fenced_write(&tx, guild)?;
        let wiped = wipe_departed_directory_guild_data_conn(&mut tx, guild)?;
        if wiped {
            tx.commit()?;
        } else {
            tx.finish_read()?;
        }
        Ok(wiped)
    })
    .await
}

fn wipe_departed_directory_guild_data_conn(
    tx: &mut crate::db_compat::Transaction,
    guild: i64,
) -> Result<bool> {
    if super::enrollments::active_enrollment_conn(tx, guild)?.is_some() {
        return Ok(false);
    }
    tx.prepare_reclaim()?;
    wipe_directory_guild_data_conn(tx, guild, true)?;
    Ok(true)
}

/// Caller holds the primary write transaction and has proved departure.
pub(super) fn wipe_directory_guild_data_conn(
    tx: &crate::db_compat::Transaction,
    guild: i64,
    reclaim: bool,
) -> Result<()> {
    let mut config_changed = false;
    let mut commands_changed = false;
    let mut settings_changed = false;
    for (index, sql) in [
        "DELETE FROM rules WHERE guild_id=?1",
        "DELETE FROM commands WHERE guild_id=?1",
        "DELETE FROM templates WHERE guild_id=?1",
        "DELETE FROM log_channels WHERE guild_id=?1",
        "DELETE FROM log_filters WHERE guild_id=?1",
        "DELETE FROM badwords WHERE guild_id=?1",
        "DELETE FROM known_bad_images WHERE guild_id=?1",
        "DELETE FROM known_spam WHERE guild_id=?1",
        "DELETE FROM guild_settings WHERE guild_id=?1",
        "DELETE FROM events WHERE guild_id=?1",
        "DELETE FROM warns WHERE guild_id=?1",
        "DELETE FROM bans WHERE guild_id=?1",
        "DELETE FROM nickname_reconcile WHERE guild_id=?1",
        "DELETE FROM bot_actions WHERE guild_id=?1",
        "DELETE FROM gateway_ban_deliveries WHERE guild_id=?1",
        "DELETE FROM cleanup_delete_journal WHERE guild_id=?1",
        "DELETE FROM analytics_member_daily WHERE guild_id=?1",
    ]
    .into_iter()
    .enumerate()
    {
        let changed = tx.execute_reclaim(sql, params![guild])? > 0;
        config_changed |= index < 9 && changed;
        commands_changed |= index == 1 && changed;
        settings_changed |= index == 8 && changed;
    }
    if config_changed {
        if reclaim {
            bump_config_version_reclaim_conn(tx)?;
        } else {
            bump_config_version_conn(tx)?;
        }
    }
    if commands_changed {
        if reclaim {
            bump_slash_version_reclaim_conn(tx)?;
        } else {
            bump_slash_registry_version_conn(tx)?;
        }
    }
    if settings_changed {
        invalidate_settings_heal_checksum_conn(tx)?;
    }
    Ok(())
}

/// Read the persisted evidence storage identity without minting a new one.
pub async fn existing_evidence_storage_id(db: &Db, configured: &str) -> Result<Option<String>> {
    if !configured.is_empty() {
        crate::db::guild_data::CaseId::try_from(configured.to_owned())?;
        return Ok(Some(configured.to_owned()));
    }
    db.with_conn(|conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let existing = tx
            .query_row(
                "SELECT value FROM meta WHERE key='evidence_storage_id'",
                (),
                |r| Ok(r.get::<String>(0)?),
            )
            .optional()?;
        tx.finish_read()?;
        Ok(existing)
    })
    .await
}

pub async fn evidence_storage_id(db: &Db, configured: &str) -> Result<String> {
    if !configured.is_empty() {
        crate::db::guild_data::CaseId::try_from(configured.to_owned())?;
        return Ok(configured.to_owned());
    }
    db.with_conn(|conn| {
        {
            let tx = conn.transaction_primary_snapshot()?;
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
            tx.finish_read()?;
        }
        let mut tx = conn.transaction_primary_write()?;
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
        tx.execute_rows(
            "INSERT INTO meta(key,value) VALUES('evidence_storage_id',?1)",
            params![id.as_str()],
            1,
        )?;
        tx.commit()?;
        Ok(id)
    })
    .await
}
