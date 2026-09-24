//! Bounded, latest-observed message state. This is transient logging data, not case evidence.
use super::tenant_logs::verify_scope;
use super::*;
use crate::db_compat::Transaction;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

const LIFETIME_MS: i64 = 86_400_000;
const MAX_TEXT: usize = 32 * 1024;
const MAX_ATTACHMENTS: usize = 16 * 1024;
const MAX_ATTACHMENT_INPUT: usize = 128;
const MAX_PAYLOAD: usize = 64 * 1024;
const MAX_BULK: usize = 100;
const ROW_RESERVE: u64 = 4096;

/// Maximum encoded row plus possible B-tree splits in the table, its two
/// indexes (primary-key autoindex and expiry), and mutation receipts. Reserve
/// 64 pages per tree using the actual page size. This is intentionally larger
/// than ordinary writes; commit admission remains the final allocated-byte check.
fn growth_reserve(tx: &impl SqlExecutor) -> Result<u64> {
    let page_size = tx.capacity_page_size()?;
    (page_size)
        .checked_mul(64 * 4)
        .and_then(|n| n.checked_add(MAX_PAYLOAD as u64))
        .ok_or_else(|| SnapshotError::Invalid.into())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotAttachment {
    pub id: i64,
    pub filename: String,
    pub size: u64,
    pub content_type: Option<String>,
    pub url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotFields {
    /// `None` means not supplied; `Some("")` is a known empty message.
    pub content: Option<String>,
    pub content_truncated: bool,
    pub content_original_bytes: Option<i64>,
    /// `None` means not supplied; `Some(vec![])` is a known empty list.
    pub attachments: Option<Vec<SnapshotAttachment>>,
    pub attachments_truncated: bool,
    pub attachments_omitted: Option<i64>,
    pub author_id: Option<i64>,
    pub author_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SnapshotObservation {
    pub channel_id: i64,
    pub message_id: i64,
    pub created_at_ms: i64,
    pub observed_at_ms: i64,
    pub edit_at_ms: Option<i64>,
    pub coverage_epoch: u64,
    pub fields: SnapshotFields,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageSnapshot {
    pub guild_id: i64,
    pub channel_id: i64,
    pub message_id: i64,
    pub fields: SnapshotFields,
    pub created_at_ms: i64,
    pub first_observed_at_ms: i64,
    pub last_observed_at_ms: i64,
    pub edit_at_ms: Option<i64>,
    pub expires_at_ms: i64,
    pub generation: i64,
    pub coverage_epoch: u64,
    /// True only while the accepted observation sequence is known continuous.
    pub continuous: bool,
    /// True when content was supplied at `edit_at_ms`, not inherited.
    pub content_supplied: bool,
    /// True when attachments were supplied at `edit_at_ms`, not inherited.
    pub attachments_supplied: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotOutcome {
    Stored,
    Duplicate,
    Stale,
    Expired,
    Missing,
    CapacityRefused,
}

#[derive(Debug, Clone)]
pub struct SnapshotTransition {
    pub outcome: SnapshotOutcome,
    pub before: Option<MessageSnapshot>,
    pub after: Option<MessageSnapshot>,
}

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("invalid snapshot observation")]
    Invalid,
    #[error("snapshot batch exceeds 100 IDs")]
    BatchLimit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotHealthState {
    Active,
    Paused,
    Unavailable,
    Disabled,
}
impl SnapshotHealthState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Unavailable => "unavailable",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotHealthUpdate {
    pub epoch: u64,
    pub state: SnapshotHealthState,
    /// Stable non-sensitive code only; never include a SQL error or message text.
    pub reason: Option<&'static str>,
    pub observed_at_ms: Option<i64>,
    pub lost_delta: u64,
    pub now_ms: i64,
}

fn truncate_utf8(value: &mut String, limit: usize) -> bool {
    if value.len() <= limit {
        return false;
    }
    let mut cut = limit;
    while !value.is_char_boundary(cut) {
        cut -= 1;
    }
    value.truncate(cut);
    true
}

fn bound_fields(fields: &mut SnapshotFields) -> Result<usize> {
    if let Some(content) = &mut fields.content {
        let original = content.len();
        if truncate_utf8(content, MAX_TEXT) {
            fields.content_truncated = true;
            fields.content_original_bytes = Some(original as i64);
        }
    }
    if let Some(name) = &mut fields.author_name {
        truncate_utf8(name, 256);
    }
    if let Some(attachments) = &mut fields.attachments {
        let original = attachments.len();
        if attachments.len() > MAX_ATTACHMENT_INPUT {
            attachments.truncate(MAX_ATTACHMENT_INPUT);
        }
        let mut clipped_metadata = false;
        for attachment in attachments.iter_mut() {
            clipped_metadata |= truncate_utf8(&mut attachment.filename, 256);
            clipped_metadata |= truncate_utf8(&mut attachment.url, 1024);
            if let Some(content_type) = &mut attachment.content_type {
                clipped_metadata |= truncate_utf8(content_type, 128);
            }
        }
        while serde_json::to_vec(attachments)?.len() > MAX_ATTACHMENTS {
            attachments.pop();
        }
        let dropped = original - attachments.len();
        fields.attachments_truncated |= clipped_metadata || dropped > 0;
        if fields.attachments_truncated {
            fields.attachments_omitted = Some(
                fields
                    .attachments_omitted
                    .unwrap_or(0)
                    .saturating_add(i64::try_from(dropped).unwrap_or(i64::MAX)),
            );
        }
    }
    let attachments_bytes = fields
        .attachments
        .as_ref()
        .map(serde_json::to_vec)
        .transpose()?
        .map_or(0, |value| value.len());
    let content_bytes = fields.content.as_ref().map_or(0, String::len);
    let author_bytes = fields.author_name.as_ref().map_or(0, String::len);
    let total = content_bytes + attachments_bytes + author_bytes + 256;
    anyhow::ensure!(total <= MAX_PAYLOAD, SnapshotError::Invalid);
    Ok(total)
}

fn validate(observation: &SnapshotObservation) -> Result<i64> {
    anyhow::ensure!(
        observation.channel_id > 0
            && observation.message_id > 0
            && observation.created_at_ms > 0
            && observation.observed_at_ms > 0
            && observation.fields.author_id.is_none_or(|id| id > 0)
            && observation.coverage_epoch <= i64::MAX as u64,
        SnapshotError::Invalid
    );
    let expires = observation
        .created_at_ms
        .checked_add(LIFETIME_MS)
        .ok_or(SnapshotError::Invalid)?;
    Ok(expires)
}

fn write_time_ms() -> Result<i64> {
    let elapsed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
    Ok(i64::try_from(elapsed.as_millis())?)
}

const SELECT: &str = "SELECT author_id,author_name,content,content_truncated,attachments,attachments_truncated,created_at_ms,first_observed_at_ms,last_observed_at_ms,edit_at_ms,expires_at_ms,generation,coverage_epoch,continuous,content_original_bytes,attachments_omitted,content_supplied,attachments_supplied FROM guild_message_snapshots WHERE guild_id=?1 AND enrollment_id=?2 AND channel_id=?3 AND message_id=?4";

fn decode(row: &Row, guild_id: i64, channel_id: i64, message_id: i64) -> Result<MessageSnapshot> {
    let attachments: Option<String> = row.get(4)?;
    let epoch: i64 = row.get(12)?;
    anyhow::ensure!(epoch >= 0, SnapshotError::Invalid);
    Ok(MessageSnapshot {
        guild_id,
        channel_id,
        message_id,
        fields: SnapshotFields {
            author_id: row.get(0)?,
            author_name: row.get(1)?,
            content: row.get(2)?,
            content_truncated: row.get(3)?,
            attachments: attachments
                .map(|json| serde_json::from_str(&json))
                .transpose()?,
            attachments_truncated: row.get(5)?,
            content_original_bytes: row.get(14)?,
            attachments_omitted: row.get(15)?,
        },
        created_at_ms: row.get(6)?,
        first_observed_at_ms: row.get(7)?,
        last_observed_at_ms: row.get(8)?,
        edit_at_ms: row.get(9)?,
        expires_at_ms: row.get(10)?,
        generation: row.get(11)?,
        coverage_epoch: epoch as u64,
        continuous: row.get(13)?,
        content_supplied: row.get::<i64>(16)? != 0,
        attachments_supplied: row.get::<i64>(17)? != 0,
    })
}

fn load(
    conn: &impl SqlExecutor,
    scope: &GuildDataScope,
    channel: i64,
    message: i64,
) -> Result<Option<MessageSnapshot>> {
    conn.query_row(
        SELECT,
        params![
            scope.guild_id(),
            scope.enrollment.id.as_str(),
            channel,
            message
        ],
        |row| decode(row, scope.guild_id(), channel, message),
    )
    .optional()
}

fn merged(
    old: &SnapshotFields,
    incoming: SnapshotFields,
    mut content_supplied: bool,
    mut attachments_supplied: bool,
    equal_version: bool,
) -> (SnapshotFields, bool, bool) {
    let mut fields = old.clone();
    if !equal_version {
        content_supplied = false;
        attachments_supplied = false;
    }
    if incoming.content.is_some() && !content_supplied {
        fields.content = incoming.content;
        fields.content_truncated = incoming.content_truncated;
        fields.content_original_bytes = incoming.content_original_bytes;
        content_supplied = true;
    }
    if incoming.attachments.is_some() && !attachments_supplied {
        fields.attachments = incoming.attachments;
        fields.attachments_truncated = incoming.attachments_truncated;
        fields.attachments_omitted = incoming.attachments_omitted;
        attachments_supplied = true;
    }
    if incoming.author_id.is_some() && fields.author_id.is_none() {
        fields.author_id = incoming.author_id;
    }
    if incoming.author_name.is_some() && fields.author_name.is_none() {
        fields.author_name = incoming.author_name;
    }
    (fields, content_supplied, attachments_supplied)
}

fn insert(
    tx: &impl SqlExecutor,
    scope: &GuildDataScope,
    row: &MessageSnapshot,
    bytes: usize,
) -> Result<()> {
    let attachments = row
        .fields
        .attachments
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    let reserve = growth_reserve(tx)?;
    tx.execute_bounded("INSERT INTO guild_message_snapshots(guild_id,enrollment_id,channel_id,message_id,author_id,author_name,content,content_truncated,content_original_bytes,attachments,attachments_truncated,attachments_omitted,content_supplied,attachments_supplied,created_at_ms,first_observed_at_ms,last_observed_at_ms,edit_at_ms,expires_at_ms,generation,coverage_epoch,continuous,encoded_bytes,reservation_bytes) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24)", params![scope.guild_id(),scope.enrollment.id.as_str(),row.channel_id,row.message_id,row.fields.author_id,row.fields.author_name.as_deref(),row.fields.content.as_deref(),row.fields.content_truncated,row.fields.content_original_bytes,attachments.as_deref(),row.fields.attachments_truncated,row.fields.attachments_omitted,row.content_supplied,row.attachments_supplied,row.created_at_ms,row.first_observed_at_ms,row.last_observed_at_ms,row.edit_at_ms,row.expires_at_ms,row.generation,row.coverage_epoch as i64,row.continuous,bytes as i64,reserve as i64], reserve)?;
    Ok(())
}

fn update(
    tx: &impl SqlExecutor,
    scope: &GuildDataScope,
    row: &MessageSnapshot,
    bytes: usize,
) -> Result<()> {
    let attachments = row
        .fields
        .attachments
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    let reserve = growth_reserve(tx)?;
    tx.execute_bounded("UPDATE guild_message_snapshots SET author_id=?1,author_name=?2,content=?3,content_truncated=?4,content_original_bytes=?5,attachments=?6,attachments_truncated=?7,attachments_omitted=?8,content_supplied=?9,attachments_supplied=?10,last_observed_at_ms=?11,edit_at_ms=?12,generation=?13,coverage_epoch=?14,continuous=?15,encoded_bytes=?16,reservation_bytes=?17 WHERE guild_id=?18 AND enrollment_id=?19 AND channel_id=?20 AND message_id=?21", params![row.fields.author_id,row.fields.author_name.as_deref(),row.fields.content.as_deref(),row.fields.content_truncated,row.fields.content_original_bytes,attachments.as_deref(),row.fields.attachments_truncated,row.fields.attachments_omitted,row.content_supplied,row.attachments_supplied,row.last_observed_at_ms,row.edit_at_ms,row.generation,row.coverage_epoch as i64,row.continuous,bytes as i64,reserve as i64,scope.guild_id(),scope.enrollment.id.as_str(),row.channel_id,row.message_id], reserve)?;
    Ok(())
}

impl GuildDataStore {
    pub async fn update_snapshot_health(&self, update: SnapshotHealthUpdate) -> Result<()> {
        anyhow::ensure!(
            update.epoch <= i64::MAX as u64 && update.now_ms > 0,
            SnapshotError::Invalid
        );
        anyhow::ensure!(
            matches!(
                update.reason,
                None | Some(
                    "capacity"
                        | "content_access"
                        | "schema"
                        | "scope"
                        | "queue_overflow"
                        | "database"
                        | "cleanup_failed"
                        | "measurement_failed"
                        | "startup_deferred"
                        | "gateway_gap"
                        | "disabled"
                )
            ),
            SnapshotError::Invalid
        );
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| {
            let mut tx = conn.transaction_primary_read()?;
            verify_identity(&tx, &expected)?;
            let (old_epoch, old_state, old_reason, old_updated): (i64,String,Option<String>,Option<i64>) = tx.query_row("SELECT epoch,state,reason,updated_at_ms FROM guild_snapshot_health WHERE singleton=1", (), |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
            if old_epoch == update.epoch as i64 && old_state == update.state.as_str() && old_reason.as_deref() == update.reason && update.lost_delta == 0 && old_updated.is_some_and(|at| update.now_ms.saturating_sub(at) < 30_000) {
                tx.finish_read()?;
                return Ok(());
            }
            tx.ensure_mutation_receipt()?;
            tx.execute_bounded("UPDATE guild_snapshot_health SET epoch=?1,state=?2,reason=?3,last_observed_at_ms=COALESCE(?4,last_observed_at_ms),lost_count=CASE WHEN epoch=?1 THEN lost_count+?5 ELSE ?5 END,expired_count=CASE WHEN epoch=?1 THEN expired_count ELSE 0 END,evicted_count=CASE WHEN epoch=?1 THEN evicted_count ELSE 0 END,counters_reset_at_ms=CASE WHEN epoch=?1 THEN counters_reset_at_ms ELSE ?6 END,updated_at_ms=?6 WHERE singleton=1",params![update.epoch as i64,update.state.as_str(),update.reason,update.observed_at_ms,update.lost_delta.min(i64::MAX as u64) as i64,update.now_ms], growth_reserve(&tx)?)?;
            tx.commit()
        }).await
    }
    /// Creates a latest snapshot once. A delayed create cannot overwrite a newer edit.
    pub async fn observe_snapshot(
        &self,
        scope: GuildDataScope,
        mut observation: SnapshotObservation,
    ) -> Result<SnapshotOutcome> {
        self.check_target(&scope.placement)?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expires = validate(&observation)?;
        let bytes = bound_fields(&mut observation.fields)?;
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let mut tx = conn.transaction_primary_read()?;
                verify_scope(&tx, &expected, &scope, true)?;
                if observation.fields.author_id.is_none() {
                    tx.finish_read()?;
                    return Ok(SnapshotOutcome::Missing);
                }
                if observation.observed_at_ms >= expires || write_time_ms()? >= expires {
                    tx.finish_read()?;
                    return Ok(SnapshotOutcome::Expired);
                }
                if load(&tx, &scope, observation.channel_id, observation.message_id)?.is_some() {
                    tx.finish_read()?;
                    return Ok(SnapshotOutcome::Duplicate);
                }
                let reserve = growth_reserve(&tx)?;
                let admission = admit_snapshot(
                    &mut tx,
                    bytes as u64,
                    0,
                    reserve,
                    observation.observed_at_ms,
                )?;
                if !admission.allowed {
                    if admission.wrote_health {
                        tx.commit()?;
                    } else {
                        tx.finish_read()?;
                    }
                    return Ok(SnapshotOutcome::CapacityRefused);
                }
                tx.ensure_mutation_receipt()?;
                let row = MessageSnapshot {
                    guild_id: scope.guild_id(),
                    channel_id: observation.channel_id,
                    message_id: observation.message_id,
                    fields: observation.fields,
                    created_at_ms: observation.created_at_ms,
                    first_observed_at_ms: observation.observed_at_ms,
                    last_observed_at_ms: observation.observed_at_ms,
                    edit_at_ms: None,
                    expires_at_ms: expires,
                    generation: 1,
                    coverage_epoch: observation.coverage_epoch,
                    continuous: true,
                    content_supplied: false,
                    attachments_supplied: false,
                };
                insert(&tx, &scope, &row, bytes)?;
                tx.commit()?;
                Ok(SnapshotOutcome::Stored)
            })
            .await
    }

    /// Reads and writes one accepted edit in one primary transaction. Equal-version
    /// partial fields may fill unknowns but cannot claim another before/after edit.
    pub async fn apply_snapshot_edit(
        &self,
        scope: GuildDataScope,
        observation: SnapshotObservation,
    ) -> Result<SnapshotTransition> {
        self.apply_snapshot_edit_inner(scope, observation, true)
            .await
    }

    /// Applies an author-omitted edit only to an already eligible, scoped row.
    /// An absent row remains absent, so this path cannot seed an unknown author.
    pub async fn apply_snapshot_edit_existing(
        &self,
        scope: GuildDataScope,
        observation: SnapshotObservation,
    ) -> Result<SnapshotTransition> {
        self.apply_snapshot_edit_inner(scope, observation, false)
            .await
    }

    async fn apply_snapshot_edit_inner(
        &self,
        scope: GuildDataScope,
        mut observation: SnapshotObservation,
        allow_insert: bool,
    ) -> Result<SnapshotTransition> {
        self.check_target(&scope.placement)?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        let expires = validate(&observation)?;
        let edit = observation.edit_at_ms.ok_or(SnapshotError::Invalid)?;
        let incoming_bytes = bound_fields(&mut observation.fields)?;
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| {
            let mut tx = conn.transaction_primary_read()?;
            verify_scope(&tx, &expected, &scope, true)?;
            if observation.observed_at_ms >= expires || write_time_ms()? >= expires { tx.finish_read()?; return Ok(SnapshotTransition { outcome: SnapshotOutcome::Expired, before: None, after: None }); }
            let old = load(&tx, &scope, observation.channel_id, observation.message_id)?;
            if (old.is_none() && observation.fields.author_id.is_none())
                || (!allow_insert && old.as_ref().and_then(|row| row.fields.author_id).is_none()) {
                tx.finish_read()?;
                return Ok(SnapshotTransition { outcome: SnapshotOutcome::Missing, before: None, after: None });
            }
            if let Some(prior) = &old {
                if prior.expires_at_ms <= observation.observed_at_ms || prior.expires_at_ms <= write_time_ms()? { tx.finish_read()?; return Ok(SnapshotTransition { outcome: SnapshotOutcome::Expired, before: None, after: None }); }
                if prior.edit_at_ms.is_some_and(|at| edit < at) { tx.finish_read()?; return Ok(SnapshotTransition { outcome: SnapshotOutcome::Stale, before: None, after: Some(prior.clone()) }); }
            }
            let equal = old.as_ref().is_some_and(|prior| prior.edit_at_ms == Some(edit));
            let inserted_content = observation.fields.content.is_some();
            let inserted_attachments = observation.fields.attachments.is_some();
            let (fields, content_supplied, attachments_supplied) = match &old {
                Some(prior) => merged(&prior.fields, observation.fields, prior.content_supplied, prior.attachments_supplied, equal),
                None => (observation.fields, inserted_content, inserted_attachments),
            };
            let mut bounded = fields;
            let bytes = bound_fields(&mut bounded)?;
            let changed = old.as_ref().is_none_or(|prior| prior.fields != bounded || (!equal && prior.edit_at_ms != Some(edit)) || prior.coverage_epoch != observation.coverage_epoch);
            if !changed { tx.finish_read()?; return Ok(SnapshotTransition { outcome: SnapshotOutcome::Duplicate, before: None, after: old }); }
            let old_bytes = if old.is_some() { tx.query_row("SELECT encoded_bytes FROM guild_message_snapshots WHERE guild_id=?1 AND enrollment_id=?2 AND channel_id=?3 AND message_id=?4", params![scope.guild_id(),scope.enrollment.id.as_str(),observation.channel_id,observation.message_id], |r| Ok(r.get::<i64>(0)?))? as u64 } else { 0 };
            let reserve = growth_reserve(&tx)?;
            let admission = admit_snapshot(&mut tx, bytes as u64, old_bytes, reserve, observation.observed_at_ms)?;
            if !admission.allowed {
                if admission.wrote_health { tx.commit()?; } else { tx.finish_read()?; }
                return Ok(SnapshotTransition { outcome: SnapshotOutcome::CapacityRefused, before: None, after: old });
            }
            tx.ensure_mutation_receipt()?;
            let before = if equal { None } else { old.clone().map(|mut row| { row.continuous &= row.coverage_epoch == observation.coverage_epoch; row }) };
            let row = MessageSnapshot { guild_id: scope.guild_id(), channel_id: observation.channel_id, message_id: observation.message_id, fields: bounded, created_at_ms: old.as_ref().map_or(observation.created_at_ms, |r| r.created_at_ms), first_observed_at_ms: old.as_ref().map_or(observation.observed_at_ms, |r| r.first_observed_at_ms), last_observed_at_ms: observation.observed_at_ms, edit_at_ms: Some(edit), expires_at_ms: old.as_ref().map_or(expires, |r| r.expires_at_ms), generation: old.as_ref().map_or(1, |r| r.generation.saturating_add(1)), coverage_epoch: observation.coverage_epoch, continuous: old.as_ref().is_some_and(|r| r.continuous && r.coverage_epoch == observation.coverage_epoch), content_supplied, attachments_supplied };
            if old.is_some() { update(&tx, &scope, &row, bytes)?; } else { insert(&tx, &scope, &row, incoming_bytes)?; }
            tx.commit()?;
            Ok(SnapshotTransition { outcome: if equal { SnapshotOutcome::Duplicate } else { SnapshotOutcome::Stored }, before, after: Some(row) })
        }).await
    }

    pub async fn take_snapshot(
        &self,
        scope: GuildDataScope,
        channel_id: i64,
        message_id: i64,
        now_ms: i64,
    ) -> Result<Option<MessageSnapshot>> {
        let mut rows = self
            .take_snapshots(scope, channel_id, vec![message_id], now_ms)
            .await?;
        Ok(rows.pop().flatten())
    }

    pub async fn take_snapshot_at_epoch(
        &self,
        scope: GuildDataScope,
        channel_id: i64,
        message_id: i64,
        now_ms: i64,
        current_epoch: u64,
    ) -> Result<Option<MessageSnapshot>> {
        let mut rows = self
            .take_snapshots_at_epoch(scope, channel_id, vec![message_id], now_ms, current_epoch)
            .await?;
        Ok(rows.pop().flatten())
    }

    /// IDs are returned in request order. The 100-ID bound keeps one transaction short.
    pub async fn take_snapshots(
        &self,
        scope: GuildDataScope,
        channel_id: i64,
        ids: Vec<i64>,
        now_ms: i64,
    ) -> Result<Vec<Option<MessageSnapshot>>> {
        self.take_snapshots_at_epoch(scope, channel_id, ids, now_ms, u64::MAX)
            .await
    }

    /// A caller-supplied boot/gap epoch prevents a persisted row from claiming
    /// uninterrupted coverage after a restart or known missed observation.
    pub async fn take_snapshots_at_epoch(
        &self,
        scope: GuildDataScope,
        channel_id: i64,
        ids: Vec<i64>,
        now_ms: i64,
        current_epoch: u64,
    ) -> Result<Vec<Option<MessageSnapshot>>> {
        self.check_target(&scope.placement)?;
        self.refuse_fenced_guild_write(scope.guild_id()).await?;
        anyhow::ensure!(
            channel_id > 0 && now_ms > 0 && ids.len() <= MAX_BULK && ids.iter().all(|id| *id > 0),
            SnapshotError::BatchLimit
        );
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| {
            let mut tx = conn.transaction_primary_read()?;
            verify_scope(&tx, &expected, &scope, true)?;
            if ids.is_empty() { tx.finish_read()?; return Ok(Vec::new()); }
            let mut unique = ids.clone();
            unique.sort_unstable();
            unique.dedup();
            let placeholders = (4..4 + unique.len()).map(|n| format!("?{n}")).collect::<Vec<_>>().join(",");
            let where_sql = format!("guild_id=?1 AND enrollment_id=?2 AND channel_id=?3 AND message_id IN ({placeholders})");
            let mut args = vec![Value::Integer(scope.guild_id()), Value::Text(scope.enrollment.id.clone()), Value::Integer(channel_id)];
            args.extend(unique.into_iter().map(Value::Integer));
            let sql = format!("SELECT author_id,author_name,content,content_truncated,attachments,attachments_truncated,created_at_ms,first_observed_at_ms,last_observed_at_ms,edit_at_ms,expires_at_ms,generation,coverage_epoch,continuous,content_original_bytes,attachments_omitted,content_supplied,attachments_supplied,message_id FROM guild_message_snapshots WHERE {where_sql}");
            let mut stmt = tx.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(args.clone()), |row| {
                let id: i64 = row.get(18)?;
                decode(row, scope.guild_id(), channel_id, id)
            })?;
            let mut found = HashMap::with_capacity(ids.len());
            for row in rows { let row = row?; found.insert(row.message_id, row); }
            drop(stmt);
            let effective_now = now_ms.max(write_time_ms()?);
            let mut consumed = HashSet::with_capacity(ids.len());
            let out = ids.into_iter().map(|id| {
                if !consumed.insert(id) { return None; }
                found.get(&id).cloned().filter(|row| row.expires_at_ms > effective_now).map(|mut row| {
                    row.continuous &= row.coverage_epoch == current_epoch;
                    row
                })
            }).collect();
            let expired_count = found.values().filter(|row| row.expires_at_ms <= effective_now).count();
            if found.is_empty() { tx.finish_read()?; }
            else {
                tx.prepare_reclaim()?;
                tx.execute_reclaim(&format!("DELETE FROM guild_message_snapshots WHERE {where_sql}"), params_from_iter(args))?;
                if expired_count > 0 {
                    tx.execute_reclaim_metadata_bounded(
                        "UPDATE guild_snapshot_health SET expired_count=expired_count+?1 WHERE singleton=1",
                        params![expired_count as i64], growth_reserve(&tx)?,
                    )?;
                }
                tx.commit()?;
            }
            Ok(out)
        }).await
    }

    /// Store-wide expiry/oldest-first eviction under a trusted maintenance handle.
    pub async fn expire_snapshots(&self, limit: usize, now_ms: i64) -> Result<usize> {
        self.cleanup_snapshots(limit, now_ms, true).await
    }

    /// Removes the oldest fixed-expiry rows when the snapshot budget is under
    /// pressure. Caller runs bounded batches and yields between them.
    pub async fn evict_snapshots(&self, limit: usize, now_ms: i64) -> Result<usize> {
        self.cleanup_snapshots(limit, now_ms, false).await
    }

    async fn cleanup_snapshots(
        &self,
        limit: usize,
        now_ms: i64,
        expired_only: bool,
    ) -> Result<usize> {
        anyhow::ensure!(
            limit > 0 && limit <= 100 && now_ms > 0,
            SnapshotError::BatchLimit
        );
        let expected = self.expected.clone();
        self.db.with_conn(move |conn| {
            let mut tx = conn.transaction_primary_read()?;
            verify_identity(&tx, &expected)?;
            let (subquery, args): (&str, Vec<Value>) = if expired_only {
                ("SELECT guild_id,enrollment_id,channel_id,message_id FROM guild_message_snapshots WHERE expires_at_ms<=?1 ORDER BY expires_at_ms LIMIT ?2",
                 vec![Value::Integer(now_ms),Value::Integer(limit as i64)])
            } else {
                ("SELECT guild_id,enrollment_id,channel_id,message_id FROM guild_message_snapshots ORDER BY expires_at_ms LIMIT ?1",
                 vec![Value::Integer(limit as i64)])
            };
            let count_sql = format!("SELECT COUNT(*) FROM ({subquery})");
            let count: i64 = tx.query_row(&count_sql, params_from_iter(args.clone()), |r| Ok(r.get(0)?))?;
            if count == 0 {
                let last: Option<i64> = tx.query_row(
                    "SELECT last_cleanup_at_ms FROM guild_snapshot_health WHERE singleton=1",
                    (), |r| Ok(r.get(0)?),
                )?;
                if last.is_some_and(|at| now_ms.saturating_sub(at) < 30_000) {
                    tx.finish_read()?;
                } else {
                    tx.prepare_reclaim()?;
                    tx.execute_reclaim_metadata_bounded(
                        "UPDATE guild_snapshot_health SET last_cleanup_at_ms=?1,updated_at_ms=?1 WHERE singleton=1",
                        params![now_ms], tx.capacity_page_size()?.saturating_mul(1024),
                    )?;
                    tx.commit()?;
                }
                return Ok(0);
            }
            tx.prepare_reclaim()?;
            let delete_sql = format!("DELETE FROM guild_message_snapshots WHERE (guild_id,enrollment_id,channel_id,message_id) IN ({subquery})");
            let bound = tx.capacity_page_size()?.saturating_mul(1024);
            tx.execute_reclaim_bounded(&delete_sql, params_from_iter(args), bound)?;
            let metadata_sql = if expired_only {
                "UPDATE guild_snapshot_health SET last_cleanup_at_ms=?1,updated_at_ms=?1,physical_sample_at_ms=NULL,expired_count=expired_count+?2 WHERE singleton=1"
            } else {
                "UPDATE guild_snapshot_health SET last_cleanup_at_ms=?1,updated_at_ms=?1,physical_sample_at_ms=NULL,evicted_count=evicted_count+?2 WHERE singleton=1"
            };
            tx.execute_reclaim_metadata_bounded(
                metadata_sql, params![now_ms, count], growth_reserve(&tx)?,
            )?;
            tx.commit()?;
            Ok(count as usize)
        }).await
    }
}

struct SnapshotAdmission {
    allowed: bool,
    wrote_health: bool,
}

fn admit_snapshot(
    tx: &mut Transaction,
    incoming_bytes: u64,
    old_bytes: u64,
    reserve: u64,
    now_ms: i64,
) -> Result<SnapshotAdmission> {
    let allocated = crate::capacity::allocated_bytes(tx)?;
    let (logical, mut sample, mut sampled_at, mut debt, paused):
        (i64, Option<i64>, Option<i64>, i64, bool) = tx.query_row(
        "SELECT estimated_snapshot_bytes,physical_sample_bytes,physical_sample_at_ms,physical_debt_bytes,capacity_paused FROM guild_snapshot_health WHERE singleton=1",
        (), |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
    let mut wrote_health = false;
    // The high-water pause is durable and shared by all bot processes. Once
    // reached, capture waits until a fresh primary sample drops below 700 MB.
    if allocated >= crate::capacity::SNAPSHOT_PAUSE_BYTES {
        if !paused {
            tx.ensure_mutation_receipt()?;
            tx.execute_bounded(
                "UPDATE guild_snapshot_health SET capacity_paused=1 WHERE singleton=1",
                (),
                growth_reserve(tx)?,
            )?;
            wrote_health = true;
        }
        return Ok(SnapshotAdmission {
            allowed: false,
            wrote_health,
        });
    }
    if paused {
        if allocated >= crate::capacity::PRESSURE_BYTES {
            return Ok(SnapshotAdmission {
                allowed: false,
                wrote_health,
            });
        }
        tx.ensure_mutation_receipt()?;
        tx.execute_bounded(
            "UPDATE guild_snapshot_health SET capacity_paused=0 WHERE singleton=1",
            (),
            growth_reserve(tx)?,
        )?;
        wrote_health = true;
    }
    let estimated = (logical.max(0) as u64)
        .saturating_add(incoming_bytes.saturating_sub(old_bytes))
        .saturating_add(ROW_RESERVE);
    if estimated > crate::capacity::SNAPSHOT_BUDGET_BYTES {
        return Ok(SnapshotAdmission {
            allowed: false,
            wrote_health,
        });
    }
    let sampled_recently = sampled_at.is_some_and(|at| now_ms.saturating_sub(at) < 300_000);
    let sample_upper = sample
        .and_then(|n| u64::try_from(n).ok())
        .unwrap_or(allocated)
        .saturating_add(debt.max(0) as u64)
        .saturating_add(reserve);
    // Refresh the detailed allocation at most once per five minutes in the
    // ordinary path. When accumulated conservative debt nears 400 MB, allow
    // one early refresh per 30 seconds; between refreshes refuse safely.
    let budget_near = sample_upper > crate::capacity::SNAPSHOT_BUDGET_BYTES;
    let refresh = sample.is_none()
        || !sampled_recently
        || (budget_near && sampled_at.is_none_or(|at| now_ms.saturating_sub(at) >= 30_000));
    if refresh {
        let attributed: Option<i64> = tx.query_row(
            "SELECT SUM(pgsize) FROM dbstat WHERE name IN ('guild_message_snapshots','guild_snapshot_expiry','guild_snapshot_health') OR name GLOB 'sqlite_autoindex_guild_message_snapshots_*'",
            (), |r| Ok(r.get(0)?),
        ).unwrap_or(None);
        let measured = attributed
            .and_then(|n| u64::try_from(n).ok())
            .unwrap_or(allocated);
        let health_reserve = growth_reserve(tx)?;
        tx.ensure_mutation_receipt()?;
        tx.execute_bounded("UPDATE guild_snapshot_health SET physical_sample_bytes=?1,physical_sample_at_ms=?2,physical_debt_bytes=?3 WHERE singleton=1",
            params![measured as i64,now_ms,health_reserve as i64], health_reserve)?;
        wrote_health = true;
        sample = Some(measured as i64);
        sampled_at = Some(now_ms);
        debt = health_reserve as i64;
    }
    let _ = sampled_at;
    // A snapshot's table, indexes, and health pages are a subset of the
    // database's allocated pages at this primary transaction point. A fresh
    // whole-file sample bounds them even if conservative per-write debt has
    // greatly exceeded actual allocation. The pending write is covered by
    // the same full B-tree/trigger reservation on either side of the minimum.
    let fresh_allocated = crate::capacity::allocated_bytes(tx)?;
    if fresh_allocated >= crate::capacity::SNAPSHOT_PAUSE_BYTES {
        tx.ensure_mutation_receipt()?;
        tx.execute_bounded(
            "UPDATE guild_snapshot_health SET capacity_paused=1 WHERE singleton=1",
            (),
            growth_reserve(tx)?,
        )?;
        return Ok(SnapshotAdmission {
            allowed: false,
            wrote_health: true,
        });
    }
    let attributed_upper = sample
        .and_then(|n| u64::try_from(n).ok())
        .unwrap_or(allocated)
        .saturating_add(debt.max(0) as u64)
        .saturating_add(reserve);
    let physical_upper = attributed_upper.min(fresh_allocated.saturating_add(reserve));
    Ok(SnapshotAdmission {
        allowed: physical_upper <= crate::capacity::SNAPSHOT_BUDGET_BYTES,
        wrote_health,
    })
}
