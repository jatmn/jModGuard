//! Inactive selected case storage. Never dispatches Discord actions or fetches evidence.
use super::tenant_logs::{valid_token, verify_scope};
use super::*;
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CaseId(String);
impl CaseId {
    pub fn new() -> Self {
        Self(format!("{:032x}", rand::random::<u128>()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl Default for CaseId {
    fn default() -> Self {
        Self::new()
    }
}
impl TryFrom<String> for CaseId {
    type Error = anyhow::Error;
    fn try_from(value: String) -> Result<Self> {
        anyhow::ensure!(valid_token(&value), CaseError::Invalid);
        Ok(Self(value))
    }
}
impl From<CaseId> for String {
    fn from(id: CaseId) -> Self {
        id.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CaseSource {
    Application {
        operation_id: CaseId,
    },
    GatewayObservation {
        observation_id: CaseId,
    },
    DiscordAudit {
        #[serde(
            serialize_with = "crate::ids::ser_i64_str",
            deserialize_with = "crate::ids::de_i64"
        )]
        entry_id: i64,
    },
}
impl CaseSource {
    pub(super) fn kind(&self) -> i64 {
        match self {
            Self::Application { .. } | Self::GatewayObservation { .. } => 0,
            Self::DiscordAudit { .. } => 1,
        }
    }

    pub(super) fn key(&self) -> Result<String> {
        match self {
            Self::Application { operation_id } => Ok(format!("app:{}", operation_id.as_str())),
            Self::GatewayObservation { observation_id } => {
                Ok(format!("app:{}", observation_id.as_str()))
            }
            Self::DiscordAudit { entry_id } => {
                anyhow::ensure!(*entry_id > 0, CaseError::Invalid);
                Ok(format!("discord:{entry_id}"))
            }
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseAction {
    Ban,
    Delete,
    Kick,
    Warning,
    Timeout,
    Unban,
    TimeoutRemoval,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseOutcome {
    Attempted,
    Succeeded,
    Failed,
    Uncertain,
}

/// Selected excerpt, not a chat archive. Guild/membership ownership comes from the case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextEvidence {
    #[serde(
        serialize_with = "crate::ids::ser_i64_str",
        deserialize_with = "crate::ids::de_i64"
    )]
    pub channel_id: i64,
    #[serde(
        serialize_with = "crate::ids::ser_i64_str",
        deserialize_with = "crate::ids::de_i64"
    )]
    pub message_id: i64,
    #[serde(
        serialize_with = "crate::ids::ser_i64_str",
        deserialize_with = "crate::ids::de_i64"
    )]
    pub author_id: i64,
    pub occurred_at_ms: i64,
    pub text: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CaseEvidence {
    Unavailable,
    Text(Vec<TextEvidence>),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseData {
    pub source: CaseSource,
    pub action: CaseAction,
    pub outcome: CaseOutcome,
    #[serde(
        serialize_with = "crate::ids::ser_i64_str",
        deserialize_with = "crate::ids::de_i64"
    )]
    pub subject_id: i64,
    #[serde(
        serialize_with = "crate::ids::ser_opt_i64_str",
        deserialize_with = "crate::ids::de_opt_i64"
    )]
    pub actor_id: Option<i64>,
    pub occurred_at_ms: i64,
    pub reason: Option<String>,
    pub evidence: CaseEvidence,
}

/// Trusted installation budgets, configurable by future operator settings. No age expiry.
#[derive(Debug, Clone, Copy)]
pub struct CaseLimits {
    pub max_record_bytes: u32,
    pub max_evidence_items: u32,
    pub max_page_size: u32,
}
impl Default for CaseLimits {
    fn default() -> Self {
        Self {
            max_record_bytes: 65536,
            max_evidence_items: 32,
            max_page_size: 100,
        }
    }
}
impl CaseLimits {
    pub(super) fn validate(self) -> Result<()> {
        anyhow::ensure!(
            self.max_record_bytes > 0 && self.max_evidence_items > 0 && self.max_page_size > 0,
            CaseError::Limit
        );
        Ok(())
    }
    pub(super) fn page(self, count: u32) -> Result<()> {
        self.validate()?;
        anyhow::ensure!(count > 0 && count <= self.max_page_size, CaseError::Limit);
        Ok(())
    }
}
#[derive(Debug, thiserror::Error)]
pub enum CaseError {
    #[error("invalid moderation case")]
    Invalid,
    #[error("moderation case exceeds configured budget")]
    Limit,
    #[error("moderation case identity conflicts")]
    Conflict,
    #[error("case cleanup requires a closed membership")]
    Active,
}
#[derive(Debug, PartialEq, Eq)]
pub enum RecordCase {
    Created(CaseId),
    Existing(CaseId),
    Removed(CaseId),
}

fn validate_data(data: &CaseData, limits: CaseLimits) -> Result<()> {
    limits.validate()?;
    data.source.key()?;
    anyhow::ensure!(
        data.subject_id > 0 && data.actor_id.is_none_or(|id| id > 0) && data.occurred_at_ms > 0,
        CaseError::Invalid
    );
    let mut text_bytes = data.reason.as_ref().map_or(0, |s| s.len());
    if let CaseEvidence::Text(items) = &data.evidence {
        anyhow::ensure!(
            !items.is_empty() && items.len() <= limits.max_evidence_items as usize,
            CaseError::Limit
        );
        for item in items {
            anyhow::ensure!(
                item.channel_id > 0
                    && item.message_id > 0
                    && item.author_id > 0
                    && item.occurred_at_ms > 0
                    && !item.text.is_empty(),
                CaseError::Invalid
            );
            text_bytes = text_bytes
                .checked_add(item.text.len())
                .ok_or(CaseError::Limit)?;
        }
    }
    anyhow::ensure!(
        text_bytes <= limits.max_record_bytes as usize,
        CaseError::Limit
    );
    Ok(())
}

impl GuildDataStore {
    /// Record observed facts under a stable source key. Retry with the same key and
    /// payload; conflicting facts fail. Removed keys never recreate their payload.
    pub async fn record_case(
        &self,
        scope: GuildDataScope,
        id: CaseId,
        data: CaseData,
        limits: CaseLimits,
    ) -> Result<RecordCase> {
        self.check_target(&scope.placement)?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        validate_data(&data, limits)?;
        let source = data.source.key()?;
        let payload = serde_json::to_string(&data)?;
        anyhow::ensure!(
            payload.len() <= limits.max_record_bytes as usize,
            CaseError::Limit
        );
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| {
            #[cfg(any(test, feature = "test-support"))]
            if stall_case_source(&source) {
                std::thread::sleep(std::time::Duration::from_millis(1_500));
            }
            let mut tx = conn.transaction_primary_read()?;
            verify_scope(&tx, &expected, &scope, true)?;
            let matched = super::case_sources::source_match(&tx, &scope, &source)?;
            // Fetch only matching identities and a SQL equality bit, never another payload.
            let mut stmt = tx.prepare("SELECT case_id,source_key,deleted,payload=?4 FROM guild_cases WHERE guild_id=?1 AND enrollment_id=?2 AND (case_id=?3 OR case_id=?5)")?;
            let rows = stmt.query_map(params![scope.guild_id(),scope.enrollment.id.as_str(),id.as_str(),payload.as_str(),matched.as_ref().map_or("", |m| m.id.as_str())], |r| Ok((r.get::<String>(0)?,r.get::<String>(1)?,r.get::<bool>(2)?,r.get::<Option<bool>>(3)?)))?;
            let mut existing = None;
            for row in rows {
                let (found, key, deleted, equal) = row?;
                anyhow::ensure!(existing.is_none() && matched.as_ref().is_some_and(|m| m.id.as_str()==found), CaseError::Conflict);
                let found = CaseId::try_from(found)?;
                existing = Some(if deleted { RecordCase::Removed(found) } else {
                    anyhow::ensure!(key==source && equal==Some(true), CaseError::Conflict);
                    RecordCase::Existing(found)
                });
            }
            drop(stmt);
            if let Some(existing) = existing { tx.finish_read()?; return Ok(existing); }
            tx.ensure_mutation_receipt()?;
            tx.execute_rows("INSERT INTO guild_cases(guild_id,enrollment_id,case_id,source_key,payload,deleted) VALUES(?1,?2,?3,?4,?5,0)", params![scope.guild_id(),scope.enrollment.id.as_str(),id.as_str(),source.as_str(),payload.as_str()], 1)?;
            tx.commit()?;
            Ok(RecordCase::Created(id))
        }).await
    }

    pub async fn case(
        &self,
        scope: GuildDataScope,
        id: CaseId,
        limits: CaseLimits,
    ) -> Result<Option<CaseData>> {
        self.check_target(&scope.placement)?;
        limits.validate()?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| {
            let tx = conn.transaction_primary_snapshot()?;
            verify_scope(&tx, &expected, &scope, true)?;
            let row = tx.query_row("SELECT CASE WHEN length(CAST(payload AS BLOB))<=?4 THEN payload ELSE NULL END FROM guild_cases WHERE guild_id=?1 AND enrollment_id=?2 AND case_id=?3 AND deleted=0", params![scope.guild_id(),scope.enrollment.id.as_str(),id.as_str(),limits.max_record_bytes as i64], |r| Ok(r.get::<Option<String>>(0)?)).optional()?;
            let result = match row {
                None => None,
                Some(None) => return Err(CaseError::Limit.into()),
                Some(Some(payload)) => { let data: CaseData = serde_json::from_str(&payload)?; validate_data(&data, limits)?; Some(data) }
            };
            tx.finish_read()?;
            Ok(result)
        }).await
    }

    /// Keyset page in opaque ID order, not chronology. Never loads evidence payloads.
    pub async fn case_ids(
        &self,
        scope: GuildDataScope,
        after: Option<CaseId>,
        count: u32,
        limits: CaseLimits,
    ) -> Result<Vec<CaseId>> {
        self.case_ids_by_removal(scope, after, count, limits, false)
            .await
    }

    /// Retained removal identities only, for scoped cleanup recovery. No payloads.
    pub async fn removed_case_ids(
        &self,
        scope: GuildDataScope,
        after: Option<CaseId>,
        count: u32,
        limits: CaseLimits,
    ) -> Result<Vec<CaseId>> {
        self.case_ids_by_removal(scope, after, count, limits, true)
            .await
    }

    async fn case_ids_by_removal(
        &self,
        scope: GuildDataScope,
        after: Option<CaseId>,
        count: u32,
        limits: CaseLimits,
        removed: bool,
    ) -> Result<Vec<CaseId>> {
        self.check_target(&scope.placement)?;
        limits.page(count)?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| {
            let tx = conn.transaction_primary_snapshot()?;
            verify_scope(&tx, &expected, &scope, true)?;
            let mut stmt = tx.prepare("SELECT case_id FROM guild_cases WHERE guild_id=?1 AND enrollment_id=?2 AND deleted=?5 AND case_id>?3 ORDER BY case_id LIMIT ?4")?;
            let rows = stmt.query_map(params![scope.guild_id(),scope.enrollment.id.as_str(),after.as_ref().map_or("",CaseId::as_str),count as i64,removed], |r| Ok(r.get::<String>(0)?))?;
            let mut ids = Vec::new();
            for row in rows { ids.push(CaseId::try_from(row?)?); }
            drop(stmt);
            tx.finish_read()?;
            Ok(ids)
        }).await
    }

    pub async fn case_count(&self, scope: GuildDataScope, limits: CaseLimits) -> Result<i64> {
        self.check_target(&scope.placement)?;
        limits.validate()?;
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let tx = conn.transaction_primary_snapshot()?;
                verify_scope(&tx, &expected, &scope, true)?;
                let count: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM guild_cases WHERE guild_id=?1 AND enrollment_id=?2 AND deleted=0",
                    params![scope.guild_id(), scope.enrollment.id.as_str()],
                    |r| Ok(r.get(0)?),
                )?;
                tx.finish_read()?;
                Ok(count)
            })
            .await
    }

    /// Remove retained text/reason/actor/subject/outcome atomically. Keep only replay
    /// identities until closed-membership cleanup. No Discord side effect is performed.
    pub async fn remove_case(&self, scope: GuildDataScope, id: CaseId) -> Result<bool> {
        self.check_target(&scope.placement)?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expected = self.expected.clone();
        let core = self.core.clone();
        self.db.with_conn(move |conn| {
            let mut tx = conn.transaction_primary_read()?;
            if let Err(error) = Self::confirm_captured_enrollment(&core, &scope) {
                tx.finish_read()?;
                return Err(error);
            }
            verify_scope(&tx, &expected, &scope, true)?;
            let present = tx.query_row("SELECT deleted=0 FROM guild_cases WHERE guild_id=?1 AND enrollment_id=?2 AND case_id=?3", params![scope.guild_id(),scope.enrollment.id.as_str(),id.as_str()], |r| Ok(r.get::<bool>(0)?)).optional()?.unwrap_or(false);
            if !present { tx.finish_read()?; return Ok(false); }
            tx.ensure_mutation_receipt()?;
            tx.execute_rows("UPDATE guild_cases SET deleted=1,payload=NULL WHERE guild_id=?1 AND enrollment_id=?2 AND case_id=?3", params![scope.guild_id(),scope.enrollment.id.as_str(),id.as_str()], 1)?;
            tx.commit()?;
            Ok(true)
        }).await
    }

    /// Bounded payload/replay-key cleanup only after the exact membership is closed.
    /// Directory erasure is coordinated separately by tenant runtime.
    pub async fn purge_closed_cases(
        &self,
        scope: GuildDataScope,
        count: u32,
        limits: CaseLimits,
    ) -> Result<usize> {
        self.check_target(&scope.placement)?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        limits.page(count)?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| {
            let mut tx = conn.transaction_primary_read()?;
            let (active,_) = verify_scope(&tx, &expected, &scope, false)?;
            anyhow::ensure!(!active, CaseError::Active);
            let any: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM guild_cases WHERE guild_id=?1 AND enrollment_id=?2)", params![scope.guild_id(),scope.enrollment.id.as_str()], |r| Ok(r.get(0)?))?;
            if !any { tx.finish_read()?; return Ok(0); }
            tx.prepare_reclaim()?;
            let removed = tx.execute_reclaim("DELETE FROM guild_cases WHERE guild_id=?1 AND enrollment_id=?2 AND case_id IN (SELECT case_id FROM guild_cases WHERE guild_id=?1 AND enrollment_id=?2 ORDER BY case_id LIMIT ?3)", params![scope.guild_id(),scope.enrollment.id.as_str(),count as i64])?;
            tx.commit()?;
            Ok(removed)
        }).await
    }

    /// Move one Attempted case to a final outcome. An already-final case is left unchanged.
    pub async fn settle_case_outcome(
        &self,
        scope: GuildDataScope,
        id: CaseId,
        outcome: CaseOutcome,
        limits: CaseLimits,
    ) -> Result<Option<CaseData>> {
        self.check_target(&scope.placement)?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let outcome_sql = outcome_sql(&outcome)?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| {
            let mut tx = conn.transaction_primary_read()?;
            verify_scope(&tx, &expected, &scope, true)?;
            let payload: Option<String> = tx.query_row(
                "SELECT payload FROM guild_cases WHERE guild_id=?1 AND enrollment_id=?2 AND case_id=?3 AND deleted=0",
                params![scope.guild_id(), scope.enrollment.id.as_str(), id.as_str()],
                |row| Ok(row.get(0)?),
            ).optional()?;
            let Some(payload) = payload else {
                tx.finish_read()?;
                return Ok(None);
            };
            let current: CaseData = serde_json::from_str(&payload)?;
            if current.outcome != CaseOutcome::Attempted {
                tx.finish_read()?;
                return Ok(None);
            }
            tx.ensure_mutation_receipt()?;
            tx.execute_rows(
                "UPDATE guild_cases SET payload=json_set(payload, '$.outcome', ?4)
                 WHERE guild_id=?1 AND enrollment_id=?2 AND case_id=?3 AND deleted=0
                   AND json_extract(payload, '$.outcome')='attempted'",
                params![scope.guild_id(), scope.enrollment.id.as_str(), id.as_str(), outcome_sql],
                1,
            )?;
            let updated: Option<String> = tx.query_row(
                "SELECT CASE WHEN length(CAST(payload AS BLOB))<=?4 THEN payload ELSE NULL END
                 FROM guild_cases WHERE guild_id=?1 AND enrollment_id=?2 AND case_id=?3 AND deleted=0",
                params![scope.guild_id(), scope.enrollment.id.as_str(), id.as_str(), limits.max_record_bytes as i64],
                |row| Ok(row.get(0)?),
            ).optional()?;
            let Some(updated) = updated else {
                tx.finish_read()?;
                return Err(CaseError::Limit.into());
            };
            let data: CaseData = serde_json::from_str(&updated)?;
            validate_data(&data, limits)?;
            anyhow::ensure!(data.outcome == outcome, CaseError::Conflict);
            tx.commit()?;
            Ok(Some(data))
        }).await
    }

    /// Settle the newest Attempted application ban whose stored reason matches the
    /// reason Discord kept. Older attempts and foreign bans stay attempted.
    pub async fn settle_matching_application_ban(
        &self,
        scope: GuildDataScope,
        subject_id: i64,
        discord_reason: &str,
        limits: CaseLimits,
    ) -> Result<Vec<(CaseId, CaseData)>> {
        self.check_target(&scope.placement)?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        anyhow::ensure!(subject_id > 0, CaseError::Invalid);
        let wanted = discord_audit_reason(discord_reason);
        if wanted.is_empty() {
            return Ok(Vec::new());
        }
        let expected = self.expected.clone();
        let chosen = self
            .db
            .with_conn({
                let scope = scope.clone();
                let expected = expected.clone();
                move |conn| {
                    let tx = conn.transaction_primary_snapshot()?;
                    verify_scope(&tx, &expected, &scope, true)?;
                    let mut stmt = tx.prepare(
                        "SELECT case_id, json_extract(payload, '$.reason') FROM guild_cases
                     WHERE guild_id=?1 AND enrollment_id=?2 AND deleted=0 AND json_valid(payload)
                       AND json_extract(payload, '$.action')='ban'
                       AND json_extract(payload, '$.outcome')='attempted'
                       AND CAST(json_extract(payload, '$.subject_id') AS INTEGER)=?3
                       AND json_type(payload, '$.source.application') IS NOT NULL
                     ORDER BY CAST(json_extract(payload, '$.occurred_at_ms') AS INTEGER) DESC, case_id DESC",
                    )?;
                    let rows = stmt.query_map(
                        params![scope.guild_id(), scope.enrollment.id.as_str(), subject_id],
                        |row| Ok((row.get::<String>(0)?, row.get::<Option<String>>(1)?)),
                    )?;
                    let mut chosen = None;
                    for row in rows {
                        let (id, reason) = row?;
                        let Some(reason) = reason else {
                            continue;
                        };
                        if discord_audit_reason(&reason) == wanted {
                            chosen = Some(CaseId::try_from(id)?);
                            break;
                        }
                    }
                    drop(stmt);
                    tx.finish_read()?;
                    Ok(chosen)
                }
            })
            .await?;
        let Some(id) = chosen else {
            return Ok(Vec::new());
        };
        let mut settled = Vec::new();
        if let Some(data) = self
            .settle_case_outcome(scope, id.clone(), CaseOutcome::Succeeded, limits)
            .await?
        {
            settled.push((id, data));
        }
        Ok(settled)
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) static STALL_CASE_SOURCE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Hold one source-key insert inside the blocking database call.
/// Bot tests use this to prove the ban deadline does not wait for the insert.
#[cfg(any(test, feature = "test-support"))]
pub fn set_case_record_stall(source: Option<String>) {
    if let Ok(mut guard) = STALL_CASE_SOURCE.lock() {
        *guard = source;
    }
}

#[cfg(any(test, feature = "test-support"))]
fn stall_case_source(source: &str) -> bool {
    STALL_CASE_SOURCE
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
        .is_some_and(|stalled| stalled == source)
}

/// Discord stores at most 512 characters of an audit reason.
fn discord_audit_reason(reason: &str) -> String {
    reason.chars().take(512).collect()
}

fn outcome_sql(outcome: &CaseOutcome) -> Result<&'static str> {
    match outcome {
        CaseOutcome::Succeeded => Ok("succeeded"),
        CaseOutcome::Failed => Ok("failed"),
        CaseOutcome::Uncertain => Ok("uncertain"),
        CaseOutcome::Attempted => Err(CaseError::Invalid.into()),
    }
}
