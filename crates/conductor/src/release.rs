//! Signed Stable/Nightly discovery. Apply/install remains inactive.
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::config::Config;
use crate::config::parse_http_url;
use crate::state::Conductor;
use crate::store::{ControlStore, now_ms};

pub const FORMAT: &str = "JMODGUARD-RELEASE-1";
pub const INDEX_FORMAT: &str = "JMODGUARD-RELEASE-INDEX-1";
const MAX_INDEX_BYTES: usize = 256 * 1024;
const MAX_MANIFEST_BYTES: usize = 128 * 1024;
const MAX_RELEASES: usize = 64;

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
        "guild_version": 8,
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
    for name in ["bot", "api", "conductor", "frontend"] {
        ensure!(
            parsed.components.get(name).is_some(),
            "incomplete publication remains unavailable"
        );
    }
    Ok(body)
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
    let mut discovered = Vec::new();
    for item in listed {
        let path = item.get("path").and_then(Value::as_str).unwrap_or_default();
        let url = absolute(feed, path)?;
        let raw = fetch_text(&conductor.http, &url, MAX_MANIFEST_BYTES).await?;
        let manifest = match verify_manifest(&raw, &cfg.release_keys) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let (compatible, blocked) = compatibility(&manifest, cfg)?;
        let row = DiscoveredRelease {
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
        };
        discovered.push(row);
    }
    if let Ok(store) = conductor.store() {
        let previous = store.list_discovered_releases().await.unwrap_or_default();
        store.replace_discovered_releases(&discovered).await?;
        store
            .meta_set("release_last_poll_ms", &now_ms().to_string())
            .await?;
        notify_new_updates(conductor, store, &previous, &discovered).await?;
    }
    Ok(discovered)
}

pub async fn catalog_for(conductor: &Conductor) -> Result<Value> {
    let settings = conductor.settings().await.unwrap_or_default();
    let mut poll_error = None;
    if discovery_available(&conductor.cfg)
        && let Err(error) = refresh(conductor).await
    {
        poll_error = Some(error.to_string());
    }
    let releases = match conductor.store() {
        Ok(store) => store.list_discovered_releases().await.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let mut body = catalog(releases, &settings.discovery_channels, &conductor.cfg);
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
    serde_json::json!({
        "available": discovery_available(cfg),
        "update_available": false,
        "installed": installed(cfg),
        "channels": channels,
        "available_count": available_count(&selected, channels, &installed_id),
        "releases": selected,
        "apply": "Point-and-click install arrives in a later release. Selecting a build here never deploys it.",
    })
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
}
