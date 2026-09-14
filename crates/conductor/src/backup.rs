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
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::settings::ControlSettings;
use crate::store::{ArchiveRow, ControlStore, now_ms, random_token};

pub const MAGIC: &[u8] = b"JMODGUARD-BACKUP-1\n";
const ZONE_MARK: &str = "jmodguard-backup-1\n";
const CHUNK: usize = 65_536;
const PASSWORD_ITERS: u32 = 100_000;
const EXCLUDED_TABLES: &[&str] = &["bot_actions", "mutation_receipts", "nickname_reconcile"];

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
    pub removed_guilds: BTreeSet<i64>,
    pub removed_object_keys: BTreeSet<String>,
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
        let snapshot = scratch.join(format!("{}.sqlite", safe_name(&capture.role)));
        snapshot_sqlite(&capture.path, &snapshot).await?;
        append_file(
            &mut writer,
            &format!("store-{}", safe_name(&capture.role)),
            &snapshot,
        )?;
        roles.push(capture.role.clone());
    }
    let mut evidence_names = Vec::new();
    if let Some(root) = &cfg.evidence_root {
        let skipped = live_removed_keys(cfg).await?;
        copy_evidence(
            root,
            &scratch.join("evidence"),
            &skipped,
            settings.backup_max_archive_bytes,
        )?;
        if scratch.join("evidence").exists() {
            for entry in fs::read_dir(scratch.join("evidence"))? {
                let path = entry?.path();
                if path.is_file() {
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .context("evidence name")?;
                    append_file(&mut writer, &format!("evidence-{name}"), &path)?;
                    evidence_names.push(name.to_string());
                }
            }
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
    let complete = zone.complete_path(id);
    fs::rename(&dest, &complete)?;
    File::open(complete.parent().context("complete dir")?)?.sync_all()?;
    zone.cleanup_scratch(id);
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

pub fn ingest_upload(zone: &BackupZone, bytes: &[u8]) -> Result<String> {
    ensure!(
        bytes.len() >= MAGIC.len() && bytes.starts_with(MAGIC),
        "unsupported archive format"
    );
    let id = random_token();
    let path = zone.quarantine_path(&id);
    let mut file = private_file(&path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(id)
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
    let restore_id = format!("restore-{}", random_token());
    let scratch = zone.scratch(&restore_id);
    private_dir(&scratch, true)?;
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
    private_dir(&target, true)?;
    let result = restore_files(&target, files, &request).await;
    if result.is_err() {
        let _ = fs::remove_dir_all(&target);
        zone.cleanup_scratch(&restore_id);
        return result.map(|_| target);
    }
    File::create(target.join("COMPLETE"))?.write_all(b"jmodguard restore verified\n")?;
    File::open(&target)?.sync_all()?;
    zone.cleanup_scratch(&restore_id);
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
        } else if let Some(file) = name.strip_prefix("evidence-") {
            if request.removed_object_keys.contains(file) {
                continue;
            }
            let dest_dir = target.join("evidence");
            private_dir(&dest_dir, true)?;
            fs::copy(path, dest_dir.join(file))?;
        } else if name == "control.json" {
            fs::copy(path, target.join("control.json"))?;
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
    if request.removed_guilds.is_empty() && request.removed_object_keys.is_empty() {
        return Ok(0);
    }
    let mut rewritten = 0usize;
    for row in store
        .list_archives(200)
        .await?
        .into_iter()
        .filter(|row| row.status == "complete")
    {
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
                append_file(&mut writer, &name, &path)?;
            } else if let Some(file) = name.strip_prefix("evidence-") {
                if request.removed_object_keys.contains(file) {
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
        encrypt_payload(
            &rebuilt,
            &zone.staging_path(&new_id),
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
        fs::rename(zone.staging_path(&new_id), zone.complete_path(&new_id))?;
        let dest = zone.complete_path(&new_id);
        let bytes = i64::try_from(fs::metadata(&dest)?.len()).unwrap_or(i64::MAX);
        store
            .insert_archive(&ArchiveRow {
                id: new_id.clone(),
                kind: row.kind.clone(),
                status: "complete".into(),
                created_ms: now_ms(),
                captured_ms: row.captured_ms,
                scope_json: serde_json::json!({ "sanitized_from": row.id }).to_string(),
                object_key: Some(format!("complete/{new_id}.jmb")),
                sha256: Some(sha256_file(&dest)?),
                bytes: Some(bytes),
                family: row.family.clone(),
                last_error: None,
            })
            .await?;
        let _ = fs::remove_file(zone.complete_path(&row.id));
        let mut old = row;
        old.status = "replaced".into();
        store.update_archive(&old).await?;
        zone.cleanup_scratch(&format!("sanitize-{}", old.id));
        rewritten += 1;
    }
    Ok(rewritten)
}

pub async fn load_cleanup_intents(
    cfg: &Config,
) -> Result<(BTreeSet<i64>, BTreeSet<String>, Vec<String>)> {
    let mut guilds = BTreeSet::new();
    let mut keys = BTreeSet::new();
    let mut ids = Vec::new();
    let Some(core) = cfg.capture_stores.iter().find(|s| s.role == "core") else {
        return Ok((guilds, keys, ids));
    };
    if !core.path.exists() {
        return Ok((guilds, keys, ids));
    }
    let db = libsql::Builder::new_local(&core.path).build().await?;
    let conn = db.connect()?;
    let mut rows = match conn
        .query(
            "SELECT intent_id, guild_id, object_key FROM backup_cleanup_intents",
            (),
        )
        .await
    {
        Ok(rows) => rows,
        Err(_) => return Ok((guilds, keys, ids)),
    };
    while let Some(row) = rows.next().await? {
        ids.push(row.get::<String>(0).unwrap_or_default());
        if let Ok(guild) = row.get::<i64>(1)
            && guild > 0
        {
            guilds.insert(guild);
        }
        if let Ok(key) = row.get::<String>(2) {
            keys.insert(key);
        }
    }
    Ok((guilds, keys, ids))
}

pub async fn clear_cleanup_intents(cfg: &Config, ids: &[String]) -> Result<()> {
    let Some(core) = cfg.capture_stores.iter().find(|s| s.role == "core") else {
        return Ok(());
    };
    let db = libsql::Builder::new_local(&core.path).build().await?;
    let conn = db.connect()?;
    for id in ids {
        let _ = conn
            .execute(
                "DELETE FROM backup_cleanup_intents WHERE intent_id = ?",
                libsql::params![id.clone()],
            )
            .await;
    }
    Ok(())
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

async fn live_removed_keys(cfg: &Config) -> Result<BTreeSet<String>> {
    Ok(load_cleanup_intents(cfg).await?.1)
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
    for table in EXCLUDED_TABLES {
        let _ = snap_conn
            .execute(&format!("DROP TABLE IF EXISTS {table}"), ())
            .await;
    }
    Ok(())
}

async fn apply_removals(path: &Path, request: &RestoreRequest) -> Result<()> {
    if request.removed_guilds.is_empty() {
        return Ok(());
    }
    let db = libsql::Builder::new_local(path).build().await?;
    let conn = db.connect()?;
    for guild in &request.removed_guilds {
        for sql in [
            "DELETE FROM guild_settings WHERE guild_id = ?",
            "DELETE FROM events WHERE guild_id = ?",
            "DELETE FROM warns WHERE guild_id = ?",
            "DELETE FROM bans WHERE guild_id = ?",
            "DELETE FROM log_channels WHERE guild_id = ?",
            "DELETE FROM badwords WHERE guild_id = ?",
            "DELETE FROM rules WHERE guild_id = ?",
            "DELETE FROM commands WHERE guild_id = ?",
            "DELETE FROM templates WHERE guild_id = ?",
            "DELETE FROM guild_enrollments WHERE guild_id = ?",
            "DELETE FROM guild_store_placements WHERE guild_id = ?",
            "DELETE FROM guild_erasure_jobs WHERE guild_id = ?",
            "DELETE FROM guild_cases WHERE guild_id = ?",
            "DELETE FROM guild_memberships WHERE guild_id = ?",
        ] {
            let _ = conn.execute(sql, libsql::params![*guild]).await;
        }
    }
    Ok(())
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
        ensure!(name_len <= 256, "payload name too long");
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
    skipped: &BTreeSet<String>,
    max_bytes: u64,
) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    private_dir(dest, true)?;
    let mut total = 0u64;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let meta = fs::symlink_metadata(entry.path())?;
        ensure!(
            !meta.file_type().is_symlink(),
            "evidence root must not contain symlinks"
        );
        if meta.is_dir() {
            continue;
        }
        ensure!(meta.is_file(), "evidence object must be a regular file");
        let name = entry.file_name().to_string_lossy().into_owned();
        if skipped.contains(&name) {
            continue;
        }
        total = total.saturating_add(meta.len());
        ensure!(
            total <= max_bytes,
            "evidence exceeds configured byte budget"
        );
        fs::copy(entry.path(), dest.join(safe_name(&name)))?;
    }
    Ok(())
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
