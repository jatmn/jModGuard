//! Coordinated removal of a departed guild's owned store rows. Not a move.
use super::tenant_logs::verify_scope;
use super::*;

impl GuildDataStore {
    /// Only for continuation of a durable directory erasure job. An absent binding
    /// proves this guild has no owned rows (schema deletion guards); a present one
    /// must match the captured placement. Finish an empty binding left by interrupted
    /// activation; populated bindings must match the full scope. Never authorizes data access.
    pub async fn finish_empty_erasure(&self, scope: GuildDataScope) -> Result<bool> {
        self.check_target(&scope.placement)?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let mut tx = conn.transaction_primary_read()?;
                verify_identity(&tx, &expected)?;
                let bound: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM guild_bindings WHERE guild_id=?1)",
                    params![scope.guild_id()],
                    |r| Ok(r.get(0)?),
                )?;
                if !bound {
                    tx.finish_read()?;
                    return Ok(true);
                }
                verify_bound_placement(&tx, &expected, &scope.placement)?;
                let membership: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM guild_memberships WHERE guild_id=?1)",
                    params![scope.guild_id()],
                    |r| Ok(r.get(0)?),
                )?;
                if !membership {
                    // Activation may stop after binding, before membership insertion.
                    // Remove only this exact empty placement, under the same snapshot.
                    tx.prepare_reclaim()?;
                    tx.execute_reclaim(
                        "DELETE FROM guild_bindings WHERE guild_id=?1",
                        params![scope.guild_id()],
                    )?;
                    tx.commit()?;
                    return Ok(true);
                }
                verify_scope(&tx, &expected, &scope, false)?;
                tx.finish_read()?;
                Ok(false)
            })
            .await
    }

    /// Delete remaining closed-scope identity after owned data is gone.
    /// Refuses an active membership. The coordinator uses `finish_empty_erasure`
    /// to resume its core suffix after this transaction has removed the binding.
    pub async fn erase_closed_scope(&self, scope: GuildDataScope) -> Result<bool> {
        self.check_target(&scope.placement)?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let mut tx = conn.transaction_primary_read()?;
                let (active, _) = verify_scope(&tx, &expected, &scope, false)?;
                anyhow::ensure!(!active, CaseError::Active);
                let snapshots: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM guild_message_snapshots WHERE guild_id=?1 AND enrollment_id=?2",
                    params![scope.guild_id(), scope.enrollment.id.as_str()],
                    |r| Ok(r.get(0)?),
                )?;
                if snapshots > 0 {
                    tx.prepare_reclaim()?;
                    let removed = tx.execute_reclaim(
                        "DELETE FROM guild_message_snapshots WHERE (guild_id,enrollment_id,channel_id,message_id) IN (SELECT guild_id,enrollment_id,channel_id,message_id FROM guild_message_snapshots WHERE guild_id=?1 AND enrollment_id=?2 LIMIT 100)",
                        params![scope.guild_id(), scope.enrollment.id.as_str()],
                    )?;
                    anyhow::ensure!(removed > 0, SnapshotError::Invalid);
                    tx.execute_reclaim_metadata_bounded(
                        "UPDATE guild_snapshot_health SET physical_sample_at_ms=NULL WHERE singleton=1",
                        (),
                        super::snapshots::growth_reserve(&tx)?,
                    )?;
                    tx.commit()?;
                    return Ok(false);
                }
                tx.prepare_reclaim()?;
                tx.execute_reclaim(
                    "DELETE FROM guild_tenant_settings WHERE guild_id=?1 AND enrollment_id=?2",
                    params![scope.guild_id(), scope.enrollment.id.as_str()],
                )?;
                for table in ["guild_member_unresolved_audits","guild_member_case_index","guild_member_state_observations","guild_member_target_checks","guild_member_audit_recovery","guild_member_stays","guild_member_pending_bans","guild_member_directory","guild_member_sync","guild_member_seed","guild_member_feature"] {
                    tx.execute_reclaim(&format!("DELETE FROM {table} WHERE guild_id=?1 AND enrollment_id=?2"),params![scope.guild_id(),scope.enrollment.id.as_str()])?;
                }
                tx.execute_reclaim(
                    "DELETE FROM guild_memberships WHERE guild_id=?1 AND enrollment_id=?2 AND active=0",
                    params![scope.guild_id(), scope.enrollment.id.as_str()],
                )?;
                tx.execute_reclaim(
                    "DELETE FROM guild_bindings WHERE guild_id=?1",
                    params![scope.guild_id()],
                )?;
                tx.commit()?;
                Ok(true)
            })
            .await
    }
}
