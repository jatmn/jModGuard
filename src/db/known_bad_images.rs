//! Bounded guild-local operator-managed SHA-256 digests. No image payloads.
use super::known_spam::SpamListChange;
use super::*;
use crate::image_spam::parse_hash;
use serde::Deserialize;

pub const MAX_KNOWN_BAD_IMAGES: usize = 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct KnownBadImage {
    pub hash: String,
    pub added_at: String,
    pub added_via: String,
    pub actor: String,
    pub note: Option<String>,
}

pub fn canonical_image_hash(raw: &str) -> Result<String> {
    anyhow::ensure!(
        parse_hash(raw).is_some(),
        "expected a 64-character SHA-256 digest"
    );
    Ok(raw.to_ascii_lowercase())
}

pub fn safe_image_note(note: Option<String>) -> Result<Option<String>> {
    let Some(note) = note else { return Ok(None) };
    let note = note.trim();
    // Notes must be safe labels, never attachment links or copied mentions/IDs.
    let mut digits = 0;
    let has_identifier = note.chars().any(|c| {
        digits = if c.is_ascii_digit() { digits + 1 } else { 0 };
        digits >= 15
    });
    anyhow::ensure!(
        note.chars().count() <= 256
            && !note.chars().any(char::is_control)
            && !note.contains("://")
            && !note.contains("<@")
            && !note.contains("<#")
            && !has_identifier,
        "note must be a short label without links, mentions, or identifiers"
    );
    Ok((!note.is_empty()).then(|| note.to_owned()))
}

fn safe_known_bad_row(mut row: KnownBadImage) -> Option<KnownBadImage> {
    row.hash = canonical_image_hash(&row.hash).ok()?;
    row.note = safe_image_note(row.note).ok()?;
    // Unknown provenance never becomes an invented operator identity.
    row.actor = "unknown".into();
    if row.added_via != "webui" {
        row.added_via = "unknown".into();
    }
    Some(row)
}

pub(super) fn safe_known_bad_rows(rows: Vec<KnownBadImage>) -> Vec<KnownBadImage> {
    rows.into_iter()
        .filter_map(safe_known_bad_row)
        .take(MAX_KNOWN_BAD_IMAGES)
        .collect()
}

pub async fn add_enrolled_known_bad_image(
    db: &Db,
    scope: ConfigurationWriteScope,
    hash: &str,
    note: Option<String>,
) -> Result<bool> {
    add_enrolled_known_bad_image_change(db, scope, hash, note)
        .await
        .map(|change| change.value.0)
}

pub(super) async fn add_enrolled_known_bad_image_change(
    db: &Db,
    scope: ConfigurationWriteScope,
    hash: &str,
    note: Option<String>,
) -> Result<SpamListChange<(bool, Option<KnownBadImage>)>> {
    anyhow::ensure!(scope.guild_id() > 0, "known-bad images are guild-only");
    let hash = canonical_image_hash(hash)?;
    let note = safe_image_note(note)?;
    db.with_conn(move |conn| {
        let tx = conn.transaction_immediate()?;
        scope.verify_conn(&tx)?;
        let (previous_revision, existing): (String, Option<KnownBadImage>) = tx.query_row(
            "SELECT coalesce((SELECT value FROM meta WHERE key='config_revision'), ''),
                    image.hash, image.added_at, image.added_via, image.actor, image.note
             FROM (SELECT 1) LEFT JOIN known_bad_images AS image
               ON image.guild_id=?1 AND image.hash=?2",
            params![scope.guild_id(), hash.as_str()],
            |r| {
                let existing = if let Some(hash) = r.get::<Option<String>>(1)? {
                    Some(KnownBadImage {
                        hash,
                        added_at: r.get(2)?,
                        added_via: r.get(3)?,
                        actor: r.get(4)?,
                        note: r.get(5)?,
                    })
                } else {
                    None
                };
                Ok((r.get(0)?, existing))
            },
        )?;
        if let Some(existing) = existing {
            tx.commit()?;
            return Ok(SpamListChange {
                value: (false, safe_known_bad_row(existing)),
                revision: previous_revision.clone(),
                previous_revision,
            });
        }
        let count: i64 = tx.query_row(
            "SELECT count(*) FROM known_bad_images WHERE guild_id=?1",
            params![scope.guild_id()],
            |r| Ok(r.get(0)?),
        )?;
        anyhow::ensure!(
            count < MAX_KNOWN_BAD_IMAGES as i64,
            "known-bad image list is full"
        );
        let row = KnownBadImage {
            hash,
            added_at: Utc::now().to_rfc3339(),
            added_via: "webui".into(),
            actor: "unknown".into(),
            note,
        };
        tx.execute_rows(
            "INSERT INTO known_bad_images (guild_id, hash, added_at, added_via, actor, note)
             VALUES (?1, ?2, ?3, 'webui', 'unknown', ?4)",
            params![
                scope.guild_id(),
                row.hash.as_str(),
                row.added_at.as_str(),
                row.note.as_deref()
            ],
            1,
        )?;
        let revision = bump_config_version_with_revision_conn(&tx)?;
        tx.commit()?;
        Ok(SpamListChange {
            value: (true, Some(row)),
            previous_revision,
            revision,
        })
    })
    .await
}

pub async fn remove_enrolled_known_bad_image(
    db: &Db,
    scope: ConfigurationWriteScope,
    hash: &str,
) -> Result<bool> {
    remove_enrolled_known_bad_image_change(db, scope, hash)
        .await
        .map(|change| change.value)
}

pub(super) async fn remove_enrolled_known_bad_image_change(
    db: &Db,
    scope: ConfigurationWriteScope,
    hash: &str,
) -> Result<SpamListChange<bool>> {
    anyhow::ensure!(scope.guild_id() > 0, "known-bad images are guild-only");
    let hash = canonical_image_hash(hash)?;
    db.with_conn(move |conn| {
        let mut tx = conn.transaction_primary_write()?;
        scope.verify_conn(&tx)?;
        let (exists, previous_revision): (bool, String) = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM known_bad_images WHERE guild_id=?1 AND hash=?2),
                    coalesce((SELECT value FROM meta WHERE key='config_revision'), '')",
            params![scope.guild_id(), hash.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if !exists {
            tx.finish_read()?;
            return Ok(SpamListChange {
                value: false,
                revision: previous_revision.clone(),
                previous_revision,
            });
        }
        tx.prepare_reclaim()?;
        let changed = tx.execute_reclaim(
            "DELETE FROM known_bad_images WHERE guild_id=?1 AND hash=?2",
            params![scope.guild_id(), hash],
        )? > 0;
        let revision = if changed {
            bump_config_version_reclaim_with_revision_conn(&tx)?
        } else {
            previous_revision.clone()
        };
        tx.commit()?;
        Ok(SpamListChange {
            value: changed,
            previous_revision,
            revision,
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_restored_rows_and_unsafe_notes_are_never_admitted() {
        let row = |hash: &str| KnownBadImage {
            hash: hash.into(),
            added_at: String::new(),
            added_via: "unrecognized".into(),
            actor: "unverified".into(),
            note: None,
        };
        assert!(
            safe_known_bad_rows(vec![row(""), row("unknown"), row(&"g".repeat(64))]).is_empty()
        );
        let safe = safe_known_bad_rows(vec![row(&"AB".repeat(32))]);
        assert_eq!(safe[0].hash, "ab".repeat(32));
        assert_eq!(safe[0].actor, "unknown");
        assert_eq!(safe[0].added_via, "unknown");
        assert!(safe_image_note(Some("scheme://label".into())).is_err());
        assert!(safe_image_note(Some("<@mention>".into())).is_err());
    }
}
