//! Owner-only independent release discovery and on-demand application.
use super::state::ApiState;
use super::{session_owner, trusted_web_origin};
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use jmodguard::db::{self, ComponentObservation, ComponentRelease, Db};
use jmodguard_updater::oci::{Registry, sha256_hex};
use jmodguard_updater::storage::Storage;
use serde::Deserialize;
use serde_json::{Value, json};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tower_sessions::Session;

fn error(status: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error":message})))
}

type Reply = Result<Json<Value>, (StatusCode, Json<Value>)>;

async fn owner(app: &Arc<ApiState>, session: &Session) -> Result<(), (StatusCode, Json<Value>)> {
    match session_owner(app, session).await {
        Ok(_) => Ok(()),
        Err(failure) if format!("{:#}", failure.0).contains("not authenticated") => {
            Err(error(StatusCode::UNAUTHORIZED, "sign in required"))
        }
        Err(_) => Err(error(StatusCode::FORBIDDEN, "bot owner required")),
    }
}

fn origin(app: &ApiState, headers: &HeaderMap) -> Result<(), (StatusCode, Json<Value>)> {
    if trusted_web_origin(&app.cfg, headers) {
        Ok(())
    } else {
        Err(error(StatusCode::FORBIDDEN, "untrusted request origin"))
    }
}

fn enabled(name: &str) -> bool {
    std::env::var(name).as_deref() == Ok("1")
}

pub async fn status(State(app): State<Arc<ApiState>>, session: Session) -> Reply {
    owner(&app, &session).await?;
    let updates = db::component_updates(&app.db)
        .await
        .map_err(|_| error(StatusCode::SERVICE_UNAVAILABLE, "update state unavailable"))?;
    let mut compatibility = serde_json::Map::new();
    let mut launchers = serde_json::Map::new();
    let now = chrono::Utc::now().timestamp();
    for row in &updates {
        if row.component != "webui" {
            launchers.insert(
                row.component.clone(),
                jmodguard_updater::launcher_status(row, now),
            );
        }
        if let Some(candidate) = &row.candidate {
            let result = jmodguard_updater::compatible(&app.db, candidate, now).await;
            compatibility.insert(
                row.component.clone(),
                match result {
                    Ok(()) => json!({"installable":true}),
                    Err(problem) => json!({"installable":false,"reason":format!("{problem:#}")}),
                },
            );
        }
    }
    Ok(Json(json!({
        "components":updates,
        "coordinated_upgrade_path":"/api/service-upgrade/recovery",
        "service_upgrade":db::UpdateControl::from_application(&app.db).upgrade().await.map_err(|_| error(StatusCode::SERVICE_UNAVAILABLE, "update control unavailable"))?,
        "compatibility":compatibility,
        "launchers":launchers,
        "unattended_enabled":enabled("JMODGUARD_UNATTENDED_UPDATES"),
        "apply_enabled":enabled("JMODGUARD_UPDATE_APPLY_ENABLED"),
        "webui_storage_configured":storage().is_ok(),
        "base_image_sha":std::env::var("JMODGUARD_BASE_SHA").unwrap_or_else(|_| "unknown".into()),
    })))
}

fn storage() -> anyhow::Result<Storage> {
    Storage::new(
        &std::env::var("BUNNY_STORAGE_ENDPOINT").unwrap_or_else(|_| "storage.bunnycdn.com".into()),
        &std::env::var("BUNNY_STORAGE_ZONE")?,
        &std::env::var("BUNNY_STORAGE_PASSWORD")?,
    )
}

#[derive(Deserialize)]
pub struct CheckRequest {
    component: String,
}

pub async fn check(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Json(input): Json<CheckRequest>,
) -> Reply {
    owner(&app, &session).await?;
    origin(&app, &headers)?;
    if !matches!(input.component.as_str(), "bot" | "api" | "webui") {
        return Err(error(StatusCode::BAD_REQUEST, "invalid component"));
    }
    check_one(&app, &input.component)
        .await
        .map_err(|_| error(StatusCode::SERVICE_UNAVAILABLE, "release check failed"))?;
    status(State(app), session).await
}

pub async fn check_one(app: &Arc<ApiState>, component: &str) -> anyhow::Result<()> {
    let lease = format!("{:032x}", rand::random::<u128>());
    let now = chrono::Utc::now().timestamp();
    if !db::claim_update_lease(&app.db, component, &lease, now, 240).await? {
        anyhow::bail!("another update operation is running");
    }
    with_update_lease(&app.db, component, &lease, 240, async {
        let state = db::component_update(&app.db, component).await?;
        let registry = Registry::ghcr()?;
        let checked = match registry.discover(component, &state.channel).await {
            Ok(candidate) => {
                db::record_update_check(
                    &app.db,
                    component,
                    &state.channel,
                    Some(candidate),
                    None,
                    now,
                )
                .await
            }
            Err(error) => {
                db::record_update_check(
                    &app.db,
                    component,
                    &state.channel,
                    None,
                    Some(format!("{error:#}")),
                    now,
                )
                .await?;
                Err(error)
            }
        };
        if component == "webui" {
            match reconcile_webui(app).await {
                Ok(()) => {
                    if let Some(desired) = db::component_update(&app.db, "webui").await?.desired {
                        db::settle_interrupted_webui(&app.db, &desired).await?;
                    }
                }
                Err(error) => tracing::warn!("WebUI Storage reconciliation failed: {error:#}"),
            }
            if let Ok(storage) = storage()
                && let Err(error) = cleanup_webui_assets(app, &storage).await
            {
                tracing::warn!("WebUI managed asset cleanup failed: {error:#}");
            }
        }
        checked
    })
    .await
}

async fn with_update_lease<T, F>(
    db: &Db,
    component: &str,
    lease: &str,
    ttl: i64,
    work: F,
) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<T>>,
{
    anyhow::ensure!(
        db::renew_update_lease(db, component, lease, chrono::Utc::now().timestamp(), ttl).await?,
        "update lease expired before work began"
    );
    let watchdog = async {
        loop {
            tokio::time::sleep(Duration::from_secs((ttl / 4).max(1) as u64)).await;
            let now = chrono::Utc::now().timestamp();
            anyhow::ensure!(
                db::renew_update_lease(db, component, lease, now, ttl).await?,
                "update lease expired or changed"
            );
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    let result = tokio::select! {
        biased;
        failure = watchdog => { failure?; unreachable!() },
        result = work => result,
    };
    let now = chrono::Utc::now().timestamp();
    let owned = db::renew_update_lease(db, component, lease, now, ttl).await?;
    db::release_update_lease(db, component, lease).await?;
    anyhow::ensure!(owned, "update lease expired or changed");
    result
}

async fn reconcile_webui(app: &Arc<ApiState>) -> anyhow::Result<()> {
    let state = db::component_update(&app.db, "webui").await?;
    let storage = storage()?;
    let root = storage.get("index.html").await?;
    let admin = storage.get("administration/index.html").await?;
    if let Some(release) = state.desired.as_ref() {
        let prefix = format!("/assets/releases/{}/", release.source_sha);
        if root.as_ref().is_some_and(|bytes| {
            bytes
                .windows(prefix.len())
                .any(|part| part == prefix.as_bytes())
        }) && admin.as_ref().is_some_and(|bytes| {
            bytes
                .windows(prefix.len())
                .any(|part| part == prefix.as_bytes())
        }) {
            let registry = Registry::ghcr()?;
            if let Some((root_hash, admin_hash)) =
                storage.verify_release(&registry, release).await?
            {
                db::record_component_running(
                    &app.db,
                    "webui",
                    Some(release.clone()),
                    ComponentObservation {
                        guild_schema: None,
                        update_protocol: 0,
                        launcher_revision: 0,
                        source_sha: release.source_sha.clone(),
                        executable_sha256: root_hash,
                        admin_sha256: Some(admin_hash),
                        base_image_sha: "Storage Zone".into(),
                        started_at: chrono::Utc::now().timestamp(),
                        min_schema: release.min_schema,
                        max_schema: release.max_schema,
                        api_capability: 1,
                        self_update_enabled: true,
                        base_abi: "static-webui-v1".into(),
                    },
                    chrono::Utc::now().timestamp(),
                )
                .await?;
                return Ok(());
            }
        }
    }
    if let Some(installed) = state.installed.as_ref() {
        let registry = Registry::ghcr()?;
        if let Some((root_hash, admin_hash)) = storage.verify_release(&registry, installed).await? {
            let now = chrono::Utc::now().timestamp();
            db::record_webui_installed_verified(
                &app.db,
                installed,
                ComponentObservation {
                    guild_schema: None,
                    update_protocol: 0,
                    launcher_revision: 0,
                    source_sha: installed.source_sha.clone(),
                    executable_sha256: root_hash,
                    admin_sha256: Some(admin_hash),
                    base_image_sha: "Storage Zone".into(),
                    started_at: state.observed.as_ref().map_or(now, |seen| seen.started_at),
                    min_schema: installed.min_schema,
                    max_schema: installed.max_schema,
                    api_capability: 1,
                    self_update_enabled: true,
                    base_abi: "static-webui-v1".into(),
                },
                now,
            )
            .await?;
            return Ok(());
        }
    }
    if state.observed.is_some() {
        db::record_webui_drift(&app.db, "Storage files differ from the installed release").await?;
    }
    Ok(())
}

async fn cleanup_webui_assets(app: &Arc<ApiState>, storage: &Storage) -> anyhow::Result<()> {
    let state = db::component_update(&app.db, "webui").await?;
    let root = storage.get("index.html").await?.unwrap_or_default();
    let admin = storage
        .get("administration/index.html")
        .await?
        .unwrap_or_default();
    for sha in db::managed_webui_assets(&app.db).await? {
        let referenced = format!("/assets/releases/{sha}/");
        let keep = state
            .installed
            .as_ref()
            .is_some_and(|r| r.source_sha == sha)
            || state.previous.as_ref().is_some_and(|r| r.source_sha == sha)
            || root
                .windows(referenced.len())
                .any(|part| part == referenced.as_bytes())
            || admin
                .windows(referenced.len())
                .any(|part| part == referenced.as_bytes());
        if !keep {
            storage.prune_release(&sha).await?;
            db::untrack_webui_assets(&app.db, &sha).await?;
        }
    }
    Ok(())
}

/// Read-only release poll. A changed candidate is logged and not recorded.
/// Discovery is injected so tests do not call the registry.
pub async fn poll_releases(
    app: &Arc<ApiState>,
    mut discover: impl AsyncFnMut(&str, &str) -> anyhow::Result<String>,
) {
    for component in ["bot", "api", "webui"] {
        let state = match db::component_update(&app.db, component).await {
            Ok(state) => state,
            Err(error) => {
                tracing::warn!(component, "release read failed: {error:#}");
                continue;
            }
        };
        match discover(component, &state.channel).await {
            Ok(digest) => {
                let unchanged = state
                    .candidate
                    .as_ref()
                    .is_some_and(|candidate| candidate.digest == digest);
                if unchanged {
                    tracing::info!(component, "release candidate unchanged");
                } else {
                    tracing::info!(
                        component,
                        "release candidate changed; automatic poll does not record it"
                    );
                }
            }
            Err(error) => tracing::warn!(component, "release discovery failed: {error:#}"),
        }
    }
}

/// API process timer is independent of browser sessions and auto-apply policy.
pub async fn poll_all(app: &Arc<ApiState>) {
    poll_releases(app, async |component, channel| {
        let registry = Registry::ghcr()?;
        Ok(registry.discover(component, channel).await?.digest)
    })
    .await;
    for component in ["bot", "api", "webui"] {
        if enabled("JMODGUARD_UNATTENDED_UPDATES")
            && enabled("JMODGUARD_UPDATE_APPLY_ENABLED")
            && let Err(error) = automatic_install(app, component).await
        {
            tracing::warn!(component, "automatic update skipped: {error:#}");
        }
    }
}

async fn automatic_install(app: &Arc<ApiState>, component: &str) -> anyhow::Result<()> {
    let state = db::component_update(&app.db, component).await?;
    if state.cadence == "off" || state.check_error.is_some() {
        return Ok(());
    }
    let Some(candidate) = state.candidate else {
        return Ok(());
    };
    if component == "webui" {
        storage()?;
    } else if !state
        .observed
        .as_ref()
        .is_some_and(|v| v.self_update_enabled)
    {
        return Ok(());
    }
    let now = chrono::Utc::now().timestamp();
    jmodguard_updater::compatible(&app.db, &candidate, now).await?;
    let lease = if component == "webui" {
        let lease = format!("{:032x}", rand::random::<u128>());
        if !db::claim_update_lease(&app.db, component, &lease, now, 600).await? {
            return Ok(());
        }
        Some(lease)
    } else {
        None
    };
    let requested =
        db::request_due_automatic_install(&app.db, component, &candidate.digest, now, true).await;
    let release = match requested {
        Ok(Some(release)) => release,
        Ok(None) => {
            if let Some(lease) = &lease {
                db::release_update_lease(&app.db, component, lease).await?;
            }
            return Ok(());
        }
        Err(error) => {
            if let Some(lease) = &lease {
                db::release_update_lease(&app.db, component, lease).await?;
            }
            return Err(error);
        }
    };
    if component == "webui" {
        let worker = Arc::clone(app);
        let lease = lease.expect("WebUI install owns its lease");
        tokio::spawn(async move {
            if let Err(error) = run_webui_install(&worker, release, &lease).await {
                tracing::error!("automatic WebUI install failed: {error:#}");
            }
        });
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct PolicyRequest {
    component: String,
    channel: String,
    cadence: String,
}

pub async fn policy(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Json(input): Json<PolicyRequest>,
) -> Reply {
    owner(&app, &session).await?;
    origin(&app, &headers)?;
    db::set_update_policy(
        &app.db,
        &input.component,
        &input.channel,
        &input.cadence,
        enabled("JMODGUARD_UNATTENDED_UPDATES"),
    )
    .await
    .map_err(|problem| error(StatusCode::BAD_REQUEST, &format!("{problem:#}")))?;
    status(State(app), session).await
}

#[derive(Deserialize)]
pub struct InstallRequest {
    component: String,
    digest: String,
    #[serde(default)]
    launcher: bool,
}

async fn queue_webui_install(
    db: &Db,
    digest: &str,
    now: i64,
) -> anyhow::Result<(ComponentRelease, String)> {
    let lease = format!("{:032x}", rand::random::<u128>());
    anyhow::ensure!(
        db::claim_update_lease(db, "webui", &lease, now, 600).await?,
        "another WebUI operation is running"
    );
    match db::request_component_install(db, "webui", digest, now).await {
        Ok(release) => Ok((release, lease)),
        Err(error) => {
            db::release_update_lease(db, "webui", &lease).await?;
            Err(error)
        }
    }
}

pub async fn install(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Json(input): Json<InstallRequest>,
) -> Reply {
    owner(&app, &session).await?;
    origin(&app, &headers)?;
    if !enabled("JMODGUARD_UPDATE_APPLY_ENABLED") {
        return Err(error(
            StatusCode::CONFLICT,
            "on-demand updates are disabled during rollout",
        ));
    }
    if !matches!(input.component.as_str(), "bot" | "api" | "webui")
        || (input.launcher && input.component == "webui")
    {
        return Err(error(StatusCode::BAD_REQUEST, "invalid update target"));
    }
    let record = db::component_update(&app.db, &input.component)
        .await
        .map_err(|_| error(StatusCode::SERVICE_UNAVAILABLE, "update state unavailable"))?;
    let candidate = record
        .candidate
        .clone()
        .ok_or_else(|| error(StatusCode::CONFLICT, "check for an update first"))?;
    if candidate.digest != input.digest
        || candidate.channel != record.channel
        || record.check_error.is_some()
    {
        return Err(error(
            StatusCode::CONFLICT,
            "candidate changed; check again",
        ));
    }
    let now = chrono::Utc::now().timestamp();
    if record
        .checked_at
        .is_none_or(|checked| checked > now || now.saturating_sub(checked) >= 7200)
    {
        return Err(error(
            StatusCode::CONFLICT,
            "release check is stale; check again",
        ));
    }
    let already_installed = if input.launcher {
        record.installed_launcher.as_ref()
    } else {
        record.installed.as_ref()
    };
    if input.component != "webui"
        && already_installed.is_some_and(|installed| installed.digest == candidate.digest)
    {
        return Err(error(
            StatusCode::CONFLICT,
            "selected release is already installed",
        ));
    }
    if input.component == "webui"
        && [&record.installed, &record.previous, &record.desired]
            .into_iter()
            .flatten()
            .any(|release| {
                release.source_sha == candidate.source_sha && release.digest != candidate.digest
            })
    {
        return Err(error(
            StatusCode::CONFLICT,
            "WebUI source SHA was republished with a different image digest; file bytes may differ, so use a new source commit",
        ));
    }
    if input.launcher {
        let advice = jmodguard_updater::launcher_status(&record, now);
        if advice["installable"] != true {
            return Err(error(
                StatusCode::CONFLICT,
                advice["reason"]
                    .as_str()
                    .unwrap_or("launcher update is not required or available"),
            ));
        }
    } else {
        jmodguard_updater::compatible(&app.db, &candidate, now)
            .await
            .map_err(|problem| {
                error(
                    StatusCode::CONFLICT,
                    &format!("candidate is incompatible: {problem:#}"),
                )
            })?;
    }
    if input.component == "webui" && storage().is_err() {
        return Err(error(
            StatusCode::CONFLICT,
            "WebUI Storage is not configured",
        ));
    }
    if input.component != "webui"
        && !record
            .observed
            .as_ref()
            .is_some_and(|state| state.self_update_enabled)
    {
        return Err(error(
            StatusCode::CONFLICT,
            "service self-update is disabled or unobserved",
        ));
    }
    let (release, lease) = if input.component == "webui" {
        let (release, lease) = queue_webui_install(&app.db, &input.digest, now)
            .await
            .map_err(|_| {
                error(
                    StatusCode::CONFLICT,
                    "another WebUI operation is running, candidate check changed, or this source SHA has a different or unknown image digest; check again",
                )
            })?;
        (release, Some(lease))
    } else if input.launcher {
        (
            db::request_launcher_install(&app.db, &input.component, &input.digest, now)
                .await
                .map_err(|_| {
                    error(
                        StatusCode::CONFLICT,
                        "candidate check changed or launcher already installed; check again",
                    )
                })?,
            None,
        )
    } else {
        (
            db::request_component_install(&app.db, &input.component, &input.digest, now)
                .await
                .map_err(|_| {
                    error(
                        StatusCode::CONFLICT,
                        "candidate check changed or application already installed; check again",
                    )
                })?,
            None,
        )
    };
    if input.component == "webui" {
        let worker = Arc::clone(&app);
        let lease = lease.expect("WebUI install owns its lease");
        tokio::spawn(async move {
            if let Err(error) = run_webui_install(&worker, release, &lease).await {
                tracing::error!("WebUI install failed: {error:#}");
            }
        });
    }
    Ok(Json(
        json!({"queued":true,"component":input.component,"digest":input.digest}),
    ))
}

async fn run_webui_install(
    app: &Arc<ApiState>,
    release: ComponentRelease,
    lease: &str,
) -> anyhow::Result<()> {
    with_update_lease(&app.db, "webui", lease, 600, async {
        let result = apply_webui(app, &release).await;
        if let Err(error) = &result {
            db::record_component_failure(&app.db, "webui", &release, &format!("{error:#}")).await?;
            if let Err(reconcile_error) = reconcile_webui(app).await {
                tracing::warn!("WebUI failure reconciliation deferred: {reconcile_error:#}");
            }
        }
        result
    })
    .await
}

async fn apply_webui(app: &Arc<ApiState>, release: &ComponentRelease) -> anyhow::Result<()> {
    let storage = storage()?;
    let now = chrono::Utc::now().timestamp();
    let result = async {
        db::track_webui_assets(&app.db, &release.source_sha, now).await?;
        let registry = Registry::ghcr()?;
        let _files = storage.publish(&registry, release).await?;
        let index = storage
            .get("index.html")
            .await?
            .ok_or_else(|| anyhow::anyhow!("WebUI entry HTML is missing after switch"))?;
        let admin = storage
            .get("administration/index.html")
            .await?
            .ok_or_else(|| anyhow::anyhow!("Administration entry HTML is missing after switch"))?;
        let observation = ComponentObservation {
            guild_schema: None,
            update_protocol: 0,
            launcher_revision: 0,
            source_sha: release.source_sha.clone(),
            executable_sha256: sha256_hex(&index),
            admin_sha256: Some(sha256_hex(&admin)),
            base_image_sha: "Storage Zone".into(),
            started_at: chrono::Utc::now().timestamp(),
            min_schema: release.min_schema,
            max_schema: release.max_schema,
            api_capability: 1,
            self_update_enabled: true,
            base_abi: "static-webui-v1".into(),
        };
        db::record_component_running(
            &app.db,
            "webui",
            Some(release.clone()),
            observation,
            chrono::Utc::now().timestamp(),
        )
        .await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = cleanup_webui_assets(app, &storage).await {
        tracing::warn!("WebUI managed asset cleanup deferred: {error:#}");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn live_webui_worker_renews_lease_through_slow_work() {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_webui_renew_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = Db::open(&path, 2).unwrap();
        let first = "a".repeat(32);
        let second = "b".repeat(32);
        let now = chrono::Utc::now().timestamp();
        assert!(
            db::claim_update_lease(&db, "webui", &first, now, 4)
                .await
                .unwrap()
        );
        with_update_lease(&db, "webui", &first, 4, async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            assert!(
                !db::claim_update_lease(&db, "webui", &second, chrono::Utc::now().timestamp(), 4,)
                    .await?
            );
            Ok(())
        })
        .await
        .unwrap();
        assert!(
            db::claim_update_lease(&db, "webui", &second, chrono::Utc::now().timestamp(), 4,)
                .await
                .unwrap()
        );
        let started = std::cell::Cell::new(false);
        assert!(
            with_update_lease(&db, "webui", &first, 4, async {
                started.set(true);
                Ok(())
            })
            .await
            .is_err()
        );
        assert!(!started.get());
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn check_lease_blocks_webui_queue_before_acknowledgement() {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_webui_queue_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = Db::open(&path, 2).unwrap();
        let release = ComponentRelease {
            upgrade: None,
            component: "webui".into(),
            channel: "latest".into(),
            release_id: "v1.0.0".into(),
            source_sha: "a".repeat(40),
            published_at: "2026-09-23T00:00:00Z".into(),
            image: "ghcr.io/jatmn/jmodguard-webui".into(),
            digest: format!("sha256:{}", "b".repeat(64)),
            min_schema: 15,
            max_schema: 15,
            result_schema: 15,
            min_api: 1,
            max_api: 1,
            launcher_protocol: 1,
            launcher: None,
            base_abi: "static-webui-v1".into(),
        };
        db::record_update_check(
            &db,
            "webui",
            &release.channel,
            Some(release.clone()),
            None,
            100,
        )
        .await
        .unwrap();
        let check_lease = "c".repeat(32);
        assert!(
            db::claim_update_lease(&db, "webui", &check_lease, 100, 240)
                .await
                .unwrap()
        );
        assert!(
            queue_webui_install(&db, &release.digest, 101)
                .await
                .is_err()
        );
        assert!(
            db::component_update(&db, "webui")
                .await
                .unwrap()
                .desired
                .is_none()
        );
        db::release_update_lease(&db, "webui", &check_lease)
            .await
            .unwrap();
        let (queued, lease) = queue_webui_install(&db, &release.digest, 102)
            .await
            .unwrap();
        assert_eq!(queued.digest, release.digest);
        assert_eq!(
            db::component_update(&db, "webui")
                .await
                .unwrap()
                .desired
                .unwrap()
                .digest,
            release.digest
        );
        db::release_update_lease(&db, "webui", &lease)
            .await
            .unwrap();
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
}
