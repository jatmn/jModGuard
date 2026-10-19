//! Bot membership observations only. These identifiers do not authorize dashboard access.
use super::*;
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuildEnrollment {
    #[serde(
        serialize_with = "crate::ids::ser_i64_str",
        deserialize_with = "crate::ids::de_i64"
    )]
    pub guild_id: i64,
    pub id: String,
    #[serde(
        serialize_with = "crate::ids::ser_i64_str",
        deserialize_with = "crate::ids::de_i64"
    )]
    pub joined_at_ns: i64,
}

pub async fn guild_enrollment(db: &Db, guild: i64) -> Result<Option<GuildEnrollment>> {
    anyhow::ensure!(guild > 0, "invalid guild id");
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let result = active_enrollment_conn(&tx, guild)?;
        tx.finish_read()?;
        Ok(result)
    })
    .await
}

/// Latest trusted membership observation, including confirmed retirement.
/// The boolean is the active state; an inactive identity cannot authorize writes.
pub async fn guild_membership_observation(
    db: &Db,
    guild: i64,
) -> Result<Option<(GuildEnrollment, bool)>> {
    anyhow::ensure!(guild > 0, "invalid guild id");
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_snapshot()?;
        let result = tx
            .query_row(
                "SELECT id,joined_at_ns,active FROM guild_enrollments WHERE guild_id=?1",
                params![guild],
                |row| {
                    Ok((
                        GuildEnrollment {
                            guild_id: guild,
                            id: row.get(0)?,
                            joined_at_ns: row.get(1)?,
                        },
                        row.get(2)?,
                    ))
                },
            )
            .optional()?;
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
    Ok(enroll_guild_with_change(db, guild, joined_at_ns).await?.0)
}

/// Return whether the observation committed a new enrollment identity. Matching
/// confirmations (including nickname resumption) do not change membership.
pub async fn enroll_guild_with_change(
    db: &Db,
    guild: i64,
    joined_at_ns: i64,
) -> Result<(Option<GuildEnrollment>, bool)> {
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
) -> Result<(Option<GuildEnrollment>, bool)> {
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
                return Ok((None, false));
            }
            if *joined == joined_at_ns && present {
                // Healthy observations are write-free; only a dormant request bound
                // to this exact membership may be resumed after positive confirmation.
                let dormant: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM nickname_reconcile WHERE guild_id=?1
                     AND enrollment_id=?2 AND state='dormant' AND job_id IS NOT NULL
                     AND nickname IS NOT NULL)",
                    params![guild, id.as_str()], |r| Ok(r.get(0)?),
                )?;
                if dormant {
                    super::guild_stores::refuse_fenced_write(&tx, guild)?;
                    tx.ensure_mutation_receipt()?;
                    // Membership confirmation cannot settle an unknown Discord edit.
                    tx.execute_rows(
                        "UPDATE nickname_reconcile SET generation=generation+1,state='pending',
                         attempt_count=0,next_attempt_at=0,
                         last_error_code=CASE WHEN last_error_code='outcome_unknown'
                             THEN last_error_code ELSE NULL END,last_error=NULL,
                         last_error_at=NULL WHERE guild_id=?1 AND enrollment_id=?2 AND state='dormant'",
                        params![guild, id.as_str()], 1,
                    )?;
                    tx.commit()?;
                } else {
                    tx.finish_read()?;
                }
                return Ok((Some(GuildEnrollment {
                    guild_id: guild,
                    id: id.clone(),
                    joined_at_ns,
                }), false));
            }
        }
        // The primary snapshot already holds the write lock: enabling recovery
        // here preserves the comparison/write atomicity without no-op receipts.
        super::guild_stores::refuse_fenced_write(&tx, guild)?;
        tx.ensure_mutation_receipt()?;
        // Membership transition and invalidation commit together. Never replay running work.
        // Departure or a newer positive join ends the previous identity. Preserve
        // its cleanup obligation before any replacement activation can be interrupted.
        // Retirement replaces the active identity, so it must not precede admission.
        // A newer departure can follow a missed join; clean the known old identity
        // as well. The earlier timestamp guard already rejected stale departures.
        if let Some((id, joined, true)) = &current
        {
            tx.execute_rows(
                "INSERT INTO guild_erasure_jobs(guild_id,enrollment_id,joined_at_ns,store_id,placement_revision,created_at)
                 SELECT guild_id,?2,?3,store_id,revision,unixepoch() FROM guild_store_placements
                 WHERE guild_id=?1 AND NOT EXISTS(SELECT 1 FROM guild_erasure_jobs WHERE guild_id=?1)",
                params![guild, id.as_str(), *joined], 1,
            )?;
        }
        tx.execute_matching(
            "UPDATE bot_actions SET state='cancelled',outcome='enrollment_ended',finished_at=unixepoch()
             WHERE guild_id=?1 AND state IN ('pending','claimed')", params![guild],
        )?;
        tx.execute_matching(
            "UPDATE bot_actions SET state='uncertain',outcome='enrollment_ended'
             WHERE guild_id=?1 AND state='running'", params![guild],
        )?;
        tx.execute_rows(
            "UPDATE nickname_reconcile SET generation=generation+1,state='dormant',
             next_attempt_at=0,last_error_code='enrollment_ended',
             last_error='fresh nickname request required',last_error_at=unixepoch()
             WHERE guild_id=?1 AND state!='dormant'", params![guild], 1,
        )?;
        let id = format!("{:032x}", rand::random::<u128>());
        tx.execute_rows(
            "INSERT INTO guild_enrollments(guild_id,id,joined_at_ns,active) VALUES(?1,?2,?3,?4)
             ON CONFLICT(guild_id) DO UPDATE SET id=excluded.id,
             joined_at_ns=excluded.joined_at_ns,active=excluded.active",
            params![guild, id.as_str(), joined_at_ns, present], 1,
        )?;
        if !present {
            let placed: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM guild_store_placements WHERE guild_id=?1)",
                params![guild], |r| Ok(r.get(0)?),
            )?;
            if !placed {
                // No store exists to visit. Settle directory ownership in the same
                // transaction as retirement.
                super::tenant_lifecycle::wipe_directory_guild_data_conn(&tx, guild, false)?;
            }
        }
        tx.commit()?;
        Ok((present.then_some(GuildEnrollment {
            guild_id: guild,
            id,
            joined_at_ns,
        }), true))
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

/// Explicit configuration scope captured at authorization. This is not a credential:
/// callers still enforce user permissions; transactions verify the enrollment identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigurationScope {
    Global,
    Guild(GuildEnrollment),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeWire {
    pub global: bool,
    pub guild_id: i64,
    pub enrollment_id: String,
    pub joined_at_ns: i64,
}

impl ConfigurationScope {
    pub fn guild_id(&self) -> i64 {
        match self {
            Self::Global => GLOBAL_SCOPE,
            Self::Guild(enrollment) => enrollment.guild_id,
        }
    }
    pub fn to_wire(&self) -> ScopeWire {
        match self {
            Self::Global => ScopeWire {
                global: true,
                guild_id: GLOBAL_SCOPE,
                enrollment_id: String::new(),
                joined_at_ns: 0,
            },
            Self::Guild(enrollment) => ScopeWire {
                global: false,
                guild_id: enrollment.guild_id,
                enrollment_id: enrollment.id.clone(),
                joined_at_ns: enrollment.joined_at_ns,
            },
        }
    }
    pub(super) fn verify_conn(&self, conn: &impl SqlExecutor) -> Result<()> {
        if let Self::Guild(expected) = self {
            verify_enrollment_conn(conn, expected)?;
        }
        Ok(())
    }
}

/// A configuration edit retains both the page's enrollment and its authorized target.
/// Only that same scope or global configuration may be targeted. User/owner permissions
/// are still enforced by the caller; this value is not a credential.
#[derive(Debug, Clone)]
pub struct ConfigurationWriteScope {
    request: ConfigurationScope,
    target: ConfigurationScope,
}
impl ConfigurationWriteScope {
    pub fn to_wires(&self) -> (ScopeWire, ScopeWire) {
        (self.request.to_wire(), self.target.to_wire())
    }
    pub fn new(request: ConfigurationScope, target: ConfigurationScope) -> Result<Self> {
        if target != request && target != ConfigurationScope::Global {
            return Err(EnrollmentChanged.into());
        }
        Ok(Self { request, target })
    }
    pub fn guild_id(&self) -> i64 {
        self.target.guild_id()
    }
    pub(super) fn verify_conn(&self, conn: &impl SqlExecutor) -> Result<()> {
        let ConfigurationScope::Guild(expected) = &self.request else {
            return Ok(());
        };
        if expected.guild_id <= 0 {
            return Err(EnrollmentChanged.into());
        }
        let target_guild = self.target.guild_id();
        let (enrollment_matches, write_fenced) = conn
            .query_row(
                "SELECT
                   EXISTS(SELECT 1 FROM guild_enrollments
                          WHERE guild_id=?1 AND id=?2 AND joined_at_ns=?3 AND active=1),
                   EXISTS(SELECT 1 FROM guild_write_fences WHERE guild_id=?4)",
                params![
                    expected.guild_id,
                    expected.id.as_str(),
                    expected.joined_at_ns,
                    target_guild
                ],
                |row| Ok((row.get::<bool>(0)?, row.get::<bool>(1)?)),
            )
            .map_err(|error| {
                tracing::warn!(
                    guild_id = expected.guild_id,
                    "configuration enrollment verification failed: {error:#}"
                );
                EnrollmentUnverified
            })?;
        if !enrollment_matches {
            return Err(EnrollmentChanged.into());
        }
        if write_fenced {
            return Err(super::guild_stores::GuildWriteFenced.into());
        }
        Ok(())
    }
}
