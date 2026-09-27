//! Local backups-only zone: authenticated encrypted archives, capture and restore.
//! Conductor never imports tenant routing, Discord tokens or the application crate.
use anyhow::{Context as _, Result, ensure};
use rand::Rng;
use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};
use ring::digest::SHA256;
use ring::hkdf::{self, HKDF_SHA256};
use ring::pbkdf2;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

use crate::config::Config;
use crate::settings::ControlSettings;
use crate::store::{ArchiveRow, ControlStore, now_ms, random_token};

pub const MAGIC: &[u8] = b"JMODGUARD-BACKUP-1\n";
const ZONE_MARK: &str = "jmodguard-backup-1\n";
const CHUNK: usize = 65_536;
const MAX_RECORD_NAME_BYTES: usize = 16_384;
const PASSWORD_ITERS: u32 = 100_000;
const EXCLUDED_TABLES: &[&str] = &["bot_actions", "mutation_receipts", "nickname_reconcile"];
const EVIDENCE_RECORD_PREFIX: &str = "evidence-v1-";
const RESTORE_STAGE_MARK: &str = "jmodguard-restore-staging-1\n";

#[derive(Clone)]
pub struct BackupZone {
    pub root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveHeader {
    pub format: u32,
    pub created_ms: i64,
    pub kind: String,
    pub wrapping: String,
    pub family: String,
    pub scope: serde_json::Value,
    pub salt_hex: String,
    pub iterations: u32,
}

#[derive(Debug, Clone, Default)]
pub struct RestoreRequest {
    pub acknowledge_historical: bool,
    pub current_state_available: bool,
    pub removed_guilds: BTreeSet<RemovedGuild>,
    pub removed_cases: BTreeSet<RemovedCase>,
    pub removed_evidence: BTreeSet<RemovedEvidence>,
    pub removed_object_keys: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RemovedGuild {
    pub guild_id: i64,
    pub enrollment_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RemovedCase {
    pub guild_id: i64,
    pub enrollment_id: String,
    pub case_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RemovedEvidence {
    pub guild_id: i64,
    pub enrollment_id: String,
    pub evidence_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct CleanupIntents {
    pub guilds: BTreeSet<RemovedGuild>,
    pub cases: BTreeSet<RemovedCase>,
    pub evidence: BTreeSet<RemovedEvidence>,
    pub object_keys: BTreeSet<String>,
    pub ids: Vec<String>,
}

impl BackupZone {
    pub fn open(path: &Path) -> Result<Self> {
        ensure_dedicated_root(path)?;
        for name in ["complete", "staging", "quarantine", "scratch"] {
            private_dir(&path.join(name), true)?;
        }
        Ok(Self {
            root: path.to_path_buf(),
        })
    }

    pub fn complete_path(&self, id: &str) -> PathBuf {
        self.root.join("complete").join(format!("{id}.jmb"))
    }
    pub fn staging_path(&self, id: &str) -> PathBuf {
        self.root.join("staging").join(format!("{id}.jmb"))
    }
    pub fn quarantine_path(&self, id: &str) -> PathBuf {
        self.root.join("quarantine").join(format!("{id}.jmb"))
    }
    pub fn scratch(&self, id: &str) -> PathBuf {
        self.root.join("scratch").join(id)
    }

    pub fn cleanup_scratch(&self, id: &str) {
        let _ = fs::remove_dir_all(self.scratch(id));
        let _ = fs::remove_file(self.staging_path(id));
    }

    pub fn cleanup_capture_workdir(&self, id: &str) {
        let _ = fs::remove_dir_all(self.scratch(id));
    }

    pub fn begin_upload(&self) -> Result<(String, File)> {
        let id = random_token();
        let file = private_file(&self.quarantine_path(&id))?;
        Ok((id, file))
    }

    pub fn remove_quarantine(&self, id: &str) {
        let _ = fs::remove_file(self.quarantine_path(id));
    }

    pub fn prune_untracked_complete(&self, keep: &BTreeSet<String>) -> Result<()> {
        for entry in fs::read_dir(self.root.join("complete"))? {
            let path = entry?.path();
            if !path.is_file() || path.extension().and_then(|value| value.to_str()) != Some("jmb") {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|value| value.to_str())
                .context("invalid complete archive name")?;
            if !keep.contains(id) {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    pub fn prune_incomplete(&self) -> Result<()> {
        for dir in ["staging", "scratch"] {
            let root = self.root.join(dir);
            if !root.exists() {
                continue;
            }
            for entry in fs::read_dir(&root)? {
                let path = entry?.path();
                let _ = if path.is_dir() {
                    fs::remove_dir_all(path)
                } else {
                    fs::remove_file(path)
                };
            }
        }
        Ok(())
    }
}

pub async fn load_or_create_key(cfg: &Config, store: &ControlStore) -> Result<[u8; 32]> {
    if let Some(key) = cfg.backup_key {
        return Ok(key);
    }
    if let Some(hex) = store.meta_get("backup_key_hex").await? {
        return parse_stored_key(&hex);
    }
    let mut key = [0u8; 32];
    rand::rng().fill_bytes(&mut key);
    store.meta_set("backup_key_hex", &hex_encode(&key)).await?;
    Ok(key)
}

fn parse_stored_key(hex: &str) -> Result<[u8; 32]> {
    ensure!(hex.len() == 64, "stored backup key is invalid");
    let mut key = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        key[i] = u8::from_str_radix(std::str::from_utf8(chunk)?, 16)?;
    }
    Ok(key)
}

pub async fn capture(
    cfg: &Config,
    store: &ControlStore,
    zone: &BackupZone,
    kind: &str,
    settings: &ControlSettings,
) -> Result<ArchiveRow> {
    ensure!(
        kind == "scheduled" || kind == "manual" || kind == "checkpoint",
        "unsupported backup kind"
    );
    let core = cfg
        .capture_stores
        .iter()
        .find(|capture| capture.role == "core")
        .context("capture requires a configured core store")?;
    ensure!(core.path.exists(), "capture store core is missing");
    let id = random_token();
    let created = now_ms();
    let roles: Vec<String> = cfg.capture_stores.iter().map(|s| s.role.clone()).collect();
    store
        .insert_archive(&ArchiveRow {
            id: id.clone(),
            kind: kind.into(),
            status: "staging".into(),
            created_ms: created,
            captured_ms: None,
            scope_json: serde_json::json!({ "stores": roles }).to_string(),
            object_key: None,
            sha256: None,
            bytes: None,
            family: "jmodguard-core-1".into(),
            last_error: None,
        })
        .await?;
    match capture_inner(cfg, store, zone, settings, kind, &id, created).await {
        Ok(row) => {
            store.update_archive(&row).await?;
            if let Err(error) = promote_staging_archive(zone, &id) {
                if let Ok(Some(mut failed)) = store.get_archive(&id).await {
                    failed.status = "failed".into();
                    failed.last_error = Some(format!("{error:#}"));
                    failed.object_key = Some(format!("staging/{id}.jmb"));
                    let _ = store.update_archive(&failed).await;
                }
                return Err(error);
            }
            Ok(row)
        }
        Err(error) => {
            zone.cleanup_scratch(&id);
            if let Ok(Some(mut row)) = store.get_archive(&id).await {
                row.status = "failed".into();
                row.last_error = Some(error.to_string());
                let _ = store.update_archive(&row).await;
            }
            Err(error)
        }
    }
}

async fn capture_inner(
    cfg: &Config,
    store: &ControlStore,
    zone: &BackupZone,
    settings: &ControlSettings,
    kind: &str,
    id: &str,
    created: i64,
) -> Result<ArchiveRow> {
    let key = load_or_create_key(cfg, store).await?;
    let scratch = zone.scratch(id);
    private_dir(&scratch, true)?;
    let payload = scratch.join("payload.bin");
    let mut writer = private_file(&payload)?;
    let mut roles = Vec::new();
    for capture in &cfg.capture_stores {
        ensure!(
            capture.path.exists(),
            "capture store {} is missing",
            capture.role
        );
        let record_name = format!("store-{}", safe_name(&capture.role));
        let logical_bytes = sqlite_logical_bytes(&capture.path).await?;
        ensure_record_fits(
            &writer,
            &record_name,
            logical_bytes,
            settings.backup_max_archive_bytes,
        )?;
        let snapshot = scratch.join(format!("{}.sqlite", safe_name(&capture.role)));
        snapshot_sqlite(&capture.path, &snapshot).await?;
        if capture.role != "core" {
            if let Some(core) = cfg
                .capture_stores
                .iter()
                .find(|store| store.role == "core")
            {
                release_member_sync_pacing_from_guild_store(&capture.path, &core.path).await?;
            }
        }
        ensure_record_fits(
            &writer,
            &record_name,
            fs::metadata(&snapshot)?.len(),
            settings.backup_max_archive_bytes,
        )?;
        append_file(&mut writer, &record_name, &snapshot)?;
        roles.push(capture.role.clone());
    }
    let mut evidence_names = Vec::new();
    if let Some(root) = &cfg.evidence_root {
        let intents = load_cleanup_intents(cfg).await?;
        let remaining = settings
            .backup_max_archive_bytes
            .saturating_sub(writer.metadata()?.len());
        for (relative, path) in copy_evidence(root, &scratch.join("evidence"), &intents, remaining)?
        {
            let record_name = evidence_record_name(&relative);
            ensure_record_fits(
                &writer,
                &record_name,
                fs::metadata(&path)?.len(),
                settings.backup_max_archive_bytes,
            )?;
            append_file(&mut writer, &record_name, &path)?;
            evidence_names.push(relative);
        }
    }
    let control = serde_json::to_vec(&serde_json::json!({
        "settings": settings.public_view(),
        "roles": roles,
        "evidence": evidence_names,
        "secrets_excluded": [
            "oauth", "discord_token", "webhook", "backup_key", "sessions", "pending_actions"
        ],
    }))?;
    ensure_record_fits(
        &writer,
        "control.json",
        control.len() as u64,
        settings.backup_max_archive_bytes,
    )?;
    append_bytes(&mut writer, "control.json", &control)?;
    writer.sync_all()?;
    drop(writer);

    let dest = zone.staging_path(id);
    encrypt_payload(
        &payload,
        &dest,
        &key,
        ArchiveHeader {
            format: 1,
            created_ms: created,
            kind: kind.into(),
            wrapping: "master".into(),
            family: "jmodguard-core-1".into(),
            scope: serde_json::json!({
                "stores": roles,
                "evidence": evidence_names,
                "control": true
            }),
            salt_hex: String::new(),
            iterations: 1,
        },
        None,
    )?;
    let bytes = fs::metadata(&dest)?.len();
    ensure!(
        bytes <= settings.backup_max_archive_bytes,
        "archive exceeds configured byte budget"
    );
    let sha = sha256_file(&dest)?;
    zone.cleanup_capture_workdir(id);
    Ok(ArchiveRow {
        id: id.to_string(),
        kind: kind.into(),
        status: "complete".into(),
        created_ms: created,
        captured_ms: Some(created),
        scope_json: serde_json::json!({ "stores": roles, "evidence": evidence_names }).to_string(),
        object_key: Some(format!("complete/{id}.jmb")),
        sha256: Some(sha),
        bytes: Some(i64::try_from(bytes).unwrap_or(i64::MAX)),
        family: "jmodguard-core-1".into(),
        last_error: None,
    })
}

pub async fn wrap_portable_archive(
    zone: &BackupZone,
    store: &ControlStore,
    cfg: &Config,
    id: &str,
    password: &str,
) -> Result<PathBuf> {
    ensure!(!password.is_empty(), "export password is required");
    let row = store.get_archive(id).await?.context("backup not found")?;
    ensure!(row.status == "complete", "backup is not complete");
    let master = load_or_create_key(cfg, store).await?;
    let scratch_id = format!("portable-{id}");
    let scratch = zone.scratch(&scratch_id);
    private_dir(&scratch, true)?;
    let plain = scratch.join("payload.bin");
    decrypt_payload(&zone.complete_path(id), &plain, &master, None)?;
    let salt = random_salt();
    let key = derive_password_key(password, &salt)?;
    let dest = scratch.join("portable.jmb");
    encrypt_payload(
        &plain,
        &dest,
        &key,
        ArchiveHeader {
            format: 1,
            created_ms: now_ms(),
            kind: "portable".into(),
            wrapping: "password".into(),
            family: row.family,
            scope: serde_json::from_str(&row.scope_json).unwrap_or(serde_json::json!({})),
            salt_hex: hex_encode(&salt),
            iterations: PASSWORD_ITERS,
        },
        Some(password),
    )?;
    Ok(dest)
}

pub async fn restore_archive(
    cfg: &Config,
    store: &ControlStore,
    zone: &BackupZone,
    id: &str,
    password: Option<&str>,
    request: RestoreRequest,
    settings: &ControlSettings,
) -> Result<PathBuf> {
    ensure_archive_id(id)?;
    if let Some(row) = store.get_archive(id).await? {
        let src = match row.status.as_str() {
            "complete" => zone.complete_path(id),
            "quarantined" => zone.quarantine_path(id),
            _ => anyhow::bail!("backup is not complete"),
        };
        return restore_file(cfg, store, zone, &src, password, request, settings).await;
    }
    let quarantined = zone.quarantine_path(id);
    ensure!(quarantined.exists(), "backup not found");
    restore_file(cfg, store, zone, &quarantined, password, request, settings).await
}

pub async fn restore_file(
    cfg: &Config,
    store: &ControlStore,
    zone: &BackupZone,
    src: &Path,
    password: Option<&str>,
    request: RestoreRequest,
    settings: &ControlSettings,
) -> Result<PathBuf> {
    if !request.current_state_available && !request.acknowledge_historical {
        anyhow::bail!("historical restore acknowledgment required");
    }
    let target = cfg
        .restore_target
        .clone()
        .context("JMODGUARD_CONDUCTOR_RESTORE_TARGET is required for restore")?;
    ensure!(
        !target.exists(),
        "restore target must be an absent directory"
    );
    let restore_stage = restore_staging_path(&target)?;
    if recover_restore_stage(&restore_stage, &target)? {
        return Ok(target);
    }
    let restore_id = format!("restore-{}", random_token());
    let scratch = zone.scratch(&restore_id);
    private_dir(&scratch, true)?;
    let result: Result<()> = async {
        let payload = scratch.join("payload.bin");
        let header = decrypt_payload(
            src,
            &payload,
            &load_or_create_key(cfg, store).await?,
            password,
        )?;
        ensure!(
            header.family == "jmodguard-core-1",
            "incompatible archive family"
        );
        let files = unpack_payload(
            &payload,
            &scratch.join("files"),
            settings.backup_max_archive_bytes,
        )?;
        private_dir(&restore_stage, true)?;
        let mut marker = private_file(&restore_stage.join(".jmodguard-restore-staging"))?;
        marker.write_all(RESTORE_STAGE_MARK.as_bytes())?;
        marker.sync_all()?;
        restore_files(&restore_stage, files, &request).await?;
        let mut complete = private_file(&restore_stage.join("COMPLETE"))?;
        complete.write_all(RESTORE_COMPLETE_MARK)?;
        complete.sync_all()?;
        File::open(&restore_stage)?.sync_all()?;
        fs::rename(&restore_stage, &target)?;
        File::open(target.parent().context("restore target has no parent")?)?.sync_all()?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_dir_all(&restore_stage);
    }
    zone.cleanup_scratch(&restore_id);
    result?;
    Ok(target)
}

async fn restore_files(
    target: &Path,
    files: Vec<(String, PathBuf)>,
    request: &RestoreRequest,
) -> Result<()> {
    for (name, path) in files {
        if let Some(role) = name.strip_prefix("store-") {
            let dest = target.join(format!("{role}.sqlite"));
            fs::copy(&path, &dest)?;
            apply_removals(&dest, request).await?;
            scrub_transient_snapshots(&dest).await?;
            ensure!(
                sqlite_logical_bytes(&dest).await? <= 900_000_000,
                "restored database exceeds application capacity budget"
            );
        } else if let Some(relative) = evidence_relative_name(&name)? {
            if evidence_is_removed(
                &relative,
                &request.removed_guilds,
                &request.removed_cases,
                &request.removed_evidence,
                &request.removed_object_keys,
            )? {
                continue;
            }
            let dest = target.join("evidence").join(&relative);
            private_dir(dest.parent().context("evidence path has no parent")?, true)?;
            fs::copy(path, dest)?;
        } else if name == "control.json" {
            fs::copy(path, target.join("control.json"))?;
        }
    }
    let core_path = target.join("core.sqlite");
    if core_path.exists() {
        for entry in fs::read_dir(target)? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "sqlite") && path != core_path {
                release_member_sync_pacing_from_guild_store(&path, &core_path).await?;
            }
        }
    }
    Ok(())
}

pub async fn sanitize_complete_archives(
    cfg: &Config,
    store: &ControlStore,
    zone: &BackupZone,
    settings: &ControlSettings,
    request: &RestoreRequest,
) -> Result<usize> {
    if request.removed_guilds.is_empty()
        && request.removed_cases.is_empty()
        && request.removed_evidence.is_empty()
        && request.removed_object_keys.is_empty()
    {
        return Ok(0);
    }
    zone.prune_untracked_complete(&store.complete_archive_ids().await?)?;
    let mut rewritten = 0usize;
    for row in store.list_complete_archives().await? {
        let master = load_or_create_key(cfg, store).await?;
        let scratch = zone.scratch(&format!("sanitize-{}", row.id));
        private_dir(&scratch, true)?;
        let plain = scratch.join("payload.bin");
        decrypt_payload(&zone.complete_path(&row.id), &plain, &master, None)?;
        let files = unpack_payload(
            &plain,
            &scratch.join("files"),
            settings.backup_max_archive_bytes,
        )?;
        let rebuilt = scratch.join("rebuilt.bin");
        let mut writer = private_file(&rebuilt)?;
        for (name, path) in files {
            if name.starts_with("store-") {
                apply_removals(&path, request).await?;
                scrub_transient_snapshots(&path).await?;
                append_file(&mut writer, &name, &path)?;
            } else if let Some(relative) = evidence_relative_name(&name)? {
                if evidence_is_removed(
                    &relative,
                    &request.removed_guilds,
                    &request.removed_cases,
                    &request.removed_evidence,
                    &request.removed_object_keys,
                )? {
                    continue;
                }
                append_file(&mut writer, &name, &path)?;
            } else {
                append_file(&mut writer, &name, &path)?;
            }
        }
        writer.sync_all()?;
        drop(writer);
        let new_id = random_token();
        let staging = zone.staging_path(&new_id);
        encrypt_payload(
            &rebuilt,
            &staging,
            &master,
            ArchiveHeader {
                format: 1,
                created_ms: now_ms(),
                kind: row.kind.clone(),
                wrapping: "master".into(),
                family: row.family.clone(),
                scope: serde_json::json!({ "sanitized_from": row.id }),
                salt_hex: String::new(),
                iterations: 1,
            },
            None,
        )?;
        let bytes = i64::try_from(fs::metadata(&staging)?.len()).unwrap_or(i64::MAX);
        let replacement = ArchiveRow {
            id: new_id.clone(),
            kind: row.kind.clone(),
            status: "complete".into(),
            created_ms: now_ms(),
            captured_ms: row.captured_ms,
            scope_json: serde_json::json!({ "sanitized_from": row.id }).to_string(),
            object_key: Some(format!("complete/{new_id}.jmb")),
            sha256: Some(sha256_file(&staging)?),
            bytes: Some(bytes),
            family: row.family.clone(),
            last_error: None,
        };
        if let Err(error) = store.replace_archive(&row.id, &replacement).await {
            let _ = fs::remove_file(&staging);
            zone.cleanup_scratch(&format!("sanitize-{}", row.id));
            return Err(error);
        }
        let complete = zone.complete_path(&new_id);
        fs::rename(&staging, &complete)?;
        fs::remove_file(zone.complete_path(&row.id))?;
        zone.cleanup_scratch(&format!("sanitize-{}", row.id));
        rewritten += 1;
    }
    Ok(rewritten)
}

pub async fn current_policy_state_available(cfg: &Config) -> Result<bool> {
    let Some(core) = cfg.capture_stores.iter().find(|store| store.role == "core") else {
        return Ok(false);
    };
    if !core.path.exists() {
        return Ok(false);
    }
    let db = libsql::Builder::new_local(&core.path).build().await?;
    let conn = db.connect()?;
    let mut rows = match conn
        .query(
            "SELECT
               EXISTS(SELECT 1 FROM meta
                      WHERE key = 'schema_family' AND value = 'jmodguard-core-1'),
               EXISTS(SELECT 1 FROM sqlite_master
                      WHERE type = 'table' AND name = 'backup_cleanup_intents')",
            (),
        )
        .await
    {
        Ok(rows) => rows,
        Err(error) if error.to_string().contains("no such table") => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let Some(row) = rows.next().await? else {
        return Ok(false);
    };
    Ok(row.get::<i64>(0)? == 1 && row.get::<i64>(1)? == 1)
}

pub async fn load_cleanup_intents(cfg: &Config) -> Result<CleanupIntents> {
    let mut intents = CleanupIntents::default();
    let Some(core) = cfg.capture_stores.iter().find(|s| s.role == "core") else {
        return Ok(intents);
    };
    if !core.path.exists() {
        return Ok(intents);
    }
    let db = libsql::Builder::new_local(&core.path).build().await?;
    let conn = db.connect()?;
    let mut rows = match conn
        .query(
            "SELECT intent_id, kind, guild_id, enrollment_id, case_id, evidence_id, object_key
             FROM backup_cleanup_intents",
            (),
        )
        .await
    {
        Ok(rows) => rows,
        Err(error) if error.to_string().contains("no such table") => return Ok(intents),
        Err(error) => return Err(error.into()),
    };
    while let Some(row) = rows.next().await? {
        let id: String = row.get(0)?;
        let kind: String = row.get(1)?;
        let guild_id: i64 = row.get(2)?;
        let enrollment_id = row.get::<String>(3).ok();
        let case_id = row.get::<String>(4).ok();
        let evidence_id = row.get::<String>(5).ok();
        let object_key = row.get::<String>(6).ok();
        intents.ids.push(id);
        match kind.as_str() {
            "guild" => {
                intents.guilds.insert(RemovedGuild {
                    guild_id,
                    enrollment_id: enrollment_id
                        .context("guild cleanup intent missing enrollment")?,
                });
            }
            "case" => {
                intents.cases.insert(RemovedCase {
                    guild_id,
                    enrollment_id: enrollment_id
                        .context("case cleanup intent missing enrollment")?,
                    case_id: case_id.context("case cleanup intent missing case")?,
                });
            }
            "evidence" => {
                intents.evidence.insert(RemovedEvidence {
                    guild_id,
                    enrollment_id: enrollment_id
                        .context("evidence cleanup intent missing enrollment")?,
                    evidence_id: evidence_id.context("evidence cleanup intent missing evidence")?,
                });
            }
            _ => anyhow::bail!("unsupported backup cleanup intent kind {kind}"),
        }
        if let Some(key) = object_key.filter(|key| !key.is_empty()) {
            intents.object_keys.insert(key);
        }
    }
    Ok(intents)
}

pub async fn clear_cleanup_intents(cfg: &Config, ids: &[String]) -> Result<()> {
    let Some(core) = cfg.capture_stores.iter().find(|s| s.role == "core") else {
        return Ok(());
    };
    let db = libsql::Builder::new_local(&core.path).build().await?;
    let conn = db.connect()?;
    for chunk in ids.chunks(100) {
        conn.execute("BEGIN IMMEDIATE", ()).await?;
        let cleared: Result<()> = async {
            let before = cleanup_allocated_bytes(&conn).await?;
            // This indexed table has no delete triggers. Cleanup claims no
            // growth, and the primary transaction rolls back if pages grow.
            ensure!(
                before <= 984_000_000,
                "cleanup database exceeds recovery ceiling"
            );
            for id in chunk {
                conn.execute(
                    "DELETE FROM backup_cleanup_intents WHERE intent_id = ?",
                    libsql::params![id.clone()],
                )
                .await?;
            }
            ensure!(
                cleanup_allocated_bytes(&conn).await? <= before,
                "cleanup database allocation grew during reclamation"
            );
            Ok(())
        }
        .await;
        match cleared {
            Ok(()) => {
                conn.execute("COMMIT", ()).await?;
            }
            Err(error) => {
                let _ = conn.execute("ROLLBACK", ()).await;
                return Err(error);
            }
        }
    }
    Ok(())
}

async fn cleanup_allocated_bytes(conn: &libsql::Connection) -> Result<u64> {
    async fn pragma(conn: &libsql::Connection, name: &str) -> Result<u64> {
        let mut rows = conn.query(&format!("PRAGMA {name}"), ()).await?;
        let value: i64 = rows
            .next()
            .await?
            .context("database capacity measurement unavailable")?
            .get(0)?;
        ensure!(value > 0, "database capacity measurement unavailable");
        Ok(value as u64)
    }
    pragma(conn, "page_count")
        .await?
        .checked_mul(pragma(conn, "page_size").await?)
        .context("database allocation overflow")
}

pub fn promote_staging_archive(zone: &BackupZone, id: &str) -> Result<()> {
    let staging = zone.staging_path(id);
    let complete = zone.complete_path(id);
    if complete.exists() {
        if staging.exists() {
            fs::remove_file(staging)?;
        }
        return Ok(());
    }
    ensure!(
        staging.exists(),
        "staging archive is missing for published backup {id}"
    );
    fs::rename(&staging, &complete)?;
    File::open(complete.parent().context("complete dir")?)?.sync_all()?;
    Ok(())
}

pub async fn recover_catalog_archive_bytes(zone: &BackupZone, store: &ControlStore) -> Result<()> {
    for row in store.list_complete_archives().await? {
        if row.status == "complete" {
            promote_staging_archive(zone, &row.id)?;
        }
    }
    for row in store.list_staging_archives().await? {
        adopt_staging_catalog_archive(zone, store, &row).await?;
    }
    store.fail_remaining_staging_archives().await?;
    Ok(())
}

pub async fn restore_fence_active(path: &Path) -> Result<bool> {
    let db = libsql::Builder::new_local(path).build().await?;
    let conn = db.connect()?;
    let mut rows = match conn
        .query("SELECT value FROM meta WHERE key = 'restore_fence'", ())
        .await
    {
        Ok(rows) => rows,
        Err(error) if error.to_string().contains("no such table: meta") => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    Ok(matches!(
        rows.next().await?.and_then(|row| row.get::<String>(0).ok()),
        Some(value) if value == "1"
    ))
}

pub async fn set_restore_fence(path: &Path, on: bool) -> Result<()> {
    let db = libsql::Builder::new_local(path).build().await?;
    let conn = db.connect()?;
    conn.execute(
        "INSERT INTO meta(key, value) VALUES ('restore_fence', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        libsql::params![if on { "1" } else { "0" }],
    )
    .await?;
    Ok(())
}

/// Restore target verification is independent of live-core fence cleanup.
/// A verified restore must not be reported as failed solely because fence clear failed.
pub fn restore_job_result(restore: Result<()>, fence_clear: Result<()>) -> Result<()> {
    match (restore, fence_clear) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => {
            tracing::error!("restore target verified; restore fence was not cleared: {error:#}");
            Ok(())
        }
        (Err(restore_error), Err(clear_error)) => Err(restore_error.context(format!(
            "restore fence also failed to clear: {clear_error:#}"
        ))),
    }
}

pub async fn clear_restore_fence(path: &Path) -> Result<()> {
    let mut last_error = None;
    for attempt in 0..5 {
        match set_restore_fence(path, false).await {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
        if attempt + 1 < 5 {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("failed to clear restore fence")))
}

/// Release installation-wide member-sync pacing leases for bots in this guild store.
async fn snap_table_exists(conn: &libsql::Connection, table: &str) -> Result<bool> {
    let mut rows = conn
        .query(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
            libsql::params![table],
        )
        .await?;
    Ok(rows.next().await?.is_some())
}

pub(crate) async fn release_member_sync_pacing_from_guild_store(
    guild_store: &Path,
    core_store: &Path,
) -> Result<()> {
    let guild_db = libsql::Builder::new_local(guild_store).build().await?;
    let guild_conn = guild_db.connect()?;
    let has_seed = {
        let mut rows = guild_conn
            .query(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='guild_member_seed'",
                (),
            )
            .await?;
        rows.next().await?.is_some()
    };
    if !has_seed {
        return Ok(());
    }
    let core_db = libsql::Builder::new_local(core_store).build().await?;
    let core_conn = core_db.connect()?;
    let has_pacing = {
        let mut rows = core_conn
            .query(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='member_sync_pacing'",
                (),
            )
            .await?;
        rows.next().await?.is_some()
    };
    if !has_pacing {
        return Ok(());
    }
    let mut rows = guild_conn
        .query(
            "SELECT DISTINCT bot_id FROM guild_member_seed WHERE bot_id > 0",
            (),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        let bot_id = row.get::<i64>(0)?;
        core_conn
            .execute(
                "UPDATE member_sync_pacing SET lease_owner=NULL, lease_until_ms=NULL WHERE bot_id=?",
                libsql::params![bot_id],
            )
            .await?;
    }
    Ok(())
}

async fn snapshot_sqlite(source: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        fs::remove_file(dest)?;
    }
    let sql_dest = sql_literal_path(dest)?;
    let db = libsql::Builder::new_local(source).build().await?;
    let conn = db.connect()?;
    conn.execute(&format!("VACUUM INTO '{sql_dest}'"), ())
        .await
        .context("sqlite snapshot failed")?;
    let snap = libsql::Builder::new_local(dest).build().await?;
    let snap_conn = snap.connect()?;
    let has_member_seed = {
        let mut rows = snap_conn
            .query(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='guild_member_seed'",
                (),
            )
            .await?;
        rows.next().await?.is_some()
    };
    if has_member_seed {
        let member_version = snap_conn
            .query("PRAGMA user_version", ())
            .await?
            .next()
            .await?
            .ok_or_else(|| anyhow::anyhow!("snapshot schema version missing"))?
            .get::<i64>(0)?;
        snap_conn
            .execute(
                if member_version >= 11 {
                    "UPDATE guild_member_seed SET reconciliation_needed=1,gap_at_ms=MAX(coalesce(gap_at_ms,0),unixepoch()*1000),gap_generation=gap_generation+1,directory_revision=directory_revision+1"
                } else {
                    // v10 (0010) has guild_member_seed without gap_generation (0011).
                    "UPDATE guild_member_seed SET reconciliation_needed=1,gap_at_ms=MAX(coalesce(gap_at_ms,0),unixepoch()*1000),directory_revision=directory_revision+1"
                },
                (),
            )
            .await?;
        let has_member_sync = snap_table_exists(&snap_conn, "guild_member_sync").await?;
        if has_member_sync {
            snap_conn.execute("UPDATE guild_member_sync SET lease_owner=NULL,lease_until_ms=NULL,generation=generation+1,status=CASE WHEN status='running' THEN 'waiting' ELSE status END,status_reason=CASE WHEN status='running' THEN 'worker claim invalidated by backup' ELSE status_reason END",()).await?;
        }
        let has_audit_recovery =
            snap_table_exists(&snap_conn, "guild_member_audit_recovery").await?;
        if has_audit_recovery {
            snap_conn.execute("UPDATE guild_member_audit_recovery SET lease_owner=NULL,lease_until_ms=NULL,generation=generation+1,status=CASE WHEN status='running' THEN 'waiting' ELSE status END,status_reason=CASE WHEN status='running' THEN 'worker claim invalidated by backup' ELSE status_reason END",()).await?;
            snap_conn.execute("UPDATE guild_member_audit_recovery SET restart_requested_at_ms=unixepoch()*1000 WHERE status!='complete' AND cursor IS NOT NULL",()).await?;
            snap_conn.execute("INSERT INTO guild_member_audit_recovery(guild_id,enrollment_id,bot_id,run_id,started_at_ms,status,next_attempt_at_ms) SELECT guild_id,enrollment_id,bot_id,lower(hex(randomblob(16))),unixepoch()*1000,'queued',unixepoch()*1000 FROM guild_member_seed WHERE initial_completed_at_ms IS NOT NULL ON CONFLICT(guild_id) DO UPDATE SET run_id=excluded.run_id,started_at_ms=excluded.started_at_ms,cursor=NULL,status='queued',status_reason=NULL,processed=0,next_attempt_at_ms=excluded.next_attempt_at_ms,request_after_ms=NULL,generation=guild_member_audit_recovery.generation+1 WHERE guild_member_audit_recovery.status='complete'",()).await?;
        }
    }
    for table in EXCLUDED_TABLES {
        let _ = snap_conn
            .execute(&format!("DROP TABLE IF EXISTS {table}"), ())
            .await;
    }
    drop(snap_conn);
    scrub_transient_snapshots(dest).await?;
    Ok(())
}

/// Sanitizes a copied database only. Restore calls this before publishing its
/// staging directory, including for an older full archive.
async fn scrub_transient_snapshots(path: &Path) -> Result<()> {
    let db = libsql::Builder::new_local(path).build().await?;
    let conn = db.connect()?;
    let mut rows = conn.query("SELECT name FROM sqlite_master WHERE type='table' AND name IN ('guild_message_snapshots','guild_snapshot_health')", ()).await?;
    let mut tables = BTreeSet::new();
    while let Some(row) = rows.next().await? {
        tables.insert(row.get::<String>(0)?);
    }
    drop(rows);
    if tables.contains("guild_message_snapshots") {
        let mut updated = conn.query("PRAGMA secure_delete=ON", ()).await?;
        let _ = updated.next().await?;
        drop(updated);
        let mut setting = conn.query("PRAGMA secure_delete", ()).await?;
        ensure!(
            setting
                .next()
                .await?
                .context("secure delete unavailable")?
                .get::<i64>(0)?
                == 1,
            "secure delete unavailable for snapshot sanitation"
        );
        drop(setting);
        conn.execute("DELETE FROM guild_message_snapshots", ())
            .await?;
    }
    if tables.contains("guild_snapshot_health") {
        conn.execute("DELETE FROM guild_snapshot_health", ())
            .await?;
        conn.execute("INSERT INTO guild_snapshot_health(singleton) VALUES(1)", ())
            .await?;
    }
    if tables.contains("guild_message_snapshots") {
        conn.execute("VACUUM", ()).await?;
    }
    Ok(())
}

async fn sqlite_logical_bytes(path: &Path) -> Result<u64> {
    let db = libsql::Builder::new_local(path).build().await?;
    let conn = db.connect()?;
    let mut rows = conn.query("PRAGMA page_count", ()).await?;
    let page_count = rows
        .next()
        .await?
        .context("sqlite page count is unavailable")?
        .get::<i64>(0)?;
    let mut rows = conn.query("PRAGMA page_size", ()).await?;
    let page_size = rows
        .next()
        .await?
        .context("sqlite page size is unavailable")?
        .get::<i64>(0)?;
    let page_count = u64::try_from(page_count).context("invalid sqlite page count")?;
    let page_size = u64::try_from(page_size).context("invalid sqlite page size")?;
    page_count
        .checked_mul(page_size)
        .context("sqlite logical size overflow")
}

fn ensure_record_fits(writer: &File, name: &str, size: u64, max_bytes: u64) -> Result<()> {
    let name_bytes = u64::try_from(safe_name(name).len()).unwrap_or(u64::MAX);
    let record_bytes = 4_u64
        .checked_add(name_bytes)
        .and_then(|value| value.checked_add(8))
        .and_then(|value| value.checked_add(size))
        .context("backup payload size overflow")?;
    let next = writer
        .metadata()?
        .len()
        .checked_add(record_bytes)
        .context("backup payload size overflow")?;
    ensure!(
        next <= max_bytes,
        "backup payload exceeds configured byte budget"
    );
    Ok(())
}

async fn apply_removals(path: &Path, request: &RestoreRequest) -> Result<()> {
    if request.removed_guilds.is_empty()
        && request.removed_cases.is_empty()
        && request.removed_evidence.is_empty()
    {
        return Ok(());
    }
    let db = libsql::Builder::new_local(path).build().await?;
    let conn = db.connect()?;

    let mut tables = BTreeSet::new();
    let mut rows = conn
        .query("SELECT name FROM sqlite_master WHERE type = 'table'", ())
        .await?;
    while let Some(row) = rows.next().await? {
        tables.insert(row.get::<String>(0)?);
    }
    let mut triggers = Vec::new();
    let mut rows = conn
        .query(
            "SELECT name, sql FROM sqlite_master
             WHERE type = 'trigger' AND sql IS NOT NULL ORDER BY name",
            (),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        triggers.push((row.get::<String>(0)?, row.get::<String>(1)?));
    }

    conn.execute("BEGIN IMMEDIATE", ()).await?;
    let result: Result<()> = async {
        // These are disposable copied databases. Temporarily removing their
        // application lifecycle triggers lets a cleanup intent erase an entire
        // owned scope, then restores the exact schema before commit.
        for (name, _) in &triggers {
            conn.execute_batch(&format!(
                "DROP TRIGGER {}",
                quote_identifier(name)
            ))
            .await?;
        }

        const GUILD_TABLES: &[&str] = &[
            "guild_settings",
            "events",
            "warns",
            "bans",
            "log_channels",
            "badwords",
            "rules",
            "commands",
            "templates",
            "guild_store_placements",
            "guild_bindings",
        ];
        const ENROLLMENT_TABLES: &[&str] = &[
            "guild_erasure_jobs",
            "guild_member_case_index",
            "guild_member_state_observations",
            "guild_member_target_checks",
            "guild_member_audit_recovery",
            "guild_member_unresolved_audits",
            "guild_member_stays",
            "guild_member_pending_bans",
            "guild_member_directory",
            "guild_member_sync",
            "guild_member_seed",
            "guild_member_feature",
            "backup_cleanup_intents",
            "guild_bunny_evidence",
            "guild_case_evidence",
            "guild_case_history",
            "guild_case_sources",
            "guild_cases",
            "guild_log_channels",
            "guild_tenant_settings",
            "guild_message_snapshots",
            "guild_memberships",
        ];
        for removed in &request.removed_guilds {
            let directory_matches = if tables.contains("guild_enrollments") {
                let mut rows = conn
                    .query(
                        "SELECT id, active FROM guild_enrollments
                         WHERE guild_id = ? LIMIT 1",
                        libsql::params![removed.guild_id],
                    )
                    .await?;
                match rows.next().await? {
                    Some(row) if row.get::<String>(0)? == removed.enrollment_id => true,
                    Some(row)
                        if !row.get::<bool>(1)? && tables.contains("guild_erasure_jobs") =>
                    {
                        let mut erasures = conn
                            .query(
                                "SELECT 1 FROM guild_erasure_jobs
                                 WHERE guild_id = ? AND enrollment_id = ? LIMIT 1",
                                libsql::params![
                                    removed.guild_id,
                                    removed.enrollment_id.clone()
                                ],
                            )
                            .await?;
                        erasures.next().await?.is_some()
                    }
                    _ => false,
                }
            } else if tables.contains("guild_memberships") {
                let mut rows = conn
                    .query(
                        "SELECT 1 FROM guild_memberships
                         WHERE guild_id = ? AND enrollment_id = ? LIMIT 1",
                        libsql::params![removed.guild_id, removed.enrollment_id.clone()],
                    )
                    .await?;
                rows.next().await?.is_some()
            } else {
                false
            };
            if tables.contains("guild_enrollments") {
                conn.execute(
                    "DELETE FROM guild_enrollments WHERE guild_id = ? AND id = ?",
                    libsql::params![removed.guild_id, removed.enrollment_id.clone()],
                )
                .await?;
            }
            for table in ENROLLMENT_TABLES {
                if tables.contains(*table) {
                    conn.execute(
                        &format!(
                            "DELETE FROM {} WHERE guild_id = ? AND enrollment_id = ?",
                            quote_identifier(table)
                        ),
                        libsql::params![removed.guild_id, removed.enrollment_id.clone()],
                    )
                    .await?;
                }
            }
            if directory_matches {
                for table in GUILD_TABLES {
                    if tables.contains(*table) {
                        conn.execute(
                            &format!("DELETE FROM {} WHERE guild_id = ?", quote_identifier(table)),
                            libsql::params![removed.guild_id],
                        )
                        .await?;
                    }
                }
            }
        }

        for removed in &request.removed_cases {
            for table in [
                "guild_member_case_index",
                "guild_bunny_evidence",
                "guild_case_evidence",
                "guild_case_history",
                "guild_case_sources",
                "guild_cases",
            ] {
                if tables.contains(table) {
                    let sql = if table == "guild_bunny_evidence" {
                        "DELETE FROM guild_bunny_evidence
                         WHERE guild_id = ? AND enrollment_id = ? AND evidence_id IN (
                           SELECT evidence_id FROM guild_case_evidence
                           WHERE guild_id = ? AND enrollment_id = ? AND case_id = ?
                         )"
                    } else {
                        match table {
                            "guild_member_case_index" => {
                                "DELETE FROM guild_member_case_index
                                 WHERE guild_id = ? AND enrollment_id = ? AND case_id = ?"
                            }
                            "guild_case_evidence" => {
                                "DELETE FROM guild_case_evidence
                                 WHERE guild_id = ? AND enrollment_id = ? AND case_id = ?"
                            }
                            "guild_case_history" => {
                                "DELETE FROM guild_case_history
                                 WHERE guild_id = ? AND enrollment_id = ? AND case_id = ?"
                            }
                            "guild_case_sources" => {
                                "DELETE FROM guild_case_sources
                                 WHERE guild_id = ? AND enrollment_id = ? AND case_id = ?"
                            }
                            _ => {
                                "DELETE FROM guild_cases
                                 WHERE guild_id = ? AND enrollment_id = ? AND case_id = ?"
                            }
                        }
                    };
                    if table == "guild_bunny_evidence" {
                        conn.execute(
                            sql,
                            libsql::params![
                                removed.guild_id,
                                removed.enrollment_id.clone(),
                                removed.guild_id,
                                removed.enrollment_id.clone(),
                                removed.case_id.clone()
                            ],
                        )
                        .await?;
                    } else {
                        conn.execute(
                            sql,
                            libsql::params![
                                removed.guild_id,
                                removed.enrollment_id.clone(),
                                removed.case_id.clone()
                            ],
                        )
                        .await?;
                    }
                }
            }
        }

        for removed in &request.removed_evidence {
            for table in ["guild_bunny_evidence", "guild_case_evidence"] {
                if tables.contains(table) {
                    conn.execute(
                        &format!(
                            "DELETE FROM {} WHERE guild_id = ? AND enrollment_id = ? AND evidence_id = ?",
                            quote_identifier(table)
                        ),
                        libsql::params![
                            removed.guild_id,
                            removed.enrollment_id.clone(),
                            removed.evidence_id.clone()
                        ],
                    )
                    .await?;
                }
            }
        }

        for (_, sql) in &triggers {
            conn.execute_batch(sql).await?;
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        let _ = conn.execute("ROLLBACK", ()).await;
        return Err(error);
    }
    conn.execute("COMMIT", ()).await?;
    Ok(())
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn encrypt_payload(
    src: &Path,
    dest: &Path,
    key: &[u8; 32],
    mut header: ArchiveHeader,
    password: Option<&str>,
) -> Result<()> {
    let salt = if header.salt_hex.is_empty() {
        random_salt()
    } else {
        decode_hex(&header.salt_hex)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid salt"))?
    };
    header.salt_hex = hex_encode(&salt);
    let content_key = if header.wrapping == "password" {
        derive_password_key(
            password.context("password wrapping requires a password")?,
            &salt,
        )?
    } else {
        derive_master_key(key, &salt)?
    };
    if dest.exists() {
        fs::remove_file(dest)?;
    }
    let mut out = private_file(dest)?;
    out.write_all(MAGIC)?;
    let header_bytes = serde_json::to_vec(&header)?;
    ensure!(header_bytes.len() <= 65_536, "header too large");
    out.write_all(&(header_bytes.len() as u32).to_le_bytes())?;
    out.write_all(&header_bytes)?;
    let aead = sealing_key(&content_key)?;
    let mut input = File::open(src)?;
    let mut buf = vec![0u8; CHUNK];
    let mut counter = 0u32;
    loop {
        let n = input.read(&mut buf)?;
        write_frame(&mut out, &aead, counter, &buf[..n])?;
        counter = counter.checked_add(1).context("archive frame overflow")?;
        if n == 0 {
            break;
        }
    }
    out.sync_all()?;
    Ok(())
}

fn decrypt_payload(
    src: &Path,
    dest: &Path,
    master: &[u8; 32],
    password: Option<&str>,
) -> Result<ArchiveHeader> {
    let mut input = File::open(src)?;
    let mut magic = vec![0u8; MAGIC.len()];
    input.read_exact(&mut magic)?;
    ensure!(magic == MAGIC, "unsupported archive format");
    let mut len_buf = [0u8; 4];
    input.read_exact(&mut len_buf)?;
    let header_len = u32::from_le_bytes(len_buf) as usize;
    ensure!(
        (8..=65_536).contains(&header_len),
        "header length out of bounds"
    );
    let mut header_bytes = vec![0u8; header_len];
    input.read_exact(&mut header_bytes)?;
    let header: ArchiveHeader = serde_json::from_slice(&header_bytes)?;
    ensure!(header.format == 1, "unsupported archive format");
    let salt = decode_hex(&header.salt_hex)?;
    let content_key = if header.wrapping == "password" {
        derive_password_key(password.context("export password is required")?, &salt)?
    } else {
        derive_master_key(master, &salt)?
    };
    decrypt_frames(&mut input, dest, &content_key)?;
    Ok(header)
}

fn decrypt_frames(input: &mut File, dest: &Path, key: &[u8; 32]) -> Result<()> {
    if dest.exists() {
        fs::remove_file(dest)?;
    }
    let mut out = private_file(dest)?;
    let aead = sealing_key(key)?;
    let mut counter = 0u32;
    loop {
        let mut len_buf = [0u8; 4];
        input.read_exact(&mut len_buf)?;
        let frame_len = u32::from_le_bytes(len_buf) as usize;
        ensure!(frame_len <= CHUNK + 16, "frame exceeds limit");
        let mut frame = vec![0u8; frame_len];
        input.read_exact(&mut frame)?;
        let nonce = frame_nonce(counter);
        aead.open_in_place(
            Nonce::assume_unique_for_key(nonce),
            Aad::empty(),
            &mut frame,
        )
        .map_err(|_| anyhow::anyhow!("archive authentication failed"))?;
        let plain_len = frame.len().saturating_sub(CHACHA20_POLY1305.tag_len());
        let plain = &frame[..plain_len];
        if plain.is_empty() {
            break;
        }
        out.write_all(plain)?;
        counter = counter.checked_add(1).context("archive frame overflow")?;
    }
    out.sync_all()?;
    Ok(())
}

fn write_frame(out: &mut File, key: &LessSafeKey, counter: u32, plain: &[u8]) -> Result<()> {
    let nonce = frame_nonce(counter);
    let mut buf = plain.to_vec();
    let tag = key
        .seal_in_place_separate_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut buf)
        .map_err(|_| anyhow::anyhow!("archive seal failed"))?;
    buf.extend_from_slice(tag.as_ref());
    out.write_all(&(buf.len() as u32).to_le_bytes())?;
    out.write_all(&buf)?;
    Ok(())
}

fn sealing_key(key: &[u8; 32]) -> Result<LessSafeKey> {
    let unbound = UnboundKey::new(&CHACHA20_POLY1305, key)
        .map_err(|_| anyhow::anyhow!("invalid archive key"))?;
    Ok(LessSafeKey::new(unbound))
}

fn frame_nonce(counter: u32) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[8..].copy_from_slice(&counter.to_be_bytes());
    nonce
}

fn derive_master_key(master: &[u8; 32], salt: &[u8]) -> Result<[u8; 32]> {
    let salt = hkdf::Salt::new(HKDF_SHA256, salt);
    let prk = salt.extract(master);
    let okm = prk
        .expand(&[b"jmodguard-backup-v1"], HKDF_SHA256)
        .map_err(|_| anyhow::anyhow!("key derivation failed"))?;
    let mut key = [0u8; 32];
    okm.fill(&mut key)
        .map_err(|_| anyhow::anyhow!("key derivation failed"))?;
    Ok(key)
}

fn derive_password_key(password: &str, salt: &[u8]) -> Result<[u8; 32]> {
    ensure!(!password.is_empty(), "empty export password");
    let mut key = [0u8; 32];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        std::num::NonZeroU32::new(PASSWORD_ITERS).expect("iters"),
        salt,
        password.as_bytes(),
        &mut key,
    );
    Ok(key)
}

fn unpack_payload(payload: &Path, dest: &Path, max_bytes: u64) -> Result<Vec<(String, PathBuf)>> {
    private_dir(dest, true)?;
    let mut input = File::open(payload)?;
    let mut out = Vec::new();
    let mut seen = 0u64;
    loop {
        let mut len_buf = [0u8; 4];
        let n = input.read(&mut len_buf)?;
        if n == 0 {
            break;
        }
        ensure!(n == 4, "truncated payload");
        let name_len = u32::from_le_bytes(len_buf) as usize;
        if name_len == 0 {
            break;
        }
        ensure!(
            name_len <= MAX_RECORD_NAME_BYTES,
            "payload record name is too long"
        );
        let mut name = vec![0u8; name_len];
        input.read_exact(&mut name)?;
        let name = String::from_utf8(name)?;
        ensure!(name == safe_name(&name), "invalid payload name");
        let mut size_buf = [0u8; 8];
        input.read_exact(&mut size_buf)?;
        let size = u64::from_le_bytes(size_buf);
        seen = seen.saturating_add(size);
        ensure!(seen <= max_bytes, "payload exceeds configured byte budget");
        let path = dest.join(safe_name(&name));
        let mut file = private_file(&path)?;
        let mut left = size;
        let mut buf = vec![0u8; CHUNK];
        while left > 0 {
            let n = left.min(CHUNK as u64) as usize;
            input.read_exact(&mut buf[..n])?;
            file.write_all(&buf[..n])?;
            left -= n as u64;
        }
        file.sync_all()?;
        out.push((name, path));
    }
    Ok(out)
}

fn append_file(writer: &mut File, name: &str, path: &Path) -> Result<()> {
    let mut file = File::open(path)?;
    let size = fs::metadata(path)?.len();
    append_named(writer, name, size, &mut file)
}

fn append_bytes(writer: &mut File, name: &str, bytes: &[u8]) -> Result<()> {
    append_named(
        writer,
        name,
        bytes.len() as u64,
        &mut std::io::Cursor::new(bytes),
    )
}

fn append_named<R: Read>(writer: &mut File, name: &str, size: u64, reader: &mut R) -> Result<()> {
    let name = safe_name(name);
    writer.write_all(&(name.len() as u32).to_le_bytes())?;
    writer.write_all(name.as_bytes())?;
    writer.write_all(&size.to_le_bytes())?;
    let mut left = size;
    let mut buf = vec![0u8; CHUNK];
    while left > 0 {
        let n = left.min(CHUNK as u64) as usize;
        reader.read_exact(&mut buf[..n])?;
        writer.write_all(&buf[..n])?;
        left -= n as u64;
    }
    Ok(())
}

fn copy_evidence(
    root: &Path,
    dest: &Path,
    intents: &CleanupIntents,
    max_bytes: u64,
) -> Result<Vec<(String, PathBuf)>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let root_meta = fs::symlink_metadata(root)?;
    ensure!(
        root_meta.is_dir() && !root_meta.file_type().is_symlink(),
        "evidence root must be a directory and not a symlink"
    );
    private_dir(dest, true)?;
    let mut total = 0u64;
    let mut copied = Vec::new();
    copy_evidence_tree(
        root,
        root,
        dest,
        intents,
        &mut total,
        max_bytes,
        &mut copied,
    )?;
    Ok(copied)
}

fn copy_evidence_tree(
    root: &Path,
    current: &Path,
    dest: &Path,
    intents: &CleanupIntents,
    total: &mut u64,
    max_bytes: u64,
    copied: &mut Vec<(String, PathBuf)>,
) -> Result<()> {
    let mut entries = fs::read_dir(current)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        ensure!(
            !meta.file_type().is_symlink(),
            "evidence root must not contain symlinks"
        );
        if meta.is_dir() {
            copy_evidence_tree(root, &path, dest, intents, total, max_bytes, copied)?;
            continue;
        }
        ensure!(meta.is_file(), "evidence object must be a regular file");
        let relative = path
            .strip_prefix(root)?
            .to_str()
            .context("evidence path must be utf-8")?
            .to_string();
        ensure_relative_path(&relative)?;
        if evidence_is_removed(
            &relative,
            &intents.guilds,
            &intents.cases,
            &intents.evidence,
            &intents.object_keys,
        )? {
            continue;
        }
        *total = total
            .checked_add(meta.len())
            .context("evidence size overflow")?;
        ensure!(
            *total <= max_bytes,
            "evidence exceeds configured byte budget"
        );
        let copied_path = dest.join(hex_encode(relative.as_bytes()));
        fs::copy(&path, &copied_path)?;
        copied.push((relative, copied_path));
    }
    Ok(())
}

fn evidence_record_name(relative: &str) -> String {
    format!(
        "{EVIDENCE_RECORD_PREFIX}{}",
        hex_encode(relative.as_bytes())
    )
}

fn evidence_relative_name(record: &str) -> Result<Option<String>> {
    if let Some(encoded) = record.strip_prefix(EVIDENCE_RECORD_PREFIX) {
        let relative = String::from_utf8(hex_decode(encoded)?)?;
        ensure_relative_path(&relative)?;
        return Ok(Some(relative));
    }
    let Some(relative) = record.strip_prefix("evidence-") else {
        return Ok(None);
    };
    ensure_relative_path(relative)?;
    Ok(Some(relative.to_string()))
}

fn evidence_is_removed(
    relative: &str,
    guilds: &BTreeSet<RemovedGuild>,
    cases: &BTreeSet<RemovedCase>,
    evidence: &BTreeSet<RemovedEvidence>,
    object_keys: &BTreeSet<String>,
) -> Result<bool> {
    ensure_relative_path(relative)?;
    let object_key = [".complete-tmp", ".complete", ".data"]
        .iter()
        .find_map(|suffix| relative.strip_suffix(suffix))
        .unwrap_or(relative);
    if object_keys.contains(relative) || object_keys.contains(object_key) {
        return Ok(true);
    }
    let parts = object_key.split('/').collect::<Vec<_>>();
    if parts.len() != 5 || parts[0] != "evidence" {
        return Ok(false);
    }
    let Ok(guild_id) = parts[1].parse::<i64>() else {
        return Ok(false);
    };
    Ok(guilds.contains(&RemovedGuild {
        guild_id,
        enrollment_id: parts[2].to_string(),
    }) || cases.contains(&RemovedCase {
        guild_id,
        enrollment_id: parts[2].to_string(),
        case_id: parts[3].to_string(),
    }) || evidence.contains(&RemovedEvidence {
        guild_id,
        enrollment_id: parts[2].to_string(),
        evidence_id: parts[4].to_string(),
    }))
}

fn ensure_relative_path(path: &str) -> Result<()> {
    ensure!(!path.is_empty(), "empty evidence path");
    let normalized = Path::new(path)
        .components()
        .map(|component| match component {
            Component::Normal(value) => value.to_str().context("evidence path must be utf-8"),
            _ => anyhow::bail!("invalid evidence path"),
        })
        .collect::<Result<Vec<_>>>()?
        .join("/");
    ensure!(normalized == path, "invalid evidence path");
    Ok(())
}

fn ensure_archive_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
        "invalid archive id"
    );
    Ok(())
}

fn restore_staging_path(target: &Path) -> Result<PathBuf> {
    let parent = target.parent().context("restore target has no parent")?;
    let name = target
        .file_name()
        .and_then(|value| value.to_str())
        .context("restore target must have a utf-8 name")?;
    Ok(parent.join(format!(".{name}.jmodguard-restore-staging")))
}

const RESTORE_COMPLETE_MARK: &[u8] = b"jmodguard restore verified\n";

fn restore_stage_marker_valid(marker: &Path) -> Result<()> {
    let contents = fs::read_to_string(marker)?;
    ensure!(
        contents == RESTORE_STAGE_MARK,
        "restore staging marker mismatch"
    );
    Ok(())
}

/// Reconcile leftover restore staging before a new attempt.
///
/// Returns `true` when a verified tree was promoted to `restore_target` and the
/// caller can treat the restore as complete.
fn recover_restore_stage(stage: &Path, restore_target: &Path) -> Result<bool> {
    if !stage.exists() {
        return Ok(false);
    }
    let meta = fs::symlink_metadata(stage)?;
    ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "restore staging path is not an owned directory"
    );
    let marker = stage.join(".jmodguard-restore-staging");
    if marker.exists() {
        restore_stage_marker_valid(&marker)?;
        let complete = stage.join("COMPLETE");
        if complete.exists() {
            ensure!(
                !restore_target.exists(),
                "restore target must be an absent directory"
            );
            fs::rename(stage, restore_target)?;
            File::open(
                restore_target
                    .parent()
                    .context("restore target has no parent")?,
            )?
            .sync_all()?;
            return Ok(true);
        }
    } else {
        ensure!(
            fs::read_dir(stage)?.next().is_none(),
            "restore staging marker is missing"
        );
    }
    fs::remove_dir_all(stage)?;
    Ok(false)
}

fn archive_file_has_magic(path: &Path) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let mut file = File::open(path)?;
    let mut prefix = vec![0u8; MAGIC.len()];
    let read = file.read(&mut prefix)?;
    Ok(read == MAGIC.len() && prefix == MAGIC)
}

async fn adopt_staging_catalog_archive(
    zone: &BackupZone,
    store: &ControlStore,
    row: &ArchiveRow,
) -> Result<()> {
    let staging = zone.staging_path(&row.id);
    if !archive_file_has_magic(&staging)? {
        return Ok(());
    }
    promote_staging_archive(zone, &row.id)?;
    let complete = zone.complete_path(&row.id);
    let bytes = fs::metadata(&complete)?.len();
    let sha = sha256_file(&complete)?;
    let captured_ms = row.captured_ms.unwrap_or(row.created_ms);
    store
        .update_archive(&ArchiveRow {
            id: row.id.clone(),
            kind: row.kind.clone(),
            status: "complete".into(),
            created_ms: row.created_ms,
            captured_ms: Some(captured_ms),
            scope_json: row.scope_json.clone(),
            object_key: Some(format!("complete/{}.jmb", row.id)),
            sha256: Some(sha),
            bytes: Some(i64::try_from(bytes).unwrap_or(i64::MAX)),
            family: row.family.clone(),
            last_error: None,
        })
        .await?;
    Ok(())
}

fn hex_decode(value: &str) -> Result<Vec<u8>> {
    ensure!(
        value.len().is_multiple_of(2),
        "invalid evidence path encoding"
    );
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let text = std::str::from_utf8(pair)?;
            u8::from_str_radix(text, 16).context("invalid evidence path encoding")
        })
        .collect()
}

fn ensure_dedicated_root(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "backup root must be absolute");
    if !path.exists() {
        private_dir(path, true)?;
        fs::write(path.join(".jmodguard-backup-zone"), ZONE_MARK)?;
        return Ok(());
    }
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "backup root must be a directory"
    );
    let mark = path.join(".jmodguard-backup-zone");
    if mark.exists() {
        ensure!(
            fs::read_to_string(&mark)? == ZONE_MARK,
            "backup zone marker mismatch"
        );
        return Ok(());
    }
    let empty = fs::read_dir(path)?.next().is_none();
    ensure!(
        empty,
        "backup root must be empty or a dedicated backup zone"
    );
    fs::write(mark, ZONE_MARK)?;
    Ok(())
}

fn private_dir(path: &Path, create: bool) -> Result<()> {
    if create && !path.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    Ok(())
}

fn private_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        private_dir(parent, true)?;
    }
    Ok(OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?)
}

fn sql_literal_path(path: &Path) -> Result<String> {
    let text = path.to_str().context("sqlite path must be utf-8")?;
    ensure!(!text.contains('\''), "sqlite path must not contain quotes");
    Ok(text.to_string())
}

fn safe_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == ':' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; CHUNK];
    let mut ctx = ring::digest::Context::new(&SHA256);
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
    }
    Ok(hex_encode(ctx.finish().as_ref()))
}

fn random_salt() -> [u8; 16] {
    let mut salt = [0u8; 16];
    rand::rng().fill_bytes(&mut salt);
    salt
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn decode_hex(hex: &str) -> Result<Vec<u8>> {
    ensure!(hex.len().is_multiple_of(2), "invalid hex");
    hex.as_bytes()
        .chunks(2)
        .map(|chunk| Ok(u8::from_str_radix(std::str::from_utf8(chunk)?, 16)?))
        .collect()
}

pub fn overdue(settings: &ControlSettings, last_complete_ms: Option<i64>) -> bool {
    let Some(last) = last_complete_ms else {
        return settings.backup_enabled;
    };
    let interval =
        i64::try_from(settings.backup_interval_hours.saturating_mul(3_600_000)).unwrap_or(i64::MAX);
    now_ms().saturating_sub(last) > interval
}

#[cfg(test)]
mod restore_job_result_tests {
    use super::restore_job_result;

    #[test]
    fn verified_restore_succeeds_when_fence_clear_fails() {
        assert!(restore_job_result(Ok(()), Err(anyhow::anyhow!("meta locked"))).is_ok());
    }

    #[test]
    fn restore_failure_is_preserved_when_fence_clears() {
        let error = restore_job_result(Err(anyhow::anyhow!("corrupt archive")), Ok(()))
            .expect_err("restore failure must surface");
        assert!(error.to_string().contains("corrupt archive"));
    }
}

#[cfg(test)]
mod restore_stage_tests {
    use super::{
        RESTORE_COMPLETE_MARK, RESTORE_STAGE_MARK, recover_restore_stage, restore_staging_path,
    };
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("jmodguard-{name}-{nanos}"))
    }

    #[test]
    fn invalid_marker_leaves_staging_tree_in_place() {
        let root = scratch_dir("invalid-marker");
        let target = root.join("restore-target");
        let stage = restore_staging_path(&target).unwrap();
        fs::create_dir_all(&stage).unwrap();
        fs::write(stage.join(".jmodguard-restore-staging"), b"").unwrap();
        fs::write(stage.join("partial"), b"data").unwrap();
        let error = recover_restore_stage(&stage, &target).expect_err("empty marker must fail");
        assert!(error.to_string().contains("marker mismatch"));
        assert!(stage.exists());
        assert!(stage.join("partial").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn verified_staging_promotes_to_restore_target() {
        let root = scratch_dir("verified-promote");
        let target = root.join("restore-target");
        let stage = restore_staging_path(&target).unwrap();
        fs::create_dir_all(&stage).unwrap();
        fs::write(stage.join(".jmodguard-restore-staging"), RESTORE_STAGE_MARK).unwrap();
        fs::write(stage.join("COMPLETE"), RESTORE_COMPLETE_MARK).unwrap();
        assert!(recover_restore_stage(&stage, &target).unwrap());
        assert!(target.join("COMPLETE").exists());
        assert!(!stage.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn exact_marker_clears_partial_staging() {
        let root = scratch_dir("partial-clear");
        let target = root.join("restore-target");
        let stage = restore_staging_path(&target).unwrap();
        fs::create_dir_all(&stage).unwrap();
        fs::write(stage.join(".jmodguard-restore-staging"), RESTORE_STAGE_MARK).unwrap();
        fs::write(stage.join("partial"), b"data").unwrap();
        assert!(!recover_restore_stage(&stage, &target).unwrap());
        assert!(!stage.exists());
        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod member_snapshot_tests {
    use super::snapshot_sqlite;

    #[tokio::test]
    async fn snapshot_v10_seed_update_does_not_reference_gap_generation() {
        let root = std::env::temp_dir().join(format!(
            "jmodguard-member-snapshot-v10-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("guild.db");
        let snapshot = root.join("snapshot.db");
        let db = libsql::Builder::new_local(&source).build().await.unwrap();
        let conn = db.connect().unwrap();
        conn.execute_batch(
            "CREATE TABLE guild_member_seed(guild_id INTEGER PRIMARY KEY,enrollment_id TEXT,bot_id INTEGER,initial_completed_at_ms INTEGER,reconciliation_needed INTEGER,gap_at_ms INTEGER,directory_revision INTEGER);
            INSERT INTO guild_member_seed VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',42,2000,0,NULL,7);",
        )
        .await
        .unwrap();
        conn.execute("PRAGMA user_version = 10", ()).await.unwrap();
        snapshot_sqlite(&source, &snapshot).await.unwrap();
        let db = libsql::Builder::new_local(&snapshot).build().await.unwrap();
        let conn = db.connect().unwrap();
        let row = conn
            .query(
                "SELECT reconciliation_needed,directory_revision FROM guild_member_seed WHERE guild_id=10",
                (),
            )
            .await
            .unwrap()
            .next()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.get::<i64>(0).unwrap(), 1);
        assert_eq!(row.get::<i64>(1).unwrap(), 8);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn snapshot_keeps_unknown_time_observations_and_queues_audit_recovery() {
        let root = std::env::temp_dir().join(format!(
            "jmodguard-member-snapshot-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("guild.db");
        let snapshot = root.join("snapshot.db");
        let db = libsql::Builder::new_local(&source).build().await.unwrap();
        let conn = db.connect().unwrap();
        conn.execute_batch("CREATE TABLE guild_member_seed(guild_id INTEGER PRIMARY KEY,enrollment_id TEXT,bot_id INTEGER,initial_completed_at_ms INTEGER,reconciliation_needed INTEGER,gap_at_ms INTEGER,gap_generation INTEGER,directory_revision INTEGER);
            INSERT INTO guild_member_seed VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',42,2000,0,NULL,0,7);
            CREATE TABLE guild_member_sync(guild_id INTEGER PRIMARY KEY,lease_owner TEXT,lease_until_ms INTEGER,generation INTEGER,status TEXT,status_reason TEXT);
            INSERT INTO guild_member_sync VALUES(10,'worker',900000,1,'running',NULL);
            CREATE TABLE guild_member_audit_recovery(guild_id INTEGER PRIMARY KEY,enrollment_id TEXT,bot_id INTEGER,run_id TEXT,started_at_ms INTEGER,status TEXT,processed INTEGER,next_attempt_at_ms INTEGER,lease_owner TEXT,lease_until_ms INTEGER,generation INTEGER,status_reason TEXT,cursor TEXT,request_after_ms INTEGER,restart_requested_at_ms INTEGER);
            INSERT INTO guild_member_audit_recovery VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',42,'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',1000,'complete',2,NULL,NULL,NULL,0,NULL,NULL,NULL,NULL);
            CREATE TABLE guild_member_state_observations(guild_id INTEGER,enrollment_id TEXT,observation_id TEXT,user_id INTEGER,kind TEXT,observed_at_ms INTEGER);
            INSERT INTO guild_member_state_observations VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','cccccccccccccccccccccccccccccccc',77,'ban_absent',1500);
            CREATE TABLE guild_member_target_checks(guild_id INTEGER,enrollment_id TEXT,user_id INTEGER,kind TEXT,reason TEXT,row_revision INTEGER,requested_at_ms INTEGER,next_attempt_at_ms INTEGER);
            INSERT INTO guild_member_target_checks VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',77,'member','gap',1,1600,1700);
            CREATE TABLE guild_member_unresolved_audits(guild_id INTEGER,enrollment_id TEXT,entry_id INTEGER,user_id INTEGER,action TEXT,occurred_at_ms INTEGER,executor_id INTEGER,reason TEXT);
            INSERT INTO guild_member_unresolved_audits VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',9007199254741993,77,'ban',1550,42,'historical own action');").await.unwrap();
        conn.execute("PRAGMA user_version = 13", ()).await.unwrap();
        snapshot_sqlite(&source, &snapshot).await.unwrap();
        let db = libsql::Builder::new_local(&snapshot).build().await.unwrap();
        let conn = db.connect().unwrap();
        let row = conn
            .query(
                "SELECT reconciliation_needed,gap_at_ms,gap_generation,directory_revision FROM guild_member_seed WHERE guild_id=10",
                (),
            )
            .await
            .unwrap()
            .next()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.get::<i64>(0).unwrap(), 1);
        assert!(row.get::<i64>(1).unwrap() > 0);
        assert_eq!(row.get::<i64>(2).unwrap(), 1);
        assert_eq!(row.get::<i64>(3).unwrap(), 8);
        let row = conn
            .query(
                "SELECT status,lease_owner FROM guild_member_sync WHERE guild_id=10",
                (),
            )
            .await
            .unwrap()
            .next()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.get::<String>(0).unwrap(), "waiting");
        assert!(row.get::<Option<String>>(1).unwrap().is_none());
        let row = conn
            .query(
                "SELECT status,cursor FROM guild_member_audit_recovery WHERE guild_id=10",
                (),
            )
            .await
            .unwrap()
            .next()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.get::<String>(0).unwrap(), "queued");
        assert!(row.get::<Option<String>>(1).unwrap().is_none());
        for table in [
            "guild_member_state_observations",
            "guild_member_target_checks",
            "guild_member_unresolved_audits",
        ] {
            let row = conn
                .query(
                    &format!("SELECT COUNT(*) FROM {table} WHERE guild_id=10"),
                    (),
                )
                .await
                .unwrap()
                .next()
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.get::<i64>(0).unwrap(), 1);
        }
        let unresolved = conn
            .query(
                "SELECT entry_id,reason FROM guild_member_unresolved_audits WHERE guild_id=10",
                (),
            )
            .await
            .unwrap()
            .next()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(unresolved.get::<i64>(0).unwrap(), 9007199254741993);
        assert_eq!(
            unresolved.get::<String>(1).unwrap(),
            "historical own action"
        );
        drop(conn);
        drop(db);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod transient_snapshot_tests {
    use super::{scrub_transient_snapshots, snapshot_sqlite};

    #[tokio::test]
    async fn backup_copy_and_older_restore_copy_omit_transient_rows() {
        let root = std::env::temp_dir().join(format!(
            "jmodguard-snapshot-backup-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let live = root.join("live.sqlite");
        let copy = root.join("copy.sqlite");
        let older = root.join("older.sqlite");
        let db = libsql::Builder::new_local(&live).build().await.unwrap();
        let conn = db.connect().unwrap();
        conn.execute_batch("CREATE TABLE guild_message_snapshots(id INTEGER PRIMARY KEY, content TEXT); INSERT INTO guild_message_snapshots VALUES(1,'private'); CREATE TABLE guild_snapshot_health(singleton INTEGER PRIMARY KEY, state TEXT NOT NULL DEFAULT 'unknown'); INSERT INTO guild_snapshot_health VALUES(1,'active');").await.unwrap();
        drop(conn);
        drop(db);
        snapshot_sqlite(&live, &copy).await.unwrap();
        let db = libsql::Builder::new_local(&live).build().await.unwrap();
        let conn = db.connect().unwrap();
        conn.execute(
            &format!("VACUUM INTO '{}'", super::sql_literal_path(&older).unwrap()),
            (),
        )
        .await
        .unwrap();
        drop(conn);
        drop(db);
        scrub_transient_snapshots(&older).await.unwrap();
        for path in [&copy, &older] {
            let db = libsql::Builder::new_local(path).build().await.unwrap();
            let conn = db.connect().unwrap();
            let mut rows = conn
                .query("SELECT COUNT(*) FROM guild_message_snapshots", ())
                .await
                .unwrap();
            assert_eq!(
                rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
                0
            );
            let mut rows = conn
                .query(
                    "SELECT state FROM guild_snapshot_health WHERE singleton=1",
                    (),
                )
                .await
                .unwrap();
            assert_eq!(
                rows.next()
                    .await
                    .unwrap()
                    .unwrap()
                    .get::<String>(0)
                    .unwrap(),
                "unknown"
            );
        }
        for path in [&copy, &older] {
            assert!(
                !std::fs::read(path)
                    .unwrap()
                    .windows(b"private".len())
                    .any(|window| window == b"private"),
                "archive copy still contains transient payload bytes"
            );
        }
        let db = libsql::Builder::new_local(&live).build().await.unwrap();
        let conn = db.connect().unwrap();
        let mut rows = conn
            .query("SELECT COUNT(*) FROM guild_message_snapshots", ())
            .await
            .unwrap();
        assert_eq!(
            rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
            1
        );
        drop(rows);
        drop(conn);
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }
}
