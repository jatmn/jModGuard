#![cfg(unix)]
//! Private adapter progress, never a caller-supplied proof that an uncertain PUT stopped.
use super::case_evidence::record;
use super::tenant_logs::verify_scope;
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BunnyAcceptance {
    Unresolved,
    Accepted,
}

impl BunnyAcceptance {
    fn from_accepted(accepted: bool) -> Self {
        if accepted {
            Self::Accepted
        } else {
            Self::Unresolved
        }
    }
}

pub(crate) struct BunnyTransfer {
    pub binding: String,
    pub sha256: Option<String>,
    pub acceptance: BunnyAcceptance,
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
            let mut tx = conn.transaction_primary_write()?;
            verify_scope(&tx, &expected, &scope, true)?;
            let job = record(&tx, &scope, &id)?.ok_or(CaseError::Conflict)?.job;
            anyhow::ensure!(job.case_id == case && job.state == EvidenceState::Capturing && job.cancellation == EvidenceCancellation::NotCancelled, CaseError::Conflict);
            tx.ensure_mutation_receipt()?;
            tx.execute_rows("INSERT INTO guild_bunny_evidence(guild_id,enrollment_id,evidence_id,storage_id,binding,sha256,accepted) VALUES(?1,?2,?3,?4,?5,?6,0)", params![scope.guild_id(),scope.enrollment.id.as_str(),id.as_str(),job.storage_id.as_str(),binding,sha256], 1)?;
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
            let tx = conn.transaction_primary_snapshot()?;
            verify_scope(&tx, &expected, &scope, false)?;
            anyhow::ensure!(record(&tx, &scope, &id)?.ok_or(CaseError::Conflict)?.job.case_id == case, CaseError::Conflict);
            let result=tx.query_row("SELECT binding,sha256,accepted FROM guild_bunny_evidence WHERE guild_id=?1 AND enrollment_id=?2 AND evidence_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),id.as_str()],|r|Ok(BunnyTransfer{binding:r.get(0)?,sha256:r.get(1)?,acceptance:BunnyAcceptance::from_accepted(r.get(2)?)}))?;
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
            let mut tx=conn.transaction_primary_write()?;
            verify_scope(&tx,&expected,&scope,false)?;
            anyhow::ensure!(record(&tx,&scope,&id)?.ok_or(CaseError::Conflict)?.job.case_id==case,CaseError::Conflict);
            let acceptance=tx.query_row("SELECT accepted FROM guild_bunny_evidence WHERE guild_id=?1 AND enrollment_id=?2 AND evidence_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),id.as_str()],|r|Ok(BunnyAcceptance::from_accepted(r.get(0)?)))?;
            if acceptance==BunnyAcceptance::Accepted {tx.finish_read()?;return Ok(());}
            tx.ensure_mutation_receipt()?;
            tx.execute_rows("UPDATE guild_bunny_evidence SET accepted=1 WHERE guild_id=?1 AND enrollment_id=?2 AND evidence_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),id.as_str()], 1)?;
            tx.commit()
        }).await
    }
}
