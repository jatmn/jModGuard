//! Signed Stable/Nightly discovery. Apply/install is owner-selected when an update root is configured.
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::config::parse_http_url;
use crate::state::Conductor;
use crate::store::{ControlStore, now_ms};

pub const FORMAT: &str = "JMODGUARD-RELEASE-1";
pub const INDEX_FORMAT: &str = "JMODGUARD-RELEASE-INDEX-1";
const MAX_INDEX_BYTES: usize = 256 * 1024;
const MAX_MANIFEST_BYTES: usize = 128 * 1024;
const MAX_RELEASES: usize = 64;
const REQUIRED_COMPONENTS: [&str; 4] = ["bot", "api", "conductor", "frontend"];
const INDEX_RETAIN: usize = 32;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredRelease {
    pub release_id: String,
    pub channel: String,
    pub version: String,
    pub source_commit: String,
    pub published_at: String,
    pub notes: String,
    pub compatible: bool,
    pub blocked_reason: Option<String>,
    pub components: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct UnsignedManifest {
    format: String,
    release_id: String,
    channel: String,
    version: String,
    source_commit: String,
    published_at: String,
    components: Value,
    schema: Schema,
    settings_schema: i64,
    backup_format: String,
    compatibility: Value,
    #[serde(default)]
    notes: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct Schema {
    core_family: String,
    core_version: i64,
    guild_family: String,
    guild_version: i64,
}

/// Guild-store schema version shared with the data crate at build time.
pub const GUILD_SCHEMA_VERSION: i64 = include!("../../../migrations/guild/current_version.rs");

pub fn discovery_available(cfg: &Config) -> bool {
    cfg.operations_configured() && cfg.release_feed.is_some() && !cfg.release_keys.is_empty()
}

pub fn installed(cfg: &Config) -> Value {
    serde_json::json!({
        "release_id": cfg.installed_release.clone().unwrap_or_else(|| format!("v{}", env!("CARGO_PKG_VERSION"))),
        "version": env!("CARGO_PKG_VERSION"),
        "core_family": "jmodguard-core-1",
        "core_version": 12,
        "guild_family": "jmodguard-guild-1",
        "guild_version": GUILD_SCHEMA_VERSION,
        "backup_format": "JMODGUARD-BACKUP-1",
        "settings_schema": 1,
    })
}

pub fn verify_manifest(raw: &str, keys: &BTreeMap<String, String>) -> Result<Value> {
    let value: Value = serde_json::from_str(raw).context("release manifest is not JSON")?;
    let signature = value
        .get("signature")
        .cloned()
        .context("release manifest is unsigned")?;
    let alg = signature
        .get("alg")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let key_id = signature
        .get("key_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mac = signature
        .get("mac")
        .and_then(Value::as_str)
        .unwrap_or_default();
    ensure!(alg == "hmac-sha256", "unsupported release signature");
    let secret = keys
        .get(key_id)
        .context("manifest signed with an unknown or retired key")?;
    let mut body = value.clone();
    if let Some(obj) = body.as_object_mut() {
        obj.remove("signature");
    }
    let canonical = canonical_json(&body);
    let expected = hex_hmac(secret, &canonical)?;
    ensure!(expected == mac, "release manifest signature mismatch");
    let parsed: UnsignedManifest = serde_json::from_value(body.clone())?;
    ensure!(parsed.format == FORMAT, "unsupported release format");
    ensure!(
        parsed.channel == "stable" || parsed.channel == "nightly",
        "unknown release channel"
    );
    ensure!(
        parsed.source_commit.len() == 40
            && parsed
                .source_commit
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "source commit must be a full lowercase SHA"
    );
    validate_publication_identity(&parsed.channel, &parsed.release_id)?;
    validate_components(&parsed.components)?;
    Ok(body)
}

fn stable_release_id(release_id: &str) -> bool {
    let Some(rest) = release_id.strip_prefix('v') else {
        return false;
    };
    let parts: Vec<&str> = rest.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

fn nightly_release_id(release_id: &str) -> bool {
    let Some(rest) = release_id.strip_prefix("nightly-") else {
        return false;
    };
    let Some((day, suffix)) = rest.split_once('-') else {
        return false;
    };
    day.len() == 8
        && day.bytes().all(|b| b.is_ascii_digit())
        && suffix.len() == 12
        && suffix
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn validate_publication_identity(channel: &str, release_id: &str) -> Result<()> {
    match channel {
        "stable" => ensure!(
            stable_release_id(release_id),
            "stable release id must be a vMAJOR.MINOR.PATCH tag"
        ),
        "nightly" => ensure!(
            nightly_release_id(release_id),
            "nightly release id must be nightly-YYYYMMDD-<sha12>"
        ),
        _ => ensure!(false, "unknown release channel"),
    }
    Ok(())
}

fn is_sha256_digest(value: &str) -> bool {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return false;
    };
    hex.len() == 64
        && hex
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn component_size(body: &serde_json::Map<String, Value>, name: &str) -> Result<i64> {
    let size = body
        .get("size")
        .context(format!("{name} size is required"))?;
    match size {
        Value::Number(number) => {
            let value = number.as_i64().filter(|value| *value > 0).or_else(|| {
                number
                    .as_u64()
                    .and_then(|value| i64::try_from(value).ok())
                    .filter(|value| *value > 0)
            });
            ensure!(value.is_some(), "{name} size must be a positive integer");
            Ok(value.expect("positive size"))
        }
        _ => anyhow::bail!("{name} size must be a positive integer"),
    }
}

fn validate_components(components: &Value) -> Result<()> {
    let Some(map) = components.as_object() else {
        anyhow::bail!("incomplete publication remains unavailable");
    };
    for name in map.keys() {
        ensure!(
            REQUIRED_COMPONENTS.contains(&name.as_str()),
            "unexpected component {name}"
        );
    }
    for name in REQUIRED_COMPONENTS {
        let Some(body) = map.get(name) else {
            anyhow::bail!("incomplete publication remains unavailable");
        };
        let Some(obj) = body.as_object() else {
            anyhow::bail!("incomplete publication remains unavailable");
        };
        if name == "frontend" {
            let files = obj.get("files").and_then(Value::as_object);
            ensure!(
                files.is_some_and(|files| {
                    files.contains_key("index.html")
                        && files.contains_key("app.js")
                        && files.contains_key("style.css")
                        && files.len() == 3
                }),
                "frontend inventory is incomplete"
            );
            ensure!(
                obj.get("sha256")
                    .and_then(Value::as_str)
                    .is_some_and(is_sha256_digest),
                "frontend sha256 is required"
            );
        } else {
            let digest = obj
                .get("digest")
                .and_then(Value::as_str)
                .context(format!("{name} digest is required"))?;
            let size = component_size(obj, name)?;
            let image = obj.get("image").and_then(Value::as_str);
            image_component(name, digest, size, image)?;
        }
    }
    Ok(())
}

fn row_from_manifest(
    manifest: &Value,
    compatible: bool,
    blocked: Option<String>,
) -> DiscoveredRelease {
    DiscoveredRelease {
        release_id: manifest
            .get("release_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        channel: manifest
            .get("channel")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        version: manifest
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        source_commit: manifest
            .get("source_commit")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        published_at: manifest
            .get("published_at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        notes: manifest
            .get("notes")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        compatible,
        blocked_reason: blocked,
        components: ["bot", "api", "conductor", "frontend"]
            .into_iter()
            .map(str::to_string)
            .collect(),
    }
}

/// Merge a poll snapshot with the prior catalog: index-listed releases stay visible
/// when a transient fetch/verify fails; releases absent from the index are dropped.
fn merge_discovered_catalog(
    previous: &[DiscoveredRelease],
    listed: &[Value],
    verified: BTreeMap<String, DiscoveredRelease>,
    retain_from_previous: &BTreeSet<String>,
) -> Vec<DiscoveredRelease> {
    let previous_by_id: BTreeMap<_, _> = previous
        .iter()
        .map(|row| (row.release_id.clone(), row.clone()))
        .collect();
    let mut listed_ids = BTreeSet::new();
    for item in listed {
        if let Some(id) = item.get("release_id").and_then(Value::as_str)
            && !id.is_empty()
        {
            listed_ids.insert(id.to_string());
        }
    }
    listed_ids.extend(verified.keys().cloned());
    listed_ids.extend(retain_from_previous.iter().cloned());

    let mut merged = BTreeMap::new();
    for id in &listed_ids {
        if let Some(row) = verified.get(id) {
            merged.insert(id.clone(), row.clone());
        } else if retain_from_previous.contains(id)
            && let Some(row) = previous_by_id.get(id)
        {
            merged.insert(id.clone(), row.clone());
        }
    }
    let mut out: Vec<_> = merged.into_values().collect();
    out.sort_by(|a, b| b.published_at.cmp(&a.published_at));
    out
}

pub fn sign_manifest(mut body: Value, key_id: &str, secret_hex: &str) -> Result<String> {
    if let Some(obj) = body.as_object_mut() {
        obj.remove("signature");
    }
    let mac = hex_hmac(secret_hex, &canonical_json(&body))?;
    if let Some(obj) = body.as_object_mut() {
        obj.insert(
            "signature".into(),
            serde_json::json!({
                "alg": "hmac-sha256",
                "key_id": key_id,
                "mac": mac,
            }),
        );
    }
    Ok(String::from_utf8(canonical_json(&body))?)
}

pub fn channel_for(mode: &str, tag: &str) -> Option<&'static str> {
    let tag = tag.strip_prefix("refs/tags/").unwrap_or(tag);
    if mode == "nightly" {
        return Some("nightly");
    }
    if matches!(mode, "tag" | "manual") && stable_release_id(tag) {
        return Some("stable");
    }
    None
}

pub fn image_component(name: &str, digest: &str, size: i64, image: Option<&str>) -> Result<Value> {
    ensure!(
        is_sha256_digest(digest),
        "{name} digest must be sha256:<64 hex>"
    );
    ensure!(size > 0, "{name} size must be a positive integer");
    let mut body = serde_json::json!({
        "digest": digest,
        "platform": "linux/amd64",
        "size": size,
    });
    if let Some(image) = image {
        body.as_object_mut()
            .expect("component object")
            .insert("image".into(), Value::String(image.to_string()));
    }
    Ok(body)
}

pub fn frontend_inventory(root: &Path) -> Result<Value> {
    let mut files = BTreeMap::new();
    for name in ["index.html", "app.js", "style.css"] {
        let path = root.join(name);
        let meta = fs::symlink_metadata(&path)
            .with_context(|| format!("frontend {name} must be a regular file"))?;
        ensure!(
            !meta.file_type().is_symlink() && meta.is_file(),
            "frontend {name} must be a regular file"
        );
        let hex =
            hex_encode(ring::digest::digest(&ring::digest::SHA256, &fs::read(&path)?).as_ref());
        files.insert(name.to_string(), hex);
    }
    let joined = ["app.js", "index.html", "style.css"]
        .into_iter()
        .map(|name| files[name].as_str())
        .collect::<String>();
    let digest =
        hex_encode(ring::digest::digest(&ring::digest::SHA256, joined.as_bytes()).as_ref());
    Ok(serde_json::json!({
        "files": files,
        "platform": "cdn",
        "sha256": format!("sha256:{digest}"),
    }))
}

fn key_id_valid(key_id: &str) -> bool {
    (2..=32).contains(&key_id.len())
        && key_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn hex32_secret(secret: &str) -> bool {
    secret.len() == 64
        && secret
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn source_commit_valid(sha: &str) -> bool {
    sha.len() == 40
        && sha
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn refuse_partial(components: &Value) -> Result<()> {
    let Some(map) = components.as_object() else {
        anyhow::bail!("failed/partial publication remains unavailable");
    };
    for name in REQUIRED_COMPONENTS {
        ensure!(
            map.contains_key(name),
            "failed/partial publication remains unavailable"
        );
    }
    Ok(())
}

fn stage_write(directory: &Path, identity: &str, payload: &Value) -> Result<PathBuf> {
    fs::create_dir_all(directory).context("release output directory")?;
    let staged = directory.join(format!(".{identity}.staging.json"));
    let final_path = directory.join(format!("{identity}.json"));
    let mut bytes = canonical_json(payload);
    bytes.push(b'\n');
    {
        let mut file = fs::File::create(&staged).context("release staging write")?;
        file.write_all(&bytes).context("release staging write")?;
        file.sync_all().context("release staging sync")?;
    }
    fs::rename(&staged, &final_path).context("release staging promote")?;
    Ok(final_path)
}

fn index_releases(rows: Vec<Value>, retain: usize) -> Result<Value> {
    ensure!(
        retain >= 1,
        "retention must be at least one complete release"
    );
    let mut by_channel: BTreeMap<String, Vec<Value>> = BTreeMap::from([
        ("stable".into(), Vec::new()),
        ("nightly".into(), Vec::new()),
    ]);
    for row in rows {
        let channel = row
            .get("channel")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        ensure!(
            by_channel.contains_key(&channel),
            "index contains an unknown channel"
        );
        by_channel.get_mut(&channel).expect("channel").push(row);
    }
    let mut kept = Vec::new();
    for items in by_channel.values_mut() {
        items.sort_by(|a, b| {
            b.get("published_at")
                .and_then(Value::as_str)
                .cmp(&a.get("published_at").and_then(Value::as_str))
        });
        kept.extend(items.iter().take(retain).cloned());
    }
    kept.sort_by(|a, b| {
        b.get("published_at")
            .and_then(Value::as_str)
            .cmp(&a.get("published_at").and_then(Value::as_str))
    });
    Ok(serde_json::json!({
        "format": INDEX_FORMAT,
        "releases": kept,
    }))
}

pub struct CompletePublication {
    pub channel: String,
    pub release_id: String,
    pub source_commit: String,
    pub published_at: String,
    pub components: Value,
    pub notes: String,
    pub key_id: String,
    pub secret_hex: String,
    pub out_dir: PathBuf,
}

pub fn write_complete_publication(request: CompletePublication) -> Result<PathBuf> {
    let CompletePublication {
        channel,
        release_id,
        source_commit: sha,
        published_at,
        components,
        notes,
        key_id,
        secret_hex,
        out_dir,
    } = request;
    refuse_partial(&components)?;
    validate_publication_identity(&channel, &release_id)?;
    ensure!(
        source_commit_valid(&sha),
        "source_commit must be a full lowercase SHA"
    );
    ensure!(key_id_valid(&key_id), "signing key id is invalid");
    ensure!(
        hex32_secret(&secret_hex),
        "signing key must be 32 bytes hex"
    );
    validate_components(&components)?;
    let version = if channel == "stable" {
        release_id.trim_start_matches('v').to_string()
    } else {
        release_id.clone()
    };
    let unsigned = serde_json::json!({
        "backup_format": "JMODGUARD-BACKUP-1",
        "channel": channel,
        "compatibility": {
            "min_conductor": "0.1.0",
            "rollback": "matching-backup",
        },
        "components": components,
        "format": FORMAT,
        "notes": notes,
        "published_at": published_at,
        "release_id": release_id,
        "schema": {
            "core_family": "jmodguard-core-1",
            "core_version": 12,
            "guild_family": "jmodguard-guild-1",
            "guild_version": GUILD_SCHEMA_VERSION,
        },
        "settings_schema": 1,
        "source_commit": sha,
        "version": version,
    });
    let signed_raw = sign_manifest(unsigned, &key_id, &secret_hex)?;
    let signed: Value = serde_json::from_str(&signed_raw)?;
    let path = stage_write(&out_dir, &release_id, &signed)?;
    let index_path = out_dir.join("index.json");
    let mut rows = Vec::new();
    if index_path.exists() {
        let existing: Value =
            serde_json::from_str(&fs::read_to_string(&index_path)?).context("release index")?;
        if let Some(list) = existing.get("releases").and_then(Value::as_array) {
            rows = list
                .iter()
                .filter(|row| {
                    row.get("release_id").and_then(Value::as_str) != Some(release_id.as_str())
                })
                .cloned()
                .collect();
        }
    }
    rows.push(serde_json::json!({
        "channel": channel,
        "path": path.file_name().and_then(|name| name.to_str()).unwrap_or_default(),
        "published_at": signed.get("published_at").cloned().unwrap_or(Value::String(published_at)),
        "release_id": release_id,
        "source_commit": sha,
    }));
    stage_write(&out_dir, "index", &index_releases(rows, INDEX_RETAIN)?)?;
    Ok(path)
}

pub fn compatibility(manifest: &Value, cfg: &Config) -> Result<(bool, Option<String>)> {
    let current = installed(cfg);
    let schema = manifest.get("schema").context("manifest missing schema")?;
    let core_family = schema
        .get("core_family")
        .and_then(Value::as_str)
        .unwrap_or("");
    let core_version = schema
        .get("core_version")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let current_core = current
        .get("core_version")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if core_family != "jmodguard-core-1" {
        return Ok((false, Some("incompatible database family".into())));
    }
    if core_version < current_core {
        return Ok((
            false,
            Some("older schema cannot replace a newer installation; restore a matching backup instead".into()),
        ));
    }
    let guild_family = schema
        .get("guild_family")
        .and_then(Value::as_str)
        .unwrap_or("");
    let guild_version = schema
        .get("guild_version")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let current_guild = current
        .get("guild_version")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if guild_family != "jmodguard-guild-1" {
        return Ok((false, Some("incompatible guild store family".into())));
    }
    if guild_version < current_guild {
        return Ok((
            false,
            Some("older guild schema cannot replace a newer installation; restore a matching backup instead".into()),
        ));
    }
    let manifest_settings = manifest
        .get("settings_schema")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let current_settings = current
        .get("settings_schema")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if manifest_settings < current_settings {
        return Ok((
            false,
            Some("older conductor settings schema cannot replace a newer installation".into()),
        ));
    }
    if manifest.get("backup_format").and_then(Value::as_str) != Some("JMODGUARD-BACKUP-1") {
        return Ok((false, Some("backup format is not compatible".into())));
    }
    Ok((true, None))
}

pub fn available_count(
    releases: &[DiscoveredRelease],
    channels: &[String],
    installed_id: &str,
) -> usize {
    releases
        .iter()
        .filter(|row| {
            row.compatible
                && row.release_id != installed_id
                && channels.iter().any(|c| c == &row.channel)
        })
        .count()
}

pub async fn refresh(conductor: &Conductor) -> Result<Vec<DiscoveredRelease>> {
    let cfg = &conductor.cfg;
    let feed = cfg
        .release_feed
        .as_deref()
        .context("release feed is not configured")?;
    ensure!(
        !cfg.release_keys.is_empty(),
        "release trust keys are not configured"
    );
    let index = fetch_json(&conductor.http, feed, MAX_INDEX_BYTES).await?;
    ensure!(
        index.get("format").and_then(Value::as_str) == Some(INDEX_FORMAT),
        "release index format is not recognized"
    );
    let listed = index
        .get("releases")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    ensure!(
        listed.len() <= MAX_RELEASES,
        "release index exceeds retention bound"
    );
    let mut verified = BTreeMap::new();
    let mut retain_from_previous = BTreeSet::new();
    for item in &listed {
        let index_id = item
            .get("release_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string);
        let path = item.get("path").and_then(Value::as_str).unwrap_or_default();
        let url = match absolute(feed, path) {
            Ok(url) => url,
            Err(_) => {
                if let Some(id) = index_id {
                    retain_from_previous.insert(id);
                }
                continue;
            }
        };
        let raw = match fetch_text(&conductor.http, &url, MAX_MANIFEST_BYTES).await {
            Ok(raw) => raw,
            Err(_) => {
                if let Some(id) = index_id {
                    retain_from_previous.insert(id);
                }
                continue;
            }
        };
        let manifest = match verify_manifest(&raw, &cfg.release_keys) {
            Ok(value) => value,
            Err(_) => {
                if let Some(id) = index_id {
                    retain_from_previous.insert(id);
                }
                continue;
            }
        };
        let (compatible, blocked) = compatibility(&manifest, cfg)?;
        let row = row_from_manifest(&manifest, compatible, blocked);
        verified.insert(row.release_id.clone(), row);
    }
    if let Ok(store) = conductor.store() {
        let previous = store.list_discovered_releases().await.unwrap_or_default();
        let discovered =
            merge_discovered_catalog(&previous, &listed, verified, &retain_from_previous);
        if discovered.is_empty() && !previous.is_empty() {
            anyhow::bail!("release feed contained no verified manifests");
        }
        store.replace_discovered_releases(&discovered).await?;
        notify_new_updates(conductor, store, &previous, &discovered).await?;
        return Ok(discovered);
    }
    let discovered = merge_discovered_catalog(&[], &listed, verified, &retain_from_previous);
    Ok(discovered)
}

async fn poll_interval_elapsed(
    store: &crate::store::ControlStore,
    interval_secs: u64,
) -> Result<bool> {
    if let Some(last) = store.meta_get("release_last_poll_ms").await?
        && let Ok(last_ms) = last.parse::<i64>()
    {
        let wait = i64::try_from(interval_secs.saturating_mul(1000)).unwrap_or(i64::MAX);
        if now_ms().saturating_sub(last_ms) < wait {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Polls the feed when `force` is true or the configured interval has elapsed.
pub async fn refresh_if_due(conductor: &Conductor, force: bool) -> Result<()> {
    if !discovery_available(&conductor.cfg) {
        return Ok(());
    }
    let store = conductor.store()?;
    if !force {
        let settings = conductor.settings().await.unwrap_or_default();
        if !poll_interval_elapsed(store, settings.discovery_interval_secs).await? {
            return Ok(());
        }
    }
    let result = refresh(conductor).await.map(|_| ());
    if !force {
        store
            .meta_set("release_last_poll_ms", &now_ms().to_string())
            .await?;
    }
    result
}

pub async fn catalog_for(conductor: &Conductor, force: bool) -> Result<Value> {
    let settings = conductor.settings().await.unwrap_or_default();
    let mut poll_error = None;
    if let Err(error) = refresh_if_due(conductor, force).await {
        poll_error = Some(error.to_string());
    }
    let releases = match conductor.store() {
        Ok(store) => store.list_discovered_releases().await.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let mut cfg = conductor.cfg.clone();
    if let Ok(store) = conductor.store()
        && let Ok(Some(id)) = store.meta_get("installed_release").await
        && !id.is_empty()
    {
        cfg.installed_release = Some(id);
    }
    let mut body = catalog(releases, &settings.discovery_channels, &cfg);
    if let Some(error) = poll_error
        && let Some(object) = body.as_object_mut()
    {
        object.insert("poll_error".into(), Value::String(error));
    }
    Ok(body)
}

pub fn catalog(releases: Vec<DiscoveredRelease>, channels: &[String], cfg: &Config) -> Value {
    let selected: Vec<_> = releases
        .iter()
        .filter(|row| channels.iter().any(|c| c == &row.channel))
        .cloned()
        .collect();
    let installed_id = installed(cfg)
        .get("release_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let apply_on = crate::update::updates_available(cfg);
    serde_json::json!({
        "available": discovery_available(cfg),
        "update_available": apply_on,
        "installed": installed(cfg),
        "channels": channels,
        "available_count": available_count(&selected, channels, &installed_id),
        "releases": selected,
        "apply": crate::update::apply_message(cfg),
    })
}

pub async fn load_verified_manifest(conductor: &Conductor, release_id: &str) -> Result<Value> {
    let cfg = &conductor.cfg;
    let feed = cfg
        .release_feed
        .as_deref()
        .context("release feed is not configured")?;
    ensure!(
        !cfg.release_keys.is_empty(),
        "release trust keys are not configured"
    );
    let index = fetch_json(&conductor.http, feed, MAX_INDEX_BYTES).await?;
    ensure!(
        index.get("format").and_then(Value::as_str) == Some(INDEX_FORMAT),
        "release index format is not recognized"
    );
    let listed = index
        .get("releases")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for item in &listed {
        let index_id = item.get("release_id").and_then(Value::as_str).unwrap_or("");
        if index_id != release_id {
            continue;
        }
        let path = item.get("path").and_then(Value::as_str).unwrap_or_default();
        let url = absolute(feed, path)?;
        let raw = fetch_text(&conductor.http, &url, MAX_MANIFEST_BYTES).await?;
        return verify_manifest(&raw, &cfg.release_keys);
    }
    anyhow::bail!("release {release_id} is not listed on the configured feed")
}

pub fn component_digest(manifest: &Value, name: &str) -> Result<String> {
    let components = manifest
        .get("components")
        .context("manifest missing components")?;
    let body = components
        .get(name)
        .with_context(|| format!("{name} is missing from the publication"))?;
    let key = if name == "frontend" {
        "sha256"
    } else {
        "digest"
    };
    body.get(key)
        .and_then(Value::as_str)
        .filter(|value| is_sha256_digest(value))
        .map(str::to_string)
        .with_context(|| format!("{name} digest is missing"))
}

async fn notify_new_updates(
    conductor: &Conductor,
    store: &ControlStore,
    previous: &[DiscoveredRelease],
    releases: &[DiscoveredRelease],
) -> Result<()> {
    let settings = conductor.settings().await?;
    if !settings.notify_enabled || !settings.notify_categories.iter().any(|c| c == "updates") {
        return Ok(());
    }
    let installed_id = installed(&conductor.cfg)
        .get("release_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let previous_ids: std::collections::BTreeSet<_> =
        previous.iter().map(|row| row.release_id.as_str()).collect();
    let newest = releases.iter().find(|row| {
        row.compatible
            && row.release_id != installed_id
            && !previous_ids.contains(row.release_id.as_str())
            && settings
                .discovery_channels
                .iter()
                .any(|c| c == &row.channel)
    });
    let Some(newest) = newest else {
        return Ok(());
    };
    let _ = store
        .insert_job(
            "notify_event",
            &serde_json::json!({
                "category": "updates",
                "message": format!("New {} release available: {}", newest.channel, newest.release_id),
                "severity": "info",
            }),
            3,
        )
        .await;
    Ok(())
}

fn hex_hmac(secret_hex: &str, body: &[u8]) -> Result<String> {
    ensure!(
        secret_hex.len() == 64,
        "release trust key must be 32 bytes hex"
    );
    let secret = hex_decode(secret_hex)?;
    let mac = ring::hmac::sign(
        &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &secret),
        body,
    );
    Ok(hex_encode(mac.as_ref()))
}

fn hex_decode(raw: &str) -> Result<Vec<u8>> {
    ensure!(raw.len().is_multiple_of(2), "hex key is truncated");
    (0..raw.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&raw[i..i + 2], 16).context("hex key"))
        .collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn canonical_json(value: &Value) -> Vec<u8> {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = vec![b'{'];
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                out.extend(serde_json::to_vec(key).expect("json key"));
                out.push(b':');
                out.extend(canonical_json(&map[key]));
            }
            out.push(b'}');
            out
        }
        Value::Array(items) => {
            let mut out = vec![b'['];
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                out.extend(canonical_json(item));
            }
            out.push(b']');
            out
        }
        other => serde_json::to_vec(other).expect("json atom"),
    }
}

fn absolute(feed: &str, path: &str) -> Result<String> {
    let feed_url = url::Url::parse(feed).context("release feed URL")?;
    let joined = if path.starts_with("http://") || path.starts_with("https://") {
        url::Url::parse(path).context("release manifest URL")?
    } else {
        feed_url.join(path).context("release manifest path")?
    };
    ensure!(
        joined.scheme() == feed_url.scheme()
            && joined.host_str() == feed_url.host_str()
            && joined.port_or_known_default() == feed_url.port_or_known_default(),
        "release artifacts must stay on the configured feed origin"
    );
    parse_http_url(joined.as_str())
}

async fn fetch_json(http: &reqwest::Client, url: &str, max: usize) -> Result<Value> {
    let text = fetch_text(http, url, max).await?;
    serde_json::from_str(&text).context("release feed is not JSON")
}

async fn fetch_text(http: &reqwest::Client, url: &str, max: usize) -> Result<String> {
    let response = http
        .get(url)
        .send()
        .await
        .context("release feed request failed")?;
    ensure!(
        response.status().is_success(),
        "release feed returned {}",
        response.status()
    );
    let bytes = response.bytes().await.context("release feed read failed")?;
    ensure!(bytes.len() <= max, "release feed exceeded size bound");
    String::from_utf8(bytes.to_vec()).context("release feed is not UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_json_matches_python_sort_keys() {
        let value = serde_json::json!({"a":1,"b":{"c":2,"d":[3,true,null]}});
        assert_eq!(
            String::from_utf8(canonical_json(&value)).unwrap(),
            r#"{"a":1,"b":{"c":2,"d":[3,true,null]}}"#
        );
        let mac = hex_hmac(&"11".repeat(32), &canonical_json(&value)).unwrap();
        assert_eq!(
            mac,
            "2f0ff9b63fa84c2e748da90cad8114e3c802279a64d536cd323c75fed77f718a"
        );
    }

    #[test]
    fn canonical_json_matches_python_utf8_strings() {
        let value = serde_json::json!({"a": 1, "notes": "café"});
        assert_eq!(
            String::from_utf8(canonical_json(&value)).unwrap(),
            r#"{"a":1,"notes":"café"}"#
        );
    }

    #[test]
    fn non_ascii_notes_manifest_verifies() {
        let body = serde_json::json!({
            "format": FORMAT,
            "release_id": "v0.2.0",
            "channel": "stable",
            "version": "0.2.0",
            "source_commit": "a".repeat(40),
            "published_at": "2026-09-15T00:00:00Z",
            "components": {
                "bot": {"digest": format!("sha256:{}", "1".repeat(64)), "platform": "linux/amd64", "size": 1},
                "api": {"digest": format!("sha256:{}", "2".repeat(64)), "platform": "linux/amd64", "size": 1},
                "conductor": {"digest": format!("sha256:{}", "3".repeat(64)), "platform": "linux/amd64", "size": 1},
                "frontend": {
                    "sha256": format!("sha256:{}", "4".repeat(64)),
                    "files": {"index.html": "a", "app.js": "b", "style.css": "c"}
                }
            },
            "schema": {
                "core_family": "jmodguard-core-1",
                "core_version": 12,
                "guild_family": "jmodguard-guild-1",
                "guild_version": 9
            },
            "settings_schema": 1,
            "backup_format": "JMODGUARD-BACKUP-1",
            "compatibility": {"min_conductor": "0.1.0", "rollback": "matching-backup"},
            "notes": "café",
        });
        let signed = sign_manifest(body, "rp1", &"a".repeat(64)).unwrap();
        let mut keys = BTreeMap::new();
        keys.insert("rp1".into(), "a".repeat(64));
        verify_manifest(&signed, &keys).expect("utf-8 notes must verify");
    }

    #[test]
    fn merge_discovered_catalog_retains_indexed_release_when_reverify_fails() {
        let previous = vec![DiscoveredRelease {
            release_id: "v0.2.0".into(),
            channel: "stable".into(),
            version: "0.2.0".into(),
            source_commit: "b".repeat(40),
            published_at: "2026-09-15T00:00:00Z".into(),
            notes: String::new(),
            compatible: true,
            blocked_reason: None,
            components: vec!["bot".into()],
        }];
        let listed = vec![
            serde_json::json!({"release_id": "v0.2.0", "path": "v0.2.0.json"}),
            serde_json::json!({"release_id": "nightly-20260915-aaaaaaaaaaaa", "path": "nightly.json"}),
        ];
        let mut verified = BTreeMap::new();
        verified.insert(
            "nightly-20260915-aaaaaaaaaaaa".into(),
            DiscoveredRelease {
                release_id: "nightly-20260915-aaaaaaaaaaaa".into(),
                channel: "nightly".into(),
                version: "n".into(),
                source_commit: "c".repeat(40),
                published_at: "2026-09-16T00:00:00Z".into(),
                notes: String::new(),
                compatible: true,
                blocked_reason: None,
                components: vec!["bot".into()],
            },
        );
        let mut retain = BTreeSet::new();
        retain.insert("v0.2.0".into());
        let merged = merge_discovered_catalog(&previous, &listed, verified, &retain);
        assert_eq!(merged.len(), 2);
        assert!(merged.iter().any(|row| row.release_id == "v0.2.0"));
        assert!(
            merged
                .iter()
                .any(|row| row.release_id == "nightly-20260915-aaaaaaaaaaaa")
        );
    }

    #[test]
    fn hollow_signed_manifest_is_refused() {
        let body = serde_json::json!({
            "format": FORMAT,
            "release_id": "v0.2.0",
            "channel": "stable",
            "version": "0.2.0",
            "source_commit": "a".repeat(40),
            "published_at": "2026-09-15T00:00:00Z",
            "components": {
                "bot": {},
                "api": {"digest": format!("sha256:{}", "2".repeat(64)), "platform": "linux/amd64", "size": 1},
                "conductor": {"digest": format!("sha256:{}", "3".repeat(64)), "platform": "linux/amd64", "size": 1},
                "frontend": {
                    "sha256": format!("sha256:{}", "4".repeat(64)),
                    "files": {"index.html": "a", "app.js": "b", "style.css": "c"}
                }
            },
            "schema": {
                "core_family": "jmodguard-core-1",
                "core_version": 12,
                "guild_family": "jmodguard-guild-1",
                "guild_version": 9
            },
            "settings_schema": 1,
            "backup_format": "JMODGUARD-BACKUP-1",
            "compatibility": {"min_conductor": "0.1.0", "rollback": "matching-backup"},
        });
        let signed = sign_manifest(body, "rp1", &"a".repeat(64)).unwrap();
        let mut keys = BTreeMap::new();
        keys.insert("rp1".into(), "a".repeat(64));
        assert!(verify_manifest(&signed, &keys).is_err());
    }

    #[test]
    fn available_count_ignores_installed_and_incompatible() {
        let rows = vec![
            DiscoveredRelease {
                release_id: "v0.1.0".into(),
                channel: "stable".into(),
                version: "0.1.0".into(),
                source_commit: "a".repeat(40),
                published_at: "t".into(),
                notes: String::new(),
                compatible: true,
                blocked_reason: None,
                components: vec!["bot".into()],
            },
            DiscoveredRelease {
                release_id: "v0.2.0".into(),
                channel: "stable".into(),
                version: "0.2.0".into(),
                source_commit: "b".repeat(40),
                published_at: "t".into(),
                notes: String::new(),
                compatible: true,
                blocked_reason: None,
                components: vec!["bot".into()],
            },
            DiscoveredRelease {
                release_id: "nightly-20260915-aaaaaaaaaaaa".into(),
                channel: "nightly".into(),
                version: "n".into(),
                source_commit: "c".repeat(40),
                published_at: "t".into(),
                notes: String::new(),
                compatible: false,
                blocked_reason: Some("older schema".into()),
                components: vec!["bot".into()],
            },
        ];
        assert_eq!(
            available_count(&rows, &["stable".into(), "nightly".into()], "v0.1.0"),
            1
        );
        assert_eq!(available_count(&rows, &["nightly".into()], "v0.1.0"), 0);
    }

    fn fixture_components(root: &Path) -> Value {
        fs::write(root.join("index.html"), "<html></html>").unwrap();
        fs::write(root.join("app.js"), "void 0;").unwrap();
        fs::write(root.join("style.css"), "body{}").unwrap();
        serde_json::json!({
            "bot": image_component("bot", &format!("sha256:{}", "1".repeat(64)), 10, Some("ghcr.io/jatmn/jmodguard-bot")).unwrap(),
            "api": image_component("api", &format!("sha256:{}", "2".repeat(64)), 11, Some("ghcr.io/jatmn/jmodguard-api")).unwrap(),
            "conductor": image_component("conductor", &format!("sha256:{}", "3".repeat(64)), 12, Some("ghcr.io/jatmn/jmodguard-conductor")).unwrap(),
            "frontend": frontend_inventory(root).unwrap(),
        })
    }

    #[test]
    fn channel_for_keeps_stable_and_nightly_separate() {
        assert_eq!(channel_for("tag", "v1.2.3"), Some("stable"));
        assert_eq!(
            channel_for("nightly", "nightly-20260915-aaaaaaaaaaaa"),
            Some("nightly")
        );
        assert_eq!(channel_for("main", ""), None);
        assert_eq!(channel_for("tag", "nightly-20260915-aaaaaaaaaaaa"), None);
        assert!(stable_release_id("v0.1.0"));
        assert!(!stable_release_id("nightly-20260915-aaaaaaaaaaaa"));
    }

    #[test]
    fn write_complete_publication_refuses_partial_then_indexes() {
        let tmp = std::env::temp_dir().join(format!("jmodguard-release-{}", std::process::id()));
        let frontend = tmp.join("frontend");
        let out = tmp.join("out");
        fs::create_dir_all(&frontend).unwrap();
        let components = fixture_components(&frontend);
        let secret = "a".repeat(64);
        assert!(
            write_complete_publication(CompletePublication {
                channel: "stable".into(),
                release_id: "v0.2.0".into(),
                source_commit: "a".repeat(40),
                published_at: "2026-09-15T00:00:00Z".into(),
                components: serde_json::json!({"bot": components["bot"]}),
                notes: String::new(),
                key_id: "rp1".into(),
                secret_hex: secret.clone(),
                out_dir: out.clone(),
            })
            .is_err()
        );
        let path = write_complete_publication(CompletePublication {
            channel: "stable".into(),
            release_id: "v0.2.0".into(),
            source_commit: "a".repeat(40),
            published_at: "2026-09-15T00:00:00Z".into(),
            components,
            notes: "notes".into(),
            key_id: "rp1".into(),
            secret_hex: secret.clone(),
            out_dir: out.clone(),
        })
        .unwrap();
        let index: Value =
            serde_json::from_str(&fs::read_to_string(out.join("index.json")).unwrap()).unwrap();
        assert_eq!(index["format"], INDEX_FORMAT);
        assert_eq!(index["releases"][0]["release_id"], "v0.2.0");
        let mut keys = BTreeMap::new();
        keys.insert("rp1".into(), secret);
        verify_manifest(&fs::read_to_string(&path).unwrap(), &keys).unwrap();
        assert!(
            fs::read_dir(&out)
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .contains(".staging.json"))
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn index_retention_keeps_channels_independent() {
        let mut rows = Vec::new();
        for index in 0..5 {
            rows.push(serde_json::json!({
                "release_id": format!("v0.0.{index}"),
                "channel": "stable",
                "published_at": format!("2026-09-0{}", index + 1),
            }));
            rows.push(serde_json::json!({
                "release_id": format!("nightly-2026090{}-{}", index + 1, "a".repeat(12)),
                "channel": "nightly",
                "published_at": format!("2026-09-1{index}"),
            }));
        }
        let index = index_releases(rows, 2).unwrap();
        let releases = index["releases"].as_array().unwrap();
        let stables = releases
            .iter()
            .filter(|row| row["channel"] == "stable")
            .count();
        let nightlies = releases
            .iter()
            .filter(|row| row["channel"] == "nightly")
            .count();
        assert_eq!(stables, 2);
        assert_eq!(nightlies, 2);
    }

    #[test]
    fn nightly_cannot_reuse_stable_identity() {
        let tmp = std::env::temp_dir().join(format!("jmodguard-release-id-{}", std::process::id()));
        let frontend = tmp.join("frontend");
        fs::create_dir_all(&frontend).unwrap();
        let components = fixture_components(&frontend);
        let secret = "a".repeat(64);
        assert!(
            write_complete_publication(CompletePublication {
                channel: "nightly".into(),
                release_id: "v0.2.0".into(),
                source_commit: "a".repeat(40),
                published_at: "2026-09-15T00:00:00Z".into(),
                components: components.clone(),
                notes: String::new(),
                key_id: "rp1".into(),
                secret_hex: secret.clone(),
                out_dir: tmp.join("out"),
            })
            .is_err()
        );
        assert!(
            write_complete_publication(CompletePublication {
                channel: "stable".into(),
                release_id: "nightly-20260915-aaaaaaaaaaaa".into(),
                source_commit: "a".repeat(40),
                published_at: "2026-09-15T00:00:00Z".into(),
                components,
                notes: String::new(),
                key_id: "rp1".into(),
                secret_hex: secret,
                out_dir: tmp.join("out2"),
            })
            .is_err()
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn validate_components_rejects_unexpected_keys_and_non_positive_size() {
        let tmp =
            std::env::temp_dir().join(format!("jmodguard-release-validate-{}", std::process::id()));
        let frontend = tmp.join("frontend");
        fs::create_dir_all(&frontend).unwrap();
        let components = fixture_components(&frontend);
        let mut extra = components.clone();
        extra
            .as_object_mut()
            .expect("components object")
            .insert("debug".into(), serde_json::json!({}));
        assert!(validate_components(&extra).is_err());

        let mut zero_size = components.clone();
        zero_size["bot"]["size"] = serde_json::json!(0);
        assert!(validate_components(&zero_size).is_err());

        let mut missing_size = components.clone();
        missing_size["api"]
            .as_object_mut()
            .expect("api")
            .remove("size");
        assert!(validate_components(&missing_size).is_err());

        validate_components(&components).unwrap();
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn release_please_config_is_stable_only() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let config: Value = serde_json::from_str(
            &fs::read_to_string(root.join("release-please-config.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(config["release-type"], "simple");
        assert_eq!(config["include-v-in-tag"], true);
        assert!(config["packages"].get(".").is_some());
        let manifest: Value = serde_json::from_str(
            &fs::read_to_string(root.join(".release-please-manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["."], "0.1.0");
    }
}
