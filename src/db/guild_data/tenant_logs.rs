//! Captured tenant operations. Not connected to service routing or Discord lifecycle.
use super::*;

/// Trusted captured directory/membership context, not user authorization. All data
/// operations verify this exact context on the store primary, never re-resolve it.
#[derive(Debug, Clone)]
pub struct GuildDataScope {
    pub(super) placement: GuildPlacement,
    pub(super) enrollment: GuildEnrollment,
}
impl GuildDataScope {
    pub fn new(placement: GuildPlacement, enrollment: GuildEnrollment) -> Result<Self> {
        anyhow::ensure!(
            enrollment.guild_id == placement.guild_id()
                && enrollment.guild_id > 0
                && enrollment.joined_at_ns > 0
                && valid_token(&enrollment.id),
            TenantDataError::Membership
        );
        Ok(Self {
            placement,
            enrollment,
        })
    }
    pub fn guild_id(&self) -> i64 {
        self.placement.guild_id()
    }
    pub fn placement(&self) -> &GuildPlacement {
        &self.placement
    }
    pub fn enrollment(&self) -> &GuildEnrollment {
        &self.enrollment
    }
}

/// A bounded list (one entry per supported event), with its optimistic edit token.
#[derive(Debug, Clone)]
pub struct LogChannelSnapshot {
    revision: String,
    channels: Vec<(String, i64)>,
}
impl LogChannelSnapshot {
    pub fn revision(&self) -> &str {
        &self.revision
    }
    pub fn channels(&self) -> &[(String, i64)] {
        &self.channels
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TenantDataError {
    #[error("guild-store membership does not match or is closed")]
    Membership,
    #[error("guild log-channel configuration changed")]
    Revision,
    #[error("invalid guild log-channel setting")]
    Setting,
}

pub(super) fn valid_token(token: &str) -> bool {
    token.len() == 32
        && token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn new_revision() -> String {
    format!("{:032x}", rand::random::<u128>())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum MembershipState {
    Active,
    Closed,
}

pub(super) struct StoredMembership {
    id: String,
    joined_at_ns: i64,
    pub(super) state: MembershipState,
    revision: String,
}

pub(super) enum ScopeRequirement {
    Active,
    IncludingClosed,
}

fn membership(conn: &impl SqlExecutor, guild: i64) -> Result<Option<StoredMembership>> {
    conn.query_row("SELECT enrollment_id,joined_at_ns,active,config_revision FROM guild_memberships WHERE guild_id=?1", params![guild], |r| Ok(StoredMembership {
        id: r.get(0)?,
        joined_at_ns: r.get(1)?,
        state: if r.get::<bool>(2)? { MembershipState::Active } else { MembershipState::Closed },
        revision: r.get(3)?,
    })).optional()
}

pub(super) fn verify_membership(
    conn: &impl SqlExecutor,
    expected: &StoreRegistration,
    scope: &GuildDataScope,
    requirement: ScopeRequirement,
) -> Result<StoredMembership> {
    verify_bound_placement(conn, expected, &scope.placement)?;
    let stored = membership(conn, scope.guild_id())?.ok_or(TenantDataError::Membership)?;
    anyhow::ensure!(
        stored.id == scope.enrollment.id
            && stored.joined_at_ns == scope.enrollment.joined_at_ns
            && match requirement {
                ScopeRequirement::Active => stored.state == MembershipState::Active,
                ScopeRequirement::IncludingClosed => true,
            },
        TenantDataError::Membership
    );
    anyhow::ensure!(valid_token(&stored.revision), TenantDataError::Revision);
    Ok(stored)
}

// Compatibility adapter for data operations that require the existing tuple contract.
pub(super) fn verify_scope(
    conn: &impl SqlExecutor,
    expected: &StoreRegistration,
    scope: &GuildDataScope,
    require_active: bool,
) -> Result<(bool, String)> {
    let requirement = if require_active {
        ScopeRequirement::Active
    } else {
        ScopeRequirement::IncludingClosed
    };
    let stored = verify_membership(conn, expected, scope, requirement)?;
    Ok((stored.state == MembershipState::Active, stored.revision))
}

impl GuildDataStore {
    /// Bind only the initial trusted membership to an already-bound placement.
    /// No automatic adoption, replacement, rejoin or reopening of a closed scope.
    pub async fn bind_initial_membership(&self, scope: GuildDataScope) -> Result<()> {
        self.check_target(&scope.placement)?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| {
            let mut tx = conn.transaction_primary_write()?;
            verify_bound_placement(&tx, &expected, &scope.placement)?;
            if membership(&tx, scope.guild_id())?.is_some() {
                verify_membership(&tx, &expected, &scope, ScopeRequirement::Active)?;
                return tx.finish_read();
            }
            tx.ensure_mutation_receipt()?;
            tx.execute_rows("INSERT INTO guild_memberships(guild_id,enrollment_id,joined_at_ns,active,config_revision) VALUES(?1,?2,?3,1,?4)", params![scope.guild_id(), scope.enrollment.id.as_str(), scope.enrollment.joined_at_ns, new_revision()], 1)?;
            tx.execute_rows("INSERT INTO guild_member_feature(guild_id,enrollment_id,seed_admitted) VALUES(?1,?2,0)", params![scope.guild_id(), scope.enrollment.id.as_str()], 1)?;
            tx.commit()
        }).await
    }

    pub async fn stored_membership(
        &self,
        placement: GuildPlacement,
    ) -> Result<Option<(GuildEnrollment, bool)>> {
        self.check_target(&placement)?;
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let tx = conn.transaction_primary_snapshot()?;
                verify_bound_placement(&tx, &expected, &placement)?;
                let row = membership(&tx, placement.guild_id())?;
                tx.finish_read()?;
                Ok(row.map(|stored| {
                    (
                        GuildEnrollment {
                            guild_id: placement.guild_id(),
                            id: stored.id,
                            joined_at_ns: stored.joined_at_ns,
                        },
                        stored.state == MembershipState::Active,
                    )
                }))
            })
            .await
    }

    /// Close this exact membership and remove its log mappings atomically. Keep a
    /// closed identity to refuse stale rebinding. This is not full guild erasure:
    /// coordinated directory/backup cleanup and subsequent enrollment are future work.
    pub async fn close_membership(&self, scope: GuildDataScope) -> Result<()> {
        self.check_target(&scope.placement)?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let mut tx = conn.transaction_primary_write()?;
                let stored =
                    verify_membership(&tx, &expected, &scope, ScopeRequirement::IncludingClosed)?;
                if stored.state == MembershipState::Closed {
                    return tx.finish_read();
                }
                tx.prepare_reclaim()?;
                tx.execute_reclaim(
                    "DELETE FROM guild_log_channels WHERE guild_id=?1 AND enrollment_id=?2",
                    params![scope.guild_id(), scope.enrollment.id.as_str()],
                )?;
                anyhow::ensure!(
                    tx.execute_reclaim_metadata_bounded(
                        "UPDATE guild_memberships SET active=0 WHERE guild_id=?1",
                        params![scope.guild_id()],
                        tx.capacity_page_size()?.saturating_mul(1024),
                    )? == 1,
                    TenantDataError::Membership
                );
                tx.commit()
            })
            .await
    }

    pub async fn log_channels(&self, scope: GuildDataScope) -> Result<LogChannelSnapshot> {
        self.check_target(&scope.placement)?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| {
            let tx = conn.transaction_primary_snapshot()?;
            let revision = verify_membership(&tx, &expected, &scope, ScopeRequirement::Active)?.revision;
            let mut stmt = tx.prepare("SELECT event_type,channel_id FROM guild_log_channels WHERE guild_id=?1 AND enrollment_id=?2 ORDER BY event_type")?;
            let rows = stmt.query_map(params![scope.guild_id(), scope.enrollment.id.as_str()], |r| Ok((r.get::<String>(0)?, r.get::<i64>(1)?)))?;
            let mut channels = Vec::new();
            for row in rows { channels.push(row?); }
            drop(stmt);
            tx.finish_read()?;
            Ok(LogChannelSnapshot { revision, channels })
        }).await
    }

    /// Change one mapping only if the caller's loaded revision is still current.
    /// Same-value/absent-delete retries with that current revision write nothing.
    /// Caller must separately verify Discord channel ownership and actor permissions.
    pub async fn set_log_channel(
        &self,
        scope: GuildDataScope,
        revision: &str,
        event: &str,
        channel: Option<i64>,
    ) -> Result<String> {
        self.check_target(&scope.placement)?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        anyhow::ensure!(valid_token(revision), TenantDataError::Revision);
        anyhow::ensure!(
            crate::types::EVENT_TYPES.contains(&event) && channel.is_none_or(|id| id > 0),
            TenantDataError::Setting
        );
        let revision = revision.to_owned();
        let event = event.to_owned();
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| {
            let mut tx = conn.transaction_primary_write()?;
            let current_revision = verify_membership(&tx, &expected, &scope, ScopeRequirement::Active)?.revision;
            anyhow::ensure!(revision == current_revision, TenantDataError::Revision);
            let current = tx.query_row("SELECT channel_id FROM guild_log_channels WHERE guild_id=?1 AND enrollment_id=?2 AND event_type=?3", params![scope.guild_id(), scope.enrollment.id.as_str(), event.as_str()], |r| Ok(r.get::<i64>(0)?)).optional()?;
            if current == channel {
                tx.finish_read()?;
                return Ok(current_revision);
            }
            if let Some(channel) = channel {
                tx.ensure_mutation_receipt()?;
                tx.execute_rows("INSERT INTO guild_log_channels(guild_id,enrollment_id,event_type,channel_id) VALUES(?1,?2,?3,?4) ON CONFLICT(guild_id,event_type) DO UPDATE SET channel_id=excluded.channel_id", params![scope.guild_id(), scope.enrollment.id.as_str(), event.as_str(), channel], 1)?;
            } else {
                tx.prepare_reclaim()?;
                tx.execute_reclaim("DELETE FROM guild_log_channels WHERE guild_id=?1 AND enrollment_id=?2 AND event_type=?3", params![scope.guild_id(), scope.enrollment.id.as_str(), event.as_str()])?;
            }
            let next = new_revision();
            if channel.is_some() {
                tx.execute_rows("UPDATE guild_memberships SET config_revision=?1 WHERE guild_id=?2", params![next.as_str(), scope.guild_id()], 1)?;
            } else {
                anyhow::ensure!(tx.execute_reclaim_metadata_bounded(
                    "UPDATE guild_memberships SET config_revision=?1 WHERE guild_id=?2",
                    params![next.as_str(), scope.guild_id()],
                    tx.capacity_page_size()?.saturating_mul(1024),
                )? == 1, TenantDataError::Membership);
            }
            tx.commit()?;
            Ok(next)
        }).await
    }
}
