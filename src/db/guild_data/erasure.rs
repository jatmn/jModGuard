//! Coordinated removal of a departed guild's owned store rows. Not a move.
use super::tenant_logs::verify_scope;
use super::*;

impl GuildDataStore {
    /// Delete remaining closed-scope identity after owned data is gone.
    /// Refuses an active membership. Matching empty retries write nothing.
    pub async fn erase_closed_scope(&self, scope: GuildDataScope) -> Result<bool> {
        self.check_target(&scope.placement)?;
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let mut tx = conn.transaction_primary_read()?;
                let (active, _) = verify_scope(&tx, &expected, &scope, false)?;
                anyhow::ensure!(!active, CaseError::Active);
                let present: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM guild_memberships WHERE guild_id=?1 AND enrollment_id=?2)",
                    params![scope.guild_id(), scope.enrollment.id.as_str()],
                    |r| Ok(r.get(0)?),
                )?;
                if !present {
                    let bound: bool = tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM guild_bindings WHERE guild_id=?1)",
                        params![scope.guild_id()],
                        |r| Ok(r.get(0)?),
                    )?;
                    if !bound {
                        tx.finish_read()?;
                        return Ok(false);
                    }
                }
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
