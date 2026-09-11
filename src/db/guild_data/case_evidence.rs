//! Inactive ownership journal. No object I/O, capture lease or deletion proof is provided.
use super::tenant_logs::verify_scope;
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceOrigin {
    pub channel_id: i64,
    pub message_id: i64,
    pub attachment_id: i64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceRequest {
    /// Trusted nonsecret installation resource alias, never a URL or credential.
    pub storage_id: CaseId,
    pub bytes: u64,
    pub origin: EvidenceOrigin,
}
#[derive(Debug, Clone, Copy)]
pub struct EvidenceLimits {
    pub max_items_per_case: u32,
    pub max_file_bytes: u64,
    pub max_case_bytes: u64,
    pub max_page_size: u32,
}
impl EvidenceLimits {
    fn validate(self) -> Result<()> {
        anyhow::ensure!(
            self.max_items_per_case > 0
                && self.max_file_bytes > 0
                && self.max_file_bytes <= i64::MAX as u64
                && self.max_case_bytes > 0
                && self.max_case_bytes <= i64::MAX as u64
                && self.max_page_size > 0,
            CaseError::Limit
        );
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceState {
    Capturing,
    Available,
    CleanupPending,
    Erased,
}
impl EvidenceState {
    fn from_sql(value: i64) -> Result<Self> {
        match value {
            0 => Ok(Self::Capturing),
            1 => Ok(Self::Available),
            2 => Ok(Self::CleanupPending),
            3 => Ok(Self::Erased),
            _ => Err(CaseError::Invalid.into()),
        }
    }
}
/// Internal lifecycle snapshot, not permission to upload/read/delete an object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceJob {
    pub case_id: CaseId,
    pub id: CaseId,
    pub storage_id: CaseId,
    pub object_key: String,
    pub reserved_bytes: u64,
    pub state: EvidenceState,
    pub cancelled: bool,
}
#[derive(Debug, PartialEq, Eq)]
pub struct ReserveEvidence {
    pub created: bool,
    pub job: EvidenceJob,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableEvidence {
    pub job: EvidenceJob,
    pub origin: EvidenceOrigin,
    pub sha256: String,
}
#[derive(Debug, Clone)]
pub enum EvidenceCapture {
    /// Caller has verified the complete stored bytes and stopped all writes to this key.
    Stored { bytes: u64, sha256: String },
    /// All possible writes have settled. Cleanup is required even after failed upload.
    Discarded,
}
#[derive(Debug, Clone, Copy)]
pub enum EvidenceWork {
    Capture,
    Delete,
}

fn job(row: &Row, scope: &GuildDataScope) -> Result<EvidenceJob> {
    let case_id = CaseId::try_from(row.get::<String>(0)?)?;
    let id = CaseId::try_from(row.get::<String>(1)?)?;
    Ok(EvidenceJob {
        object_key: format!(
            "evidence/{}/{}/{}/{}",
            scope.guild_id(),
            scope.enrollment.id,
            case_id.as_str(),
            id.as_str()
        ),
        case_id,
        id,
        storage_id: CaseId::try_from(row.get::<String>(2)?)?,
        reserved_bytes: u64::try_from(row.get::<i64>(3)?)?,
        state: EvidenceState::from_sql(row.get(4)?)?,
        cancelled: row.get(5)?,
    })
}
struct Record {
    job: EvidenceJob,
    origin: Option<EvidenceOrigin>,
    sha256: Option<String>,
}
fn record(conn: &impl SqlExecutor, scope: &GuildDataScope, id: &CaseId) -> Result<Option<Record>> {
    conn.query_row("SELECT case_id,evidence_id,storage_id,reserved_bytes,state,cancelled,channel_id,message_id,attachment_id,sha256 FROM guild_case_evidence WHERE guild_id=?1 AND enrollment_id=?2 AND evidence_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),id.as_str()],|r| {
        let origin = match r.get::<Option<i64>>(6)? {
            Some(channel_id)=>Some(EvidenceOrigin{channel_id,message_id:r.get(7)?,attachment_id:r.get(8)?}),None=>None
        };
        Ok(Record{job:job(r,scope)?,origin,sha256:r.get(9)?})
    }).optional()
}
fn live(conn: &impl SqlExecutor, scope: &GuildDataScope, case: &CaseId) -> Result<bool> {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM guild_cases WHERE guild_id=?1 AND enrollment_id=?2 AND case_id=?3 AND deleted=0)",params![scope.guild_id(),scope.enrollment.id.as_str(),case.as_str()],|r|Ok(r.get(0)?))
}

impl GuildDataStore {
    /// Reserve before any external write. Existing reservations are snapshots for
    /// reconciliation, never automatic permission to restart an interrupted upload.
    pub async fn reserve_case_evidence(
        &self,
        scope: GuildDataScope,
        case: CaseId,
        id: CaseId,
        request: EvidenceRequest,
        limits: EvidenceLimits,
    ) -> Result<ReserveEvidence> {
        self.check_target(&scope.placement)?;
        limits.validate()?;
        anyhow::ensure!(
            request.bytes > 0 && request.bytes <= limits.max_file_bytes,
            CaseError::Limit
        );
        anyhow::ensure!(
            request.origin.channel_id > 0
                && request.origin.message_id > 0
                && request.origin.attachment_id > 0,
            CaseError::Invalid
        );
        let expected = self.expected.clone();
        self.db.with_conn(move|conn|{
            let mut tx=conn.transaction_primary_read()?;
            verify_scope(&tx,&expected,&scope,true)?;
            anyhow::ensure!(live(&tx,&scope,&case)?,CaseError::Conflict);
            if let Some(old)=record(&tx,&scope,&id)? {
                anyhow::ensure!(old.job.case_id==case && old.job.storage_id==request.storage_id && old.job.reserved_bytes==request.bytes && (old.job.cancelled || old.origin.as_ref()==Some(&request.origin)),CaseError::Conflict);
                tx.finish_read()?;return Ok(ReserveEvidence{created:false,job:old.job});
            }
            let (count,bytes)=tx.query_row("SELECT COUNT(*),COALESCE(SUM(reserved_bytes),0) FROM guild_case_evidence WHERE guild_id=?1 AND enrollment_id=?2 AND case_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),case.as_str()],|r|Ok((r.get::<i64>(0)?,r.get::<i64>(1)?)))?;
            anyhow::ensure!(count<i64::from(limits.max_items_per_case) && bytes>=0 && (bytes as u64).checked_add(request.bytes).is_some_and(|sum|sum<=limits.max_case_bytes),CaseError::Limit);
            tx.ensure_mutation_receipt()?;
            tx.execute("INSERT INTO guild_case_evidence(guild_id,enrollment_id,case_id,evidence_id,storage_id,reserved_bytes,state,cancelled,channel_id,message_id,attachment_id) VALUES(?1,?2,?3,?4,?5,?6,0,0,?7,?8,?9)",params![scope.guild_id(),scope.enrollment.id.as_str(),case.as_str(),id.as_str(),request.storage_id.as_str(),request.bytes as i64,request.origin.channel_id,request.origin.message_id,request.origin.attachment_id])?;
            let result=record(&tx,&scope,&id)?.ok_or(CaseError::Conflict)?.job;
            tx.commit()?;Ok(ReserveEvidence{created:true,job:result})
        }).await
    }

    /// A trusted uploader/reconciler must ensure no writes remain in flight before
    /// calling this, including Discarded. Timeouts alone cannot establish settlement.
    pub async fn settle_evidence_capture(
        &self,
        scope: GuildDataScope,
        case: CaseId,
        id: CaseId,
        result: EvidenceCapture,
    ) -> Result<EvidenceState> {
        self.check_target(&scope.placement)?;
        if let EvidenceCapture::Stored { bytes, sha256 } = &result {
            anyhow::ensure!(
                *bytes > 0
                    && sha256.len() == 64
                    && sha256
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                CaseError::Invalid
            );
        }
        let expected = self.expected.clone();
        self.db.with_conn(move|conn|{
            let mut tx=conn.transaction_primary_read()?;
            let (active,_)=verify_scope(&tx,&expected,&scope,false)?;
            let old=record(&tx,&scope,&id)?.ok_or(CaseError::Conflict)?;
            anyhow::ensure!(old.job.case_id==case,CaseError::Conflict);
            if let EvidenceCapture::Stored{bytes,..}=&result {anyhow::ensure!(*bytes==old.job.reserved_bytes,CaseError::Conflict);}
            if old.job.state!=EvidenceState::Capturing {
                if old.job.state==EvidenceState::Available {
                    anyhow::ensure!(matches!(&result,EvidenceCapture::Stored{sha256,..} if Some(sha256)==old.sha256.as_ref()),CaseError::Conflict);
                }
                tx.finish_read()?;return Ok(old.job.state);
            }
            let digest=match result {EvidenceCapture::Stored{sha256,..} if active && !old.job.cancelled && live(&tx,&scope,&case)?=>Some(sha256),_=>None};
            tx.ensure_mutation_receipt()?;
            if let Some(digest)=digest {
                tx.execute("UPDATE guild_case_evidence SET state=1,sha256=?4 WHERE guild_id=?1 AND enrollment_id=?2 AND evidence_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),id.as_str(),digest.as_str()])?;
                tx.commit()?;Ok(EvidenceState::Available)
            } else {
                tx.execute("UPDATE guild_case_evidence SET state=2,cancelled=1,channel_id=NULL,message_id=NULL,attachment_id=NULL,sha256=NULL WHERE guild_id=?1 AND enrollment_id=?2 AND evidence_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),id.as_str()])?;
                tx.commit()?;Ok(EvidenceState::CleanupPending)
            }
        }).await
    }

    /// Acknowledge verified object absence after all writes have settled. This only
    /// journals a trusted worker's result; it does not perform or prove external deletion.
    pub async fn confirm_evidence_deleted(
        &self,
        scope: GuildDataScope,
        case: CaseId,
        id: CaseId,
    ) -> Result<bool> {
        self.check_target(&scope.placement)?;
        let expected = self.expected.clone();
        self.db.with_conn(move|conn|{
            let mut tx=conn.transaction_primary_read()?;verify_scope(&tx,&expected,&scope,false)?;
            let old=record(&tx,&scope,&id)?.ok_or(CaseError::Conflict)?;
            anyhow::ensure!(old.job.case_id==case,CaseError::Conflict);
            if old.job.state==EvidenceState::Erased {tx.finish_read()?;return Ok(false);}
            anyhow::ensure!(old.job.state==EvidenceState::CleanupPending,CaseError::Conflict);
            tx.ensure_mutation_receipt()?;
            tx.execute("UPDATE guild_case_evidence SET state=3 WHERE guild_id=?1 AND enrollment_id=?2 AND evidence_id=?3",params![scope.guild_id(),scope.enrollment.id.as_str(),id.as_str()])?;
            tx.commit()?;Ok(true)
        }).await
    }

    pub async fn available_case_evidence(
        &self,
        scope: GuildDataScope,
        case: CaseId,
        id: CaseId,
    ) -> Result<Option<AvailableEvidence>> {
        self.check_target(&scope.placement)?;
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let tx = conn.transaction_primary_read()?;
                verify_scope(&tx, &expected, &scope, true)?;
                if !live(&tx, &scope, &case)? {
                    tx.finish_read()?;
                    return Ok(None);
                }
                let result = match record(&tx, &scope, &id)? {
                    Some(r) if r.job.case_id == case && r.job.state == EvidenceState::Available => {
                        Some(AvailableEvidence {
                            job: r.job,
                            origin: r.origin.ok_or(CaseError::Invalid)?,
                            sha256: r.sha256.ok_or(CaseError::Invalid)?,
                        })
                    }
                    _ => None,
                };
                tx.finish_read()?;
                Ok(result)
            })
            .await
    }

    /// Prepare closed-membership object cleanup in bounded batches. Closure itself
    /// denies content access immediately without scanning all retained evidence.
    pub async fn cancel_closed_evidence(
        &self,
        scope: GuildDataScope,
        count: u32,
        limits: EvidenceLimits,
    ) -> Result<usize> {
        self.check_target(&scope.placement)?;
        limits.validate()?;
        anyhow::ensure!(count > 0 && count <= limits.max_page_size, CaseError::Limit);
        let expected = self.expected.clone();
        self.db.with_conn(move|conn|{
            let mut tx=conn.transaction_primary_read()?;
            let (active,_)=verify_scope(&tx,&expected,&scope,false)?;
            anyhow::ensure!(!active,CaseError::Active);
            let any:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM guild_case_evidence WHERE guild_id=?1 AND enrollment_id=?2 AND cancelled=0)",params![scope.guild_id(),scope.enrollment.id.as_str()],|r|Ok(r.get(0)?))?;
            if !any {tx.finish_read()?;return Ok(0);}
            tx.ensure_mutation_receipt()?;
            let count=tx.execute("UPDATE guild_case_evidence SET cancelled=1,state=CASE state WHEN 0 THEN 0 ELSE 2 END,channel_id=NULL,message_id=NULL,attachment_id=NULL,sha256=NULL WHERE guild_id=?1 AND enrollment_id=?2 AND evidence_id IN (SELECT evidence_id FROM guild_case_evidence WHERE guild_id=?1 AND enrollment_id=?2 AND cancelled=0 ORDER BY evidence_id LIMIT ?3)",params![scope.guild_id(),scope.enrollment.id.as_str(),i64::from(count)])?;
            tx.commit()?;Ok(count)
        }).await
    }

    /// Internal reconciliation/cleanup page, including closed memberships. A snapshot
    /// is not a worker claim or permission to repeat a possibly in-flight upload.
    pub async fn evidence_jobs(
        &self,
        scope: GuildDataScope,
        work: EvidenceWork,
        after: Option<CaseId>,
        count: u32,
        limits: EvidenceLimits,
    ) -> Result<Vec<EvidenceJob>> {
        self.check_target(&scope.placement)?;
        limits.validate()?;
        anyhow::ensure!(count > 0 && count <= limits.max_page_size, CaseError::Limit);
        let state = match work {
            EvidenceWork::Capture => 0,
            EvidenceWork::Delete => 2,
        };
        let expected = self.expected.clone();
        self.db.with_conn(move|conn|{
            let tx=conn.transaction_primary_read()?;let (active,_)=verify_scope(&tx,&expected,&scope,false)?;
            let mut stmt=tx.prepare("SELECT case_id,evidence_id,storage_id,reserved_bytes,state,cancelled FROM guild_case_evidence WHERE guild_id=?1 AND enrollment_id=?2 AND state=?3 AND evidence_id>?4 ORDER BY evidence_id LIMIT ?5")?;
            let rows=stmt.query_map(params![scope.guild_id(),scope.enrollment.id.as_str(),state,after.as_ref().map_or("",CaseId::as_str),i64::from(count)],|r|{let mut job=job(r,&scope)?;job.cancelled|=!active;Ok(job)})?;
            let jobs=rows.collect::<Result<Vec<_>>>()?;drop(stmt);tx.finish_read()?;Ok(jobs)
        }).await
    }
}
