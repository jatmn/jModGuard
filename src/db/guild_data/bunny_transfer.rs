#![cfg(unix)]
//! Private adapter progress, never a caller-supplied proof that an uncertain PUT stopped.
use super::case_evidence::record;
use super::tenant_logs::verify_scope;
use super::*;

pub(crate) struct BunnyTransfer {
    pub binding: String,
    pub sha256: Option<String>,
    pub accepted: bool,
}
impl GuildDataStore {
    pub(crate) async fn start_bunny_transfer(
        &self,
        scope: GuildDataScope,
        case: CaseId,
        id: CaseId,
        binding: String,
        sha256: String,
    ) -> Result<()> {
        self.check_target(&scope.placement)?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| {
            let mut tx = conn.transaction_primary_read()?;
            verify_scope(&tx, &expected, &scope, true)?;
            let job = record(&tx, &scope, &id)?.ok_or(CaseError::Conflict)?.job;
            anyhow::ensure!(job.case_id == case && job.state == EvidenceState::Capturing && !job.cancelled, CaseError::Conflict);
            tx.ensure_mutation_receipt()?;
            tx.execute("INSERT INTO guild_bunny_evidence(guild_id,enrollment_id,evidence_id,storage_id,binding,sha256,accepted) VALUES(?1,?2,?3,?4,?5,?6,0)", params![scope.guild_id(),scope.enrollment.id.as_str(),id.as_str(),job.storage_id.as_str(),binding,sha256])?;
            tx.commit()
        }).await
    }
    pub(crate) async fn bunny_transfer(
        &self,
        scope: GuildDataScope,
        case: CaseId,
        id: CaseId,
    ) -> Result<BunnyTransfer> {
        self.check_target(&scope.placement)?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| {
            let tx = conn.transaction_primary_read()?;
            verify_scope(&tx, &expected, &scope, false)?;
            anyhow::ensure!(record(&tx, &scope, &id)?.ok_or(CaseError::Conflict)?.job.case_id == case, CaseError::Conflict);
            let result=tx.query_row("SELECT binding,sha256,accepted FROM guild_bunny_evidence WHERE guild_id=?1 AND enrollment_id=?2 AND evidence_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),id.as_str()],|r|Ok(BunnyTransfer{binding:r.get(0)?,sha256:r.get(1)?,accepted:r.get(2)?}))?;
            tx.finish_read()?;
            Ok(result)
        }).await
    }
    pub(crate) async fn accept_bunny_transfer(
        &self,
        scope: GuildDataScope,
        case: CaseId,
        id: CaseId,
    ) -> Result<()> {
        self.check_target(&scope.placement)?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| {
            let mut tx=conn.transaction_primary_read()?;
            verify_scope(&tx,&expected,&scope,false)?;
            anyhow::ensure!(record(&tx,&scope,&id)?.ok_or(CaseError::Conflict)?.job.case_id==case,CaseError::Conflict);
            let accepted:bool=tx.query_row("SELECT accepted FROM guild_bunny_evidence WHERE guild_id=?1 AND enrollment_id=?2 AND evidence_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),id.as_str()],|r|Ok(r.get(0)?))?;
            if accepted {tx.finish_read()?;return Ok(());}
            tx.ensure_mutation_receipt()?;
            tx.execute("UPDATE guild_bunny_evidence SET accepted=1 WHERE guild_id=?1 AND enrollment_id=?2 AND evidence_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),id.as_str()])?;
            tx.commit()
        }).await
    }
}
