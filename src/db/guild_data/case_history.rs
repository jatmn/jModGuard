//! Inactive append-only case history; original facts and Discord state stay unchanged.
use super::tenant_logs::verify_scope;
use super::*;
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseReasonCorrection {
    Clear,
    Set(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CaseHistoryBody {
    Note {
        text: String,
    },
    Correction {
        outcome: Option<CaseOutcome>,
        reason: Option<CaseReasonCorrection>,
        explanation: String,
    },
    /// Selected caller-observed policy context, not the whole configuration or proof of execution.
    Policy {
        rule_id: String,
        revision: String,
        description: String,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseHistoryData {
    #[serde(
        serialize_with = "crate::ids::ser_opt_i64_str",
        deserialize_with = "crate::ids::de_opt_i64"
    )]
    pub actor_id: Option<i64>,
    pub recorded_at_ms: i64,
    pub body: CaseHistoryBody,
}

/// Explicit trusted budgets; no new installation defaults are chosen here. Entry
/// bytes/page counts reuse CaseLimits. Evidence-item budget has no effect on text history.
#[derive(Debug, Clone, Copy)]
pub struct CaseHistoryLimits {
    pub entry: CaseLimits,
    pub max_entries: u32,
    pub max_total_bytes: u64,
}
impl CaseHistoryLimits {
    fn validate(self) -> Result<()> {
        self.entry.validate()?;
        anyhow::ensure!(
            self.max_entries > 0
                && self.max_total_bytes > 0
                && self.max_total_bytes <= i64::MAX as u64,
            CaseError::Limit
        );
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseHistoryRef {
    pub id: CaseId,
    /// Primary insertion order within this case; not a timestamp or a global cursor.
    pub sequence: i64,
}
#[derive(Debug, PartialEq, Eq)]
pub enum AppendCaseHistory {
    Created(CaseHistoryRef),
    Existing(CaseHistoryRef),
}

fn validate(data: &CaseHistoryData, limits: CaseHistoryLimits) -> Result<()> {
    limits.validate()?;
    anyhow::ensure!(
        data.actor_id.is_none_or(|id| id > 0) && data.recorded_at_ms > 0,
        CaseError::Invalid
    );
    let strings: [&str; 3] = match &data.body {
        CaseHistoryBody::Note { text } => {
            anyhow::ensure!(!text.trim().is_empty(), CaseError::Invalid);
            [text, "", ""]
        }
        CaseHistoryBody::Correction {
            outcome,
            reason,
            explanation,
        } => {
            anyhow::ensure!(
                (outcome.is_some() || reason.is_some()) && !explanation.trim().is_empty(),
                CaseError::Invalid
            );
            match reason {
                Some(CaseReasonCorrection::Set(reason)) => [explanation, reason, ""],
                _ => [explanation, "", ""],
            }
        }
        CaseHistoryBody::Policy {
            rule_id,
            revision,
            description,
        } => {
            anyhow::ensure!(
                !rule_id.trim().is_empty()
                    && !revision.trim().is_empty()
                    && !description.trim().is_empty(),
                CaseError::Invalid
            );
            [rule_id, revision, description]
        }
    };
    let bytes = strings.into_iter().try_fold(0usize, |total, text| {
        total.checked_add(text.len()).ok_or(CaseError::Limit)
    })?;
    anyhow::ensure!(
        bytes <= limits.entry.max_record_bytes as usize,
        CaseError::Limit
    );
    Ok(())
}

fn live_parent(conn: &impl SqlExecutor, scope: &GuildDataScope, case: &CaseId) -> Result<bool> {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM guild_cases WHERE guild_id=?1 AND enrollment_id=?2 AND case_id=?3 AND deleted=0)", params![scope.guild_id(),scope.enrollment.id.as_str(),case.as_str()], |r| Ok(r.get(0)?))
}

impl GuildDataStore {
    /// Retain the same entry ID and payload across retries. Different entries append;
    /// they never overwrite original facts or compute an authoritative effective outcome.
    pub async fn append_case_history(
        &self,
        scope: GuildDataScope,
        case: CaseId,
        id: CaseId,
        data: CaseHistoryData,
        limits: CaseHistoryLimits,
    ) -> Result<AppendCaseHistory> {
        self.check_target(&scope.placement)?;
        validate(&data, limits)?;
        let payload = serde_json::to_string(&data)?;
        anyhow::ensure!(
            payload.len() <= limits.entry.max_record_bytes as usize,
            CaseError::Limit
        );
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| {
            let mut tx=conn.transaction_primary_read()?;
            verify_scope(&tx,&expected,&scope,true)?;
            anyhow::ensure!(live_parent(&tx,&scope,&case)?, CaseError::Conflict);
            let existing=tx.query_row("SELECT sequence,payload=?5 FROM guild_case_history WHERE guild_id=?1 AND enrollment_id=?2 AND case_id=?3 AND entry_id=?4",params![scope.guild_id(),scope.enrollment.id.as_str(),case.as_str(),id.as_str(),payload.as_str()],|r|Ok((r.get::<i64>(0)?,r.get::<bool>(1)?))).optional()?;
            if let Some((sequence,equal))=existing {
                anyhow::ensure!(equal,CaseError::Conflict);
                tx.finish_read()?;
                return Ok(AppendCaseHistory::Existing(CaseHistoryRef{id,sequence}));
            }
            let (count,last,bytes)=tx.query_row("SELECT COUNT(*),COALESCE(MAX(sequence),0),COALESCE(SUM(length(CAST(payload AS BLOB))),0) FROM guild_case_history WHERE guild_id=?1 AND enrollment_id=?2 AND case_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),case.as_str()],|r|Ok((r.get::<i64>(0)?,r.get::<i64>(1)?,r.get::<i64>(2)?)))?;
            anyhow::ensure!(count>=0 && count<i64::from(limits.max_entries) && bytes>=0 && (bytes as u64).checked_add(payload.len() as u64).is_some_and(|total|total<=limits.max_total_bytes),CaseError::Limit);
            let sequence=last.checked_add(1).filter(|n|*n>0).ok_or(CaseError::Limit)?;
            tx.ensure_mutation_receipt()?;
            tx.execute("INSERT INTO guild_case_history(guild_id,enrollment_id,case_id,entry_id,sequence,payload) VALUES(?1,?2,?3,?4,?5,?6)",params![scope.guild_id(),scope.enrollment.id.as_str(),case.as_str(),id.as_str(),sequence,payload.as_str()])?;
            tx.commit()?;
            Ok(AppendCaseHistory::Created(CaseHistoryRef{id,sequence}))
        }).await
    }

    pub async fn case_history_entry(
        &self,
        scope: GuildDataScope,
        case: CaseId,
        id: CaseId,
        limits: CaseHistoryLimits,
    ) -> Result<Option<CaseHistoryData>> {
        self.check_target(&scope.placement)?;
        limits.validate()?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| {
            let tx=conn.transaction_primary_read()?;
            verify_scope(&tx,&expected,&scope,true)?;
            if !live_parent(&tx,&scope,&case)? { tx.finish_read()?; return Ok(None); }
            let row=tx.query_row("SELECT CASE WHEN length(CAST(payload AS BLOB))<=?5 THEN payload ELSE NULL END FROM guild_case_history WHERE guild_id=?1 AND enrollment_id=?2 AND case_id=?3 AND entry_id=?4",params![scope.guild_id(),scope.enrollment.id.as_str(),case.as_str(),id.as_str(),i64::from(limits.entry.max_record_bytes)],|r|Ok(r.get::<Option<String>>(0)?)).optional()?;
            let result=match row {
                None=>None,
                Some(None)=>return Err(CaseError::Limit.into()),
                Some(Some(payload))=>{let data:CaseHistoryData=serde_json::from_str(&payload)?;validate(&data,limits)?;Some(data)}
            };
            tx.finish_read()?;
            Ok(result)
        }).await
    }

    /// Sequence keyset page of IDs only. Missing/removed parent yields an empty page;
    /// invalid captured membership still fails. No history payloads are bulk-loaded.
    pub async fn case_history_ids(
        &self,
        scope: GuildDataScope,
        case: CaseId,
        after_sequence: i64,
        count: u32,
        limits: CaseHistoryLimits,
    ) -> Result<Vec<CaseHistoryRef>> {
        self.check_target(&scope.placement)?;
        limits.validate()?;
        limits.entry.page(count)?;
        anyhow::ensure!(after_sequence >= 0, CaseError::Invalid);
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| {
            let tx=conn.transaction_primary_read()?;
            verify_scope(&tx,&expected,&scope,true)?;
            if !live_parent(&tx,&scope,&case)? { tx.finish_read()?; return Ok(Vec::new()); }
            let mut stmt=tx.prepare("SELECT entry_id,sequence FROM guild_case_history WHERE guild_id=?1 AND enrollment_id=?2 AND case_id=?3 AND sequence>?4 ORDER BY sequence LIMIT ?5")?;
            let rows=stmt.query_map(params![scope.guild_id(),scope.enrollment.id.as_str(),case.as_str(),after_sequence,i64::from(count)],|r|Ok((r.get::<String>(0)?,r.get::<i64>(1)?)))?;
            let mut refs=Vec::new();
            for row in rows {let (id,sequence)=row?;refs.push(CaseHistoryRef{id:CaseId::try_from(id)?,sequence});}
            drop(stmt);
            tx.finish_read()?;
            Ok(refs)
        }).await
    }
}
