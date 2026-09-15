//! Owner-selected release apply and code-only rollback.
//! Compatible rollback never restores an old database or rewinds later settings.
use anyhow::{Context as _, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

use crate::config::Config;
use crate::release;
use crate::state::Conductor;
use crate::store::now_ms;
use crate::update_adapter::{Adapter, COMPONENTS, RECOVER_FORMAT, RecoverInventory};

#[derive(Debug, Deserialize)]
pub struct ApplyRequest {
    pub release_id: String,
    #[serde(default)]
    pub preop: String,
    #[serde(default)]
    pub confirm_interruption: bool,
}

#[derive(Debug, Deserialize)]
pub struct RollbackRequest {
    #[serde(default)]
    pub confirm_rollback: bool,
}

pub fn updates_available(cfg: &Config) -> bool {
    release::discovery_available(cfg) && cfg.update_root.is_some()
}

pub fn apply_message(cfg: &Config) -> &'static str {
    if updates_available(cfg) {
        "Owner can install a selected compatible release. There is no silent automatic update."
    } else {
        "Point-and-click install is not configured. Set JMODGUARD_CONDUCTOR_UPDATE_ROOT. Selecting a build never deploys it automatically."
    }
}

pub fn recover_status(cfg: &Config) -> Value {
    let Some(root) = &cfg.update_root else {
        return json!({ "configured": false });
    };
    match Adapter::load_recover(root) {
        Ok(Some(body)) => json!({
            "configured": true,
            "job_id": body.job_id,
            "status": body.status,
            "phase": body.phase,
            "component": body.component,
            "release_id": body.release_id,
        }),
        Ok(None) => json!({ "configured": true, "status": "idle" }),
        Err(error) => json!({ "configured": true, "error": format!("{error:#}") }),
    }
}

pub async fn current_installed_id(conductor: &Conductor) -> String {
    if let Ok(store) = conductor.store()
        && let Ok(Some(id)) = store.meta_get("installed_release").await
        && !id.is_empty()
    {
        return id;
    }
    release::installed(&conductor.cfg)
        .get("release_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

pub async fn plan(conductor: &Conductor, release_id: &str) -> Result<Value> {
    ensure!(
        updates_available(&conductor.cfg),
        "point-and-click install is not configured"
    );
    let manifest = release::load_verified_manifest(conductor, release_id).await?;
    let (compatible, blocked) = release::compatibility(&manifest, &conductor.cfg)?;
    ensure!(
        compatible,
        "{}",
        blocked.unwrap_or_else(|| "release is not compatible".into())
    );
    let channel = manifest
        .get("channel")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let desired = component_digests(&manifest)?;
    let adapter = Adapter::from_config(&conductor.cfg, conductor.http.clone())?;
    let inventory = adapter.load_inventory()?;
    let previous: BTreeMap<String, String> = COMPONENTS
        .iter()
        .map(|name| {
            (
                (*name).to_string(),
                inventory
                    .components
                    .get(*name)
                    .map(|row| row.digest.clone())
                    .unwrap_or_default(),
            )
        })
        .collect();
    Ok(json!({
        "release_id": release_id,
        "channel": channel,
        "compatible": true,
        "installed": current_installed_id(conductor).await,
        "components": COMPONENTS,
        "desired": desired,
        "previous": previous,
        "interruptions": {
            "frontend": "Static file switch under the update root. No process drain.",
            "api": "Local one-replica stop then start. Overlapping replicas are not this adapter.",
            "bot": "Exclusive Gateway worker: drain and confirm the old process is dead before activate. Discord is not replayed.",
            "conductor": "Replaced last. Independent `jmodguard-conductor recover-update` reads update_root/recover.json and does not use the new conductor or private ops."
        },
        "checkpoint": "Uses an R4 checkpoint archive when capture is configured. Compatible rollback switches retained binaries and does not restore the old database or rewind later settings or revocations.",
        "database": "This operation does not migrate or restore application databases.",
        "confirm_interruption": true,
        "order": COMPONENTS,
    }))
}

pub async fn enqueue(conductor: &Conductor, request: ApplyRequest) -> Result<String> {
    ensure!(
        updates_available(&conductor.cfg),
        "point-and-click install is not configured"
    );
    ensure!(
        request.confirm_interruption,
        "confirm the bot exclusive drain, local API restart, and conductor recover-update path"
    );
    let preop = match request.preop.as_str() {
        "" | "fresh" => "fresh",
        "recent" => "recent",
        "skip" => "skip",
        _ => anyhow::bail!("preop must be fresh, recent, or skip"),
    };
    if preop != "skip" {
        ensure!(
            conductor.capture_available(),
            "checkpoint capture is not configured; pass preop=skip only after accepting no pre-update archive"
        );
    }
    let release_id = request.release_id.trim();
    ensure!(!release_id.is_empty(), "release_id is required");
    let plan = plan(conductor, release_id).await?;
    let payload = json!({
        "release_id": release_id,
        "preop": preop,
        "rollback": false,
        "phase": "planned",
        "plan": plan,
    });
    conductor
        .store()?
        .insert_update_job_if_idle("release_update", &payload, 1)
        .await?
        .context("a backup, guild-move or update job is already running")
}

pub async fn enqueue_rollback(
    conductor: &Conductor,
    job_id: &str,
    request: RollbackRequest,
) -> Result<String> {
    ensure!(
        updates_available(&conductor.cfg),
        "point-and-click install is not configured"
    );
    ensure!(
        request.confirm_rollback,
        "confirm code-only rollback; the previous database is not restored"
    );
    let store = conductor.store()?;
    let job = store
        .get_job(job_id)
        .await?
        .context("update job was not found")?;
    ensure!(job.kind == "release_update", "job is not a release update");
    let payload: Value = serde_json::from_str(&job.payload_json)?;
    ensure!(
        payload.get("rollback").and_then(Value::as_bool) != Some(true),
        "that job is already a rollback"
    );
    ensure!(
        job.status == "succeeded"
            || payload.get("phase").and_then(Value::as_str) == Some("committed"),
        "rollback requires a committed update; failed or uncertain jobs use recover-update or retry"
    );
    let previous_id = payload
        .get("plan")
        .and_then(|plan| plan.get("installed"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let previous = payload
        .get("plan")
        .and_then(|plan| plan.get("previous"))
        .cloned()
        .context("committed update is missing previous digests")?;
    let rollback_payload = json!({
        "release_id": previous_id,
        "preop": "skip",
        "rollback": true,
        "phase": "planned",
        "source_job_id": job_id,
        "plan": {
            "release_id": previous_id,
            "desired": previous,
            "previous": payload.get("plan").and_then(|plan| plan.get("desired")).cloned().unwrap_or(json!({})),
            "installed": payload.get("release_id").cloned().unwrap_or(json!("")),
            "components": COMPONENTS,
            "database": "Code-only rollback. Later settings and revocations stay. Restore a matching backup to rewind data.",
        },
    });
    store
        .insert_update_job_if_idle("release_update", &rollback_payload, 1)
        .await?
        .context("a backup, guild-move or update job is already running")
}

pub async fn run(conductor: &Conductor, job_id: &str, payload_json: &str) -> Result<()> {
    let mut payload: Value = serde_json::from_str(payload_json)?;
    let rollback = payload.get("rollback").and_then(Value::as_bool) == Some(true);
    if !rollback {
        let preop = payload
            .get("preop")
            .and_then(Value::as_str)
            .unwrap_or("fresh");
        checkpoint(conductor, preop).await?;
        set_phase(
            conductor,
            job_id,
            &mut payload,
            "checkpointed",
            "",
            "running",
        )
        .await?;
    }
    let adapter = Adapter::from_config(&conductor.cfg, conductor.http.clone())?;
    let desired = payload
        .get("plan")
        .and_then(|plan| plan.get("desired"))
        .and_then(Value::as_object)
        .cloned()
        .context("update job missing desired digests")?;
    let previous = payload
        .get("plan")
        .and_then(|plan| plan.get("previous"))
        .cloned()
        .unwrap_or(json!({}));
    let release_id = payload
        .get("release_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let previous_release_id = payload
        .get("plan")
        .and_then(|plan| plan.get("installed"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut inventory = adapter.load_inventory()?;
    let order: Vec<&str> = if rollback {
        COMPONENTS.iter().rev().copied().collect()
    } else {
        COMPONENTS.to_vec()
    };

    set_phase(conductor, job_id, &mut payload, "staged", "", "running").await?;
    for name in &order {
        let digest = desired
            .get(*name)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .with_context(|| format!("{name} digest is missing"))?;
        if adapter.missing_artifact(digest) {
            anyhow::bail!("release artifact is missing for {name} ({digest})");
        }
        adapter.stage(digest)?;
    }

    let recover = RecoverInventory {
        format: RECOVER_FORMAT.into(),
        job_id: job_id.to_string(),
        release_id: release_id.clone(),
        previous_release_id: previous_release_id.clone(),
        previous: string_map(&previous),
        desired: map_strings(&desired),
        phase: "deploying".into(),
        component: String::new(),
        status: "replacing".into(),
    };
    adapter.write_recover(&recover)?;

    set_phase(
        conductor,
        job_id,
        &mut payload,
        "deploying",
        "",
        "replacing",
    )
    .await?;
    for name in &order {
        let digest = desired
            .get(*name)
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut recover = Adapter::load_recover(&adapter.root)?.context("recover inventory")?;
        recover.component = (*name).to_string();
        recover.phase = "deploying".into();
        adapter.write_recover(&recover)?;
        let previous_record = inventory.components.get(*name).cloned();
        if let Some(record) = &previous_record {
            adapter
                .drain(name, record)
                .await
                .map_err(|error| mark_uncertain(&adapter, &recover, error))?;
            adapter
                .confirm_dead(name, record)
                .await
                .map_err(|error| mark_uncertain(&adapter, &recover, error))?;
        }
        let activated = adapter
            .activate(name, digest, previous_record.as_ref())
            .await
            .map_err(|error| mark_uncertain(&adapter, &recover, error))?;
        match adapter.wait_ready(name, &activated).await {
            Ok(()) => {
                inventory.components.insert((*name).to_string(), activated);
                inventory.release_id = release_id.clone();
                adapter.save_inventory(&inventory)?;
            }
            Err(error) => {
                if let Some(previous_record) = previous_record.as_ref()
                    && !previous_record.digest.is_empty()
                    && previous_record.digest != digest
                {
                    let _ = adapter.drain(name, &activated).await;
                    if adapter.stage(&previous_record.digest).is_ok()
                        && let Ok(restored) = adapter
                            .activate(name, &previous_record.digest, Some(&activated))
                            .await
                        && adapter.wait_ready(name, &restored).await.is_ok()
                    {
                        inventory.components.insert((*name).to_string(), restored);
                        adapter.save_inventory(&inventory)?;
                        anyhow::bail!(
                            "{name} failed readiness; previous digest restored. {error:#}"
                        );
                    }
                }
                return Err(mark_uncertain(&adapter, &recover, error));
            }
        }
    }

    set_phase(
        conductor,
        job_id,
        &mut payload,
        "verifying",
        "",
        "replacing",
    )
    .await?;
    for name in &order {
        if let Some(record) = inventory.components.get(*name) {
            adapter.wait_ready(name, record).await?;
        }
    }

    inventory.release_id = release_id.clone();
    adapter.save_inventory(&inventory)?;
    if let Ok(store) = conductor.store() {
        store.meta_set("installed_release", &release_id).await?;
    }
    let mut recover = Adapter::load_recover(&adapter.root)?.context("recover inventory")?;
    recover.status = "committed".into();
    recover.phase = "committed".into();
    recover.component = String::new();
    adapter.write_recover(&recover)?;
    set_phase(
        conductor,
        job_id,
        &mut payload,
        "committed",
        "",
        "committed",
    )
    .await?;
    Ok(())
}

/// Independent of the replacement conductor and of private ops.
pub async fn recover_update(cfg: &Config) -> Result<Value> {
    let adapter = Adapter::from_config(cfg, reqwest::Client::new())?;
    let Some(mut recover) = Adapter::load_recover(&adapter.root)? else {
        anyhow::bail!("no recover inventory at update_root/recover.json");
    };
    if recover.status == "committed" {
        return Ok(json!({
            "status": "committed",
            "message": "last update already committed; nothing to recover",
        }));
    }
    if recover.status == "recovered" {
        return Ok(json!({
            "status": "recovered",
            "message": "previous recover-update already restored previous code",
        }));
    }
    let mut inventory = adapter.load_inventory()?;
    for name in COMPONENTS.iter().rev() {
        let Some(digest) = recover
            .previous
            .get(*name)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        if adapter.missing_artifact(digest) {
            anyhow::bail!("previous {name} artifact is missing for recover-update ({digest})");
        }
        adapter.stage(digest)?;
        let current = inventory.components.get(*name).cloned();
        if let Some(record) = &current {
            let _ = adapter.drain(name, record).await;
            let _ = adapter.confirm_dead(name, record).await;
        }
        let restored = adapter.activate(name, digest, current.as_ref()).await?;
        adapter.wait_ready(name, &restored).await?;
        inventory.components.insert((*name).to_string(), restored);
    }
    inventory.release_id = recover.previous_release_id.clone();
    adapter.save_inventory(&inventory)?;
    recover.status = "recovered".into();
    recover.phase = "recovered".into();
    adapter.write_recover(&recover)?;
    Ok(json!({
        "status": "recovered",
        "release_id": recover.previous_release_id,
        "job_id": recover.job_id,
        "database": "Code restored from recover.json. The previous database was not applied.",
    }))
}

async fn checkpoint(conductor: &Conductor, preop: &str) -> Result<()> {
    if preop == "skip" {
        return Ok(());
    }
    if preop == "recent"
        && let Some(last) = conductor.store()?.last_complete_ms().await?
        && now_ms().saturating_sub(last) < 6 * 3600 * 1000
    {
        return Ok(());
    }
    let settings = conductor.settings().await?;
    crate::backup::capture(
        &conductor.cfg,
        conductor.store()?,
        conductor.backup_zone()?,
        "checkpoint",
        &settings,
    )
    .await?;
    Ok(())
}

async fn set_phase(
    conductor: &Conductor,
    job_id: &str,
    payload: &mut Value,
    phase: &str,
    component: &str,
    recover_status: &str,
) -> Result<()> {
    if let Some(object) = payload.as_object_mut() {
        object.insert("phase".into(), json!(phase));
        if !component.is_empty() {
            object.insert("component".into(), json!(component));
        }
    }
    if let Ok(store) = conductor.store() {
        store.patch_job_payload(job_id, payload).await?;
    }
    if let Ok(adapter) = Adapter::from_config(&conductor.cfg, conductor.http.clone())
        && let Ok(Some(mut recover)) = Adapter::load_recover(&adapter.root)
    {
        recover.phase = phase.to_string();
        recover.status = recover_status.to_string();
        if !component.is_empty() {
            recover.component = component.to_string();
        }
        let _ = adapter.write_recover(&recover);
    }
    let _ = recover_status;
    Ok(())
}

fn mark_uncertain(
    adapter: &Adapter,
    recover: &RecoverInventory,
    error: anyhow::Error,
) -> anyhow::Error {
    let mut body = recover.clone();
    body.status = "uncertain".into();
    let _ = adapter.write_recover(&body);
    anyhow::anyhow!("update ownership uncertain: {error:#}")
}

fn component_digests(manifest: &Value) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for name in COMPONENTS {
        out.insert(name.to_string(), release::component_digest(manifest, name)?);
    }
    Ok(out)
}

fn string_map(value: &Value) -> BTreeMap<String, String> {
    value.as_object().map(map_strings).unwrap_or_default()
}

fn map_strings(map: &serde_json::Map<String, Value>) -> BTreeMap<String, String> {
    map.iter()
        .filter_map(|(key, value)| {
            value
                .as_str()
                .map(|digest| (key.clone(), digest.to_string()))
        })
        .collect()
}
