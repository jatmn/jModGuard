//! Explicit source association only; no Discord matching heuristics or case merging.
use super::tenant_logs::verify_scope;
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseSourceMatch {
    pub id: CaseId,
    pub removed: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LinkCaseSource {
    Linked(CaseSourceMatch),
    Existing(CaseSourceMatch),
}

pub(super) fn source_match(
    conn: &impl SqlExecutor,
    scope: &GuildDataScope,
    key: &str,
) -> Result<Option<CaseSourceMatch>> {
    let row = conn.query_row("SELECT c.case_id,c.deleted FROM guild_case_sources s JOIN guild_cases c USING(guild_id,enrollment_id,case_id) WHERE s.guild_id=?1 AND s.enrollment_id=?2 AND s.source_key=?3", params![scope.guild_id(),scope.enrollment.id.as_str(),key], |r| Ok((r.get::<String>(0)?,r.get::<bool>(1)?))).optional()?;
    row.map(|(id, removed)| {
        Ok(CaseSourceMatch {
            id: CaseId::try_from(id)?,
            removed,
        })
    })
    .transpose()
}

impl GuildDataStore {
    /// Primary snapshot only. Callers still need permissions and scope checks when
    /// acting on this result. Removed matches carry no case payload.
    pub async fn case_for_source(
        &self,
        scope: GuildDataScope,
        source: CaseSource,
    ) -> Result<Option<CaseSourceMatch>> {
        self.check_target(&scope.placement)?;
        let key = source.key()?;
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let tx = conn.transaction_primary_read()?;
                verify_scope(&tx, &expected, &scope, true)?;
                let result = source_match(&tx, &scope, &key)?;
                tx.finish_read()?;
                Ok(result)
            })
            .await
    }

    /// Bind a proven counterpart to an existing case using an already-bound anchor.
    /// At most one source of each kind; conflicts never merge cases or replace facts.
    /// Late links to removed parents retain replay suppression without payload revival.
    pub async fn link_case_source(
        &self,
        scope: GuildDataScope,
        id: CaseId,
        anchor: CaseSource,
        source: CaseSource,
    ) -> Result<LinkCaseSource> {
        self.check_target(&scope.placement)?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let anchor = anchor.key()?;
        let audit_entry_id = match &source {
            CaseSource::DiscordAudit { entry_id } => Some(*entry_id),
            _ => None,
        };
        let key = source.key()?;
        let kind = source.kind();
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| {
            let mut tx = conn.transaction_primary_read()?;
            verify_scope(&tx, &expected, &scope, true)?;
            let parent = source_match(&tx, &scope, &anchor)?.ok_or(CaseError::Conflict)?;
            anyhow::ensure!(parent.id == id, CaseError::Conflict);
            let staged = if let Some(entry_id) = audit_entry_id {
                tx.query_row("SELECT user_id,action FROM guild_member_unresolved_audits WHERE guild_id=?1 AND enrollment_id=?2 AND entry_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),entry_id],|r|Ok((r.get::<i64>(0)?,r.get::<String>(1)?))).optional()?
            } else {None};
            if let Some((user_id,action)) = &staged && !parent.removed {
                let facts:(Option<i64>,Option<String>)=tx.query_row("SELECT CAST(json_extract(payload,'$.subject_id') AS INTEGER),json_extract(payload,'$.action') FROM guild_cases WHERE guild_id=?1 AND enrollment_id=?2 AND case_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),id.as_str()],|r|Ok((r.get(0)?,r.get(1)?)))?;
                anyhow::ensure!(facts.0==Some(*user_id) && facts.1.as_deref()==Some(action.as_str()),CaseError::Conflict);
            }
            if let Some(existing) = source_match(&tx, &scope, &key)? {
                anyhow::ensure!(existing.id == id, CaseError::Conflict);
                if audit_entry_id.is_some() && !parent.removed {
                    tx.ensure_mutation_receipt()?;
                    super::member_directory::retain_confirmed_ban_history(&tx, &scope, &key)?;
                    if let Some(entry_id)=audit_entry_id.filter(|_|staged.is_some()) {
                        tx.execute_rows("DELETE FROM guild_member_unresolved_audits WHERE guild_id=?1 AND enrollment_id=?2 AND entry_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),entry_id], 1)?;
                    }
                    tx.commit()?;
                } else if let Some(entry_id)=audit_entry_id.filter(|_|staged.is_some()) {
                    tx.ensure_mutation_receipt()?;
                    tx.execute_rows("DELETE FROM guild_member_unresolved_audits WHERE guild_id=?1 AND enrollment_id=?2 AND entry_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),entry_id], 1)?;
                    tx.commit()?;
                } else {tx.finish_read()?;}
                return Ok(LinkCaseSource::Existing(existing));
            }
            let occupied: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM guild_case_sources WHERE guild_id=?1 AND enrollment_id=?2 AND case_id=?3 AND source_kind=?4)", params![scope.guild_id(),scope.enrollment.id.as_str(),id.as_str(),kind], |r| Ok(r.get(0)?))?;
            anyhow::ensure!(!occupied, CaseError::Conflict);
            tx.ensure_mutation_receipt()?;
            tx.execute_rows("INSERT INTO guild_case_sources(guild_id,enrollment_id,case_id,source_key,source_kind) VALUES(?1,?2,?3,?4,?5)", params![scope.guild_id(),scope.enrollment.id.as_str(),id.as_str(),key.as_str(),kind], 1)?;
            if audit_entry_id.is_some() && !parent.removed {
                super::member_directory::retain_confirmed_ban_history(&tx, &scope, &key)?;
            }
            if let Some(entry_id)=audit_entry_id.filter(|_|staged.is_some()) {
                let removed=tx.execute_rows("DELETE FROM guild_member_unresolved_audits WHERE guild_id=?1 AND enrollment_id=?2 AND entry_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),entry_id], 1)?;
                anyhow::ensure!(removed==1,CaseError::Conflict);
            }
            tx.commit()?;
            Ok(LinkCaseSource::Linked(parent))
        }).await
    }
}
