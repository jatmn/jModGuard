//! Nonsecret directory catalog foundation. Not live routing or user authorization.
//! Initial placement is one-shot.
use super::*;

/// Opaque installation-assigned identity, never a database URL, token or provider ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreId(String);
impl StoreId {
    pub fn new() -> Self {
        Self(format!("{:032x}", rand::random::<u128>()))
    }
    pub fn parse(value: &str) -> Result<Self> {
        anyhow::ensure!(
            value.len() == 32
                && value
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
            "invalid guild store identity"
        );
        Ok(Self(value.to_owned()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl Default for StoreId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorePolicy {
    Shared,
    Dedicated { guild_id: i64 },
}
impl StorePolicy {
    fn owner(self) -> Option<i64> {
        match self {
            Self::Shared => None,
            Self::Dedicated { guild_id } => Some(guild_id),
        }
    }
    fn permits(self, guild: i64) -> bool {
        self.owner().is_none_or(|owner| owner == guild)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreRegistration {
    id: StoreId,
    policy: StorePolicy,
}
impl StoreRegistration {
    pub fn id(&self) -> &StoreId {
        &self.id
    }
    pub fn policy(&self) -> StorePolicy {
        self.policy
    }
    pub(crate) fn from_parts(id: StoreId, policy: StorePolicy) -> Self {
        Self { id, policy }
    }
}

/// Initial placement identity. A revision is not a distributed writer lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuildPlacement {
    guild_id: i64,
    store_id: StoreId,
    revision: String,
}
impl GuildPlacement {
    pub fn guild_id(&self) -> i64 {
        self.guild_id
    }
    pub fn store_id(&self) -> &StoreId {
        &self.store_id
    }
    pub fn revision(&self) -> &str {
        &self.revision
    }
    pub(crate) fn from_parts(guild_id: i64, store_id: StoreId, revision: String) -> Result<Self> {
        StoreId::parse(&revision)?;
        anyhow::ensure!(guild_id > 0, "invalid guild id");
        Ok(Self {
            guild_id,
            store_id,
            revision,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogConflict {
    #[error("guild store is already registered with different ownership")]
    Registration,
    #[error("guild store is not registered")]
    UnknownStore,
    #[error("dedicated store belongs to another guild")]
    DedicatedOwner,
    #[error("guild already has a different database placement")]
    Placement,
}

/// Persisted guild-scoped fence; an upgrade must not silently unlock existing state.
#[derive(Debug, thiserror::Error)]
#[error("this server has an unresolved database write fence")]
pub struct GuildWriteFenced;

pub(super) fn guild_write_fenced(conn: &impl SqlExecutor, guild: i64) -> Result<bool> {
    if guild <= 0 {
        return Ok(false);
    }
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM guild_write_fences WHERE guild_id=?1)",
        params![guild],
        |r| Ok(r.get::<i64>(0)? == 1),
    )
}

pub(super) fn refuse_fenced_write(conn: &impl SqlExecutor, guild: i64) -> Result<()> {
    if guild_write_fenced(conn, guild)? {
        return Err(GuildWriteFenced.into());
    }
    Ok(())
}

pub async fn guild_is_write_fenced(db: &Db, guild: i64) -> Result<bool> {
    anyhow::ensure!(guild > 0, "invalid guild id");
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_read()?;
        let fenced = guild_write_fenced(&tx, guild)?;
        tx.finish_read()?;
        Ok(fenced)
    })
    .await
}

fn registration_conn(conn: &impl SqlExecutor, id: &StoreId) -> Result<Option<StoreRegistration>> {
    let owner = conn
        .query_row(
            "SELECT dedicated_guild_id FROM guild_store_catalog WHERE store_id=?1",
            params![id.as_str()],
            |row| Ok(row.get::<Option<i64>>(0)?),
        )
        .optional()?;
    Ok(owner.map(|owner| StoreRegistration {
        id: id.clone(),
        policy: owner.map_or(StorePolicy::Shared, |guild_id| StorePolicy::Dedicated {
            guild_id,
        }),
    }))
}

fn placement_conn(conn: &impl SqlExecutor, guild: i64) -> Result<Option<GuildPlacement>> {
    let row = conn
        .query_row(
            "SELECT store_id, revision FROM guild_store_placements WHERE guild_id=?1",
            params![guild],
            |row| Ok((row.get::<String>(0)?, row.get::<String>(1)?)),
        )
        .optional()?;
    let Some((id, revision)) = row else {
        return Ok(None);
    };
    let store_id = StoreId::parse(&id)?;
    // Fail closed if an imported/tampered catalog bypassed its storage constraints.
    let store = registration_conn(conn, &store_id)?.ok_or(CatalogConflict::UnknownStore)?;
    anyhow::ensure!(store.policy.permits(guild), CatalogConflict::DedicatedOwner);
    StoreId::parse(&revision)?;
    Ok(Some(GuildPlacement {
        guild_id: guild,
        store_id,
        revision,
    }))
}

pub async fn store_registration(db: &Db, id: &StoreId) -> Result<Option<StoreRegistration>> {
    let id = id.clone();
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_read()?;
        let value = registration_conn(&tx, &id)?;
        tx.finish_read()?;
        Ok(value)
    })
    .await
}

/// Record an operator-selected identity and immutable sharing policy, not credentials.
/// Retain the same ID when retrying. A matching retry writes nothing.
pub async fn register_store(
    db: &Db,
    id: StoreId,
    policy: StorePolicy,
) -> Result<StoreRegistration> {
    anyhow::ensure!(
        policy.owner().is_none_or(|guild| guild > 0),
        "invalid dedicated guild id"
    );
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_read()?;
        if let Some(existing) = registration_conn(&tx, &id)? {
            anyhow::ensure!(existing.policy == policy, CatalogConflict::Registration);
            tx.finish_read()?;
            return Ok(existing);
        }
        tx.ensure_mutation_receipt()?;
        tx.execute_rows(
            "INSERT INTO guild_store_catalog(store_id,dedicated_guild_id) VALUES(?1,?2)",
            params![id.as_str(), policy.owner()],
            1,
        )?;
        tx.commit()?;
        Ok(StoreRegistration { id, policy })
    })
    .await
}

pub async fn guild_placement(db: &Db, guild: i64) -> Result<Option<GuildPlacement>> {
    anyhow::ensure!(guild > 0, "invalid guild id");
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_read()?;
        let value = placement_conn(&tx, guild)?;
        tx.finish_read()?;
        Ok(value)
    })
    .await
}

/// Assign an unplaced guild. This cannot change an existing placement or move data.
/// No membership is inferred, provisioned or authorized by the catalog entry.
pub async fn assign_initial_placement(
    db: &Db,
    guild: i64,
    store_id: StoreId,
) -> Result<GuildPlacement> {
    anyhow::ensure!(guild > 0, "invalid guild id");
    db.with_conn(move |conn| {
        assign_initial_placement_transaction(conn.transaction_primary_read()?, guild, store_id)
    })
    .await
}

/// Runtime admission: retirement either precedes this check or observes the
/// placement in its own primary transaction and admits the cleanup obligation.
pub(crate) async fn admit_tenant_placement(
    db: &Db,
    enrollment: GuildEnrollment,
    store_id: StoreId,
) -> Result<Option<GuildPlacement>> {
    anyhow::ensure!(enrollment.guild_id > 0, "invalid guild id");
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_read()?;
        let current = super::enrollments::active_enrollment_conn(&tx, enrollment.guild_id)?;
        let queued: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM guild_erasure_jobs WHERE guild_id=?1)",
            params![enrollment.guild_id],
            |r| Ok(r.get(0)?),
        )?;
        if current.as_ref() != Some(&enrollment) || queued {
            tx.finish_read()?;
            return Ok(None);
        }
        assign_initial_placement_transaction(tx, enrollment.guild_id, store_id).map(Some)
    })
    .await
}

fn assign_initial_placement_transaction(
    mut tx: crate::db_compat::Transaction,
    guild: i64,
    store_id: StoreId,
) -> Result<GuildPlacement> {
    let store = registration_conn(&tx, &store_id)?.ok_or(CatalogConflict::UnknownStore)?;
    anyhow::ensure!(store.policy.permits(guild), CatalogConflict::DedicatedOwner);
    if let Some(existing) = placement_conn(&tx, guild)? {
        anyhow::ensure!(existing.store_id == store_id, CatalogConflict::Placement);
        tx.finish_read()?;
        return Ok(existing);
    }
    let revision = format!("{:032x}", rand::random::<u128>());
    tx.ensure_mutation_receipt()?;
    tx.execute_rows(
        "INSERT INTO guild_store_placements(guild_id,store_id,revision) VALUES(?1,?2,?3)",
        params![guild, store_id.as_str(), revision.as_str()],
        1,
    )?;
    tx.commit()?;
    Ok(GuildPlacement {
        guild_id: guild,
        store_id,
        revision,
    })
}

/// Remove a placement after owned live data is erased.
pub async fn clear_erased_placement(
    db: &Db,
    guild: i64,
    expected: &GuildPlacement,
) -> Result<bool> {
    anyhow::ensure!(
        guild > 0 && expected.guild_id() == guild,
        "invalid guild id"
    );
    let store_id = expected.store_id().as_str().to_owned();
    let revision = expected.revision().to_owned();
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_read()?;
        match placement_conn(&tx, guild)? {
            Some(current)
                if current.store_id.as_str() == store_id && current.revision == revision =>
            {
                tx.prepare_reclaim()?;
                tx.execute_reclaim(
                    "DELETE FROM guild_store_placements WHERE guild_id=?1 AND store_id=?2 AND revision=?3",
                    params![guild, store_id.as_str(), revision.as_str()],
                )?;
                tx.commit()?;
                Ok(true)
            }
            None => {
                tx.finish_read()?;
                Ok(false)
            }
            Some(_) => Err(CatalogConflict::Placement.into()),
        }
    })
    .await
}

pub async fn shared_store_registration(db: &Db) -> Result<Option<StoreRegistration>> {
    catalog_registration(db, None).await
}

pub async fn dedicated_store_registration(
    db: &Db,
    guild_id: i64,
) -> Result<Option<StoreRegistration>> {
    anyhow::ensure!(guild_id > 0, "invalid guild id");
    catalog_registration(db, Some(guild_id)).await
}

async fn catalog_registration(db: &Db, owner: Option<i64>) -> Result<Option<StoreRegistration>> {
    db.with_conn(move |conn| {
        let tx = conn.transaction_primary_read()?;
        let row = match owner {
            None => tx
                .query_row(
                    "SELECT store_id FROM guild_store_catalog WHERE dedicated_guild_id IS NULL ORDER BY store_id LIMIT 1",
                    (),
                    |r| Ok(r.get::<String>(0)?),
                )
                .optional()?,
            Some(guild_id) => tx
                .query_row(
                    "SELECT store_id FROM guild_store_catalog WHERE dedicated_guild_id=?1 ORDER BY store_id LIMIT 1",
                    params![guild_id],
                    |r| Ok(r.get::<String>(0)?),
                )
                .optional()?,
        };
        tx.finish_read()?;
        Ok(match row {
            Some(id) => Some(StoreRegistration {
                id: StoreId::parse(&id)?,
                policy: owner.map_or(StorePolicy::Shared, |guild_id| StorePolicy::Dedicated {
                    guild_id,
                }),
            }),
            None => None,
        })
    })
    .await
}
