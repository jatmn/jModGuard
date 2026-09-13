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
                    tx.ensure_mutation_receipt()?;
                    tx.execute(
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
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let mut tx = conn.transaction_primary_read()?;
                let (active, _) = verify_scope(&tx, &expected, &scope, false)?;
                anyhow::ensure!(!active, CaseError::Active);
                tx.ensure_mutation_receipt()?;
                tx.execute(
                    "DELETE FROM guild_tenant_settings WHERE guild_id=?1 AND enrollment_id=?2",
                    params![scope.guild_id(), scope.enrollment.id.as_str()],
                )?;
                tx.execute(
                    "DELETE FROM guild_memberships WHERE guild_id=?1 AND enrollment_id=?2 AND active=0",
                    params![scope.guild_id(), scope.enrollment.id.as_str()],
                )?;
                tx.execute(
                    "DELETE FROM guild_bindings WHERE guild_id=?1",
                    params![scope.guild_id()],
                )?;
                tx.commit()?;
                Ok(true)
            })
            .await
    }
}
