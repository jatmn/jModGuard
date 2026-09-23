//! Owner-only independent release discovery and on-demand application.
use super::state::ApiState;
use super::{is_bot_owner, require_auth, trusted_web_origin};
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use jmodguard::db::{self, ComponentObservation, ComponentRelease};
use jmodguard_updater::oci::{Registry, sha256_hex};
use jmodguard_updater::storage::Storage;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tower_sessions::Session;

fn error(status: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error":message})))
}

type Reply = Result<Json<Value>, (StatusCode, Json<Value>)>;

async fn owner(app: &Arc<ApiState>, session: &Session) -> Result<(), (StatusCode, Json<Value>)> {
    let auth = require_auth(app, session)
        .await
        .map_err(|_| error(StatusCode::UNAUTHORIZED, "sign in required"))?;
    if !is_bot_owner(app, &auth) {
        return Err(error(StatusCode::FORBIDDEN, "bot owner required"));
    }
    Ok(())
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
    let now = chrono::Utc::now().timestamp();
    for row in &updates {
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
        "compatibility":compatibility,
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
    let checked = async {
        let state = db::component_update(&app.db, component).await?;
        let registry = Registry::ghcr()?;
        match registry.discover(component, &state.channel).await {
            Ok(candidate) => {
                db::record_update_check(&app.db, component, Some(candidate), None, now).await
            }
            Err(error) => {
                db::record_update_check(&app.db, component, None, Some(format!("{error:#}")), now)
                    .await?;
                Err(error)
            }
        }
    }
    .await;
    if component == "webui" {
        if let Err(error) = reconcile_webui(app).await {
            tracing::warn!("WebUI Storage reconciliation failed: {error:#}");
        }
        if let Ok(storage) = storage()
            && let Err(error) = cleanup_webui_assets(app, &storage).await
        {
            tracing::warn!("WebUI managed asset cleanup failed: {error:#}");
        }
    }
    db::release_update_lease(&app.db, component, &lease).await?;
    checked
}

async fn reconcile_webui(app: &Arc<ApiState>) -> anyhow::Result<()> {
    let state = db::component_update(&app.db, "webui").await?;
    let storage = storage()?;
    let root = storage.get("index.html").await?;
    let admin = storage.get("administration/index.html").await?;
    if state.observed.as_ref().is_some_and(|observed| {
        root.as_ref()
            .is_some_and(|bytes| sha256_hex(bytes) == observed.executable_sha256)
            && observed.admin_sha256.as_ref().is_none_or(|hash| {
                admin
                    .as_ref()
                    .is_some_and(|bytes| sha256_hex(bytes) == *hash)
            })
    }) {
        return Ok(());
    }
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
    if state.observed.is_some() {
        db::record_webui_drift(&app.db, "Storage HTML differs from the installed release").await?;
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

/// API process timer is independent of browser sessions and auto-apply policy.
pub async fn poll_all(app: &Arc<ApiState>) {
    for component in ["bot", "api", "webui"] {
        if let Err(error) = check_one(app, component).await {
            tracing::warn!(component, "release discovery failed: {error:#}");
        }
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
    let Some(release) =
        db::request_due_automatic_install(&app.db, component, &candidate.digest, now, true).await?
    else {
        return Ok(());
    };
    if component == "webui" {
        let worker = Arc::clone(app);
        tokio::spawn(async move {
            if let Err(error) = apply_webui(&worker, release).await {
                tracing::error!("automatic WebUI install failed: {error:#}");
                let _ =
                    db::record_component_failure(&worker.db, "webui", &format!("{error:#}")).await;
                let _ = reconcile_webui(&worker).await;
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
    if input.component == "webui"
        && record.installed.as_ref().is_some_and(|installed| {
            installed.source_sha == candidate.source_sha && installed.digest != candidate.digest
        })
    {
        return Err(error(
            StatusCode::CONFLICT,
            "WebUI source SHA was republished with different bytes; use a new source commit",
        ));
    }
    let now = chrono::Utc::now().timestamp();
    jmodguard_updater::compatible(&app.db, &candidate, now)
        .await
        .map_err(|_| {
            error(
                StatusCode::CONFLICT,
                "candidate is incompatible with current services",
            )
        })?;
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
    let release = if input.launcher {
        db::request_launcher_install(&app.db, &input.component, &input.digest).await
    } else {
        db::request_component_install(&app.db, &input.component, &input.digest).await
    }
    .map_err(|_| error(StatusCode::CONFLICT, "candidate changed; check again"))?;
    if input.component == "webui" {
        let worker = Arc::clone(&app);
        tokio::spawn(async move {
            if let Err(error) = apply_webui(&worker, release).await {
                tracing::error!("WebUI install failed: {error:#}");
                let _ =
                    db::record_component_failure(&worker.db, "webui", &format!("{error:#}")).await;
                let _ = reconcile_webui(&worker).await;
            }
        });
    }
    Ok(Json(
        json!({"queued":true,"component":input.component,"digest":input.digest}),
    ))
}

async fn apply_webui(app: &Arc<ApiState>, release: ComponentRelease) -> anyhow::Result<()> {
    let storage = storage()?;
    let lease = format!("{:032x}", rand::random::<u128>());
    let now = chrono::Utc::now().timestamp();
    if !db::claim_update_lease(&app.db, "webui", &lease, now, 600).await? {
        anyhow::bail!("another WebUI operation is running");
    }
    let result = async {
        db::track_webui_assets(&app.db, &release.source_sha, now).await?;
        let registry = Registry::ghcr()?;
        let _files = storage.publish(&registry, &release).await?;
        let index = storage
            .get("index.html")
            .await?
            .ok_or_else(|| anyhow::anyhow!("WebUI entry HTML is missing after switch"))?;
        let admin = storage
            .get("administration/index.html")
            .await?
            .ok_or_else(|| anyhow::anyhow!("Administration entry HTML is missing after switch"))?;
        let observation = ComponentObservation {
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
    db::release_update_lease(&app.db, "webui", &lease).await?;
    result
}
